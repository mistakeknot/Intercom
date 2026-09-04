package config

import (
	"os"
	"testing"
)

func TestLoadConfig(t *testing.T) {
	tomlContent := `
[server]
bind = "127.0.0.1:7340"
request_timeout_ms = 30000

[storage]
postgres_dsn = "postgres://localhost/intercom"
groups_dir = "groups"

[runtimes]
default_runtime = "claude"

[runtimes.profiles.claude]
provider = "anthropic"
default_model = "claude-opus-4-6"
required_env = ["CLAUDE_CODE_OAUTH_TOKEN"]
`
	f, err := os.CreateTemp("", "intercom-*.toml")
	if err != nil {
		t.Fatal(err)
	}
	f.WriteString(tomlContent)
	f.Close()
	defer os.Remove(f.Name())

	cfg, err := Load(f.Name())
	if err != nil {
		t.Fatalf("Load failed: %v", err)
	}
	if cfg.Server.Bind != "127.0.0.1:7340" {
		t.Errorf("expected bind 127.0.0.1:7340, got %s", cfg.Server.Bind)
	}
	if cfg.Runtimes.DefaultRuntime != "claude" {
		t.Errorf("expected default_runtime claude, got %s", cfg.Runtimes.DefaultRuntime)
	}
	if cfg.Runtimes.Profiles["claude"].DefaultModel != "claude-opus-4-6" {
		t.Errorf("expected model claude-opus-4-6, got %s", cfg.Runtimes.Profiles["claude"].DefaultModel)
	}
	if cfg.Storage.PostgresDSN != "postgres://localhost/intercom" {
		t.Errorf("expected postgres DSN, got %s", cfg.Storage.PostgresDSN)
	}
}

func TestLoadConfigEnvOverride(t *testing.T) {
	tomlContent := `
[server]
bind = "127.0.0.1:7340"
[storage]
postgres_dsn = "postgres://localhost/intercom"
`
	f, err := os.CreateTemp("", "intercom-*.toml")
	if err != nil {
		t.Fatal(err)
	}
	f.WriteString(tomlContent)
	f.Close()
	defer os.Remove(f.Name())

	t.Setenv("INTERCOMD_BIND", "0.0.0.0:8080")

	cfg, err := Load(f.Name())
	if err != nil {
		t.Fatalf("Load failed: %v", err)
	}
	if cfg.Server.Bind != "0.0.0.0:8080" {
		t.Errorf("expected env override 0.0.0.0:8080, got %s", cfg.Server.Bind)
	}
}

func TestLoadConfigDefaults(t *testing.T) {
	tomlContent := `
[server]
bind = "127.0.0.1:9999"
`
	f, err := os.CreateTemp("", "intercom-*.toml")
	if err != nil {
		t.Fatal(err)
	}
	f.WriteString(tomlContent)
	f.Close()
	defer os.Remove(f.Name())

	cfg, err := Load(f.Name())
	if err != nil {
		t.Fatalf("Load failed: %v", err)
	}

	if cfg.Server.Bind != "127.0.0.1:9999" {
		t.Errorf("expected bind from file, got %s", cfg.Server.Bind)
	}
	if cfg.Server.RequestTimeoutMs != 30000 {
		t.Errorf("expected default timeout 30000, got %d", cfg.Server.RequestTimeoutMs)
	}
	if cfg.Storage.GroupsDir != "groups" {
		t.Errorf("expected default groups_dir, got %s", cfg.Storage.GroupsDir)
	}
	if cfg.Orchestrator.MaxConcurrentContainers != 3 {
		t.Errorf("expected default max_concurrent 3, got %d", cfg.Orchestrator.MaxConcurrentContainers)
	}
	if cfg.Scheduler.Timezone != "UTC" {
		t.Errorf("expected default timezone UTC, got %s", cfg.Scheduler.Timezone)
	}
	if cfg.Runtimes.DefaultRuntime != "claude" {
		t.Errorf("expected default runtime claude, got %s", cfg.Runtimes.DefaultRuntime)
	}
	// Should get default profiles when none specified, including opt-in Astra.
	if len(cfg.Runtimes.Profiles) != 4 {
		t.Errorf("expected 4 default profiles, got %d", len(cfg.Runtimes.Profiles))
	}
	if astra := cfg.Runtimes.Profiles["astra"]; astra.DefaultModel != "gpt-6-astra" || astra.ReasoningEffort != "high" || astra.ServiceTier != "standard" {
		t.Errorf("unexpected Astra profile: %+v", astra)
	}
}

