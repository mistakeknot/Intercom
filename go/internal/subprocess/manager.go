package subprocess

import (
	"context"
	"fmt"
	"log/slog"
	"sync"
	"time"
)

// Manager tracks active subprocess slots, enforces concurrency limits,
// and manages per-group session files with auto-reset on overflow.
type Manager struct {
	maxConcurrent int
	mu            sync.Mutex
	active        map[string]*Process // keyed by group folder

	// Sessions tracks per-group session IDs and their JSONL files.
	// Nil if session management is disabled.
	Sessions *SessionTracker
}

// ManagerConfig holds configuration for creating a Manager.
type ManagerConfig struct {
	MaxConcurrent   int
	DataDir         string // root data dir for session files
	SessionMaxBytes int64  // max JSONL file size before auto-reset (0 = no limit)
}

func NewManager(maxConcurrent int) *Manager {
	return &Manager{
		maxConcurrent: maxConcurrent,
		active:        make(map[string]*Process),
	}
}

// NewManagerWithSessions creates a Manager with session file tracking enabled.
func NewManagerWithSessions(cfg ManagerConfig) *Manager {
	m := &Manager{
		maxConcurrent: cfg.MaxConcurrent,
		active:        make(map[string]*Process),
	}
	if cfg.DataDir != "" && cfg.SessionMaxBytes > 0 {
		m.Sessions = NewSessionTracker(cfg.DataDir, cfg.SessionMaxBytes)
	}
	return m
}

// PreDispatchSessionCheck runs the session size guard before spawning a subprocess.
// Returns true if the session was reset (caller should start fresh).
func (m *Manager) PreDispatchSessionCheck(ctx context.Context, groupFolder string, store SessionStore) (bool, error) {
	if m.Sessions == nil {
		return false, nil
	}
	return m.Sessions.CheckAndReset(ctx, groupFolder, store)
}

// PostDispatchSessionUpdate handles session tracking after a subprocess completes.
// If the process timed out waiting for a result, clears the session for retry.
// If a new session ID was produced, persists it.
func (m *Manager) PostDispatchSessionUpdate(ctx context.Context, groupFolder string, proc *Process, newSessionID string, store SessionStore) error {
	if m.Sessions == nil {
		return nil
	}

	// Result timeout → clear session for fresh retry
	if proc.TimedOutResult() {
		slog.Warn("result timeout — clearing session for fresh retry", "group", groupFolder)
		return m.Sessions.ClearSession(ctx, groupFolder, store)
	}

	// Persist new session ID if provided
	if newSessionID != "" {
		return m.Sessions.PersistSession(ctx, groupFolder, newSessionID, store)
	}

	return nil
}

// Acquire reserves a slot for a group. Returns error if at capacity or group already running.
func (m *Manager) Acquire(groupFolder string) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	if _, exists := m.active[groupFolder]; exists {
		return fmt.Errorf("group %q already has an active process", groupFolder)
	}
	if len(m.active) >= m.maxConcurrent {
		return fmt.Errorf("at capacity (%d/%d concurrent processes)", len(m.active), m.maxConcurrent)
	}
	return nil
}

// Register stores a started process for tracking.
func (m *Manager) Register(groupFolder string, proc *Process) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.active[groupFolder] = proc
}

// Release removes a process from tracking.
func (m *Manager) Release(groupFolder string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.active, groupFolder)
}

// ActiveCount returns how many processes are currently running.
func (m *Manager) ActiveCount() int {
	m.mu.Lock()
	defer m.mu.Unlock()
	return len(m.active)
}

// ActiveGroups returns the list of currently active group folders.
func (m *Manager) ActiveGroups() []string {
	m.mu.Lock()
	defer m.mu.Unlock()
	groups := make([]string, 0, len(m.active))
	for g := range m.active {
		groups = append(groups, g)
	}
	return groups
}

// KillAll terminates all active processes. Used during shutdown.
func (m *Manager) KillAll() {
	m.mu.Lock()
	defer m.mu.Unlock()
	for group, proc := range m.active {
		slog.Info("killing subprocess", "group", group)
		proc.Kill()
	}
}

// WaitAll waits for all active processes to exit, with a timeout.
func (m *Manager) WaitAll(timeout time.Duration) {
	m.mu.Lock()
	procs := make(map[string]*Process, len(m.active))
	for k, v := range m.active {
		procs[k] = v
	}
	m.mu.Unlock()

	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	var wg sync.WaitGroup
	for group, proc := range procs {
		wg.Add(1)
		go func(g string, p *Process) {
			defer wg.Done()
			select {
			case <-p.Done():
			case <-ctx.Done():
				slog.Warn("force-killing subprocess on timeout", "group", g)
				p.Kill()
			}
		}(group, proc)
	}
	wg.Wait()
}
