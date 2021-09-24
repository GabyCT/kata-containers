// Copyright (c) 2021 Apple Inc.
//
// SPDX-License-Identifier: Apache-2.0
//

#![allow(unknown_lints)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{ensure, Context, Result};
use async_recursion::async_recursion;
use nix::mount::{umount, MsFlags};
use slog::{debug, error, info, warn, Logger};
use thiserror::Error;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{self, Duration};

use crate::mount::baremount;
use crate::protocols::agent as protos;

/// The maximum number of file system entries agent will watch for each mount.
const MAX_ENTRIES_PER_STORAGE: usize = 16;

/// The maximum size of a watchable mount in bytes.
const MAX_SIZE_PER_WATCHABLE_MOUNT: u64 = 1024 * 1024;

/// How often to check for modified files.
const WATCH_INTERVAL_SECS: u64 = 2;

/// Destination path for tmpfs
const WATCH_MOUNT_POINT_PATH: &str = "/run/kata-containers/shared/containers/watchable/";

/// Represents a single watched storage entry which may have multiple files to watch.
#[derive(Default, Debug, Clone)]
struct Storage {
    /// A mount point without inotify capabilities.
    source_mount_point: PathBuf,

    /// The target mount point, where the watched files will be copied/mirrored
    /// when being changed, added or removed. This will be subdirectory of a tmpfs
    target_mount_point: PathBuf,

    /// Flag to indicate that the Storage should be watched. Storage will be watched until
    /// the source becomes too large, either in number of files (>16) or total size (>1MB).
    watch: bool,

    /// The list of files to watch from the source mount point and updated in the target one.
    watched_files: HashMap<PathBuf, SystemTime>,
}

#[derive(Error, Debug)]
pub enum WatcherError {
    #[error(
        "Too many file system entries within to watch within: {mnt} ({count} must be < {})",
        MAX_ENTRIES_PER_STORAGE
    )]
    MountTooManyFiles { count: usize, mnt: String },

    #[error(
        "Mount too large to watch: {mnt} ({size} must be < {})",
        MAX_SIZE_PER_WATCHABLE_MOUNT
    )]
    MountTooLarge { size: u64, mnt: String },
}

impl Drop for Storage {
    fn drop(&mut self) {
        if !&self.watch {
            // If we weren't watching this storage entry, it means that a bind mount
            // was created.
            let _ = umount(&self.target_mount_point);
        }
        let _ = std::fs::remove_dir_all(&self.target_mount_point);
    }
}

impl Storage {
    async fn new(storage: protos::Storage) -> Result<Storage> {
        let entry = Storage {
            source_mount_point: PathBuf::from(&storage.source),
            target_mount_point: PathBuf::from(&storage.mount_point),
            watch: true,
            watched_files: HashMap::new(),
        };
        Ok(entry)
    }

    async fn update_target(&self, logger: &Logger, source_path: impl AsRef<Path>) -> Result<()> {
        let source_file_path = source_path.as_ref();

        let dest_file_path = if self.source_mount_point.is_file() {
            // Simple file to file copy
            // Assume target mount is a file path
            self.target_mount_point.clone()
        } else {
            let dest_file_path = self.make_target_path(&source_file_path)?;

            if let Some(path) = dest_file_path.parent() {
                debug!(logger, "Creating destination directory: {}", path.display());
                fs::create_dir_all(path)
                    .await
                    .with_context(|| format!("Unable to mkdir all for {}", path.display()))?;
            }

            dest_file_path
        };

        debug!(
            logger,
            "Copy from {} to {}",
            source_file_path.display(),
            dest_file_path.display()
        );
        fs::copy(&source_file_path, &dest_file_path)
            .await
            .with_context(|| {
                format!(
                    "Copy from {} to {} failed",
                    source_file_path.display(),
                    dest_file_path.display()
                )
            })?;

        Ok(())
    }

