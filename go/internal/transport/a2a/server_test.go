package a2a

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/mistakeknot/intercom/internal/transport"
)

func newTestServer(t *testing.T) *Server {
	t.Helper()
	return New(Config{
		AgentName: "hermes",
		Card: AgentCard{
			Name:        "Hermes",
			Description: "Sylveste personal-assistant overlay",
			URL:         "https://example.test/agents/hermes",
			Version:     "0.1.0",
			Capabilities: AgentCapabilities{
				Streaming:         false,
				PushNotifications: false,
			},
			Skills: []AgentSkill{
				{
					ID:          "chat",
					Name:        "Chat",
					Description: "Conversational interaction",
					Tags:        []string{"chat", "personal"},
				},
			},
			SecuritySchemes: map[string]SecurityScheme{
				"oauth2": {
					Type:        "oauth2",
					Description: "OAuth2 with Resource Indicators per Gridfire-v1",
				},
			},
		},
	})
}

func TestAgentCardEndpoint(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := http.Get(ts.URL + "/.well-known/agent.json")
	if err != nil {
		t.Fatalf("GET agent.json: %v", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "application/json" {
		t.Fatalf("Content-Type = %q, want application/json", ct)
	}

	var card AgentCard
	if err := json.NewDecoder(resp.Body).Decode(&card); err != nil {
		t.Fatalf("decode card: %v", err)
	}
	if card.Name != "Hermes" {
		t.Errorf("Name = %q, want Hermes", card.Name)
	}
	if card.ProtocolVersion != "1.0" {
		t.Errorf("ProtocolVersion = %q, want 1.0 (default)", card.ProtocolVersion)
	}
	if len(card.Skills) != 1 || card.Skills[0].ID != "chat" {
		t.Errorf("Skills = %+v, want one chat skill", card.Skills)
	}
	if _, ok := card.SecuritySchemes["oauth2"]; !ok {
		t.Errorf("SecuritySchemes missing oauth2: %+v", card.SecuritySchemes)
	}
}

func TestSendMessageRoundTrip(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	ch, err := srv.Subscribe(ctx)
	if err != nil {
		t.Fatalf("Subscribe: %v", err)
	}

	body := SendMessageRequest{
		Message: Message{
			MessageID: "test-msg-001",
			Role:      RoleUser,
			ContextID: "sylveste-ewy3.4.1",
			Parts: []Part{
				{Text: "hello from peer"},
			},
			Metadata: map[string]string{
				"sylveste.senderUri": "sylveste://agent/test-peer",
			},
		},
	}
	buf, _ := json.Marshal(body)

	respCh := make(chan *http.Response, 1)
	errCh := make(chan error, 1)
	go func() {
		resp, err := http.Post(ts.URL+"/messages", "application/json", bytes.NewReader(buf))
		if err != nil {
			errCh <- err
			return
		}
		respCh <- resp
	}()

	select {
	case inbound := <-ch:
		if inbound.TransportName != "a2a" {
			t.Errorf("TransportName = %q, want a2a", inbound.TransportName)
		}
		if inbound.ContextID != "sylveste-ewy3.4.1" {
			t.Errorf("ContextID = %q", inbound.ContextID)
		}
		if inbound.SenderURI != "sylveste://agent/test-peer" {
			t.Errorf("SenderURI = %q", inbound.SenderURI)
		}
		if len(inbound.Parts) != 1 || inbound.Parts[0].Text != "hello from peer" {
			t.Errorf("Parts = %+v", inbound.Parts)
		}
		if inbound.WireMetadata["a2a.messageId"] != "test-msg-001" {
			t.Errorf("WireMetadata a2a.messageId = %q", inbound.WireMetadata["a2a.messageId"])
		}
		if inbound.WireMetadata["a2a.recipientAgent"] != "hermes" {
			t.Errorf("WireMetadata a2a.recipientAgent = %q", inbound.WireMetadata["a2a.recipientAgent"])
		}
	case err := <-errCh:
		t.Fatalf("POST /messages failed before inbound delivery: %v", err)
	case <-time.After(time.Second):
		t.Fatal("inbound channel did not receive message within 1s")
	}

	select {
	case resp := <-respCh:
		defer resp.Body.Close()
		if resp.StatusCode != http.StatusAccepted {
			t.Fatalf("status = %d, want 202", resp.StatusCode)
		}
		var smr SendMessageResponse
		if err := json.NewDecoder(resp.Body).Decode(&smr); err != nil {
			t.Fatalf("decode response: %v", err)
		}
		if smr.Task == nil {
			t.Fatal("response Task is nil")
		}
		if smr.Task.ContextID != "sylveste-ewy3.4.1" {
			t.Errorf("response Task.ContextID = %q", smr.Task.ContextID)
		}
		if smr.Task.Status.State != TaskStateWorking {
			t.Errorf("response Task.Status.State = %q, want WORKING", smr.Task.Status.State)
		}
	case err := <-errCh:
		t.Fatalf("POST /messages: %v", err)
	case <-time.After(time.Second):
		t.Fatal("POST /messages did not return within 1s")
	}

	h := srv.Health(ctx)
	if !h.Healthy {
		t.Error("Health.Healthy = false after successful inbound")
	}
	if h.LastInboundAt == 0 {
		t.Error("Health.LastInboundAt not updated after inbound")
	}
}

func TestSendMessageRejectsMissingMessageID(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	body := []byte(`{"message":{"role":"ROLE_USER","parts":[{"text":"hi"}]}}`)
	resp, err := http.Post(ts.URL+"/messages", "application/json", bytes.NewReader(body))
	if err != nil {
		t.Fatalf("POST /messages: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", resp.StatusCode)
	}
}

func TestStreamingEndpointReturns501(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := http.Post(ts.URL+"/messages:stream", "application/json", strings.NewReader("{}"))
	if err != nil {
		t.Fatalf("POST /messages:stream: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusNotImplemented {
		t.Fatalf("status = %d, want 501", resp.StatusCode)
	}
}

func TestServerImplementsTransport(t *testing.T) {
	var _ transport.Transport = (*Server)(nil)
}

func TestSendReturnsOutboundNotImplemented(t *testing.T) {
	srv := newTestServer(t)
	err := srv.Send(context.Background(), transport.OutboundMessage{
		RecipientURI: "sylveste://agent/peer",
		ContextID:    "sylveste-test",
		Parts:        []transport.Part{{Kind: transport.PartText, Text: "hi"}},
	})
	if err == nil {
		t.Fatal("Send did not return error")
	}
	if err != ErrOutboundNotImplemented {
		t.Errorf("Send err = %v, want ErrOutboundNotImplemented", err)
	}
}
