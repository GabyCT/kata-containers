// Code generated by go-swagger; DO NOT EDIT.

package operations

// This file was generated by the swagger tool.
// Editing this file might prove futile when you re-run the swagger generate command

import (
	"fmt"
	"io"

	"github.com/go-openapi/runtime"

	strfmt "github.com/go-openapi/strfmt"

	models "github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/firecracker/client/models"
)

// PutGuestDriveByIDReader is a Reader for the PutGuestDriveByID structure.
type PutGuestDriveByIDReader struct {
	formats strfmt.Registry
}

// ReadResponse reads a server response into the received o.
func (o *PutGuestDriveByIDReader) ReadResponse(response runtime.ClientResponse, consumer runtime.Consumer) (interface{}, error) {
	switch response.Code() {

	case 204:
		result := NewPutGuestDriveByIDNoContent()
		if err := result.readResponse(response, consumer, o.formats); err != nil {
			return nil, err
		}
		return result, nil

	case 400:
		result := NewPutGuestDriveByIDBadRequest()
		if err := result.readResponse(response, consumer, o.formats); err != nil {
			return nil, err
		}
		return nil, result

	default:
		result := NewPutGuestDriveByIDDefault(response.Code())
		if err := result.readResponse(response, consumer, o.formats); err != nil {
			return nil, err
		}
		if response.Code()/100 == 2 {
			return result, nil
		}
		return nil, result
	}
}

// NewPutGuestDriveByIDNoContent creates a PutGuestDriveByIDNoContent with default headers values
func NewPutGuestDriveByIDNoContent() *PutGuestDriveByIDNoContent {
	return &PutGuestDriveByIDNoContent{}
}

/*PutGuestDriveByIDNoContent handles this case with default header values.

Drive created/updated
*/
type PutGuestDriveByIDNoContent struct {
}

func (o *PutGuestDriveByIDNoContent) Error() string {
	return fmt.Sprintf("[PUT /drives/{drive_id}][%d] putGuestDriveByIdNoContent ", 204)
}

func (o *PutGuestDriveByIDNoContent) readResponse(response runtime.ClientResponse, consumer runtime.Consumer, formats strfmt.Registry) error {

	return nil
}

// NewPutGuestDriveByIDBadRequest creates a PutGuestDriveByIDBadRequest with default headers values
func NewPutGuestDriveByIDBadRequest() *PutGuestDriveByIDBadRequest {
	return &PutGuestDriveByIDBadRequest{}
}

/*PutGuestDriveByIDBadRequest handles this case with default header values.

Drive cannot be created/updated due to bad input
*/
type PutGuestDriveByIDBadRequest struct {
	Payload *models.Error
}

func (o *PutGuestDriveByIDBadRequest) Error() string {
	return fmt.Sprintf("[PUT /drives/{drive_id}][%d] putGuestDriveByIdBadRequest  %+v", 400, o.Payload)
}

func (o *PutGuestDriveByIDBadRequest) readResponse(response runtime.ClientResponse, consumer runtime.Consumer, formats strfmt.Registry) error {

	o.Payload = new(models.Error)

	// response payload
	if err := consumer.Consume(response.Body(), o.Payload); err != nil && err != io.EOF {
		return err
	}

	return nil
}

// NewPutGuestDriveByIDDefault creates a PutGuestDriveByIDDefault with default headers values
func NewPutGuestDriveByIDDefault(code int) *PutGuestDriveByIDDefault {
	return &PutGuestDriveByIDDefault{
		_statusCode: code,
	}
}

/*PutGuestDriveByIDDefault handles this case with default header values.

Internal server error.
*/
type PutGuestDriveByIDDefault struct {
	_statusCode int

	Payload *models.Error
}

// Code gets the status code for the put guest drive by ID default response
func (o *PutGuestDriveByIDDefault) Code() int {
	return o._statusCode
}

func (o *PutGuestDriveByIDDefault) Error() string {
	return fmt.Sprintf("[PUT /drives/{drive_id}][%d] putGuestDriveByID default  %+v", o._statusCode, o.Payload)
}

func (o *PutGuestDriveByIDDefault) readResponse(response runtime.ClientResponse, consumer runtime.Consumer, formats strfmt.Registry) error {

	o.Payload = new(models.Error)

	// response payload
	if err := consumer.Consume(response.Body(), o.Payload); err != nil && err != io.EOF {
		return err
	}

	return nil
}
