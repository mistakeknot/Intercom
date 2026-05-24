package a2a

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/mistakeknot/intercom/internal/transport"
)

// Config configures a Server. AgentName is the Sylveste agent identity served
// by this endpoint (e.g. "hermes"); Card is the AgentCard returned at
// /.well-known/agent.json. InboundBuffer controls Subscribe channel capacity;
// per the transport contract this MUST stay around 100 to deliver backpressure
// to the wire (here: HTTP request handlers block until the channel drains).
//
// Store is the Task store used by /tasks endpoints and by POST /messages to
// register newly-created tasks. If nil, New constructs an in-memory store.
// Tests can inject a custom store; v2 will inject the Dolt-backed variant.
type Config struct {
	AgentName     string
	Card          AgentCard
	InboundBuffer int
	Store         *Store
}

// Server is the A2A HTTP transport. Implements transport.Transport.
//
// Subscribe is called once per process lifecycle by the routing layer; Send
// is called per outbound message. Inbound messages arriving on POST /messages
// are translated to transport.InboundMessage and pushed onto the Subscribe
// channel. The HTTP handler blocks until the channel accepts the message,
// delivering backpressure to the calling A2A peer (which sees a slow response
// and naturally throttles).
//
// The outbound path is stubbed for v1 — Send returns an error noting the
// outbound client lands in a sub-bead. Inbound is sufficient for the bulk of
// human-↔-agent traffic; outbound agent-↔-agent calls become important when
// Sylveste agents start invoking external A2A peers.
type Server struct {
	cfg     Config
	store   *Store
	broker  *Broker
	mux     *http.ServeMux
	inbound chan transport.InboundMessage

	subscribedOnce sync.Once
	subscribeErr   error

	lastInbound  atomic.Int64
	lastOutbound atomic.Int64
}

// New constructs a Server with the given config. The HTTP handler is reachable
// via [Server.Handler]; mount it on whatever listener the host process owns
// (Intercom hosts one listener and demuxes by Host header for multi-agent
// co-residency).
//
// Routes registered (Go 1.22+ method-aware patterns):
//
//	GET  /.well-known/agent.json   Agent Card discovery
//	POST /messages                 SendMessage (sync ack)
//	POST /messages:stream          501 — SSE under sub-bead .1
//	GET  /tasks                    ListTasks with cursor pagination
//	GET  /tasks/{id}               GetTask; id may carry :subscribe suffix → 501
//	POST /tasks/{id}               id MUST carry :cancel suffix → CancelTask
func New(cfg Config) *Server {
	if cfg.InboundBuffer <= 0 {
		cfg.InboundBuffer = 100
	}
	if cfg.Card.ProtocolVersion == "" {
		cfg.Card.ProtocolVersion = "1.0"
	}
	if cfg.Store == nil {
		cfg.Store = NewStore()
	}
	broker := NewBroker()
	cfg.Store.SetPublisher(broker)
	cfg.Card.Capabilities.Streaming = true
	s := &Server{
		cfg:     cfg,
		store:   cfg.Store,
		broker:  broker,
		mux:     http.NewServeMux(),
		inbound: make(chan transport.InboundMessage, cfg.InboundBuffer),
	}
	s.mux.HandleFunc("GET /.well-known/agent.json", s.handleAgentCard)
	s.mux.HandleFunc("POST /messages", s.handleMessages)
	s.mux.HandleFunc("POST /messages:stream", s.handleMessagesStream)
	s.mux.HandleFunc("GET /tasks", s.handleListTasks)
	s.mux.HandleFunc("GET /tasks/{id}", s.handleGetTask)
	s.mux.HandleFunc("POST /tasks/{id}", s.handlePostTask)
	return s
}

// Handler returns the HTTP handler for this A2A server.
func (s *Server) Handler() http.Handler {
	return s.mux
}

// Store returns the Task store wired into this server. Useful for tests and
// for the routing layer to mark tasks COMPLETED when a sprint finishes.
func (s *Server) Store() *Store {
	return s.store
}

// Name reports the transport identifier ("a2a"). Stable across releases.
func (s *Server) Name() string { return "a2a" }

// Send delivers an outbound message to a remote A2A peer.
//
// v1: not yet implemented. The outbound client lands in a sub-bead. Returning
// ErrOutboundNotImplemented lets the routing layer fall back to other transports
// or surface the gap to the user without crashing.
func (s *Server) Send(ctx context.Context, msg transport.OutboundMessage) error {
	return ErrOutboundNotImplemented
}

// ErrOutboundNotImplemented is returned by Send until the outbound A2A client
// lands. Routing layer recognizes it and either retries on another transport
// or surfaces a friendly message; never treated as a transient error.
var ErrOutboundNotImplemented = errors.New("a2a: outbound client not implemented in v1 — see sylveste-ewy3.4.1 sub-beads")

// Subscribe yields canonical inbound messages translated from POST /messages.
// Called once per process lifecycle by the routing layer; subsequent calls
// return ErrAlreadySubscribed.
func (s *Server) Subscribe(ctx context.Context) (<-chan transport.InboundMessage, error) {
	s.subscribedOnce.Do(func() {
		go func() {
			<-ctx.Done()
			close(s.inbound)
		}()
	})
	if s.subscribeErr != nil {
		return nil, s.subscribeErr
	}
	return s.inbound, nil
}

