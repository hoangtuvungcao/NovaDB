package novadb

import (
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"math"
	"strconv"
	"time"
)

// HLCTimestamp encodes a hybrid logical clock value as <16 hex>-<8 hex>.
func HLCTimestamp(physicalMs uint64, counter uint32) string {
	return fmt.Sprintf("%016x-%08x", physicalMs, counter)
}

// RowID encodes a primary key as a canonical sync row ID (i:, r:, t:, b:).
func RowID(key any) (string, error) {
	switch k := key.(type) {
	case string:
		return "t:" + k, nil
	case []byte:
		return "b:" + base64.StdEncoding.EncodeToString(k), nil
	case int:
		return "i:" + strconv.FormatInt(int64(k), 10), nil
	case int32:
		return "i:" + strconv.FormatInt(int64(k), 10), nil
	case int64:
		return "i:" + strconv.FormatInt(k, 10), nil
	case float64:
		switch {
		case math.IsNaN(k):
			return "r:nan", nil
		case math.IsInf(k, 1):
			return "r:infinity", nil
		case math.IsInf(k, -1):
			return "r:-infinity", nil
		}
		return "r:" + strconv.FormatFloat(k, 'g', -1, 64), nil
	}
	return "", bindErrorf("unsupported row ID type %T", key)
}

func newUUID() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// NewChange builds an upsert/delete change envelope with a fresh change ID,
// the current HLC time and created_at_ms.
func NewChange(table string, key any, operation string, payload map[string]any, deviceID string, seq int64) (Change, error) {
	rowID, err := RowID(key)
	if err != nil {
		return Change{}, err
	}
	if operation != "upsert" && operation != "delete" {
		return Change{}, bindErrorf("operation must be upsert or delete")
	}
	raw, err := json.Marshal(payload)
	if err != nil {
		return Change{}, fmt.Errorf("novadb: encode payload: %w", err)
	}
	now := time.Now().UnixMilli()
	return Change{
		Seq:         seq,
		ChangeID:    newUUID(),
		Table:       table,
		RowID:       rowID,
		Operation:   operation,
		Payload:     raw,
		HLC:         HLCTimestamp(uint64(now), 0),
		DeviceID:    deviceID,
		CreatedAtMs: now,
	}, nil
}
