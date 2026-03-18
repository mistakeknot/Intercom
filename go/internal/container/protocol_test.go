package container

import (
	"encoding/json"
	"testing"
)

func TestContainerInputSerializesCamelCase(t *testing.T) {
	input := ContainerInput{
		Prompt:        "hello",
		SessionID:     "sess-123",
		GroupFolder:   "main",
		ChatJID:       "tg:123",
		IsMain:        true,
		AssistantName: "Amtiskaw",
	}
	data, err := json.Marshal(input)
	if err != nil {
		t.Fatal(err)
	}
	s := string(data)

	// camelCase fields must be present
	for _, field := range []string{"chatJid", "groupFolder", "isMain", "sessionId"} {
		if !contains([]string{field}, field) { // dummy; real check below
		}
		if !jsonContains(s, field) {
			t.Errorf("expected field %q in JSON output", field)
		}
	}

	// Optional nil/empty fields should be absent
	for _, field := range []string{"model", "secrets", "previousContext"} {
		if jsonContains(s, field) {
			t.Errorf("optional empty field %q should not appear in JSON output", field)
		}
	}
}

func TestContainerOutputDeserializesFromNodeFormat(t *testing.T) {
	raw := `{"status":"success","result":"Hello!","newSessionId":"sess-456"}`
	var output ContainerOutput
	if err := json.Unmarshal([]byte(raw), &output); err != nil {
		t.Fatal(err)
	}
	if output.Status != StatusSuccess {
		t.Errorf("expected success, got %q", output.Status)
	}
	if output.Result == nil || *output.Result != "Hello!" {
		t.Error("expected result 'Hello!'")
	}
	if output.NewSessionID != "sess-456" {
		t.Errorf("expected session ID 'sess-456', got %q", output.NewSessionID)
	}
}

func TestContainerOutputErrorStatus(t *testing.T) {
	raw := `{"status":"error","result":null,"error":"Container exited with code 1"}`
	var output ContainerOutput
	if err := json.Unmarshal([]byte(raw), &output); err != nil {
		t.Fatal(err)
	}
	if output.Status != StatusError {
		t.Errorf("expected error, got %q", output.Status)
	}
	if output.Result != nil {
		t.Error("expected nil result")
	}
	if output.Error == "" {
		t.Error("expected non-empty error")
	}
}

func TestStreamEventToolStart(t *testing.T) {
	raw := `{"type":"tool_start","toolName":"Read","toolInput":"/path/to/file"}`
	var event StreamEvent
	if err := json.Unmarshal([]byte(raw), &event); err != nil {
		t.Fatal(err)
	}
	if event.Type != "tool_start" {
		t.Errorf("expected tool_start, got %q", event.Type)
	}
	if event.ToolName != "Read" {
		t.Errorf("expected ToolName 'Read', got %q", event.ToolName)
	}
	if event.ToolInput != "/path/to/file" {
		t.Errorf("expected ToolInput '/path/to/file', got %q", event.ToolInput)
	}
}

func TestStreamEventTextDelta(t *testing.T) {
	raw := `{"type":"text_delta","text":"Hello "}`
	var event StreamEvent
	if err := json.Unmarshal([]byte(raw), &event); err != nil {
		t.Fatal(err)
	}
	if event.Type != "text_delta" {
		t.Errorf("expected text_delta, got %q", event.Type)
	}
	if event.Text != "Hello " {
		t.Errorf("expected text 'Hello ', got %q", event.Text)
	}
}

func TestContainerImageNames(t *testing.T) {
	tests := []struct {
		runtime RuntimeKind
		want    string
	}{
		{RuntimeClaude, "intercom-agent:latest"},
		{RuntimeGemini, "intercom-agent-gemini:latest"},
		{RuntimeCodex, "intercom-agent-codex:latest"},
	}
	for _, tt := range tests {
		if got := ContainerImage(tt.runtime); got != tt.want {
			t.Errorf("ContainerImage(%q) = %q, want %q", tt.runtime, got, tt.want)
		}
	}
}

func TestRunnerContainerPaths(t *testing.T) {
	tests := []struct {
		runtime RuntimeKind
		want    string
	}{
		{RuntimeClaude, "/app/agent-runner/src"},
		{RuntimeGemini, "/app/gemini-runner/src"},
		{RuntimeCodex, "/app/codex-runner/src"},
	}
	for _, tt := range tests {
		if got := RunnerContainerPath(tt.runtime); got != tt.want {
			t.Errorf("RunnerContainerPath(%q) = %q, want %q", tt.runtime, got, tt.want)
		}
	}
}

func TestExtractMarkersSinglePair(t *testing.T) {
	buf := "some noise " + OutputStartMarker + `{"status":"success","result":"hi"}` + OutputEndMarker + "trailing"
	results, consumed := ExtractOutputMarkers(buf)
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if results[0] != `{"status":"success","result":"hi"}` {
		t.Errorf("unexpected result: %q", results[0])
	}
	if consumed == 0 {
		t.Error("expected non-zero consumed")
	}
	if buf[consumed:] != "trailing" {
		t.Errorf("expected 'trailing' after consumed, got %q", buf[consumed:])
	}
}

func TestExtractMarkersMultiplePairs(t *testing.T) {
	buf := OutputStartMarker + `{"status":"success","result":null}` + OutputEndMarker +
		OutputStartMarker + `{"status":"success","result":"done"}` + OutputEndMarker
	results, consumed := ExtractOutputMarkers(buf)
	if len(results) != 2 {
		t.Fatalf("expected 2 results, got %d", len(results))
	}
	if consumed != len(buf) {
		t.Errorf("expected full consumption, got %d/%d", consumed, len(buf))
	}
}

func TestExtractMarkersIncompletePair(t *testing.T) {
	buf := OutputStartMarker + `{"status":"success"}`
	results, consumed := ExtractOutputMarkers(buf)
	if len(results) != 0 {
		t.Fatalf("expected 0 results, got %d", len(results))
	}
	if consumed != 0 {
		t.Errorf("expected 0 consumed, got %d", consumed)
	}
}

func TestExtractMarkersEmptyBuffer(t *testing.T) {
	results, consumed := ExtractOutputMarkers("")
	if len(results) != 0 || consumed != 0 {
		t.Error("expected empty results and 0 consumed")
	}
}

func TestContainerOutputWithStreamEvent(t *testing.T) {
	raw := `{"status":"success","result":null,"event":{"type":"tool_start","toolName":"Bash","toolInput":"ls"}}`
	var output ContainerOutput
	if err := json.Unmarshal([]byte(raw), &output); err != nil {
		t.Fatal(err)
	}
	if output.Event == nil {
		t.Fatal("expected non-nil event")
	}
	if output.Event.Type != "tool_start" {
		t.Errorf("expected tool_start, got %q", output.Event.Type)
	}
	if output.Event.ToolName != "Bash" {
		t.Errorf("expected ToolName 'Bash', got %q", output.Event.ToolName)
	}
}

func jsonContains(s, field string) bool {
	return len(s) > 0 && len(field) > 0 &&
		indexOf(s, `"`+field+`"`) >= 0
}
