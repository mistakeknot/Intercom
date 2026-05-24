package a2a

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"

	"github.com/mistakeknot/intercom/internal/transport"
)

// TestOutbound_RoundTrip verifies the canonical happy path: a caller Server's
// Send delivers a canonical OutboundMessage to a peer Server's Subscribe
// channel as a canonical InboundMessage, with SenderURI populated from the
// caller's AgentName.
//
// This is the acceptance criterion on sylveste-ewy3.4.1.3 — peer-to-peer
// agent traffic over native A2A without any external mock.
func TestOutbound_RoundTrip(t *testing.T) {
	peer := New(Config{
		AgentName: "peer-agent",
		Card: AgentCard{
			Name:    "Peer",
			URL:     "https://example.test/agents/peer",
			Version: "0.1.0",
		},
	})
	peerHTTP := httptest.NewServer(peer.Handler())
	defer peerHTTP.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	peerCh, err := peer.Subscribe(ctx)
	if err != nil {
		t.Fatalf("peer Subscribe: %v", err)
	}

	caller := New(Config{
		AgentName: "caller-agent",
		Card: AgentCard{
			Name:    "Caller",
			URL:     "https://example.test/agents/caller",
			Version: "0.1.0",
		},
		Resolver: MapResolver{
			"sylveste://agent/peer-agent": peerHTTP.URL,
		},
	})

	sendErr := make(chan error, 1)
	go func() {
		sendErr <- caller.Send(ctx, transport.OutboundMessage{
			RecipientURI: "sylveste://agent/peer-agent",
			ContextID:    "sylveste-test-ctx",
			TaskID:       "task-outbound-1",
			Parts:        []transport.Part{{Kind: transport.PartText, Text: "ping"}},
		})
	}()

	select {
	case inbound := <-peerCh:
		if inbound.TransportName != "a2a" {
			t.Errorf("TransportName = %q, want a2a", inbound.TransportName)
		}
		if inbound.SenderURI != "sylveste://agent/caller-agent" {
			t.Errorf("SenderURI = %q, want sylveste://agent/caller-agent", inbound.SenderURI)
		}
		if inbound.ContextID != "sylveste-test-ctx" {
			t.Errorf("ContextID = %q, want sylveste-test-ctx", inbound.ContextID)
		}
		if len(inbound.Parts) != 1 || inbound.Parts[0].Text != "ping" {
			t.Errorf("Parts = %+v", inbound.Parts)
		}
		if inbound.WireMetadata["a2a.recipientAgent"] != "peer-agent" {
			t.Errorf("recipientAgent = %q, want peer-agent", inbound.WireMetadata["a2a.recipientAgent"])
		}
	case <-time.After(time.Second):
		t.Fatal("peer did not receive inbound within 1s")
	}

	select {
	case err := <-sendErr:
		if err != nil {
			t.Errorf("caller.Send err = %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("caller.Send did not return within 1s")
	}

	h := caller.Health(ctx)
	if h.LastOutboundAt == 0 {
		t.Error("LastOutboundAt not updated after successful Send")
	}
}

// TestOutbound_NoResolver verifies Send fails cleanly when no Resolver is
// configured — the routing layer can detect this via errors.Is and either
// fall back to another transport or surface a configuration error.
func TestOutbound_NoResolver(t *testing.T) {
	srv := New(Config{
		AgentName: "no-resolver",
		Card:      AgentCard{Name: "NoResolver", URL: "u", Version: "v"},
	})
	err := srv.Send(context.Background(), transport.OutboundMessage{
		RecipientURI: "sylveste://agent/peer",
		Parts:        []transport.Part{{Kind: transport.PartText, Text: "x"}},
	})
	if !errors.Is(err, ErrNoResolver) {
		t.Errorf("Send err = %v, want ErrNoResolver", err)
	}
}

// TestOutbound_UnknownRecipient verifies that resolver lookup failure
// surfaces as a wrapped ErrUnknownRecipient detectable by errors.Is.
func TestOutbound_UnknownRecipient(t *testing.T) {
	srv := New(Config{
		AgentName: "u-r",
		Card:      AgentCard{Name: "UR", URL: "u", Version: "v"},
		Resolver:  MapResolver{},
	})
	err := srv.Send(context.Background(), transport.OutboundMessage{
		RecipientURI: "sylveste://agent/unknown",
		Parts:        []transport.Part{{Kind: transport.PartText, Text: "x"}},
	})
	if !errors.Is(err, ErrUnknownRecipient) {
		t.Errorf("Send err = %v, want wrapped ErrUnknownRecipient", err)
	}
}

// TestOutbound_BearerTokenPassThrough confirms a token added via
// WithBearerToken reaches the peer as Authorization: Bearer <token>. The
// stub server observes only the /messages POST so the (empty-auth) card
// fetch doesn't pollute the observation.
func TestOutbound_BearerTokenPassThrough(t *testing.T) {
	var observedAuth atomic.Value
	stub := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/.well-known/agent.json":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"name":"Stub","version":"v","url":"u","protocolVersion":"1.0","capabilities":{"streaming":false}}`))
		case "/messages":
			observedAuth.Store(r.Header.Get("Authorization"))
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusAccepted)
			_, _ = w.Write([]byte(`{}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer stub.Close()

	srv := New(Config{
		AgentName: "caller",
		Card:      AgentCard{Name: "Caller", URL: "u", Version: "v"},
		Resolver:  MapResolver{"sylveste://agent/stub": stub.URL},
	})
	ctx := WithBearerToken(context.Background(), "my-token")
	if err := srv.Send(ctx, transport.OutboundMessage{
		RecipientURI: "sylveste://agent/stub",
		Parts:        []transport.Part{{Kind: transport.PartText, Text: "hi"}},
	}); err != nil {
		t.Fatalf("Send: %v", err)
	}
	got, _ := observedAuth.Load().(string)
	if got != "Bearer my-token" {
		t.Errorf("Authorization = %q, want Bearer my-token", got)
	}
}

// TestOutbound_NoBearerTokenOmitsHeader checks that Authorization is absent
// when no token is in the context — important so the peer doesn't see a
// stray empty "Bearer " from a misconfigured caller.
func TestOutbound_NoBearerTokenOmitsHeader(t *testing.T) {
	var observedAuth atomic.Value
	observedAuth.Store("__unset__")
	stub := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/messages" {
			observedAuth.Store(r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusAccepted)
		_, _ = w.Write([]byte(`{}`))
	}))
	defer stub.Close()

	srv := New(Config{
		AgentName: "caller",
		Card:      AgentCard{Name: "Caller", URL: "u", Version: "v"},
		Resolver:  MapResolver{"sylveste://agent/stub": stub.URL},
	})
	if err := srv.Send(context.Background(), transport.OutboundMessage{
		RecipientURI: "sylveste://agent/stub",
		Parts:        []transport.Part{{Kind: transport.PartText, Text: "x"}},
	}); err != nil {
		t.Fatalf("Send: %v", err)
	}
	got, _ := observedAuth.Load().(string)
	if got != "" {
		t.Errorf("Authorization = %q, want empty (no token)", got)
	}
}

// TestOutbound_PeerNon2xxReturnsError verifies that a peer's error response
// is propagated rather than silently swallowed. The card endpoint is still
// served as 200 so the failure is unambiguously on the /messages POST.
func TestOutbound_PeerNon2xxReturnsError(t *testing.T) {
	stub := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/.well-known/agent.json":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"name":"Stub","version":"v","url":"u","protocolVersion":"1.0","capabilities":{"streaming":false}}`))
		default:
			http.Error(w, "no", http.StatusForbidden)
		}
	}))
	defer stub.Close()

	srv := New(Config{
		AgentName: "caller",
		Card:      AgentCard{Name: "Caller", URL: "u", Version: "v"},
		Resolver:  MapResolver{"sylveste://agent/stub": stub.URL},
	})
	err := srv.Send(context.Background(), transport.OutboundMessage{
		RecipientURI: "sylveste://agent/stub",
		Parts:        []transport.Part{{Kind: transport.PartText, Text: "hi"}},
	})
	if err == nil {
		t.Fatal("Send err = nil, want non-2xx error")
	}
}

