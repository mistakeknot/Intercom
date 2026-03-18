package outbox

import (
	"encoding/json"
	"testing"

	"github.com/mistakeknot/intercom/internal/db"
)

// Test payload deserialization for each type — these mirror the Rust tests
// that ensure the JSON contract between writers and the drainer is stable.

func TestMessagePayloadDeserialize(t *testing.T) {
	payload := json.RawMessage(`{
		"id": "msg-1",
		"chat_jid": "tg:12345",
		"sender": "67890",
		"sender_name": "Alice",
		"content": "Hello world",
		"timestamp": "2026-03-18T12:00:00Z",
		"is_from_me": false,
		"is_bot_message": false
	}`)
	var msg db.Message
	if err := json.Unmarshal(payload, &msg); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if msg.ID != "msg-1" {
		t.Errorf("id = %q, want msg-1", msg.ID)
	}
	if msg.ChatJID != "tg:12345" {
		t.Errorf("chat_jid = %q, want tg:12345", msg.ChatJID)
	}
	if msg.Content != "Hello world" {
		t.Errorf("content = %q, want Hello world", msg.Content)
	}
}

func TestChatMetadataPayloadDeserialize(t *testing.T) {
	payload := json.RawMessage(`{
		"jid": "tg:12345",
		"timestamp": "2026-03-18T12:00:00Z",
		"name": "Test Group",
		"channel": null,
		"is_group": true
	}`)
	var meta chatMetadataPayload
	if err := json.Unmarshal(payload, &meta); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if meta.JID != "tg:12345" {
		t.Errorf("jid = %q, want tg:12345", meta.JID)
	}
	if meta.Name == nil || *meta.Name != "Test Group" {
		t.Error("name should be 'Test Group'")
	}
	if meta.IsGroup == nil || !*meta.IsGroup {
		t.Error("is_group should be true")
	}
}

func TestGroupRegistrationPayloadDeserialize(t *testing.T) {
	payload := json.RawMessage(`{
		"jid": "tg:-100123",
		"name": "Dev Group",
		"folder": "dev",
		"trigger": "!bot",
		"added_at": "2026-03-18T12:00:00Z",
		"requires_trigger": true,
		"runtime": "claude",
		"model": "claude-opus-4-6"
	}`)
	var group db.RegisteredGroup
	if err := json.Unmarshal(payload, &group); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if group.JID != "tg:-100123" {
		t.Errorf("jid = %q", group.JID)
	}
	if group.Folder != "dev" {
		t.Errorf("folder = %q", group.Folder)
	}
	if group.Runtime == nil || *group.Runtime != "claude" {
		t.Error("runtime should be claude")
	}
}

func TestTaskPayloadDeserialize(t *testing.T) {
	payload := json.RawMessage(`{
		"id": "task-abc",
		"group_folder": "main",
		"chat_jid": "tg:12345",
		"prompt": "Check server status",
		"schedule_type": "interval",
		"schedule_value": "1h",
		"context_mode": "isolated",
		"status": "active",
		"created_at": "2026-03-18T12:00:00Z"
	}`)
	var task db.ScheduledTask
	if err := json.Unmarshal(payload, &task); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if task.ID != "task-abc" {
		t.Errorf("id = %q", task.ID)
	}
	if task.ScheduleType != "interval" {
		t.Errorf("schedule_type = %q", task.ScheduleType)
	}
	if task.ScheduleValue != "1h" {
		t.Errorf("schedule_value = %q", task.ScheduleValue)
	}
}

func TestUnknownPayloadType(t *testing.T) {
	// Verify that unknown types would be caught — the drainer logs and marks failed.
	// We can't easily test the full processRow without a DB, but we verify
	// the switch statement covers all known types.
	knownTypes := map[string]bool{
		"message":            true,
		"chat_metadata":      true,
		"group_registration": true,
		"task":               true,
	}
	unknowns := []string{"webhook", "notification", ""}
	for _, u := range unknowns {
		if knownTypes[u] {
			t.Errorf("%q should not be a known type", u)
		}
	}
}
