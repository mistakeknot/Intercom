package a2a

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
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

// Streaming behavior (POST /messages:stream + GET /tasks/{id}:subscribe) is
// covered in stream_test.go. This file historically tested 501 stubs; now
// folded into the real behavior tests.

func TestTaskLifecycle_CreateListGetCancel(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	ch, err := srv.Subscribe(ctx)
	if err != nil {
		t.Fatalf("Subscribe: %v", err)
	}
	// Drain inbound so handleMessages can hand off without blocking.
	go func() {
		for range ch {
		}
	}()

	// 1. Create three tasks via POST /messages.
	var taskIDs []string
	for i := 0; i < 3; i++ {
		body := SendMessageRequest{
			Message: Message{
				MessageID: "msg-" + strconv.Itoa(i),
				Role:      RoleUser,
				ContextID: "sylveste-bead-lifecycle",
				Parts:     []Part{{Text: "hi"}},
			},
		}
		buf, _ := json.Marshal(body)
		resp, err := http.Post(ts.URL+"/messages", "application/json", bytes.NewReader(buf))
		if err != nil {
			t.Fatalf("POST /messages #%d: %v", i, err)
		}
		var smr SendMessageResponse
		if decErr := json.NewDecoder(resp.Body).Decode(&smr); decErr != nil {
			resp.Body.Close()
			t.Fatalf("decode #%d: %v", i, decErr)
		}
		resp.Body.Close()
		if smr.Task == nil {
			t.Fatalf("response #%d Task nil", i)
		}
		if smr.Task.Status.State != TaskStateWorking {
			t.Errorf("response #%d State = %q, want WORKING", i, smr.Task.Status.State)
		}
		taskIDs = append(taskIDs, smr.Task.ID)
	}

	// 2. GET /tasks lists all three.
	resp, err := http.Get(ts.URL + "/tasks")
	if err != nil {
		t.Fatalf("GET /tasks: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /tasks status = %d, want 200", resp.StatusCode)
	}
	var listResp ListTasksResponse
	if decErr := json.NewDecoder(resp.Body).Decode(&listResp); decErr != nil {
		t.Fatalf("decode list: %v", decErr)
	}
	if len(listResp.Tasks) != 3 {
		t.Errorf("List returned %d tasks, want 3", len(listResp.Tasks))
	}
	if listResp.NextCursor != "" {
		t.Errorf("NextCursor = %q, want empty (small page)", listResp.NextCursor)
	}

	// 3. GET /tasks?limit=2 paginates.
	resp, err = http.Get(ts.URL + "/tasks?limit=2")
	if err != nil {
		t.Fatalf("GET /tasks?limit=2: %v", err)
	}
	var page1 ListTasksResponse
	_ = json.NewDecoder(resp.Body).Decode(&page1)
	resp.Body.Close()
	if len(page1.Tasks) != 2 {
		t.Errorf("page1 len = %d, want 2", len(page1.Tasks))
	}
	if page1.NextCursor == "" {
		t.Error("page1 NextCursor empty, want cursor for page2")
	}

	resp, err = http.Get(ts.URL + "/tasks?limit=2&cursor=" + page1.NextCursor)
	if err != nil {
		t.Fatalf("GET /tasks page2: %v", err)
	}
	var page2 ListTasksResponse
	_ = json.NewDecoder(resp.Body).Decode(&page2)
	resp.Body.Close()
	if len(page2.Tasks) != 1 {
		t.Errorf("page2 len = %d, want 1", len(page2.Tasks))
	}
	if page2.NextCursor != "" {
		t.Errorf("page2 NextCursor = %q, want empty", page2.NextCursor)
	}

	// 4. GET /tasks/{id} returns the task by ID.
	resp, err = http.Get(ts.URL + "/tasks/" + taskIDs[0])
	if err != nil {
		t.Fatalf("GET /tasks/{id}: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /tasks/{id} status = %d, want 200", resp.StatusCode)
	}
	var got Task
	if decErr := json.NewDecoder(resp.Body).Decode(&got); decErr != nil {
		t.Fatalf("decode get: %v", decErr)
	}
	if got.ID != taskIDs[0] {
		t.Errorf("ID = %q, want %q", got.ID, taskIDs[0])
	}

	// 5. GET /tasks/unknown returns 404.
	resp404, err := http.Get(ts.URL + "/tasks/unknown-task-id")
	if err != nil {
		t.Fatalf("GET unknown: %v", err)
	}
	resp404.Body.Close()
	if resp404.StatusCode != http.StatusNotFound {
		t.Errorf("GET unknown status = %d, want 404", resp404.StatusCode)
	}

	// 6. POST /tasks/{id}:cancel transitions to CANCELLED.
	respCancel, err := http.Post(ts.URL+"/tasks/"+taskIDs[1]+":cancel", "application/json", nil)
	if err != nil {
		t.Fatalf("POST :cancel: %v", err)
	}
	defer respCancel.Body.Close()
	if respCancel.StatusCode != http.StatusOK {
		t.Fatalf("cancel status = %d, want 200", respCancel.StatusCode)
	}
	var cancelled Task
	if decErr := json.NewDecoder(respCancel.Body).Decode(&cancelled); decErr != nil {
		t.Fatalf("decode cancel: %v", decErr)
	}
	if cancelled.Status.State != TaskStateCancelled {
		t.Errorf("cancelled State = %q, want CANCELLED", cancelled.Status.State)
	}

	// 7. Second cancel on the same task returns 409 (already terminal).
	respCancel2, err := http.Post(ts.URL+"/tasks/"+taskIDs[1]+":cancel", "application/json", nil)
	if err != nil {
		t.Fatalf("POST :cancel #2: %v", err)
	}
	respCancel2.Body.Close()
	if respCancel2.StatusCode != http.StatusConflict {
		t.Errorf("second cancel status = %d, want 409", respCancel2.StatusCode)
	}

	// 8. GET /tasks/{id}:subscribe streams the current state then closes when terminal.
	// taskIDs[1] is already CANCELLED at this point; backfill emits one final event.
	respSub, err := http.Get(ts.URL + "/tasks/" + taskIDs[1] + ":subscribe")
	if err != nil {
		t.Fatalf("GET :subscribe: %v", err)
	}
	defer respSub.Body.Close()
	if respSub.StatusCode != http.StatusOK {
		t.Errorf("subscribe status = %d, want 200", respSub.StatusCode)
	}
	if ct := respSub.Header.Get("Content-Type"); ct != "text/event-stream" {
		t.Errorf("subscribe Content-Type = %q, want text/event-stream", ct)
	}
}

func TestPostTaskWithoutActionReturns405(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	task := srv.Store().Create("ctx")
	resp, err := http.Post(ts.URL+"/tasks/"+task.ID, "application/json", nil)
	if err != nil {
		t.Fatalf("POST /tasks/{id}: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusMethodNotAllowed {
		t.Errorf("status = %d, want 405", resp.StatusCode)
	}
}

func TestListTasksInvalidLimitReturns400(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := http.Get(ts.URL + "/tasks?limit=notanumber")
	if err != nil {
		t.Fatalf("GET /tasks: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", resp.StatusCode)
	}
}

func TestServerImplementsTransport(t *testing.T) {
	var _ transport.Transport = (*Server)(nil)
}

// Send behavior — the round-trip happy path, error cases (no resolver,
// unknown recipient, peer non-2xx), bearer-token pass-through, and agent
// card cache hits — is covered in outbound_test.go.