func TestLoadConfigFullExample(t *testing.T) {
	// Test with the actual intercom.toml.example format
	tomlContent := `
[server]
bind = "127.0.0.1:7340"
request_timeout_ms = 30000
max_body_bytes = 1048576

[storage]
postgres_dsn = "postgres://intercom:intercom@localhost:5432/intercom"
groups_dir = "groups"

[runtimes]
preserve_legacy_runtime_ids = true
default_runtime = "claude"

[runtimes.profiles.claude]
provider = "anthropic"
default_model = "claude-opus-4-6"
required_env = ["CLAUDE_CODE_OAUTH_TOKEN"]

[runtimes.profiles.gemini]
provider = "code-assist"
default_model = "gemini-3.1-pro"
required_env = [
  "GEMINI_REFRESH_TOKEN",
  "GEMINI_OAUTH_CLIENT_ID",
  "GEMINI_OAUTH_CLIENT_SECRET",
]

[runtimes.profiles.codex]
provider = "openai"
default_model = "gpt-5.3-codex"
required_env = [
  "CODEX_OAUTH_ACCESS_TOKEN",
  "CODEX_OAUTH_REFRESH_TOKEN",
  "CODEX_OAUTH_ID_TOKEN",
  "CODEX_OAUTH_ACCOUNT_ID",
]

[orchestrator]
enabled = true
max_concurrent_containers = 3
poll_interval_ms = 1000
idle_timeout_ms = 300000
main_group_folder = "main"

[scheduler]
enabled = false
poll_interval_ms = 10000
timezone = "UTC"

[events]
enabled = false
poll_interval_ms = 1000
batch_size = 20
stale_phase_threshold_secs = 7200

[demarch]
enabled = true
require_main_group_for_writes = true
read_allowlist = [
  "ic run current --json",
  "bd list --json",
]
write_allowlist = [
  "bd create --json",
]
`
	f, err := os.CreateTemp("", "intercom-full-*.toml")
	if err != nil {
		t.Fatal(err)
	}
	f.WriteString(tomlContent)
	f.Close()
	defer os.Remove(f.Name())

	cfg, err := Load(f.Name())
	if err != nil {
		t.Fatalf("Load failed: %v", err)
	}

	if !cfg.Runtimes.PreserveLegacyRuntimeIDs {
		t.Error("expected preserve_legacy_runtime_ids true")
	}
	if len(cfg.Runtimes.Profiles) != 4 {
		t.Errorf("expected 4 profiles after opt-in defaults, got %d", len(cfg.Runtimes.Profiles))
	}
	codex := cfg.Runtimes.Profiles["codex"]
	if codex.Provider != "openai" {
		t.Errorf("expected codex provider openai, got %s", codex.Provider)
	}
	if len(codex.RequiredEnv) != 4 {
		t.Errorf("expected 4 codex env vars, got %d", len(codex.RequiredEnv))
	}
	if !cfg.Orchestrator.Enabled {
		t.Error("expected orchestrator enabled")
	}
	if cfg.Orchestrator.MainGroupFolder != "main" {
		t.Errorf("expected main_group_folder main, got %s", cfg.Orchestrator.MainGroupFolder)
	}
	if len(cfg.Demarch.ReadAllowlist) != 2 {
		t.Errorf("expected 2 read allowlist entries, got %d", len(cfg.Demarch.ReadAllowlist))
	}
}
