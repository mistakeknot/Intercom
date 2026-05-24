package a2a

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/mistakeknot/intercom/internal/transport"
)

// Resolver maps a canonical sylveste://agent/<name> URI to the base HTTP URL
// at which that peer's A2A endpoints are mounted (e.g.
// https://hermes.sylveste.example). The resolved value is the prefix under
// which /messages, /tasks, and /.well-known live; no trailing slash.
//
// MapResolver is sufficient for tests and small static deployments. Production
// callers wire a resolver that queries the agent registry / DNS / service
// catalogue.
type Resolver interface {
	// Resolve returns the base URL for recipientURI, or an error wrapping
	// ErrUnknownRecipient when the recipient is unknown.
	Resolve(ctx context.Context, recipientURI string) (string, error)
}

// MapResolver is a static recipient→baseURL map. Safe for concurrent reads;
// callers MUST NOT mutate after the resolver has been wired into a Server.
type MapResolver map[string]string

// Resolve looks recipientURI up in the map. Returns a wrapped
// ErrUnknownRecipient when no entry matches.
func (m MapResolver) Resolve(_ context.Context, recipientURI string) (string, error) {
	base, ok := m[recipientURI]
	if !ok {
		return "", fmt.Errorf("a2a: %w: %s", ErrUnknownRecipient, recipientURI)
	}
	return base, nil
}

// ErrUnknownRecipient is the sentinel a Resolver returns when the recipient
// URI is not in its mapping. Wrapped errors carry the URI for diagnostics;
// callers use errors.Is to detect the condition.
var ErrUnknownRecipient = errors.New("unknown recipient")

// ErrNoResolver is returned by Send when the Server was constructed without
// a Resolver. The transport cannot route outbound messages without one.
var ErrNoResolver = errors.New("a2a: outbound Resolver not configured")

// bearerTokenKey is the context key for a pre-acquired OAuth2 bearer token.
// Per docs/canon/intercom-transport-target.md §Authentication, token
// acquisition (client_credentials grant + Resource Indicators) lives in
// sylveste-ewy3.4.1.4 (Gridfire-v1 integration). The outbound client at this
// stage simply passes whatever the caller put in the context through to the
// Authorization header.
type bearerTokenKey struct{}

// WithBearerToken returns ctx carrying token. Send reads it back and emits
// Authorization: Bearer <token>. An empty token is treated as "no token".
func WithBearerToken(ctx context.Context, token string) context.Context {
	return context.WithValue(ctx, bearerTokenKey{}, token)
}

func bearerTokenFromContext(ctx context.Context) string {
	v, _ := ctx.Value(bearerTokenKey{}).(string)
	return v
}

// DefaultAgentCardTTL is how long agent cards stay cached after a successful
// fetch. Five minutes balances liveness against the cost of refetching on
// every outbound send.
const DefaultAgentCardTTL = 5 * time.Minute

// cardCacheEntry holds one peer's agent card and the time it was fetched.
type cardCacheEntry struct {
	card      AgentCard
	fetchedAt time.Time
}

// agentCardCache stores per-base-URL cards. Safe for concurrent use.
type agentCardCache struct {
	mu      sync.RWMutex
	ttl     time.Duration
	entries map[string]cardCacheEntry
}

func newAgentCardCache(ttl time.Duration) *agentCardCache {
	if ttl <= 0 {
		ttl = DefaultAgentCardTTL
	}
	return &agentCardCache{
		ttl:     ttl,
		entries: map[string]cardCacheEntry{},
	}
}

// get returns the cached card and true when fresh; zero value + false
// otherwise (entry missing or beyond TTL).
func (c *agentCardCache) get(baseURL string) (AgentCard, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	e, ok := c.entries[baseURL]
	if !ok || time.Since(e.fetchedAt) > c.ttl {
		return AgentCard{}, false
	}
	return e.card, true
}

// put records a fresh fetch.
func (c *agentCardCache) put(baseURL string, card AgentCard) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.entries[baseURL] = cardCacheEntry{card: card, fetchedAt: time.Now()}
}

