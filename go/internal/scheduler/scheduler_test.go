package scheduler

import (
	"testing"
	"time"
)

func TestComputeNextRunInterval(t *testing.T) {
	result := computeNextRun("interval", "1h")
	if result == "" {
		t.Fatal("expected non-empty result for valid interval")
	}
	parsed, err := time.Parse(time.RFC3339Nano, result)
	if err != nil {
		t.Fatalf("failed to parse result: %v", err)
	}
	// Should be ~1 hour from now
	diff := time.Until(parsed)
	if diff < 59*time.Minute || diff > 61*time.Minute {
		t.Errorf("expected ~1h from now, got %v", diff)
	}
}

func TestComputeNextRunIntervalInvalid(t *testing.T) {
	result := computeNextRun("interval", "not-a-duration")
	if result != "" {
		t.Errorf("expected empty result for invalid interval, got %q", result)
	}
}

func TestComputeNextRunCron(t *testing.T) {
	// Every day at midnight local time → stored as UTC
	result := computeNextRun("cron", "0 0 * * *")
	if result == "" {
		t.Fatal("expected non-empty result for valid cron expression")
	}
	parsed, err := time.Parse(time.RFC3339Nano, result)
	if err != nil {
		t.Fatalf("failed to parse result: %v", err)
	}
	// Should be in the future
	if !parsed.After(time.Now().UTC()) {
		t.Error("expected next run to be in the future")
	}
	// Should be at local midnight converted to UTC — just verify it's valid and in range
	diff := time.Until(parsed)
	if diff < 0 || diff > 25*time.Hour {
		t.Errorf("expected next run within 25h, got %v", diff)
	}
}

func TestComputeNextRunCronEvery5Min(t *testing.T) {
	result := computeNextRun("cron", "*/5 * * * *")
	if result == "" {
		t.Fatal("expected non-empty result for */5 cron")
	}
	parsed, err := time.Parse(time.RFC3339Nano, result)
	if err != nil {
		t.Fatalf("failed to parse result: %v", err)
	}
	if !parsed.After(time.Now()) {
		t.Error("expected next run to be in the future")
	}
	// Minute should be divisible by 5
	if parsed.Minute()%5 != 0 {
		t.Errorf("expected minute divisible by 5, got %d", parsed.Minute())
	}
}

func TestComputeNextRunCronInvalid(t *testing.T) {
	result := computeNextRun("cron", "not a cron")
	if result != "" {
		t.Errorf("expected empty result for invalid cron, got %q", result)
	}
}

func TestComputeNextRunOneShot(t *testing.T) {
	result := computeNextRun("one_shot", "")
	if result != "" {
		t.Errorf("expected empty result for one_shot, got %q", result)
	}
}
