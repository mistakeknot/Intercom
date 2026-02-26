use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IntercomConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub runtimes: RuntimeConfig,
    pub demarch: DemarchConfig,
    pub events: EventsConfig,
    pub orchestrator: OrchestratorConfig,
    pub scheduler: SchedulerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    /// Enable the event consumer loop.
    pub enabled: bool,
    /// Poll interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Max events per poll batch.
    pub batch_size: u32,
    /// Chat JID to send push notifications to (usually main group).
    pub notification_jid: Option<String>,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_ms: 1000,
            batch_size: 20,
            notification_jid: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub request_timeout_ms: u64,
    pub max_body_bytes: usize,
    /// URL of the Node host's callback server for message/task forwarding.
    pub host_callback_url: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7340".to_string(),
            request_timeout_ms: 30_000,
            max_body_bytes: 1_048_576,
            host_callback_url: "http://127.0.0.1:7341".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub postgres_dsn: Option<String>,
    pub sqlite_legacy_path: String,
    pub groups_dir: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            postgres_dsn: None,
            sqlite_legacy_path: "store/messages.db".to_string(),
            groups_dir: "groups".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub preserve_legacy_runtime_ids: bool,
    pub default_runtime: String,
    pub profiles: BTreeMap<String, RuntimeProfile>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "claude".to_string(),
            RuntimeProfile {
                provider: "anthropic".to_string(),
                default_model: "claude-opus-4-6".to_string(),
                required_env: vec!["CLAUDE_CODE_OAUTH_TOKEN".to_string()],
            },
        );
        profiles.insert(
            "gemini".to_string(),
            RuntimeProfile {
                provider: "code-assist".to_string(),
                default_model: "gemini-3.1-pro".to_string(),
                required_env: vec![
                    "GEMINI_REFRESH_TOKEN".to_string(),
                    "GEMINI_OAUTH_CLIENT_ID".to_string(),
                    "GEMINI_OAUTH_CLIENT_SECRET".to_string(),
                ],
            },
        );
        profiles.insert(
            "codex".to_string(),
            RuntimeProfile {
                provider: "openai".to_string(),
                default_model: "gpt-5.3-codex".to_string(),
                required_env: vec![
                    "CODEX_OAUTH_ACCESS_TOKEN".to_string(),
                    "CODEX_OAUTH_REFRESH_TOKEN".to_string(),
                    "CODEX_OAUTH_ID_TOKEN".to_string(),
                    "CODEX_OAUTH_ACCOUNT_ID".to_string(),
                ],
            },
        );

        Self {
            preserve_legacy_runtime_ids: true,
            default_runtime: "claude".to_string(),
            profiles,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RuntimeProfile {
    pub provider: String,
    pub default_model: String,
    pub required_env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    /// Enable the Rust orchestrator (message loop, queue, container dispatch).
    /// When false, intercomd runs as a sidecar only — Node remains the orchestrator.
    pub enabled: bool,
    /// Maximum concurrent containers across all groups.
    pub max_concurrent_containers: usize,
    /// Poll interval for the message loop (milliseconds).
    pub poll_interval_ms: u64,
    /// Idle timeout before closing container stdin (milliseconds).
    pub idle_timeout_ms: u64,
    /// Folder name for the main group.
    pub main_group_folder: String,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent_containers: 3,
            poll_interval_ms: 1000,
            idle_timeout_ms: 300_000,
            main_group_folder: "main".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// Enable the task scheduler loop.
    pub enabled: bool,
    /// Poll interval for due tasks (milliseconds).
    pub poll_interval_ms: u64,
    /// IANA timezone for cron expressions.
    pub timezone: String,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_ms: 10_000,
            timezone: "UTC".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DemarchConfig {
    pub enabled: bool,
    pub require_main_group_for_writes: bool,
    pub read_allowlist: Vec<String>,
    pub write_allowlist: Vec<String>,
}

impl Default for DemarchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            require_main_group_for_writes: true,
            read_allowlist: vec![
                "ic run current --json".to_string(),
                "ic run status --json".to_string(),
                "ic run phase --json".to_string(),
                "ic run artifact list --json".to_string(),
                "ic run artifact get --json".to_string(),
                "ic events tail --consumer=intercom --json".to_string(),
                "bd list --json".to_string(),
                "bd ready --json".to_string(),
                "bd show --json".to_string(),
            ],
            write_allowlist: vec![
                "bd create --json".to_string(),
                "bd update --json".to_string(),
                "bd close --json".to_string(),
                "ic gate override --json".to_string(),
                "ic run create --json".to_string(),
            ],
        }
    }
}

pub fn load_config(path: impl AsRef<Path>) -> anyhow::Result<IntercomConfig> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(IntercomConfig::default().with_env_overrides());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;

    let parsed: IntercomConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config file: {}", path.display()))?;

    Ok(parsed.with_env_overrides())
}

impl IntercomConfig {
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(bind) = std::env::var("INTERCOMD_BIND") {
            if !bind.trim().is_empty() {
                self.server.bind = bind;
            }
        }

        if let Ok(dsn) = std::env::var("INTERCOM_POSTGRES_DSN") {
            if !dsn.trim().is_empty() {
                self.storage.postgres_dsn = Some(dsn);
            }
        }

        if let Ok(url) = std::env::var("HOST_CALLBACK_URL") {
            if !url.trim().is_empty() {
                self.server.host_callback_url = url;
            }
        }

        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_three_runtime_profiles() {
        let cfg = IntercomConfig::default();
        assert!(cfg.runtimes.profiles.contains_key("claude"));
        assert!(cfg.runtimes.profiles.contains_key("gemini"));
        assert!(cfg.runtimes.profiles.contains_key("codex"));
    }

    #[test]
    fn parse_toml_uses_defaults_for_missing_fields() {
        let parsed: IntercomConfig = toml::from_str(
            r#"
            [server]
            bind = "127.0.0.1:9999"
            "#,
        )
        .expect("parse toml");

        assert_eq!(parsed.server.bind, "127.0.0.1:9999");
        assert_eq!(parsed.server.request_timeout_ms, 30_000);
        assert!(parsed.runtimes.profiles.contains_key("claude"));
    }
}
