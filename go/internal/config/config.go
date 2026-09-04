package config

import (
	"os"

	"github.com/BurntSushi/toml"
)

type Config struct {
	Server       ServerConfig       `toml:"server"`
	Storage      StorageConfig      `toml:"storage"`
	Runtimes     RuntimesConfig     `toml:"runtimes"`
	Orchestrator OrchestratorConfig `toml:"orchestrator"`
	Pool         PoolConfig         `toml:"pool"`
	Scheduler    SchedulerConfig    `toml:"scheduler"`
	Events       EventsConfig       `toml:"events"`
	Demarch      DemarchConfig      `toml:"demarch"`
}

type ServerConfig struct {
	Bind             string `toml:"bind"`
	RequestTimeoutMs int    `toml:"request_timeout_ms"`
	MaxBodyBytes     int    `toml:"max_body_bytes"`
	HostCallbackURL  string `toml:"host_callback_url"`
}

type StorageConfig struct {
	PostgresDSN  string `toml:"postgres_dsn"`
	SQLiteLegacy string `toml:"sqlite_legacy_path"`
	GroupsDir    string `toml:"groups_dir"`
}

type RuntimesConfig struct {
	PreserveLegacyRuntimeIDs bool                      `toml:"preserve_legacy_runtime_ids"`
	DefaultRuntime           string                    `toml:"default_runtime"`
	Profiles                 map[string]RuntimeProfile `toml:"profiles"`
}

type RuntimeProfile struct {
	Runtime         string   `toml:"runtime"`
	Provider        string   `toml:"provider"`
	DefaultModel    string   `toml:"default_model"`
	ReasoningEffort string   `toml:"reasoning_effort"`
	ServiceTier     string   `toml:"service_tier"`
	RequiredEnv     []string `toml:"required_env"`
}

type OrchestratorConfig struct {
	Enabled                 bool   `toml:"enabled"`
	UseOutbox               bool   `toml:"use_outbox"`
	MaxConcurrentContainers int    `toml:"max_concurrent_containers"`
	PollIntervalMs          int    `toml:"poll_interval_ms"`
	IdleTimeoutMs           int    `toml:"idle_timeout_ms"`
	MainGroupFolder         string `toml:"main_group_folder"`
	SessionMaxBytes         int    `toml:"session_max_bytes"`
	ResultTimeoutMs         int    `toml:"result_timeout_ms"`
}

type PoolConfig struct {
	Enabled         bool `toml:"enabled"`
	Prewarm         bool `toml:"prewarm"`
	IdleTimeoutSecs int  `toml:"idle_timeout_secs"`
	MaxContainers   int  `toml:"max_containers"`
	MemoryWarnMB    int  `toml:"memory_warn_mb"`
}

type SchedulerConfig struct {
	Enabled        bool   `toml:"enabled"`
	PollIntervalMs int    `toml:"poll_interval_ms"`
	Timezone       string `toml:"timezone"`
}

type EventsConfig struct {
	Enabled                 bool   `toml:"enabled"`
	PollIntervalMs          int    `toml:"poll_interval_ms"`
	BatchSize               int    `toml:"batch_size"`
	NotificationJID         string `toml:"notification_jid"`
	StalePhaseThresholdSecs int    `toml:"stale_phase_threshold_secs"`
}

type DemarchConfig struct {
	Enabled                   bool     `toml:"enabled"`
	RequireMainGroupForWrites bool     `toml:"require_main_group_for_writes"`
	ReadAllowlist             []string `toml:"read_allowlist"`
	WriteAllowlist            []string `toml:"write_allowlist"`
}

func Load(path string) (*Config, error) {
	var cfg Config
	if _, err := toml.DecodeFile(path, &cfg); err != nil {
		return nil, err
	}
	applyEnvOverrides(&cfg)
	applyDefaults(&cfg)
	return &cfg, nil
}

func applyEnvOverrides(cfg *Config) {
	if v := os.Getenv("INTERCOMD_BIND"); v != "" {
		cfg.Server.Bind = v
	}
	if v := os.Getenv("INTERCOM_POSTGRES_DSN"); v != "" {
		cfg.Storage.PostgresDSN = v
	}
	if v := os.Getenv("HOST_CALLBACK_URL"); v != "" {
		cfg.Server.HostCallbackURL = v
	}
}

