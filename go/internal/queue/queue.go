// Package queue implements a per-group message queue that ensures
// only one agent subprocess runs per group at a time.
package queue

import (
	"context"
	"log/slog"
	"sync"
	"time"
)

// ProcessFunc is called when a group has pending messages.
// Returns true if messages were processed successfully.
type ProcessFunc func(ctx context.Context, chatJID string) bool

// Queue manages per-group message processing with fair scheduling.
// Each group gets at most one concurrent processor.
type Queue struct {
	processFn ProcessFunc
	mu        sync.Mutex
	pending   map[string]bool // groups with pending messages
	running   map[string]bool // groups currently being processed
	wakeup    chan struct{}
}

func New(processFn ProcessFunc) *Queue {
	return &Queue{
		processFn: processFn,
		pending:   make(map[string]bool),
		running:   make(map[string]bool),
		wakeup:    make(chan struct{}, 1),
	}
}

// Enqueue marks a group as having pending messages.
func (q *Queue) Enqueue(chatJID string) {
	q.mu.Lock()
	q.pending[chatJID] = true
	q.mu.Unlock()

	// Non-blocking wakeup signal
	select {
	case q.wakeup <- struct{}{}:
	default:
	}
}

// Run starts the dispatch loop. Blocks until ctx is cancelled.
func (q *Queue) Run(ctx context.Context) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-q.wakeup:
		case <-time.After(1 * time.Second):
		}

		q.dispatchReady(ctx)
	}
}

func (q *Queue) dispatchReady(ctx context.Context) {
	q.mu.Lock()
	// Find groups that are pending but not running
	var ready []string
	for jid := range q.pending {
		if !q.running[jid] {
			ready = append(ready, jid)
		}
	}
	// Mark as running and remove from pending
	for _, jid := range ready {
		q.running[jid] = true
		delete(q.pending, jid)
	}
	q.mu.Unlock()

	// Dispatch each ready group
	for _, jid := range ready {
		jid := jid
		go func() {
			defer func() {
				q.mu.Lock()
				delete(q.running, jid)
				q.mu.Unlock()
			}()

			slog.Debug("processing group", "chat_jid", jid)
			ok := q.processFn(ctx, jid)
			if !ok {
				slog.Warn("group processing returned false", "chat_jid", jid)
			}
		}()
	}
}

// PendingCount returns the number of groups waiting.
func (q *Queue) PendingCount() int {
	q.mu.Lock()
	defer q.mu.Unlock()
	return len(q.pending)
}

// RunningCount returns the number of groups currently processing.
func (q *Queue) RunningCount() int {
	q.mu.Lock()
	defer q.mu.Unlock()
	return len(q.running)
}