// TestAgentCardCache_HitsAfterFirstFetch verifies the card cache: three
// outbound sends to the same peer issue exactly one GET /.well-known/agent.json.
// Without caching this would balloon to N card fetches per N sends.
func TestAgentCardCache_HitsAfterFirstFetch(t *testing.T) {
	var hits atomic.Int32
	stub := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/.well-known/agent.json":
			hits.Add(1)
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"name":"Stub","version":"v","url":"u","protocolVersion":"1.0","capabilities":{"streaming":false}}`))
		default:
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusAccepted)
			_, _ = w.Write([]byte(`{}`))
		}
	}))
	defer stub.Close()

	srv := New(Config{
		AgentName: "caller",
		Card:      AgentCard{Name: "Caller", URL: "u", Version: "v"},
		Resolver:  MapResolver{"sylveste://agent/stub": stub.URL},
		// TTL well above test duration so first fetch satisfies all later sends.
		AgentCardTTL: 5 * time.Minute,
	})
	ctx := context.Background()
	for i := 0; i < 3; i++ {
		if err := srv.Send(ctx, transport.OutboundMessage{
			RecipientURI: "sylveste://agent/stub",
			Parts:        []transport.Part{{Kind: transport.PartText, Text: "x"}},
		}); err != nil {
			t.Fatalf("Send #%d: %v", i, err)
		}
	}
	if hits.Load() != 1 {
		t.Errorf("agent.json fetches = %d, want 1 (cached after first)", hits.Load())
	}
}

// TestAgentCardCache_TTLExpiry verifies entries expire correctly: a fresh
// fetch happens once the TTL passes. Uses a 10ms TTL so the test stays fast.
func TestAgentCardCache_TTLExpiry(t *testing.T) {
	cache := newAgentCardCache(10 * time.Millisecond)
	cache.put("https://peer", AgentCard{Name: "Peer"})

	if _, ok := cache.get("https://peer"); !ok {
		t.Fatal("cache miss immediately after put")
	}
	time.Sleep(20 * time.Millisecond)
	if _, ok := cache.get("https://peer"); ok {
		t.Error("cache hit after TTL expiry")
	}
}

// TestMapResolver_UnknownReturnsWrappedSentinel exercises the sentinel
// pathway directly to lock down errors.Is behavior for callers that build
// their own error handling on top.
func TestMapResolver_UnknownReturnsWrappedSentinel(t *testing.T) {
	r := MapResolver{}
	_, err := r.Resolve(context.Background(), "sylveste://agent/x")
	if !errors.Is(err, ErrUnknownRecipient) {
		t.Errorf("Resolve err = %v, want wrapped ErrUnknownRecipient", err)
	}
}
