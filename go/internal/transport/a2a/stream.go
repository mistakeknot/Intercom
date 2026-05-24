package a2a

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"time"
)

// handleMessagesStream is the SSE counterpart of handleMessages (POST /messages:stream).
//
// It performs the same task-creation + inbound handoff as the sync endpoint,
// but instead of returning a single JSON envelope it holds the HTTP connection
// open and streams TaskStatusUpdateEvent / TaskArtifactUpdateEvent / MessageEvent
// values until the task reaches a terminal state or the client disconnects.
//
// Wire format: standard SSE (`event: <kind>\ndata: <json>\n\n`). The first
// event is always a SUBMITTED status carrying the freshly-minted task; the
// final event has Status.Final=true.
func (s *Server) handleMessagesStream(w http.ResponseWriter, r *http.Request) {
	if s.broker == nil {
		// Defensive — New always constructs a broker, but a future code path
		// could disable it. Better to 501 than to hang with no event source.
		s.handleNotImplemented(w, r)
		return
	}

	var req SendMessageRequest
	dec := json.NewDecoder(r.Body)
	dec.DisallowUnknownFields()
	if err := dec.Decode(&req); err != nil {
		http.Error(w, "decode SendMessageRequest: "+err.Error(), http.StatusBadRequest)
		return
	}
	if req.Message.MessageID == "" {
		http.Error(w, "message.messageId required", http.StatusBadRequest)
		return
	}
	if req.Message.Role == "" {
		req.Message.Role = RoleUser
	}

	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "streaming unsupported by HTTP layer", http.StatusInternalServerError)
		return
	}

	task := s.store.Create(req.Message.ContextID)

	// Subscribe BEFORE handing off to the inbound channel so the SUBMITTED →
	// WORKING transition (published by Store.UpdateStatus) is guaranteed to
	// land on this subscriber. Race-free by construction.
	sub := s.broker.Subscribe(task.ID)
	defer s.broker.Unsubscribe(task.ID, sub)

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	// Emit the initial SUBMITTED event up front so clients see a task handle
	// immediately even if the inbound handoff blocks on backpressure.
	if err := writeStreamEvent(w, flusher, StreamEvent{
		Kind: StreamEventStatus,
		Status: &TaskStatusUpdateEvent{
			TaskID:    task.ID,
			ContextID: task.ContextID,
			Status:    task.Status,
			Final:     false,
		},
	}); err != nil {
		return
	}

	inbound := ToInbound(req.Message, s.cfg.AgentName)
	if inbound.WireMetadata == nil {
		inbound.WireMetadata = map[string]string{}
	}
	inbound.WireMetadata["a2a.taskId"] = task.ID

	select {
	case s.inbound <- inbound:
		s.lastInbound.Store(time.Now().UnixNano())
		// UpdateStatus publishes a WORKING event to the broker; we'll see it
		// in the range loop below alongside any subsequent transitions.
		if _, err := s.store.UpdateStatus(task.ID, TaskStateWorking); err != nil {
			// Already terminal somehow (shouldn't happen for a fresh task).
			// Either way the broker subscriber will see the terminal event.
			_ = err
		}
	case <-r.Context().Done():
		if _, err := s.store.Cancel(task.ID); err != nil {
			_ = err
		}
		return
	}

	streamUntilFinal(r, w, flusher, sub)
}

// handleTaskSubscribe is the SSE handler for GET /tasks/{id}:subscribe.
//
// Backfills the current task state as the first event, then streams further
// updates until the task reaches terminal state or the client disconnects.
// If the task is already terminal at subscribe time, the handler emits one
// final event and closes.
func (s *Server) handleTaskSubscribe(w http.ResponseWriter, r *http.Request, id string) {
	if s.broker == nil {
		s.handleNotImplemented(w, r)
		return
	}

	task, ok := s.store.Get(id)
	if !ok {
		http.Error(w, "task not found: "+id, http.StatusNotFound)
		return
	}

	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "streaming unsupported by HTTP layer", http.StatusInternalServerError)
		return
	}

	sub := s.broker.Subscribe(id)
	defer s.broker.Unsubscribe(id, sub)

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	// Backfill current status. If already terminal, write final and return —
	// no further events will arrive on the subscriber channel.
	final := isTerminalState(task.Status.State)
	if err := writeStreamEvent(w, flusher, StreamEvent{
		Kind: StreamEventStatus,
		Status: &TaskStatusUpdateEvent{
			TaskID:    task.ID,
			ContextID: task.ContextID,
			Status:    task.Status,
			Final:     final,
		},
	}); err != nil {
		return
	}
	if final {
		return
	}

	streamUntilFinal(r, w, flusher, sub)
}

// streamUntilFinal pumps events from sub onto the SSE response until the
// channel closes, the request context cancels, or a Final=true status event
// is observed. Shared by both streaming handlers.
func streamUntilFinal(r *http.Request, w http.ResponseWriter, flusher http.Flusher, sub <-chan StreamEvent) {
	for {
		select {
		case <-r.Context().Done():
			return
		case evt, ok := <-sub:
			if !ok {
				return
			}
			if err := writeStreamEvent(w, flusher, evt); err != nil {
				return
			}
			if evt.Kind == StreamEventStatus && evt.Status != nil && evt.Status.Final {
				return
			}
		}
	}
}

// writeStreamEvent renders evt to the SSE wire and flushes. Returns the first
// write error encountered; callers use this to detect client disconnect and
// abort the streaming loop.
func writeStreamEvent(w http.ResponseWriter, flusher http.Flusher, evt StreamEvent) error {
	payload, err := marshalStreamPayload(evt)
	if err != nil {
		return err
	}
	if _, err := fmt.Fprintf(w, "event: %s\ndata: %s\n\n", evt.Kind, payload); err != nil {
		return err
	}
	flusher.Flush()
	return nil
}

// marshalStreamPayload picks the appropriate sub-struct for evt.Kind and
// returns its JSON encoding. Returns an error if the event is malformed
// (kind/body mismatch).
func marshalStreamPayload(evt StreamEvent) ([]byte, error) {
	switch evt.Kind {
	case StreamEventStatus:
		if evt.Status == nil {
			return nil, errors.New("a2a: status event has nil Status body")
		}
		return json.Marshal(evt.Status)
	case StreamEventArtifact:
		if evt.Artifact == nil {
			return nil, errors.New("a2a: artifact event has nil Artifact body")
		}
		return json.Marshal(evt.Artifact)
	case StreamEventMessage:
		if evt.Message == nil {
			return nil, errors.New("a2a: message event has nil Message body")
		}
		return json.Marshal(evt.Message)
	default:
		return nil, fmt.Errorf("a2a: unknown stream event kind %q", evt.Kind)
	}
}

