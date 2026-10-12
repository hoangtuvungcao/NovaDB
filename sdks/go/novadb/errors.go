package novadb

import (
	"errors"
	"fmt"
	"net/http"
)

// Sentinel errors usable with errors.Is.
var (
	ErrBadRequest      = errors.New("novadb: bad request")
	ErrUnauthorized    = errors.New("novadb: unauthorized")
	ErrNotFound        = errors.New("novadb: not found")
	ErrConflict        = errors.New("novadb: conflict")
	ErrPayloadTooLarge = errors.New("novadb: payload too large")
	ErrServer          = errors.New("novadb: server error")
	ErrTimeout         = errors.New("novadb: timeout")
	ErrNetwork         = errors.New("novadb: network error")
	ErrBind            = errors.New("novadb: bind error")
	ErrMissingWhere    = errors.New("novadb: refusing to UPDATE/DELETE without Where(); call All() to affect every row")
)

// APIError is returned when the server answers with a non-2xx status.
// Use errors.Is(err, novadb.ErrNotFound) etc. to test the category, or
// errors.As(err, &apiErr) to read the details.
type APIError struct {
	Status  int    // HTTP status code
	Code    string // server error code, e.g. "bad_request"
	Message string // server error message
	Body    string // raw response body
}

func (e *APIError) Error() string {
	return fmt.Sprintf("novadb: %s (HTTP %d): %s", e.Code, e.Status, e.Message)
}

// Is lets errors.Is match an APIError against the sentinel categories.
func (e *APIError) Is(target error) bool {
	switch target {
	case ErrBadRequest:
		return e.Code == "bad_request" || e.Status == http.StatusBadRequest
	case ErrUnauthorized:
		return e.Code == "unauthorized" || e.Status == http.StatusUnauthorized
	case ErrNotFound:
		return e.Code == "not_found" || e.Status == http.StatusNotFound
	case ErrConflict:
		return e.Code == "conflict" || e.Status == http.StatusConflict
	case ErrPayloadTooLarge:
		return e.Code == "payload_too_large" || e.Status == http.StatusRequestEntityTooLarge
	case ErrTimeout:
		return e.Status == http.StatusRequestTimeout
	case ErrServer:
		return e.Status >= 500
	}
	return false
}

// BindError describes a client-side parameter binding failure.
type BindError struct{ Msg string }

func (e *BindError) Error() string        { return "novadb: " + e.Msg }
func (e *BindError) Is(target error) bool { return target == ErrBind }

func bindErrorf(format string, args ...any) error {
	return &BindError{Msg: fmt.Sprintf(format, args...)}
}

type wrappedError struct {
	kind error
	err  error
	msg  string
}

func (e *wrappedError) Error() string        { return e.msg }
func (e *wrappedError) Is(target error) bool { return target == e.kind }
func (e *wrappedError) Unwrap() error        { return e.err }
