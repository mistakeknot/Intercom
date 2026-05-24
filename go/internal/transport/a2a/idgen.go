package a2a

import (
	"strconv"
	"sync/atomic"
	"time"
)

// formatMessageID builds the canonical message-ID string. Kept in its own file
// to keep translate.go free of formatting concerns.
func formatMessageID(nanos int64, counter uint64) string {
	return "a2a-" + strconv.FormatInt(nanos, 10) + "-" + strconv.FormatUint(counter, 10)
}

// taskCounter monotonically increments to disambiguate task IDs created in
// the same nanosecond. Process-local; not persisted because the in-memory
// store is itself process-local in v1.
var taskCounter atomic.Uint64

// generateTaskID returns a unique Task ID. Format: task-<unix-nanos>-<counter>.
// Distinct prefix from generateMessageID to make collision impossible across
// the two ID spaces and to make grep/diagnosis trivial.
func generateTaskID() string {
	now := time.Now().UnixNano()
	c := taskCounter.Add(1)
	return "task-" + strconv.FormatInt(now, 10) + "-" + strconv.FormatUint(c, 10)
}
