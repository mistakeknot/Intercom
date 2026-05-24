package a2a

import (
	"errors"
	"sync"
	"time"
)

// Store is the v1 in-memory Task store. Tracks every Task created via
// POST /messages plus any externally-created tasks, exposes lookup +
// pagination + cancellation, and enforces terminal-state transitions per
// A2A spec §4.2.
//
// V2 lands a Dolt-backed store (bead sub-follow-up): the in-memory variant
// loses state on process restart, which is acceptable for the v1 spike but
// not for production where Tasks must survive intercomd restarts. Migration
// path: same interface, Dolt backend underneath; callers see no change.
//
// Concurrency: methods are safe for concurrent calls. List takes a read
// lock; Create / UpdateStatus / Cancel take a write lock. Reads return
// shallow copies of the Task struct — callers MUST NOT mutate nested
// slices/maps (Artifacts, History, Metadata) because they share the
// underlying storage with the live entry.
type Store struct {
	mu    sync.RWMutex
	tasks map[string]*Task
	order []string // insertion order; cursor pagination walks this index

	publisher Publisher // nil disables event publishing; set via SetPublisher
}

// Publisher is the broker side of the store→SSE event flow. The store calls
// Publish on each non-terminal status transition and PublishFinal on each
// terminal transition. The handler subscribes to the broker independently.
//
// The interface lets tests inject a recording publisher and lets the v2
// Dolt-backed store reuse the same Broker by satisfying this interface.
type Publisher interface {
	Publish(taskID string, evt StreamEvent)
	PublishFinal(taskID string, evt StreamEvent)
}

// NewStore returns an empty in-memory Task store. Call [Store.SetPublisher]
// to wire it to a broker for SSE streaming.
func NewStore() *Store {
	return &Store{
		tasks: make(map[string]*Task),
	}
}

// SetPublisher wires the store to a Publisher (typically [Broker]). Calling
// with nil disables publishing. Idempotent and safe at any time, but in
// practice it is called once at construction time in [New].
func (s *Store) SetPublisher(p Publisher) {
	s.mu.Lock()
	s.publisher = p
	s.mu.Unlock()
}

// ErrTaskNotFound is returned when an ID is unknown to the store.
var ErrTaskNotFound = errors.New("a2a: task not found")

// ErrTaskTerminal is returned when a transition is attempted on a task in
// a terminal state (COMPLETED, CANCELLED, FAILED, REJECTED).
var ErrTaskTerminal = errors.New("a2a: task is in terminal state")

// Create inserts a fresh Task in TASK_STATE_SUBMITTED for the given context
// (bead ID) and returns a copy of the stored entry. The Task ID is freshly
// generated; the caller does not pick the ID.
func (s *Store) Create(contextID string) Task {
	id := generateTaskID()
	now := time.Now().UTC().Format(time.RFC3339Nano)
	t := &Task{
		ID:        id,
		ContextID: contextID,
		Status: TaskStatus{
			State:     TaskStateSubmitted,
			Timestamp: now,
		},
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	s.tasks[id] = t
	s.order = append(s.order, id)
	return *t
}

// Get returns a shallow copy of the stored task and ok=true, or zero-value
// and ok=false if id is unknown.
func (s *Store) Get(id string) (Task, bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	t, ok := s.tasks[id]
	if !ok {
		return Task{}, false
	}
	return *t, true
}

// List returns up to limit tasks in insertion order, plus a nextCursor
// suitable for the next call. cursor is the ID of the last task returned
// in the previous page; pass "" for the first page. nextCursor is empty
// when there are no more tasks.
//
// limit is clamped to (0, 200]; values outside that range fall back to 50.
func (s *Store) List(cursor string, limit int) (tasks []Task, nextCursor string) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}

	s.mu.RLock()
	defer s.mu.RUnlock()

	startIdx := 0
	if cursor != "" {
		for i, id := range s.order {
			if id == cursor {
				startIdx = i + 1
				break
			}
		}
	}

	endIdx := startIdx + limit
	if endIdx > len(s.order) {
		endIdx = len(s.order)
	}

	if startIdx >= endIdx {
		return nil, ""
	}

	tasks = make([]Task, 0, endIdx-startIdx)
	for i := startIdx; i < endIdx; i++ {
		if t, ok := s.tasks[s.order[i]]; ok {
			tasks = append(tasks, *t)
		}
	}

	if endIdx < len(s.order) {
		nextCursor = s.order[endIdx-1]
	}
	return tasks, nextCursor
}

// UpdateStatus transitions a task to newState and stamps Status.Timestamp.
// Returns the updated task copy. Returns ErrTaskNotFound if id is unknown
// and ErrTaskTerminal if the current state is already terminal.
//
// Terminal-state guard: A2A treats COMPLETED, CANCELLED, FAILED, and
// REJECTED as terminal; once a task reaches one of these it cannot move
// (the next interaction needs a new task). The store enforces this.
//
// When a publisher is wired (via SetPublisher), a TaskStatusUpdateEvent is
// emitted on every successful transition. Final=true on terminal transitions.
func (s *Store) UpdateStatus(id string, newState TaskState) (Task, error) {
	s.mu.Lock()
	t, ok := s.tasks[id]
	if !ok {
		s.mu.Unlock()
		return Task{}, ErrTaskNotFound
	}
	if isTerminalState(t.Status.State) {
		s.mu.Unlock()
		return Task{}, ErrTaskTerminal
	}
	t.Status.State = newState
	t.Status.Timestamp = time.Now().UTC().Format(time.RFC3339Nano)
	updated := *t
	pub := s.publisher
	s.mu.Unlock()

	if pub != nil {
		evt := StreamEvent{
			Kind: StreamEventStatus,
			Status: &TaskStatusUpdateEvent{
				TaskID:    updated.ID,
				ContextID: updated.ContextID,
				Status:    updated.Status,
				Final:     isTerminalState(updated.Status.State),
			},
		}
		if evt.Status.Final {
			pub.PublishFinal(updated.ID, evt)
		} else {
			pub.Publish(updated.ID, evt)
		}
	}
	return updated, nil
}

// Cancel transitions a non-terminal task to TASK_STATE_CANCELLED. Returns
// the updated task copy. Equivalent to UpdateStatus(id, TaskStateCancelled)
// but kept as a separate method to make the call site obvious.
func (s *Store) Cancel(id string) (Task, error) {
	return s.UpdateStatus(id, TaskStateCancelled)
}

// Len reports how many tasks are currently stored. Cheap; intended for
// diagnostics and the /health endpoint.
func (s *Store) Len() int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return len(s.tasks)
}

// isTerminalState reports whether s is one of the four A2A terminal states.
// Kept package-private; the store is the only caller.
func isTerminalState(s TaskState) bool {
	switch s {
	case TaskStateCompleted, TaskStateCancelled, TaskStateFailed, TaskStateRejected:
		return true
	}
	return false
}
