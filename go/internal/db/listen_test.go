package db

import "testing"

func TestRedactDSN(t *testing.T) {
	tests := []struct {
		input string
		want  string
	}{
		{"postgres://user:secret@localhost:5432/db", "postgres://user:***@localhost:5432/db"},
		{"postgres://localhost:5432/db", "postgres://localhost:5432/db"},
		{"postgres://user@localhost:5432/db", "postgres://user@localhost:5432/db"},
		{"", ""},
		{"not-a-dsn", "not-a-dsn"},
		{"postgres://u:p@h/d?sslmode=disable", "postgres://u:***@h/d?sslmode=disable"},
	}
	for _, tt := range tests {
		got := redactDSN(tt.input)
		if got != tt.want {
			t.Errorf("redactDSN(%q) = %q, want %q", tt.input, got, tt.want)
		}
	}
}

func TestNudgeNonBlocking(t *testing.T) {
	ch := make(chan struct{}, 1)

	// First nudge should send
	nudge(ch)
	select {
	case <-ch:
	default:
		t.Error("expected signal after first nudge")
	}

	// Nudge on full channel should not block
	ch <- struct{}{} // fill the channel
	nudge(ch)        // should not block
	// Drain
	<-ch
}
