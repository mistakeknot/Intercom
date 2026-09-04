package smoke

import (
	"context"
	"os"
	"testing"

	"github.com/mistakeknot/intercom/internal/config"
	"github.com/mistakeknot/intercom/internal/routing"
	"github.com/mistakeknot/intercom/internal/telegram"
)

// TestConfigLoadsDefaults verifies the full config pipeline works.
func TestConfigLoadsDefaults(t *testing.T) {
	// Write minimal TOML
	tmp := t.TempDir()
	path := tmp + "/intercom.toml"
	if err := writeFile(path, `[server]
bind = "127.0.0.1:7340"
`); err != nil {
		t.Fatal(err)
	}

	cfg, err := config.Load(path)
	if err != nil {
		t.Fatalf("config.Load: %v", err)
	}
	if cfg.Server.Bind != "127.0.0.1:7340" {
		t.Errorf("bind = %q, want 127.0.0.1:7340", cfg.Server.Bind)
	}
	if cfg.Runtimes.DefaultRuntime != "claude" {
		t.Errorf("default_runtime = %q, want claude", cfg.Runtimes.DefaultRuntime)
	}
	if len(cfg.Runtimes.Profiles) != 4 {
		t.Errorf("profiles count = %d, want 4", len(cfg.Runtimes.Profiles))
	}
}

// TestMessengerDisabledWithoutToken verifies Messenger is disabled when no token.
func TestMessengerDisabledWithoutToken(t *testing.T) {
	m := telegram.NewMessenger("")
	if m.IsEnabled() {
		t.Error("messenger should be disabled without token")
	}
}

// TestMessengerSendRejectsEmpty verifies empty messages are rejected.
func TestMessengerSendRejectsEmpty(t *testing.T) {
	m := telegram.NewMessenger("fake-token")
	_, err := m.SendText(context.Background(), "12345", "")
	if err == nil {
		t.Error("expected error for empty message")
	}
}

// TestRouterFallback verifies ConfigRouter returns configured model.
func TestRouterFallback(t *testing.T) {
	r := &routing.ConfigRouter{Model: "claude-opus-4-6", Runtime: "claude"}
	model, runtime, err := r.SelectModel(context.Background(), "default")
	if err != nil {
		t.Fatalf("SelectModel: %v", err)
	}
	if model != "claude-opus-4-6" {
		t.Errorf("model = %q, want claude-opus-4-6", model)
	}
	if runtime != "claude" {
		t.Errorf("runtime = %q, want claude", runtime)
	}
}

// TestModelCatalogCompleteness verifies all expected models exist.
func TestModelCatalogCompleteness(t *testing.T) {
	catalog := telegram.ModelCatalog()
	expected := map[string]bool{
		"claude-opus-4-6":   false,
		"claude-sonnet-4-6": false,
		"gemini-3.1-pro":    false,
		"gemini-2.5-flash":  false,
		"gpt-5.3-codex":     false,
		"gpt-6-astra":       false,
	}
	for _, m := range catalog {
		expected[m.ID] = true
	}
	for id, found := range expected {
		if !found {
			t.Errorf("missing model %q in catalog", id)
		}
	}
}

func writeFile(path, content string) error {
	return os.WriteFile(path, []byte(content), 0644)
}
