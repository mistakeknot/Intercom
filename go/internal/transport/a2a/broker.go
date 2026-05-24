package a2a

import (
	"sync"
)

// Broker fans out per-task lifecycle events to SSE subscribers.
//
// One subscriber set per Task ID. Subscribers receive every event published
// for that task in publish order. When a task reaches a terminal state, the
// broker closes all subscriber channels for that task and removes the entry
// — late subscribers for a terminated task get a non-nil closed channel
// (see Subscribe), so their handler's range loop returns immediately.
//
// Channels are buffered (DefaultSubscriberBuffer); a slow subscriber drops
// events rather than blocking the publisher. The drop is silent because
// terminal-state events are sent specifically with [Broker.PublishFinal]
// which retries once with the lock released to guarantee delivery; transient
// status events are best-effort by design.
type Broker struct {
	mu          sync.Mutex
	subscribers map[string][]chan StreamEvent
	terminated  map[string]struct{}
}

// DefaultSubscriberBuffer is the per-subscriber channel capacity. Sized to
// hold a comfortable burst of status + artifact events for a fast task
// without blocking the publisher; lifted to the package level so tests can
// exercise the slow-subscriber drop path with a smaller buffer.
const DefaultSubscriberBuffer = 16

// NewBroker returns an empty Broker. Cheap; one Broker per Server.
func NewBroker() *Broker {
	return &Broker{
		subscribers: make(map[string][]chan StreamEvent),
		terminated:  make(map[string]struct{}),
	}
}

// Subscribe returns a channel that will receive every event published for
// taskID from this point forward. If the task is already terminated the
// returned channel is closed; callers MUST check via the comma-ok idiom in
// their range loop.
//
// Callers MUST call [Broker.Unsubscribe] with the returned channel when done
// (typically via defer) to release the subscriber slot. Forgetting to
// unsubscribe leaks the channel until the broker is garbage collected.
func (b *Broker) Subscribe(taskID string) <-chan StreamEvent {
	b.mu.Lock()
	defer b.mu.Unlock()

	if _, done := b.terminated[taskID]; done {
		ch := make(chan StreamEvent)
		close(ch)
		return ch
	}

	ch := make(chan StreamEvent, DefaultSubscriberBuffer)
	b.subscribers[taskID] = append(b.subscribers[taskID], ch)
	return ch
}

// Unsubscribe removes ch from taskID's subscriber set and closes it.
// Safe to call after the task has terminated; Subscribe-on-terminated already
// returns a closed channel, so Unsubscribe is a no-op in that path.
func (b *Broker) Unsubscribe(taskID string, ch <-chan StreamEvent) {
	b.mu.Lock()
	defer b.mu.Unlock()

	subs := b.subscribers[taskID]
	for i, s := range subs {
		if (<-chan StreamEvent)(s) == ch {
			b.subscribers[taskID] = append(subs[:i], subs[i+1:]...)
			close(s)
			break
		}
	}
	if len(b.subscribers[taskID]) == 0 {
		delete(b.subscribers, taskID)
	}
}

// Publish delivers evt to every current subscriber of taskID. Non-blocking:
// subscribers whose channels are full silently drop the event. For events
// where loss is unacceptable (terminal-state transition), use [Broker.PublishFinal].
func (b *Broker) Publish(taskID string, evt StreamEvent) {
	b.mu.Lock()
	subs := append([]chan StreamEvent(nil), b.subscribers[taskID]...)
	b.mu.Unlock()

	for _, ch := range subs {
		select {
		case ch <- evt:
		default:
			// subscriber lagging; drop this event
		}
	}
}

// PublishFinal delivers a terminal-state event and closes all subscribers
// for taskID. Subscribers see the final event (best-effort with a slow-path
// retry) and then their channel closes, exiting the handler's range loop.
//
// After PublishFinal, the broker marks the task as terminated; later
// Subscribe calls for the same ID return an immediately-closed channel.
func (b *Broker) PublishFinal(taskID string, evt StreamEvent) {
	b.mu.Lock()
	subs := b.subscribers[taskID]
	delete(b.subscribers, taskID)
	b.terminated[taskID] = struct{}{}
	b.mu.Unlock()

	for _, ch := range subs {
		// Best-effort send: try non-blocking first, then block as a fallback
		// because final events MUST reach live subscribers. A slow subscriber
		// holds the publisher here briefly; acceptable for the terminal case.
		select {
		case ch <- evt:
		default:
			ch <- evt
		}
		close(ch)
	}
}

// SubscriberCount returns how many subscribers are currently attached to
// taskID. Diagnostic; used by tests and the /health surface (future).
func (b *Broker) SubscriberCount(taskID string) int {
	b.mu.Lock()
	defer b.mu.Unlock()
	return len(b.subscribers[taskID])
}
