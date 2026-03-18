package telegram

import "testing"

func TestSplitForTelegram(t *testing.T) {
	text := make([]rune, 9005)
	for i := range text {
		text[i] = 'a'
	}
	chunks := splitForTelegram(string(text), MaxTextChars)
	if len(chunks) != 3 {
		t.Errorf("expected 3 chunks, got %d", len(chunks))
	}
	total := 0
	for _, c := range chunks {
		n := len([]rune(c))
		if n > MaxTextChars {
			t.Errorf("chunk exceeds limit: %d", n)
		}
		total += n
	}
	if total != 9005 {
		t.Errorf("expected total 9005, got %d", total)
	}
}

func TestSplitShortText(t *testing.T) {
	chunks := splitForTelegram("hello", MaxTextChars)
	if len(chunks) != 1 {
		t.Errorf("expected 1 chunk, got %d", len(chunks))
	}
}

func TestTruncateForTelegram(t *testing.T) {
	text := make([]rune, 5000)
	for i := range text {
		text[i] = 'x'
	}
	result, truncated := truncateForTelegram(string(text), MaxTextChars)
	if !truncated {
		t.Error("expected truncated=true")
	}
	if len([]rune(result)) != MaxTextChars {
		t.Errorf("expected %d runes, got %d", MaxTextChars, len([]rune(result)))
	}

	short, trunc := truncateForTelegram("short", MaxTextChars)
	if trunc {
		t.Error("expected truncated=false for short text")
	}
	if short != "short" {
		t.Errorf("expected 'short', got %q", short)
	}
}

func TestNormalizeChatID(t *testing.T) {
	if normalizeChatID("tg:12345") != "12345" {
		t.Error("should strip tg: prefix")
	}
	if normalizeChatID("12345") != "12345" {
		t.Error("should pass through bare IDs")
	}
}

func TestRuntimeForModel(t *testing.T) {
	tests := []struct {
		model   string
		runtime string
	}{
		{"claude-opus-4-6", "claude"},
		{"claude-sonnet-4-6", "claude"},
		{"gemini-3.1-pro", "gemini"},
		{"gpt-5.3-codex", "codex"},
		{"claude-unknown", "claude"},
		{"gemini-future", "gemini"},
		{"o4-mini", "codex"},
		{"unknown-model", "claude"}, // default
	}
	for _, tt := range tests {
		if got := RuntimeForModel(tt.model); got != tt.runtime {
			t.Errorf("RuntimeForModel(%q) = %q, want %q", tt.model, got, tt.runtime)
		}
	}
}