    async fn scan(&mut self, logger: &Logger) -> Result<usize> {
        debug!(logger, "Scanning for changes");

        let mut remove_list = Vec::new();
        let mut updated_files: Vec<PathBuf> = Vec::new();

        // Remove deleted files for tracking list
        self.watched_files.retain(|st, _| {
            if st.exists() {
                true
            } else {
                remove_list.push(st.to_path_buf());
                false
            }
        });

        // Delete from target
        for path in remove_list {
            // File has been deleted, remove it from target mount
            let target = self.make_target_path(path)?;
            debug!(logger, "Removing file from mount: {}", target.display());
            let _ = fs::remove_file(target).await;
        }

        // Scan new & changed files
        self.scan_path(
            logger,
            self.source_mount_point.clone().as_path(),
            &mut updated_files,
        )
        .await
        .with_context(|| "Scan path failed")?;

        // Update identified files:
        for path in &updated_files {
            if let Err(e) = self.update_target(logger, path.as_path()).await {
                error!(logger, "failure in update_target: {:?}", e);
            }
        }

        Ok(updated_files.len())
    }

    #[async_recursion]
    async fn scan_path(
        &mut self,
        logger: &Logger,
        path: &Path,
        update_list: &mut Vec<PathBuf>,
    ) -> Result<u64> {
        let mut size: u64 = 0;
        debug!(logger, "Scanning path: {}", path.display());

        if path.is_file() {
            let metadata = path
                .metadata()
                .with_context(|| format!("Failed to query metadata for: {}", path.display()))?;

            let modified = metadata
                .modified()
                .with_context(|| format!("Failed to get modified date for: {}", path.display()))?;

            size += metadata.len();

            // Insert will return old entry if any
            if let Some(old_st) = self.watched_files.insert(path.to_path_buf(), modified) {
                if modified > old_st {
                    update_list.push(PathBuf::from(&path))
                }
            } else {
                // Storage just added, copy to target
                debug!(logger, "New entry: {}", path.display());
                update_list.push(PathBuf::from(&path))
            }

            ensure!(
                self.watched_files.len() <= MAX_ENTRIES_PER_STORAGE,
                WatcherError::MountTooManyFiles {
                    count: self.watched_files.len(),
                    mnt: self.source_mount_point.display().to_string()
                }
            );
        } else {
            // Scan dir recursively
            let mut entries = fs::read_dir(path)
                .await
                .with_context(|| format!("Failed to read dir: {}", path.display()))?;

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let res_size = self
                    .scan_path(logger, path.as_path(), update_list)
                    .await
                    .with_context(|| format!("Unable to scan inner path: {}", path.display()))?;
                size += res_size;
            }
        }

        ensure!(
            size <= MAX_SIZE_PER_WATCHABLE_MOUNT,
            WatcherError::MountTooLarge {
                size,
                mnt: self.source_mount_point.display().to_string()
            }
        );

        Ok(size)
    }

    fn make_target_path(&self, source_file_path: impl AsRef<Path>) -> Result<PathBuf> {
        let relative_path = source_file_path
            .as_ref()
            .strip_prefix(&self.source_mount_point)
            .with_context(|| {
                format!(
                    "Failed to strip prefix: {} - {}",
                    source_file_path.as_ref().display().to_string(),
                    &self.source_mount_point.display()
                )
            })?;

        let dest_file_path = Path::new(&self.target_mount_point).join(relative_path);
        Ok(dest_file_path)
    }
}

#[derive(Default, Debug)]
struct SandboxStorages(Vec<Storage>);

impl SandboxStorages {
    async fn add(
        &mut self,
        list: impl IntoIterator<Item = protos::Storage>,

        logger: &Logger,
    ) -> Result<()> {
        for storage in list.into_iter() {
            let entry = Storage::new(storage)
                .await
                .with_context(|| "Failed to add storage")?;

            // If the storage source is a directory, let's create the target mount point:
            if entry.source_mount_point.as_path().is_dir() {
                fs::create_dir_all(&entry.target_mount_point)
                    .await
                    .with_context(|| {
                        format!(
                            "Unable to mkdir all for {}",
                            entry.target_mount_point.display()
                        )
                    })?;
            }

            self.0.push(entry);
        }

        // Perform initial copy
        self.check(logger)
            .await
            .with_context(|| "Failed to perform initial check")?;

        Ok(())
    }