func applyDefaults(cfg *Config) {
	if cfg.Server.Bind == "" {
		cfg.Server.Bind = "127.0.0.1:7340"
	}
	if cfg.Server.RequestTimeoutMs == 0 {
		cfg.Server.RequestTimeoutMs = 30000
	}
	if cfg.Server.MaxBodyBytes == 0 {
		cfg.Server.MaxBodyBytes = 1048576
	}
	if cfg.Server.HostCallbackURL == "" {
		cfg.Server.HostCallbackURL = "http://127.0.0.1:7341"
	}
	if cfg.Storage.GroupsDir == "" {
		cfg.Storage.GroupsDir = "groups"
	}
	if cfg.Storage.SQLiteLegacy == "" {
		cfg.Storage.SQLiteLegacy = "store/messages.db"
	}
	if cfg.Runtimes.DefaultRuntime == "" {
		cfg.Runtimes.DefaultRuntime = "claude"
	}
	if cfg.Runtimes.Profiles == nil {
		cfg.Runtimes.Profiles = defaultProfiles()
	}
	if _, ok := cfg.Runtimes.Profiles["astra"]; !ok {
		cfg.Runtimes.Profiles["astra"] = astraRuntimeProfile()
	}
	if cfg.Orchestrator.MaxConcurrentContainers == 0 {
		cfg.Orchestrator.MaxConcurrentContainers = 3
	}
	if cfg.Orchestrator.PollIntervalMs == 0 {
		cfg.Orchestrator.PollIntervalMs = 1000
	}
	if cfg.Orchestrator.IdleTimeoutMs == 0 {
		cfg.Orchestrator.IdleTimeoutMs = 300000
	}
	if cfg.Orchestrator.MainGroupFolder == "" {
		cfg.Orchestrator.MainGroupFolder = "main"
	}
	if cfg.Orchestrator.SessionMaxBytes == 0 {
		cfg.Orchestrator.SessionMaxBytes = 512 * 1024
	}
	if cfg.Orchestrator.ResultTimeoutMs == 0 {
		cfg.Orchestrator.ResultTimeoutMs = 180000
	}
	if cfg.Pool.IdleTimeoutSecs == 0 {
		cfg.Pool.IdleTimeoutSecs = 1800
	}
	if cfg.Pool.MaxContainers == 0 {
		cfg.Pool.MaxContainers = 20
	}
	if cfg.Pool.MemoryWarnMB == 0 {
		cfg.Pool.MemoryWarnMB = 4096
	}
	if cfg.Scheduler.PollIntervalMs == 0 {
		cfg.Scheduler.PollIntervalMs = 10000
	}
	if cfg.Scheduler.Timezone == "" {
		cfg.Scheduler.Timezone = "UTC"
	}
	if cfg.Events.PollIntervalMs == 0 {
		cfg.Events.PollIntervalMs = 1000
	}
	if cfg.Events.BatchSize == 0 {
		cfg.Events.BatchSize = 20
	}
	if cfg.Events.StalePhaseThresholdSecs == 0 {
		cfg.Events.StalePhaseThresholdSecs = 7200
	}
	if cfg.Demarch.ReadAllowlist == nil {
		cfg.Demarch.ReadAllowlist = defaultReadAllowlist()
	}
	if cfg.Demarch.WriteAllowlist == nil {
		cfg.Demarch.WriteAllowlist = defaultWriteAllowlist()
	}
}

func defaultProfiles() map[string]RuntimeProfile {
	return map[string]RuntimeProfile{
		"claude": {
			Runtime:      "claude",
			Provider:     "anthropic",
			DefaultModel: "claude-opus-4-6",
			RequiredEnv:  []string{"CLAUDE_CODE_OAUTH_TOKEN"},
		},
		"gemini": {
			Runtime:      "gemini",
			Provider:     "code-assist",
			DefaultModel: "gemini-3.1-pro",
			RequiredEnv: []string{
				"GEMINI_REFRESH_TOKEN",
				"GEMINI_OAUTH_CLIENT_ID",
				"GEMINI_OAUTH_CLIENT_SECRET",
			},
		},
		"codex": {
			Runtime:      "codex",
			Provider:     "openai",
			DefaultModel: "gpt-5.3-codex",
			RequiredEnv: []string{
				"CODEX_OAUTH_ACCESS_TOKEN",
				"CODEX_OAUTH_REFRESH_TOKEN",
				"CODEX_OAUTH_ID_TOKEN",
				"CODEX_OAUTH_ACCOUNT_ID",
			},
		},
		"astra": astraRuntimeProfile(),
	}
}

func astraRuntimeProfile() RuntimeProfile {
	return RuntimeProfile{
		Runtime:         "codex",
		Provider:        "openai",
		DefaultModel:    "gpt-6-astra",
		ReasoningEffort: "high",
		ServiceTier:     "standard",
		RequiredEnv: []string{
			"CODEX_OAUTH_ACCESS_TOKEN",
			"CODEX_OAUTH_REFRESH_TOKEN",
			"CODEX_OAUTH_ID_TOKEN",
			"CODEX_OAUTH_ACCOUNT_ID",
		},
	}
}

func defaultReadAllowlist() []string {
	return []string{
		"ic run current --json",
		"ic run status --json",
		"ic run phase --json",
		"ic run artifact list --json",
		"ic run artifact get --json",
		"ic run tokens --json",
		"ic dispatch list --json",
		"ic events tail --consumer=intercom --json",
		"bd list --json",
		"bd ready --json",
		"bd show --json",
	}
}

func defaultWriteAllowlist() []string {
	return []string{
		"bd create --json",
		"bd update --json",
		"bd close --json",
		"ic gate override --json",
		"ic run create --json",
		"ic state set --json",
		"ic run set --json",
		"ic run cancel --json",
	}
}