// sendOutbound translates msg into an A2A SendMessageRequest and POSTs it to
// the resolved peer endpoint. Used by Server.Send.
//
// Error cases (wrapped for diagnostics):
//
//   - ErrNoResolver         no Resolver configured on the Server
//   - ErrUnknownRecipient   resolver doesn't know msg.RecipientURI
//   - HTTP transport error  network failure, DNS failure, context cancel
//   - peer status error     peer returned non-2xx
//
// On success returns nil and updates lastOutbound.
//
// The peer's /.well-known/agent.json is fetched best-effort to warm the cache;
// fetch failures are NOT fatal — the actual /messages POST proceeds either
// way. Capability negotiation that depends on the card (e.g. refusing to
// stream to a peer without Capabilities.Streaming) lives in a future bead.
func (s *Server) sendOutbound(ctx context.Context, msg transport.OutboundMessage) error {
	if s.resolver == nil {
		return ErrNoResolver
	}
	baseURL, err := s.resolver.Resolve(ctx, msg.RecipientURI)
	if err != nil {
		return fmt.Errorf("a2a: resolve %s: %w", msg.RecipientURI, err)
	}
	baseURL = strings.TrimRight(baseURL, "/")

	// Warm the card cache. Errors here are intentionally swallowed; the
	// outbound POST is the load-bearing call.
	_, _ = s.fetchAgentCard(ctx, baseURL)

	a2aMsg := FromOutbound(msg)
	if a2aMsg.Metadata == nil {
		a2aMsg.Metadata = map[string]string{}
	}
	// Identify ourselves so the peer's extractSenderURI attributes us
	// correctly. Without this the peer sees an anonymous inbound message.
	a2aMsg.Metadata["sylveste.senderUri"] = SenderURIForAgent(s.cfg.AgentName)

	payload := SendMessageRequest{Message: a2aMsg}
	buf, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("a2a: marshal SendMessageRequest: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+"/messages", bytes.NewReader(buf))
	if err != nil {
		return fmt.Errorf("a2a: build outbound request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	if token := bearerTokenFromContext(ctx); token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}

	resp, err := s.httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("a2a: POST %s/messages: %w", baseURL, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("a2a: peer returned %d for POST %s/messages", resp.StatusCode, baseURL)
	}

	// Drain the body so the underlying connection can be reused by the
	// HTTP client's keepalive pool. We don't surface the Task / Message
	// reply to the routing layer yet — async correlation lives on the
	// streaming path (sylveste-ewy3.4.1.1).
	_ = json.NewDecoder(resp.Body).Decode(&SendMessageResponse{})

	s.lastOutbound.Store(time.Now().UnixNano())
	return nil
}

// fetchAgentCard does GET /.well-known/agent.json against baseURL with the
// cache in front. Returns the cached value on hit; on miss issues an HTTP
// request and caches the result.
//
// Failures surface to the caller; sendOutbound treats them as non-fatal.
func (s *Server) fetchAgentCard(ctx context.Context, baseURL string) (AgentCard, error) {
	if s.cardCache == nil {
		// Defensive: only reachable if New() was bypassed in a test.
		return AgentCard{}, errors.New("a2a: card cache not initialized")
	}
	if card, ok := s.cardCache.get(baseURL); ok {
		return card, nil
	}

	cardURL := baseURL + "/.well-known/agent.json"
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, cardURL, nil)
	if err != nil {
		return AgentCard{}, fmt.Errorf("a2a: build card request: %w", err)
	}
	req.Header.Set("Accept", "application/json")

	resp, err := s.httpClient.Do(req)
	if err != nil {
		return AgentCard{}, fmt.Errorf("a2a: GET %s: %w", cardURL, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return AgentCard{}, fmt.Errorf("a2a: card fetch %s returned %d", cardURL, resp.StatusCode)
	}
	var card AgentCard
	if err := json.NewDecoder(resp.Body).Decode(&card); err != nil {
		return AgentCard{}, fmt.Errorf("a2a: decode agent card: %w", err)
	}
	s.cardCache.put(baseURL, card)
	return card, nil
}
