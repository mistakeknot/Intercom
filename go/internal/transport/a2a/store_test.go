package a2a

import (
	"errors"
	"testing"
)

func TestStoreCreateAndGet(t *testing.T) {
	s := NewStore()

	task := s.Create("sylveste-bead-001")
	if task.ID == "" {
		t.Fatal("Create returned empty ID")
	}
	if task.ContextID != "sylveste-bead-001" {
		t.Errorf("ContextID = %q, want sylveste-bead-001", task.ContextID)
	}
	if task.Status.State != TaskStateSubmitted {
		t.Errorf("initial State = %q, want SUBMITTED", task.Status.State)
	}
	if task.Status.Timestamp == "" {
		t.Error("Status.Timestamp not set")
	}

	got, ok := s.Get(task.ID)
	if !ok {
		t.Fatal("Get returned ok=false for just-created task")
	}
	if got.ID != task.ID {
		t.Errorf("Get returned different ID: got %q, want %q", got.ID, task.ID)
	}

	_, ok = s.Get("nonexistent")
	if ok {
		t.Error("Get returned ok=true for unknown ID")
	}
}

func TestStoreUpdateStatusTransitions(t *testing.T) {
	s := NewStore()
	task := s.Create("sylveste-bead-002")

	for _, state := range []TaskState{TaskStateWorking, TaskStateInputRequired, TaskStateWorking, TaskStateCompleted} {
		got, err := s.UpdateStatus(task.ID, state)
		if err != nil {
			t.Fatalf("UpdateStatus(%q): %v", state, err)
		}
		if got.Status.State != state {
			t.Errorf("Status.State = %q, want %q", got.Status.State, state)
		}
	}

	// Terminal: COMPLETED. Further transitions must fail.
	_, err := s.UpdateStatus(task.ID, TaskStateWorking)
	if !errors.Is(err, ErrTaskTerminal) {
		t.Errorf("UpdateStatus from COMPLETED: err = %v, want ErrTaskTerminal", err)
	}
}

func TestStoreCancel(t *testing.T) {
	s := NewStore()

	t.Run("active task cancels", func(t *testing.T) {
		task := s.Create("sylveste-bead-003")
		got, err := s.Cancel(task.ID)
		if err != nil {
			t.Fatalf("Cancel: %v", err)
		}
		if got.Status.State != TaskStateCancelled {
			t.Errorf("State after Cancel = %q, want CANCELLED", got.Status.State)
		}
	})

	t.Run("terminal task rejects cancel", func(t *testing.T) {
		task := s.Create("sylveste-bead-004")
		if _, err := s.UpdateStatus(task.ID, TaskStateCompleted); err != nil {
			t.Fatalf("setup: %v", err)
		}
		_, err := s.Cancel(task.ID)
		if !errors.Is(err, ErrTaskTerminal) {
			t.Errorf("Cancel on COMPLETED: err = %v, want ErrTaskTerminal", err)
		}
	})

	t.Run("unknown task returns not-found", func(t *testing.T) {
		_, err := s.Cancel("nonexistent")
		if !errors.Is(err, ErrTaskNotFound) {
			t.Errorf("Cancel on unknown: err = %v, want ErrTaskNotFound", err)
		}
	})
}

func TestStoreListPagination(t *testing.T) {
	s := NewStore()
	for i := 0; i < 7; i++ {
		s.Create("sylveste-bead-list")
	}

	first, next := s.List("", 3)
	if len(first) != 3 {
		t.Fatalf("first page len = %d, want 3", len(first))
	}
	if next == "" {
		t.Fatal("first page nextCursor empty, expected more")
	}

	second, next2 := s.List(next, 3)
	if len(second) != 3 {
		t.Errorf("second page len = %d, want 3", len(second))
	}
	if next2 == "" {
		t.Error("second page nextCursor empty, expected 1 more")
	}

	third, next3 := s.List(next2, 3)
	if len(third) != 1 {
		t.Errorf("third page len = %d, want 1", len(third))
	}
	if next3 != "" {
		t.Errorf("third page nextCursor = %q, want empty", next3)
	}

	// IDs across pages should be unique and cover all 7 tasks.
	seen := map[string]bool{}
	for _, page := range [][]Task{first, second, third} {
		for _, task := range page {
			if seen[task.ID] {
				t.Errorf("task %q appeared on multiple pages", task.ID)
			}
			seen[task.ID] = true
		}
	}
	if len(seen) != 7 {
		t.Errorf("saw %d distinct tasks, want 7", len(seen))
	}
}

func TestStoreListLimitClamp(t *testing.T) {
	s := NewStore()
	s.Create("ctx")

	// limit <= 0 falls back to default (50, which exceeds our 1 entry).
	tasks, _ := s.List("", 0)
	if len(tasks) != 1 {
		t.Errorf("List(limit=0) len = %d, want 1", len(tasks))
	}

	tasks, _ = s.List("", -5)
	if len(tasks) != 1 {
		t.Errorf("List(limit=-5) len = %d, want 1", len(tasks))
	}

	tasks, _ = s.List("", 9999)
	if len(tasks) != 1 {
		t.Errorf("List(limit=9999) len = %d, want 1", len(tasks))
	}
}

func TestStoreListEmptyAndUnknownCursor(t *testing.T) {
	s := NewStore()

	tasks, next := s.List("", 10)
	if len(tasks) != 0 {
		t.Errorf("empty store List len = %d, want 0", len(tasks))
	}
	if next != "" {
		t.Errorf("empty store nextCursor = %q, want empty", next)
	}

	s.Create("ctx")
	// Unknown cursor: implementation treats it as start-from-beginning (find loop
	// doesn't match, startIdx stays 0). Document this behavior so any change is
	// intentional.
	tasks, _ = s.List("does-not-exist", 10)
	if len(tasks) != 1 {
		t.Errorf("unknown cursor: len = %d, want 1 (fall back to start)", len(tasks))
	}
}

func TestStoreLenAndConcurrency(t *testing.T) {
	s := NewStore()
	if s.Len() != 0 {
		t.Errorf("empty Len = %d, want 0", s.Len())
	}

	done := make(chan struct{})
	go func() {
		for i := 0; i < 100; i++ {
			s.Create("ctx")
		}
		close(done)
	}()
	for i := 0; i < 100; i++ {
		_, _ = s.List("", 5)
		_ = s.Len()
	}
	<-done

	if s.Len() != 100 {
		t.Errorf("Len after 100 creates = %d, want 100", s.Len())
	}
}

func TestSplitTaskIDSuffix(t *testing.T) {
	cases := []struct {
		in         string
		wantID     string
		wantAction string
	}{
		{"task-1234", "task-1234", ""},
		{"task-1234:cancel", "task-1234", "cancel"},
		{"task-1234:subscribe", "task-1234", "subscribe"},
		// Multiple colons: split at the last one (action suffix is the trailing token).
		{"a2a-12-34:cancel", "a2a-12-34", "cancel"},
		{"", "", ""},
		{":cancel", "", "cancel"},
	}
	for _, tc := range cases {
		id, action := splitTaskIDSuffix(tc.in)
		if id != tc.wantID || action != tc.wantAction {
			t.Errorf("splitTaskIDSuffix(%q) = (%q,%q), want (%q,%q)", tc.in, id, action, tc.wantID, tc.wantAction)
		}
	}
}
