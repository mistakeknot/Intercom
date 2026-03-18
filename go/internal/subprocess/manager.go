package subprocess

import (
	"context"
	"fmt"
	"log/slog"
	"sync"
	"time"
)

// Manager tracks active subprocess slots and enforces concurrency limits.
type Manager struct {
	maxConcurrent int
	mu            sync.Mutex
	active        map[string]*Process // keyed by group folder
}

func NewManager(maxConcurrent int) *Manager {
	return &Manager{
		maxConcurrent: maxConcurrent,
		active:        make(map[string]*Process),
	}
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
