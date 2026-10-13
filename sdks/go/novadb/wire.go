package novadb

import (
	"encoding/json"
	"math"
	"math/big"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"time"
)

// ServerParamsMode selects how bound parameters reach the server.
type ServerParamsMode int

const (
	// ServerParamsAuto (default) probes the server once and sends params to it
	// when supported, falling back to client-side binding on old servers.
	ServerParamsAuto ServerParamsMode = iota
	// ServerParamsOn always sends params to the server (no probe).
	ServerParamsOn
	// ServerParamsOff always binds client-side (legacy path, see StringEncoding).
	ServerParamsOff
)

// capState caches the result of the server-side params capability probe. It is
// shared by clients derived with Use / WithShardKey / WithShard.
type capState struct {
	mu        sync.Mutex
	known     bool
	supported bool
}

// splitArgs classifies call arguments: named (single Named / map[string]any)
// or positional.
func splitArgs(args []any) (named map[string]any, positional []any, isNamed bool) {
	if len(args) == 1 {
		switch m := args[0].(type) {
		case Named:
			return m, nil, true
		case map[string]any:
			return m, nil, true
		}
	}
	return nil, args, false
}

// wireValue converts one Go value into a JSON-safe server parameter.
// blob is true for []byte, which the server's JSON params cannot carry.
func wireValue(v any) (out any, blob bool, err error) {
	switch x := v.(type) {
	case nil:
		return nil, false, nil
	case bool:
		return x, false, nil
	case string:
		if strings.ContainsRune(x, 0) {
			return nil, false, bindErrorf("string parameters cannot contain NUL characters; bind []byte instead")
		}
		return x, false, nil
	case []byte:
		if x == nil {
			return nil, false, nil
		}
		return nil, true, nil
	case time.Time:
		return x.Format(time.RFC3339Nano), false, nil
	case *big.Int:
		if x == nil {
			return nil, false, nil
		}
		if !x.IsInt64() {
			return nil, false, bindErrorf("integer %s is outside the 64-bit range", x.String())
		}
		return json.Number(x.String()), false, nil
	case float32:
		return wireFloat(float64(x), 32)
	case float64:
		return wireFloat(x, 64)
	}
	rv := reflect.ValueOf(v)
	switch rv.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return json.Number(strconv.FormatInt(rv.Int(), 10)), false, nil
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		u := rv.Uint()
		if u > math.MaxInt64 {
			return nil, false, bindErrorf("integer %d is outside the 64-bit signed range", u)
		}
		return json.Number(strconv.FormatUint(u, 10)), false, nil
	case reflect.Pointer:
		if rv.IsNil() {
			return nil, false, nil
		}
		return wireValue(rv.Elem().Interface())
	case reflect.String:
		return wireValue(rv.String())
	case reflect.Bool:
		return rv.Bool(), false, nil
	case reflect.Float32, reflect.Float64:
		return wireFloat(rv.Float(), 64)
	case reflect.Slice:
		if rv.Type().Elem().Kind() == reflect.Uint8 {
			if rv.IsNil() {
				return nil, false, nil
			}
			return nil, true, nil
		}
	}
	return nil, false, bindErrorf("cannot bind parameter of type %T; marshal JSON values to a string first", v)
}

func wireFloat(f float64, bits int) (any, bool, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return nil, false, bindErrorf("cannot bind non-finite float %v", f)
	}
	s := strconv.FormatFloat(f, 'g', -1, bits)
	if !strings.ContainsAny(s, ".eE") {
		s += ".0" // keep it a REAL on the server, not an INTEGER
	}
	return json.Number(s), false, nil
}

// wireParams converts call arguments into the JSON `params` value ([]any,
// map[string]any or nil). needsInline reports that a []byte forces
// client-side binding for this statement.
func wireParams(args []any) (params any, needsInline bool, err error) {
	if len(args) == 0 {
		return nil, false, nil
	}
	named, positional, isNamed := splitArgs(args)
	if isNamed {
		m := make(map[string]any, len(named))
		for k, v := range named {
			w, blob, err := wireValue(v)
			if err != nil {
				return nil, false, err
			}
			if blob {
				needsInline = true
			}
			m[strings.TrimPrefix(k, ":")] = w
		}
		return m, needsInline, nil
	}
	list := make([]any, len(positional))
	for i, v := range positional {
		w, blob, err := wireValue(v)
		if err != nil {
			return nil, false, err
		}
		if blob {
			needsInline = true
		}
		list[i] = w
	}
	return list, needsInline, nil
}