    async fn check(&mut self, logger: &Logger) -> Result<()> {
        for entry in self.0.iter_mut().filter(|e| e.watch) {
            if let Err(e) = entry.scan(logger).await {
                match e.downcast_ref::<WatcherError>() {
                    Some(WatcherError::MountTooLarge { .. })
                    | Some(WatcherError::MountTooManyFiles { .. }) => {
                        //
                        // If the mount we were watching is too large (bytes), or contains too many unique files,
                        // we no longer want to watch. Instead, we'll attempt to create a bind mount and mark this storage
                        // as non-watchable. if there's an error in creating bind mount, we'll continue watching.
                        //
                        // Ensure the target mount point exists:
                        if !entry.target_mount_point.as_path().exists() {
                            if entry.source_mount_point.as_path().is_dir() {
                                fs::create_dir_all(entry.target_mount_point.as_path())
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "create dir for bindmount {:?}",
                                            entry.target_mount_point.as_path()
                                        )
                                    })?;
                            } else {
                                fs::File::create(entry.target_mount_point.as_path())
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "create file {:?}",
                                            entry.target_mount_point.as_path()
                                        )
                                    })?;
                            }
                        }

                        match baremount(
                            entry.source_mount_point.to_str().unwrap(),
                            entry.target_mount_point.to_str().unwrap(),
                            "bind",
                            MsFlags::MS_BIND,
                            "bind",
                            logger,
                        ) {
                            Ok(_) => {
                                entry.watch = false;
                                info!(logger, "watchable mount replaced with bind mount")
                            }
                            Err(e) => error!(logger, "unable to replace watchable: {:?}", e),
                        }
                    }
                    _ => warn!(logger, "scan error: {:?}", e),
                }
            }
        }

        Ok(())
    }
}

/// Handles watchable mounts. The watcher will manage one or more mounts for one or more containers. For each
/// mount that is added, the watcher will maintain a list of files to monitor, and periodically checks for new,
/// removed or changed (modified date) files. When a change is identified, the watcher will either copy the new
/// or updated file to a target mount point, or remove the removed file from the target mount point.  All WatchableStorage
/// target mount points are expected to reside within a single tmpfs, whose root is created by the BindWatcher.
///
/// This is a temporary workaround to handle config map updates until we get inotify on 9p/virtio-fs.
/// More context on this:
/// - https://github.com/kata-containers/runtime/issues/1505
/// - https://github.com/kata-containers/kata-containers/issues/1879
#[derive(Debug, Default)]
pub struct BindWatcher {
    /// Container ID -> Vec of watched entries
    sandbox_storages: Arc<Mutex<HashMap<String, SandboxStorages>>>,
    watch_thread: Option<task::JoinHandle<()>>,
}

impl Drop for BindWatcher {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl BindWatcher {
    pub fn new() -> BindWatcher {
        Default::default()
    }

    pub async fn add_container(
        &mut self,
        id: String,
        mounts: impl IntoIterator<Item = protos::Storage>,
        logger: &Logger,
    ) -> Result<()> {
        if self.watch_thread.is_none() {
            // Virtio-fs shared path is RO by default, so we back the target-mounts by tmpfs.
            self.mount(logger).await?;

            // Spawn background thread to monitor changes
            self.watch_thread = Some(Self::spawn_watcher(
                logger.clone(),
                Arc::clone(&self.sandbox_storages),
                WATCH_INTERVAL_SECS,
            ));
        }

        self.sandbox_storages
            .lock()
            .await
            .entry(id)
            .or_insert_with(SandboxStorages::default)
            .add(mounts, logger)
            .await
            .with_context(|| "Failed to add container")?;

        Ok(())
    }