// ErrAlreadySubscribed is returned by a second call to Subscribe on the same
// Server instance. The contract is one Subscribe per process lifecycle.
var ErrAlreadySubscribed = errors.New("a2a: Subscribe already called")

// Health reports the transport's operational state. Cheap; called by /health
// and SessionStart hooks frequently.
func (s *Server) Health(ctx context.Context) transport.Health {
	return transport.Health{
		Healthy:        true,
		LastInboundAt:  s.lastInbound.Load(),
		LastOutboundAt: s.lastOutbound.Load(),
	}
}

func (s *Server) handleAgentCard(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(s.cfg.Card); err != nil {
		http.Error(w, "encode agent card: "+err.Error(), http.StatusInternalServerError)
		return
	}
}

func (s *Server) handleMessages(w http.ResponseWriter, r *http.Request) {
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

	// Register a Task BEFORE pushing to the inbound channel so listing/cancel
	// is possible even mid-flight. Initial state is SUBMITTED; transitions to
	// WORKING once we successfully hand off to the routing layer.
	task := s.store.Create(req.Message.ContextID)

	inbound := ToInbound(req.Message, s.cfg.AgentName)
	if inbound.WireMetadata == nil {
		inbound.WireMetadata = map[string]string{}
	}
	inbound.WireMetadata["a2a.taskId"] = task.ID

	// Deliver to the inbound channel under request context. Block here gives
	// backpressure to the calling peer — they see a slow response and naturally
	// throttle without us buffering unboundedly. Cancel the task and return
	// 408 if the request context is cancelled before handoff.
	select {
	case s.inbound <- inbound:
		s.lastInbound.Store(time.Now().UnixNano())
		if updated, err := s.store.UpdateStatus(task.ID, TaskStateWorking); err == nil {
			task = updated
		}
	case <-r.Context().Done():
		// Best effort: mark cancelled so /tasks listings reflect the abandoned task.
		if updated, err := s.store.Cancel(task.ID); err == nil {
			task = updated
		}
		http.Error(w, "request cancelled", http.StatusRequestTimeout)
		return
	}

	resp := SendMessageResponse{
		Task: &task,
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusAccepted)
	if err := json.NewEncoder(w).Encode(resp); err != nil {
		// Already wrote 202 — nothing more we can do.
		_ = err
	}
}

func (s *Server) handleListTasks(w http.ResponseWriter, r *http.Request) {
	cursor := r.URL.Query().Get("cursor")
	limit := 50
	if raw := r.URL.Query().Get("limit"); raw != "" {
		n, err := strconv.Atoi(raw)
		if err != nil {
			http.Error(w, "invalid limit: "+err.Error(), http.StatusBadRequest)
			return
		}
		limit = n
	}

	tasks, next := s.store.List(cursor, limit)
	resp := ListTasksResponse{
		Tasks:      tasks,
		NextCursor: next,
	}

	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(resp); err != nil {
		http.Error(w, "encode list response: "+err.Error(), http.StatusInternalServerError)
		return
	}
}

func (s *Server) handleGetTask(w http.ResponseWriter, r *http.Request) {
	id, suffix := splitTaskIDSuffix(r.PathValue("id"))
	if suffix == "subscribe" {
		s.handleTaskSubscribe(w, r, id)
		return
	}
	if suffix != "" {
		http.Error(w, "unknown task action: "+suffix, http.StatusBadRequest)
		return
	}

	task, ok := s.store.Get(id)
	if !ok {
		http.Error(w, "task not found: "+id, http.StatusNotFound)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(task); err != nil {
		http.Error(w, "encode task: "+err.Error(), http.StatusInternalServerError)
		return
	}
}

func (s *Server) handlePostTask(w http.ResponseWriter, r *http.Request) {
	id, suffix := splitTaskIDSuffix(r.PathValue("id"))
	switch suffix {
	case "cancel":
		task, err := s.store.Cancel(id)
		switch {
		case errors.Is(err, ErrTaskNotFound):
			http.Error(w, "task not found: "+id, http.StatusNotFound)
			return
		case errors.Is(err, ErrTaskTerminal):
			http.Error(w, "task already in terminal state", http.StatusConflict)
			return
		case err != nil:
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		if encErr := json.NewEncoder(w).Encode(task); encErr != nil {
			http.Error(w, "encode task: "+encErr.Error(), http.StatusInternalServerError)
		}
	case "":
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
	default:
		http.Error(w, "unknown task action: "+suffix, http.StatusBadRequest)
	}
}

// splitTaskIDSuffix separates an "<id>:<action>" path-value into its parts.
// Returns (id, action) where action is "cancel", "subscribe", etc. When the
// path value has no colon, action is empty and id is the full value.
func splitTaskIDSuffix(v string) (id, action string) {
	if i := strings.LastIndex(v, ":"); i >= 0 {
		return v[:i], v[i+1:]
	}
	return v, ""
}

func (s *Server) handleNotImplemented(w http.ResponseWriter, r *http.Request) {
	http.Error(w, "endpoint not implemented in v1 — see sylveste-ewy3.4.1 sub-beads", http.StatusNotImplemented)
}
