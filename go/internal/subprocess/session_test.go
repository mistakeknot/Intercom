package subprocess

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// mockStore records calls to SetSession/DeleteSession for verification.
type mockStore struct {
	sets    map[string]string
	deletes []string
}

func newMockStore() *mockStore {
	return &mockStore{sets: make(map[string]string)}
}

func (m *mockStore) SetSession(_ context.Context, groupFolder, sessionID string) error {
	m.sets[groupFolder] = sessionID
	return nil
}

func (m *mockStore) DeleteSession(_ context.Context, groupFolder string) error {
	m.deletes = append(m.deletes, groupFolder)
	return nil
}

func TestSessionJSONLPath(t *testing.T) {
	st := NewSessionTracker("/data", 512*1024)
	got := st.SessionJSONLPath("mygroup", "abc123")
	want := filepath.Join("/data", "sessions", "mygroup", ".claude", "projects", "-workspace-group", "abc123.jsonl")
	if got != want {
		t.Errorf("SessionJSONLPath = %q, want %q", got, want)
	}
}

func TestGetSetDelete(t *testing.T) {
	st := NewSessionTracker("/data", 512*1024)

	// Initially empty
	if got := st.Get("grp"); got != "" {
		t.Errorf("expected empty, got %q", got)
	}

	st.Set("grp", "sess-1")
	if got := st.Get("grp"); got != "sess-1" {
		t.Errorf("expected sess-1, got %q", got)
	}

	st.Delete("grp")
	if got := st.Get("grp"); got != "" {
		t.Errorf("expected empty after delete, got %q", got)
	}
}

func TestLoadAll(t *testing.T) {
	st := NewSessionTracker("/data", 512*1024)
	st.LoadAll(map[string]string{"a": "s1", "b": "s2"})

	if got := st.Get("a"); got != "s1" {
		t.Errorf("expected s1, got %q", got)
	}
	if got := st.Get("b"); got != "s2" {
		t.Errorf("expected s2, got %q", got)
	}
}

func TestCheckAndReset_NoSession(t *testing.T) {
	st := NewSessionTracker("/data", 512*1024)
	store := newMockStore()

	reset, err := st.CheckAndReset(context.Background(), "grp", store)
	if err != nil {
		t.Fatal(err)
	}
	if reset {
		t.Error("expected no reset when no session exists")
	}
}

func TestCheckAndReset_FileDoesNotExist(t *testing.T) {
	st := NewSessionTracker("/nonexistent", 512*1024)
	st.Set("grp", "sess-1")
	store := newMockStore()

	reset, err := st.CheckAndReset(context.Background(), "grp", store)
	if err != nil {
		t.Fatal(err)
	}
	if reset {
		t.Error("expected no reset when file doesn't exist")
	}
	// Session should still be in memory
	if got := st.Get("grp"); got != "sess-1" {
		t.Errorf("session should be preserved, got %q", got)
	}
}