    pub async fn remove_container(&self, id: &str) {
        self.sandbox_storages.lock().await.remove(id);
    }

    fn spawn_watcher(
        logger: Logger,
        sandbox_storages: Arc<Mutex<HashMap<String, SandboxStorages>>>,
        interval_secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(interval_secs));

            loop {
                interval.tick().await;

                debug!(&logger, "Looking for changed files");
                for (_, entries) in sandbox_storages.lock().await.iter_mut() {
                    if let Err(err) = entries.check(&logger).await {
                        // We don't fail background loop, but rather log error instead.
                        warn!(logger, "Check failed: {}", err);
                    }
                }
            }
        })
    }

    async fn mount(&self, logger: &Logger) -> Result<()> {
        fs::create_dir_all(WATCH_MOUNT_POINT_PATH).await?;

        baremount(
            "tmpfs",
            WATCH_MOUNT_POINT_PATH,
            "tmpfs",
            MsFlags::empty(),
            "",
            logger,
        )?;

        Ok(())
    }

    fn cleanup(&mut self) {
        if let Some(handle) = self.watch_thread.take() {
            // Stop our background thread
            handle.abort();
        }

        let _ = umount(WATCH_MOUNT_POINT_PATH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::is_mounted;
    use crate::skip_if_not_root;
    use std::fs;
    use std::thread;

    async fn create_test_storage(dir: &Path, id: &str) -> Result<(protos::Storage, PathBuf)> {
        let src_path = dir.join(format!("src{}", id));
        let src_filename = src_path.to_str().expect("failed to create src filename");
        let dest_path = dir.join(format!("dest{}", id));
        let dest_filename = dest_path.to_str().expect("failed to create dest filename");

        std::fs::create_dir_all(src_filename).expect("failed to create path");

        let storage = protos::Storage {
            source: src_filename.to_string(),
            mount_point: dest_filename.to_string(),
            ..Default::default()
        };

        Ok((storage, src_path))
    }

    #[tokio::test]
    async fn test_empty_sourcedir_check() {
        //skip_if_not_root!();
        let dir = tempfile::tempdir().expect("failed to create tempdir");

        let logger = slog::Logger::root(slog::Discard, o!());

        let src_path = dir.path().join("src");
        let dest_path = dir.path().join("dest");
        let src_filename = src_path.to_str().expect("failed to create src filename");
        let dest_filename = dest_path.to_str().expect("failed to create dest filename");

        std::fs::create_dir_all(src_filename).expect("failed to create path");

        let storage = protos::Storage {
            source: src_filename.to_string(),
            mount_point: dest_filename.to_string(),
            ..Default::default()
        };

        let mut entries = SandboxStorages {
            ..Default::default()
        };

        entries
            .add(std::iter::once(storage), &logger)
            .await
            .unwrap();

        assert!(entries.check(&logger).await.is_ok());
        assert_eq!(entries.0.len(), 1);

        assert_eq!(std::fs::read_dir(src_path).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(dest_path).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn test_single_file_check() {
        //skip_if_not_root!();
        let dir = tempfile::tempdir().expect("failed to create tempdir");

        let logger = slog::Logger::root(slog::Discard, o!());

        let src_file_path = dir.path().join("src.txt");
        let dest_file_path = dir.path().join("dest.txt");

        let src_filename = src_file_path
            .to_str()
            .expect("failed to create src filename");
        let dest_filename = dest_file_path
            .to_str()
            .expect("failed to create dest filename");

        let storage = protos::Storage {
            source: src_filename.to_string(),
            mount_point: dest_filename.to_string(),
            ..Default::default()
        };

        //create file
        fs::write(src_file_path, "original").unwrap();

        let mut entries = SandboxStorages::default();

        entries
            .add(std::iter::once(storage), &logger)
            .await
            .unwrap();

        assert!(entries.check(&logger).await.is_ok());
        assert_eq!(entries.0.len(), 1);

        // there should only be 2 files
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);

        assert_eq!(fs::read_to_string(dest_file_path).unwrap(), "original");
    }

    #[tokio::test]
    async fn test_watch_entries() {
        skip_if_not_root!();

        // If there's an error with an entry, let's make sure it is removed, and that the
        // mount-destination behaves like a standard bind-mount.

        // Create an entries vector with four storage objects: storage0,1,2,3.
        // 0th we'll have fail due to too many files before running a check
        // 1st will just have a single medium sized file, we'll keep it watchable throughout
        // 2nd will have a large file (<1MB), but we'll later make larger to make unwatchable
        // 3rd will have several files, and later we'll make unwatchable by having too many files.
        // We'll run check a couple of times to verify watchable is always watchable, and unwatchable bind mounts
        // match our expectations.
        let dir = tempfile::tempdir().expect("failed to create tempdir");

        let (storage0, src0_path) = create_test_storage(dir.path(), "1")
            .await
            .expect("failed to create storage");
        let (storage1, src1_path) = create_test_storage(dir.path(), "2")
            .await
            .expect("failed to create storage");
        let (storage2, src2_path) = create_test_storage(dir.path(), "3")
            .await
            .expect("failed to create storage");
        let (storage3, src3_path) = create_test_storage(dir.path(), "4")
            .await
            .expect("failed to create storage");

        // setup storage0: too many files
        for i in 1..21 {
            fs::write(src0_path.join(format!("{}.txt", i)), "original").unwrap();
        }

        // setup storage1: two small files
        std::fs::File::create(src1_path.join("small.txt"))
            .unwrap()
            .set_len(10)
            .unwrap();
        fs::write(src1_path.join("foo.txt"), "original").unwrap();

        // setup storage2: large file, but still watchable
        std::fs::File::create(src2_path.join("large.txt"))
            .unwrap()
            .set_len(MAX_SIZE_PER_WATCHABLE_MOUNT)
            .unwrap();

        // setup storage3: many files, but still watchable
        for i in 1..MAX_ENTRIES_PER_STORAGE + 1 {
            fs::write(src3_path.join(format!("{}.txt", i)), "original").unwrap();
        }

        let logger = slog::Logger::root(slog::Discard, o!());

        let mut entries = SandboxStorages {
            ..Default::default()
        };

        entries
            .add(std::iter::once(storage0), &logger)
            .await
            .unwrap();
        entries
            .add(std::iter::once(storage1), &logger)
            .await
            .unwrap();
        entries
            .add(std::iter::once(storage2), &logger)
            .await
            .unwrap();
        entries
            .add(std::iter::once(storage3), &logger)
            .await
            .unwrap();

        assert!(entries.check(&logger).await.is_ok());
        // Check that there are four entries
        assert_eq!(entries.0.len(), 4);

        //verify that storage 0 is no longer going to be watched, but 1,2,3 are
        assert!(!entries.0[0].watch);
        assert!(entries.0[1].watch);
        assert!(entries.0[2].watch);
        assert!(entries.0[3].watch);

        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 8);

        //verify target mount points contain expected number of entries:
        assert_eq!(
            std::fs::read_dir(entries.0[0].target_mount_point.as_path())
                .unwrap()
                .count(),
            20
        );
        assert_eq!(
            std::fs::read_dir(entries.0[1].target_mount_point.as_path())
                .unwrap()
                .count(),
            2
        );
        assert_eq!(
            std::fs::read_dir(entries.0[2].target_mount_point.as_path())
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_dir(entries.0[3].target_mount_point.as_path())
                .unwrap()
                .count(),
            MAX_ENTRIES_PER_STORAGE
        );

        // Add two files to storage 0, verify it is updated without needing to run check:
        fs::write(src0_path.join("1.txt"), "updated").unwrap();
        fs::write(src0_path.join("foo.txt"), "new").unwrap();
        fs::write(src0_path.join("bar.txt"), "new").unwrap();
        assert_eq!(
            std::fs::read_dir(entries.0[0].target_mount_point.as_path())
                .unwrap()
                .count(),
            22
        );
        assert_eq!(
            fs::read_to_string(&entries.0[0].target_mount_point.as_path().join("1.txt")).unwrap(),
            "updated"
        );

        //
        // Prepare for second check: update mount sources
        //

        // source 3 will become unwatchable
        fs::write(src3_path.join("foo.txt"), "updated").unwrap();

        // source 2 will become unwatchable:
        std::fs::File::create(src2_path.join("small.txt"))
            .unwrap()
            .set_len(10)
            .unwrap();

        // source 1: expect just an update
        fs::write(src1_path.join("foo.txt"), "updated").unwrap();

        assert!(entries.check(&logger).await.is_ok());

        // verify that only storage 1 is still watchable
        assert!(!entries.0[0].watch);
        assert!(entries.0[1].watch);
        assert!(!entries.0[2].watch);
        assert!(!entries.0[3].watch);

        // Verify storage 1 was updated, and storage 2,3 are up to date despite no watch
        assert_eq!(
            std::fs::read_dir(entries.0[0].target_mount_point.as_path())
                .unwrap()
                .count(),
            22
        );
        assert_eq!(
            std::fs::read_dir(entries.0[1].target_mount_point.as_path())
                .unwrap()
                .count(),
            2
        );
        assert_eq!(
            fs::read_to_string(&entries.0[1].target_mount_point.as_path().join("foo.txt")).unwrap(),
            "updated"
        );

        assert_eq!(
            std::fs::read_dir(entries.0[2].target_mount_point.as_path())
                .unwrap()
                .count(),
            2
        );
        assert_eq!(
            std::fs::read_dir(entries.0[3].target_mount_point.as_path())
                .unwrap()
                .count(),
            MAX_ENTRIES_PER_STORAGE + 1
        );

        // verify that we can remove files as well, but that it isn't observed until check is run
        // for a watchable mount:
        fs::remove_file(src1_path.join("foo.txt")).unwrap();
        assert_eq!(
            std::fs::read_dir(entries.0[1].target_mount_point.as_path())
                .unwrap()
                .count(),
            2
        );
        assert!(entries.check(&logger).await.is_ok());
        assert_eq!(
            std::fs::read_dir(entries.0[1].target_mount_point.as_path())
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn watch_directory_too_large() {
        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let mut entry = Storage::new(protos::Storage {
            source: source_dir.path().display().to_string(),
            mount_point: dest_dir.path().display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        let logger = slog::Logger::root(slog::Discard, o!());

        // Create a file that is too large:
        std::fs::File::create(source_dir.path().join("big.txt"))
            .unwrap()
            .set_len(MAX_SIZE_PER_WATCHABLE_MOUNT + 1)
            .unwrap();
        thread::sleep(Duration::from_secs(1));

        // Expect to receive a MountTooLarge error
        match entry.scan(&logger).await {
            Ok(_) => panic!("expected error"),
            Err(e) => match e.downcast_ref::<WatcherError>() {
                Some(WatcherError::MountTooLarge { .. }) => {}
                _ => panic!("unexpected error"),
            },
        }
        fs::remove_file(source_dir.path().join("big.txt")).unwrap();

        std::fs::File::create(source_dir.path().join("big.txt"))
            .unwrap()
            .set_len(MAX_SIZE_PER_WATCHABLE_MOUNT - 1)
            .unwrap();
        thread::sleep(Duration::from_secs(1));

        assert!(entry.scan(&logger).await.is_ok());

        std::fs::File::create(source_dir.path().join("too-big.txt"))
            .unwrap()
            .set_len(2)
            .unwrap();
        thread::sleep(Duration::from_secs(1));

        // Expect to receive a MountTooLarge error
        match entry.scan(&logger).await {
            Ok(_) => panic!("expected error"),
            Err(e) => match e.downcast_ref::<WatcherError>() {
                Some(WatcherError::MountTooLarge { .. }) => {}
                _ => panic!("unexpected error"),
            },
        }

        fs::remove_file(source_dir.path().join("big.txt")).unwrap();
        fs::remove_file(source_dir.path().join("too-big.txt")).unwrap();

        // Up to 16 files should be okay:
        for i in 1..MAX_ENTRIES_PER_STORAGE + 1 {
            fs::write(source_dir.path().join(format!("{}.txt", i)), "original").unwrap();
        }

        assert_eq!(entry.scan(&logger).await.unwrap(), MAX_ENTRIES_PER_STORAGE);

        // 17 files is too many:
        fs::write(source_dir.path().join("17.txt"), "updated").unwrap();
        thread::sleep(Duration::from_secs(1));

        // Expect to receive a MountTooManyFiles error
        match entry.scan(&logger).await {
            Ok(_) => panic!("expected error"),
            Err(e) => match e.downcast_ref::<WatcherError>() {
                Some(WatcherError::MountTooManyFiles { .. }) => {}
                _ => panic!("unexpected error"),
            },
        }
    }

    #[tokio::test]
    async fn watch_directory() {
        // Prepare source directory:
        // ./tmp/1.txt
        // ./tmp/A/B/2.txt
        let source_dir = tempfile::tempdir().unwrap();
        fs::write(source_dir.path().join("1.txt"), "one").unwrap();
        fs::create_dir_all(source_dir.path().join("A/B")).unwrap();
        fs::write(source_dir.path().join("A/B/1.txt"), "two").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();

        let mut entry = Storage::new(protos::Storage {
            source: source_dir.path().display().to_string(),
            mount_point: dest_dir.path().display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        let logger = slog::Logger::root(slog::Discard, o!());

        assert_eq!(entry.scan(&logger).await.unwrap(), 2);

        // Should copy no files since nothing is changed since last check
        assert_eq!(entry.scan(&logger).await.unwrap(), 0);

        // Should copy 1 file
        thread::sleep(Duration::from_secs(1));
        fs::write(source_dir.path().join("A/B/1.txt"), "updated").unwrap();
        assert_eq!(entry.scan(&logger).await.unwrap(), 1);
        assert_eq!(
            fs::read_to_string(dest_dir.path().join("A/B/1.txt")).unwrap(),
            "updated"
        );

        // Should copy no new files after copy happened
        assert_eq!(entry.scan(&logger).await.unwrap(), 0);

        // Update another file
        fs::write(source_dir.path().join("1.txt"), "updated").unwrap();
        assert_eq!(entry.scan(&logger).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn watch_file() {
        let source_dir = tempfile::tempdir().unwrap();
        let source_file = source_dir.path().join("1.txt");

        fs::write(&source_file, "one").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let dest_file = dest_dir.path().join("1.txt");

        let mut entry = Storage::new(protos::Storage {
            source: source_file.display().to_string(),
            mount_point: dest_file.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        let logger = slog::Logger::root(slog::Discard, o!());

        assert_eq!(entry.scan(&logger).await.unwrap(), 1);

        thread::sleep(Duration::from_secs(1));
        fs::write(&source_file, "two").unwrap();
        assert_eq!(entry.scan(&logger).await.unwrap(), 1);
        assert_eq!(fs::read_to_string(&dest_file).unwrap(), "two");
        assert_eq!(entry.scan(&logger).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn delete_file() {
        let source_dir = tempfile::tempdir().unwrap();
        let source_file = source_dir.path().join("1.txt");
        fs::write(&source_file, "one").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let target_file = dest_dir.path().join("1.txt");

        let mut entry = Storage::new(protos::Storage {
            source: source_dir.path().display().to_string(),
            mount_point: dest_dir.path().display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        let logger = slog::Logger::root(slog::Discard, o!());

        assert_eq!(entry.scan(&logger).await.unwrap(), 1);
        assert_eq!(entry.watched_files.len(), 1);

        assert!(target_file.exists());
        assert!(entry.watched_files.contains_key(&source_file));

        // Remove source file
        fs::remove_file(&source_file).unwrap();

        assert_eq!(entry.scan(&logger).await.unwrap(), 0);

        assert_eq!(entry.watched_files.len(), 0);
        assert!(!target_file.exists());
    }

    #[tokio::test]
    async fn make_target_path() {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();

        let source_dir = source_dir.path();
        let target_dir = target_dir.path();

        let entry = Storage::new(protos::Storage {
            source: source_dir.display().to_string(),
            mount_point: target_dir.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        assert_eq!(
            entry.make_target_path(source_dir.join("1.txt")).unwrap(),
            target_dir.join("1.txt")
        );

        assert_eq!(
            entry
                .make_target_path(source_dir.join("a/b/2.txt"))
                .unwrap(),
            target_dir.join("a/b/2.txt")
        );
    }

    #[tokio::test]
    async fn create_tmpfs() {
        skip_if_not_root!();

        let logger = slog::Logger::root(slog::Discard, o!());
        let mut watcher = BindWatcher::default();

        watcher.mount(&logger).await.unwrap();
        assert!(is_mounted(WATCH_MOUNT_POINT_PATH).unwrap());

        watcher.cleanup();
        assert!(!is_mounted(WATCH_MOUNT_POINT_PATH).unwrap());
    }

    #[tokio::test]
    async fn spawn_thread() {
        skip_if_not_root!();

        let source_dir = tempfile::tempdir().unwrap();
        fs::write(source_dir.path().join("1.txt"), "one").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();

        let storage = protos::Storage {
            source: source_dir.path().display().to_string(),
            mount_point: dest_dir.path().display().to_string(),
            ..Default::default()
        };

        let logger = slog::Logger::root(slog::Discard, o!());
        let mut watcher = BindWatcher::default();

        watcher
            .add_container("test".into(), std::iter::once(storage), &logger)
            .await
            .unwrap();

        thread::sleep(Duration::from_secs(WATCH_INTERVAL_SECS));

        let out = fs::read_to_string(dest_dir.path().join("1.txt")).unwrap();
        assert_eq!(out, "one");
    }

    #[tokio::test]
    async fn verify_container_cleanup_watching() {
        skip_if_not_root!();

        let source_dir = tempfile::tempdir().unwrap();
        fs::write(source_dir.path().join("1.txt"), "one").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();

        let storage = protos::Storage {
            source: source_dir.path().display().to_string(),
            mount_point: dest_dir.path().display().to_string(),
            ..Default::default()
        };

        let logger = slog::Logger::root(slog::Discard, o!());
        let mut watcher = BindWatcher::default();

        watcher
            .add_container("test".into(), std::iter::once(storage), &logger)
            .await
            .unwrap();

        thread::sleep(Duration::from_secs(WATCH_INTERVAL_SECS));

        let out = fs::read_to_string(dest_dir.path().join("1.txt")).unwrap();
        assert!(dest_dir.path().exists());
        assert_eq!(out, "one");

        watcher.remove_container("test").await;

        thread::sleep(Duration::from_secs(WATCH_INTERVAL_SECS));
        assert!(!dest_dir.path().exists());

        for i in 1..21 {
            fs::write(source_dir.path().join(format!("{}.txt", i)), "fluff").unwrap();
        }

        // verify non-watched storage is cleaned up correctly
        let storage1 = protos::Storage {
            source: source_dir.path().display().to_string(),
            mount_point: dest_dir.path().display().to_string(),
            ..Default::default()
        };

        watcher
            .add_container("test".into(), std::iter::once(storage1), &logger)
            .await
            .unwrap();

        thread::sleep(Duration::from_secs(WATCH_INTERVAL_SECS));

        assert!(dest_dir.path().exists());
        assert!(is_mounted(dest_dir.path().to_str().unwrap()).unwrap());

        watcher.remove_container("test").await;

        thread::sleep(Duration::from_secs(WATCH_INTERVAL_SECS));

        assert!(!dest_dir.path().exists());
        assert!(!is_mounted(dest_dir.path().to_str().unwrap()).unwrap());
    }
}
