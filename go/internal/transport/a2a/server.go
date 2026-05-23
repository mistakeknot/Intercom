package a2a

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
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
type Config struct {
	AgentName     string
	Card          AgentCard
	InboundBuffer int
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
func New(cfg Config) *Server {
	if cfg.InboundBuffer <= 0 {
		cfg.InboundBuffer = 100
	}
	if cfg.Card.ProtocolVersion == "" {
		cfg.Card.ProtocolVersion = "1.0"
	}
	if cfg.Card.Capabilities.Streaming || cfg.Card.Capabilities.PushNotifications {
		// Surface in the card but the v1 server does not implement these yet;
		// requests to streaming/push endpoints return 501.
	}
	s := &Server{
		cfg:     cfg,
		mux:     http.NewServeMux(),
		inbound: make(chan transport.InboundMessage, cfg.InboundBuffer),
	}
	s.mux.HandleFunc("/.well-known/agent.json", s.handleAgentCard)
	s.mux.HandleFunc("/messages", s.handleMessages)
	s.mux.HandleFunc("/messages:stream", s.handleNotImplemented)
	s.mux.HandleFunc("/tasks", s.handleNotImplemented)
	s.mux.HandleFunc("/tasks/", s.handleNotImplemented)
	return s
}

// Handler returns the HTTP handler for this A2A server.
func (s *Server) Handler() http.Handler {
	return s.mux
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
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(s.cfg.Card); err != nil {
		http.Error(w, "encode agent card: "+err.Error(), http.StatusInternalServerError)
		return
	}
}

func (s *Server) handleMessages(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
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

	inbound := ToInbound(req.Message, s.cfg.AgentName)

	// Deliver to the inbound channel under request context. Block here gives
	// backpressure to the calling peer — they see a slow response and naturally
	// throttle without us buffering unboundedly.
	select {
	case s.inbound <- inbound:
		s.lastInbound.Store(time.Now().UnixNano())
	case <-r.Context().Done():
		http.Error(w, "request cancelled", http.StatusRequestTimeout)
		return
	}

	// v1 synchronous reply: acknowledge receipt with a Task in WORKING state.
	// The streaming path (POST /messages:stream) will return the same Task and
	// then stream status updates over SSE; that lands in a sub-bead.
	resp := SendMessageResponse{
		Task: &Task{
			ID:        generateMessageID(),
			ContextID: req.Message.ContextID,
			Status: TaskStatus{
				State:     TaskStateWorking,
				Timestamp: time.Now().UTC().Format(time.RFC3339Nano),
			},
		},
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusAccepted)
	if err := json.NewEncoder(w).Encode(resp); err != nil {
		// Already wrote 202 — nothing more we can do.
		_ = err
	}
}

func (s *Server) handleNotImplemented(w http.ResponseWriter, r *http.Request) {
	http.Error(w, "endpoint not implemented in v1 — see sylveste-ewy3.4.1 sub-beads", http.StatusNotImplemented)
}