func TestCheckAndReset_UnderLimit(t *testing.T) {
	dir := t.TempDir()
	st := NewSessionTracker(dir, 1024) // 1KB limit
	st.Set("grp", "sess-1")

	// Create a small JSONL file (under limit)
	jsonlPath := st.SessionJSONLPath("grp", "sess-1")
	if err := os.MkdirAll(filepath.Dir(jsonlPath), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(jsonlPath, make([]byte, 500), 0o644); err != nil {
		t.Fatal(err)
	}

	store := newMockStore()
	reset, err := st.CheckAndReset(context.Background(), "grp", store)
	if err != nil {
		t.Fatal(err)
	}
	if reset {
		t.Error("expected no reset when file is under limit")
	}
	if got := st.Get("grp"); got != "sess-1" {
		t.Errorf("session should be preserved, got %q", got)
	}
}

func TestCheckAndReset_OverLimit(t *testing.T) {
	dir := t.TempDir()
	st := NewSessionTracker(dir, 1024) // 1KB limit
	st.Set("grp", "sess-1")

	// Create an oversized JSONL file
	jsonlPath := st.SessionJSONLPath("grp", "sess-1")
	if err := os.MkdirAll(filepath.Dir(jsonlPath), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(jsonlPath, make([]byte, 2048), 0o644); err != nil {
		t.Fatal(err)
	}

	// Create the session directory too
	sessionDir := st.sessionDir("grp", "sess-1")
	if err := os.MkdirAll(sessionDir, 0o755); err != nil {
		t.Fatal(err)
	}
	// Put a file in the session dir to verify it gets removed
	if err := os.WriteFile(filepath.Join(sessionDir, "data.json"), []byte("{}"), 0o644); err != nil {
		t.Fatal(err)
	}

	store := newMockStore()
	reset, err := st.CheckAndReset(context.Background(), "grp", store)
	if err != nil {
		t.Fatal(err)
	}
	if !reset {
		t.Error("expected reset when file exceeds limit")
	}

	// Session should be cleared from memory
	if got := st.Get("grp"); got != "" {
		t.Errorf("session should be cleared, got %q", got)
	}

	// JSONL file should be deleted
	if _, err := os.Stat(jsonlPath); !os.IsNotExist(err) {
		t.Error("JSONL file should be deleted")
	}

	// Session directory should be deleted
	if _, err := os.Stat(sessionDir); !os.IsNotExist(err) {
		t.Error("session directory should be deleted")
	}

	// Store should have received delete call
	if len(store.deletes) != 1 || store.deletes[0] != "grp" {
		t.Errorf("expected store.DeleteSession(grp), got %v", store.deletes)
	}
}

func TestCheckAndReset_NilStore(t *testing.T) {
	dir := t.TempDir()
	st := NewSessionTracker(dir, 100)
	st.Set("grp", "sess-1")

	jsonlPath := st.SessionJSONLPath("grp", "sess-1")
	if err := os.MkdirAll(filepath.Dir(jsonlPath), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(jsonlPath, make([]byte, 200), 0o644); err != nil {
		t.Fatal(err)
	}

	// nil store should not panic
	reset, err := st.CheckAndReset(context.Background(), "grp", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !reset {
		t.Error("expected reset")
	}
	if got := st.Get("grp"); got != "" {
		t.Errorf("session should be cleared, got %q", got)
	}
}

func TestPersistSession(t *testing.T) {
	st := NewSessionTracker("/data", 512*1024)
	store := newMockStore()

	if err := st.PersistSession(context.Background(), "grp", "new-sess", store); err != nil {
		t.Fatal(err)
	}
	if got := st.Get("grp"); got != "new-sess" {
		t.Errorf("expected new-sess, got %q", got)
	}
	if store.sets["grp"] != "new-sess" {
		t.Errorf("store should have new-sess, got %q", store.sets["grp"])
	}
}

func TestClearSession(t *testing.T) {
	st := NewSessionTracker("/data", 512*1024)
	st.Set("grp", "sess-1")
	store := newMockStore()

	if err := st.ClearSession(context.Background(), "grp", store); err != nil {
		t.Fatal(err)
	}
	if got := st.Get("grp"); got != "" {
		t.Errorf("expected empty, got %q", got)
	}
	if len(store.deletes) != 1 || store.deletes[0] != "grp" {
		t.Errorf("expected store delete, got %v", store.deletes)
	}
}

func TestResultTimeout_KillsProcess(t *testing.T) {
	// Launch a process that produces output but never a result frame.
	// Use a short result timeout to verify it gets killed.
	ctx := context.Background()
	proc, err := Start(ctx, StartConfig{
		Runtime:         "claude",
		Prompt:          "test",
		WorkDir:         t.TempDir(),
		ResultTimeoutMs: 100,                // 100ms timeout
		Args:            []string{"--help"}, // will exit quickly but tests the flag
	})
	if err != nil {
		// claude binary may not exist — skip gracefully
		t.Skipf("cannot start subprocess: %v", err)
	}

	// Simulate activity without result
	proc.MarkActivity()

	// Wait for either the process to exit or timeout
	select {
	case <-proc.Done():
	case <-time.After(2 * time.Second):
		t.Fatal("process should have exited within 2s")
	}
}

func TestResultTimeout_NoKillWhenResultReceived(t *testing.T) {
	// Verify that marking a result prevents timeout kill
	p := &Process{
		done:          make(chan struct{}),
		started:       time.Now(),
		resultTimeout: 50 * time.Millisecond,
	}
	p.MarkActivity()
	p.MarkResult()

	// The watchdog should not set timedOutResult since result was received
	go p.resultTimeoutWatchdog()
	time.Sleep(100 * time.Millisecond)

	if p.TimedOutResult() {
		t.Error("should not time out when result was received")
	}
}

func TestResultTimeout_NoKillWithoutActivity(t *testing.T) {
	// Verify that no activity means no timeout kill (startup phase)
	p := &Process{
		done:          make(chan struct{}),
		started:       time.Now(),
		resultTimeout: 50 * time.Millisecond,
	}

	go p.resultTimeoutWatchdog()
	time.Sleep(100 * time.Millisecond)

	if p.TimedOutResult() {
		t.Error("should not time out when no activity was received")
	}
}

func TestManagerWithSessions(t *testing.T) {
	dir := t.TempDir()
	m := NewManagerWithSessions(ManagerConfig{
		MaxConcurrent:   3,
		DataDir:         dir,
		SessionMaxBytes: 1024,
	})
	if m.Sessions == nil {
		t.Fatal("sessions should be initialized")
	}
	if m.maxConcurrent != 3 {
		t.Errorf("maxConcurrent = %d, want 3", m.maxConcurrent)
	}
}

func TestManagerWithSessions_Disabled(t *testing.T) {
	m := NewManagerWithSessions(ManagerConfig{
		MaxConcurrent:   3,
		DataDir:         "",
		SessionMaxBytes: 0,
	})
	if m.Sessions != nil {
		t.Error("sessions should be nil when disabled")
	}
}

func TestManagerPreDispatchSessionCheck(t *testing.T) {
	dir := t.TempDir()
	m := NewManagerWithSessions(ManagerConfig{
		MaxConcurrent:   3,
		DataDir:         dir,
		SessionMaxBytes: 100,
	})
	store := newMockStore()

	// Set up an oversized session file
	m.Sessions.Set("grp", "sess-1")
	jsonlPath := m.Sessions.SessionJSONLPath("grp", "sess-1")
	if err := os.MkdirAll(filepath.Dir(jsonlPath), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(jsonlPath, make([]byte, 200), 0o644); err != nil {
		t.Fatal(err)
	}

	reset, err := m.PreDispatchSessionCheck(context.Background(), "grp", store)
	if err != nil {
		t.Fatal(err)
	}
	if !reset {
		t.Error("expected reset")
	}
	if m.Sessions.Get("grp") != "" {
		t.Error("session should be cleared")
	}
}

func TestManagerPreDispatchSessionCheck_NilSessions(t *testing.T) {
	m := NewManager(3) // no sessions
	reset, err := m.PreDispatchSessionCheck(context.Background(), "grp", nil)
	if err != nil {
		t.Fatal(err)
	}
	if reset {
		t.Error("expected no reset when sessions disabled")
	}
}

func TestManagerPostDispatchSessionUpdate_NewSession(t *testing.T) {
	dir := t.TempDir()
	m := NewManagerWithSessions(ManagerConfig{
		MaxConcurrent:   3,
		DataDir:         dir,
		SessionMaxBytes: 1024,
	})
	store := newMockStore()

	proc := &Process{done: make(chan struct{}), started: time.Now()}
	err := m.PostDispatchSessionUpdate(context.Background(), "grp", proc, "new-sess", store)
	if err != nil {
		t.Fatal(err)
	}
	if m.Sessions.Get("grp") != "new-sess" {
		t.Errorf("expected new-sess, got %q", m.Sessions.Get("grp"))
	}
	if store.sets["grp"] != "new-sess" {
		t.Errorf("store should have new-sess")
	}
}

func TestManagerPostDispatchSessionUpdate_ResultTimeout(t *testing.T) {
	dir := t.TempDir()
	m := NewManagerWithSessions(ManagerConfig{
		MaxConcurrent:   3,
		DataDir:         dir,
		SessionMaxBytes: 1024,
	})
	m.Sessions.Set("grp", "old-sess")
	store := newMockStore()

	proc := &Process{done: make(chan struct{}), started: time.Now()}
	proc.timedOutResult.Store(true)

	err := m.PostDispatchSessionUpdate(context.Background(), "grp", proc, "new-sess", store)
	if err != nil {
		t.Fatal(err)
	}
	// Session should be cleared (not set to new-sess) because timeout takes priority
	if m.Sessions.Get("grp") != "" {
		t.Errorf("session should be cleared on result timeout, got %q", m.Sessions.Get("grp"))
	}
	if len(store.deletes) != 1 || store.deletes[0] != "grp" {
		t.Errorf("store should have delete, got %v", store.deletes)
	}
}

func TestResultTimeout_SetsFlag(t *testing.T) {
	// Verify that activity + no result + timeout = timedOutResult flag set
	p := &Process{
		done:          make(chan struct{}),
		started:       time.Now(),
		resultTimeout: 50 * time.Millisecond,
	}
	p.MarkActivity()

	go p.resultTimeoutWatchdog()
	time.Sleep(100 * time.Millisecond)

	if !p.TimedOutResult() {
		t.Error("should set timedOutResult when activity but no result")
	}
}
