package a2a

import "strconv"

// formatMessageID builds the canonical message-ID string. Kept in its own file
// to keep translate.go free of formatting concerns.
func formatMessageID(nanos int64, counter uint64) string {
	return "a2a-" + strconv.FormatInt(nanos, 10) + "-" + strconv.FormatUint(counter, 10)
}
