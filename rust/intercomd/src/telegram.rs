use std::path::PathBuf;

use anyhow::{Context, anyhow};
use intercom_core::IntercomConfig;
use reqwest::Client;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub const TELEGRAM_MAX_TEXT_CHARS: usize = 4096;
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

#[derive(Clone)]
pub struct TelegramBridge {
    client: Client,
    bot_token: Option<String>,
    sqlite_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramIngressRequest {
    pub chat_jid: String,
    pub chat_name: Option<String>,
    pub chat_type: Option<String>,
    pub message_id: String,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub persist: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramIngressResponse {
    pub accepted: bool,
    pub reason: Option<String>,
    pub normalized_content: String,
    pub group_name: Option<String>,
    pub group_folder: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub parity: TelegramIngressParity,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramIngressParity {
    pub trigger_required: bool,
    pub trigger_present: bool,
    pub runtime_profile_found: bool,
    pub runtime_fallback_used: bool,
    pub model_fallback_used: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramSendRequest {
    pub jid: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramSendResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub message_ids: Vec<String>,
    pub chunks_planned: usize,
    pub chunks_sent: usize,
    pub chunk_lengths: Vec<usize>,
    pub parity: TelegramSendParity,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramSendParity {
    pub max_chars_per_chunk: usize,
    pub all_chunks_within_limit: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramEditRequest {
    pub jid: String,
    pub message_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramEditResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub truncated: bool,
    pub parity_max_chars: usize,
}

#[derive(Debug, Deserialize)]
struct TelegramApiEnvelope {
    ok: bool,
    result: Option<serde_json::Value>,
    description: Option<String>,
}

#[derive(Debug, Clone)]
struct RegisteredGroupRow {
    name: String,
    folder: String,
    trigger_pattern: String,
    requires_trigger: bool,
    runtime: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone)]
struct RuntimeResolution {
    runtime: String,
    model: String,
    runtime_profile_found: bool,
    runtime_fallback_used: bool,
    model_fallback_used: bool,
}

impl TelegramBridge {
    pub fn new(config: &IntercomConfig) -> Self {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        Self {
            client: Client::new(),
            bot_token,
            sqlite_path: PathBuf::from(&config.storage.sqlite_legacy_path),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.bot_token.is_some()
    }

    /// Convenience: send a text message to a JID (chat_id).
    /// Used by the orchestrator to deliver agent output.
    pub async fn send_text_to_jid(&self, jid: &str, text: &str) -> anyhow::Result<()> {
        self.send_message(TelegramSendRequest {
            jid: jid.to_string(),
            text: text.to_string(),
        })
        .await?;
        Ok(())
    }

    pub fn route_ingress(
        &self,
        config: &IntercomConfig,
        request: TelegramIngressRequest,
    ) -> anyhow::Result<TelegramIngressResponse> {
        let conn = self.open_sqlite()?;
        let group = load_registered_group(&conn, &request.chat_jid)?;

        if request.persist {
            ensure_telegram_persistence_schema(&conn)?;
            persist_chat_metadata(&conn, &request)?;
        }

        let Some(group) = group else {
            return Ok(TelegramIngressResponse {
                accepted: false,
                reason: Some("unregistered_group".to_string()),
                normalized_content: request.content,
                group_name: None,
                group_folder: None,
                runtime: None,
                model: None,
                parity: TelegramIngressParity {
                    trigger_required: false,
                    trigger_present: false,
                    runtime_profile_found: false,
                    runtime_fallback_used: false,
                    model_fallback_used: false,
                },
            });
        };

        let trigger_required = group.folder != "main" && group.requires_trigger;
        let trigger_present =
            !trigger_required || trigger_matches(&request.content, &group.trigger_pattern);
        let runtime = resolve_runtime(config, &group);

        if request.persist {
            persist_inbound_message(&conn, &request)?;
        }

        let accepted = !trigger_required || trigger_present;
        let reason = if accepted {
            None
        } else {
            Some("trigger_required".to_string())
        };

        Ok(TelegramIngressResponse {
            accepted,
            reason,
            normalized_content: request.content,
            group_name: Some(group.name),
            group_folder: Some(group.folder),
            runtime: Some(runtime.runtime),
            model: Some(runtime.model),
            parity: TelegramIngressParity {
                trigger_required,
                trigger_present,
                runtime_profile_found: runtime.runtime_profile_found,
                runtime_fallback_used: runtime.runtime_fallback_used,
                model_fallback_used: runtime.model_fallback_used,
            },
        })
    }

    pub async fn send_message(
        &self,
        request: TelegramSendRequest,
    ) -> anyhow::Result<TelegramSendResponse> {
        let token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set for intercomd"))?;

        if request.text.trim().is_empty() {
            return Err(anyhow!("cannot send an empty Telegram message"));
        }

        let chat_id = normalize_chat_id(&request.jid);
        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/sendMessage");
        let chunks = split_for_telegram(&request.text, TELEGRAM_MAX_TEXT_CHARS);
        let chunk_lengths = chunks
            .iter()
            .map(|chunk| chunk.chars().count())
            .collect::<Vec<_>>();
        let mut sent_calls = 0_usize;
        let mut message_ids = Vec::new();

        for chunk in &chunks {
            let response = self
                .client
                .post(&endpoint)
                .json(&serde_json::json!({
                    "chat_id": chat_id,
                    "text": chunk,
                }))
                .send()
                .await
                .context("failed to call Telegram sendMessage")?;

            let body: TelegramApiEnvelope = response
                .json()
                .await
                .context("failed to parse Telegram sendMessage response")?;
            if !body.ok {
                return Err(anyhow!(body.description.unwrap_or_else(|| {
                    "Telegram sendMessage returned ok=false".to_string()
                })));
            }

            sent_calls += 1;
            if let Some(message_id) = body
                .result
                .as_ref()
                .and_then(|value| value.get("message_id"))
                .and_then(|value| value.as_i64())
            {
                message_ids.push(message_id.to_string());
            }
        }

        Ok(TelegramSendResponse {
            ok: true,
            error: None,
            message_ids,
            chunks_planned: chunks.len(),
            chunks_sent: sent_calls,
            chunk_lengths: chunk_lengths.clone(),
            parity: TelegramSendParity {
                max_chars_per_chunk: TELEGRAM_MAX_TEXT_CHARS,
                all_chunks_within_limit: chunk_lengths
                    .iter()
                    .all(|len| *len <= TELEGRAM_MAX_TEXT_CHARS),
            },
        })
    }

    pub async fn edit_message(
        &self,
        request: TelegramEditRequest,
    ) -> anyhow::Result<TelegramEditResponse> {
        let token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set for intercomd"))?;
        let chat_id = normalize_chat_id(&request.jid);
        let message_id = request
            .message_id
            .parse::<i64>()
            .with_context(|| format!("invalid message_id `{}`", request.message_id))?;

        let (text, truncated) = truncate_for_telegram(&request.text, TELEGRAM_MAX_TEXT_CHARS);
        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/editMessageText");
        let response = self
            .client
            .post(&endpoint)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": text,
            }))
            .send()
            .await
            .context("failed to call Telegram editMessageText")?;

        let body: TelegramApiEnvelope = response
            .json()
            .await
            .context("failed to parse Telegram editMessageText response")?;
        if !body.ok {
            return Err(anyhow!(body.description.unwrap_or_else(|| {
                "Telegram editMessageText returned ok=false".to_string()
            })));
        }

        Ok(TelegramEditResponse {
            ok: true,
            error: None,
            truncated,
            parity_max_chars: TELEGRAM_MAX_TEXT_CHARS,
        })
    }

    fn open_sqlite(&self) -> anyhow::Result<Connection> {
        Connection::open(&self.sqlite_path).with_context(|| {
            format!(
                "failed to open sqlite database for Telegram routing: {}",
                self.sqlite_path.display()
            )
        })
    }
}

impl TelegramSendResponse {
    pub fn from_error(err: impl Into<String>) -> Self {
        let error = err.into();
        Self {
            ok: false,
            error: Some(error),
            message_ids: Vec::new(),
            chunks_planned: 0,
            chunks_sent: 0,
            chunk_lengths: Vec::new(),
            parity: TelegramSendParity {
                max_chars_per_chunk: TELEGRAM_MAX_TEXT_CHARS,
                all_chunks_within_limit: true,
            },
        }
    }
}

impl TelegramEditResponse {
    pub fn from_error(err: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(err.into()),
            truncated: false,
            parity_max_chars: TELEGRAM_MAX_TEXT_CHARS,
        }
    }
}

fn normalize_chat_id(jid: &str) -> &str {
    jid.strip_prefix("tg:").unwrap_or(jid)
}

fn split_for_telegram(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut chars_in_current = 0_usize;

    for ch in text.chars() {
        if chars_in_current >= max_chars {
            chunks.push(current);
            current = String::new();
            chars_in_current = 0;
        }
        current.push(ch);
        chars_in_current += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn truncate_for_telegram(text: &str, max_chars: usize) -> (String, bool) {
    let mut output = String::new();
    let mut count = 0_usize;

    for ch in text.chars() {
        if count >= max_chars {
            return (output, true);
        }
        output.push(ch);
        count += 1;
    }

    (output, false)
}

fn trigger_matches(content: &str, trigger_pattern: &str) -> bool {
    let trigger = trigger_pattern.trim();
    if trigger.is_empty() {
        return true;
    }

    let content = content.trim_start();
    if content.len() < trigger.len() {
        return false;
    }

    content
        .get(..trigger.len())
        .map(|prefix| prefix.eq_ignore_ascii_case(trigger))
        .unwrap_or(false)
}

fn resolve_runtime(config: &IntercomConfig, group: &RegisteredGroupRow) -> RuntimeResolution {
    let requested_runtime = group
        .runtime
        .as_deref()
        .unwrap_or(&config.runtimes.default_runtime);

    let mut runtime = requested_runtime.to_string();
    let mut profile = config.runtimes.profiles.get(requested_runtime);
    let mut runtime_profile_found = profile.is_some();
    let mut runtime_fallback_used = false;

    if profile.is_none() {
        if let Some(default_profile) = config
            .runtimes
            .profiles
            .get(&config.runtimes.default_runtime)
        {
            profile = Some(default_profile);
            runtime = config.runtimes.default_runtime.clone();
            runtime_fallback_used = true;
        } else if let Some((name, first_profile)) = config.runtimes.profiles.iter().next() {
            profile = Some(first_profile);
            runtime = name.clone();
            runtime_fallback_used = true;
        } else {
            runtime_profile_found = false;
        }
    }

    let model = if let Some(model) = &group.model {
        model.clone()
    } else if let Some(profile) = profile {
        profile.default_model.clone()
    } else {
        "unknown".to_string()
    };

    RuntimeResolution {
        runtime,
        model,
        runtime_profile_found,
        runtime_fallback_used,
        model_fallback_used: group.model.is_none(),
    }
}

fn load_registered_group(
    conn: &Connection,
    chat_jid: &str,
) -> anyhow::Result<Option<RegisteredGroupRow>> {
    if !sqlite_has_table(conn, "registered_groups")? {
        return Ok(None);
    }

    let has_requires_trigger = sqlite_has_column(conn, "registered_groups", "requires_trigger")?;
    let has_runtime = sqlite_has_column(conn, "registered_groups", "runtime")?;
    let has_model = sqlite_has_column(conn, "registered_groups", "model")?;

    let requires_expr = if has_requires_trigger {
        "COALESCE(requires_trigger, 1)"
    } else {
        "1 AS requires_trigger"
    };
    let runtime_expr = if has_runtime {
        "runtime"
    } else {
        "NULL AS runtime"
    };
    let model_expr = if has_model { "model" } else { "NULL AS model" };

    let query = format!(
        "SELECT name, folder, trigger_pattern, {requires_expr}, {runtime_expr}, {model_expr}
         FROM registered_groups
         WHERE jid = ?1
         LIMIT 1"
    );

    conn.query_row(&query, params![chat_jid], |row| {
        let requires_trigger: i64 = row.get(3)?;
        Ok(RegisteredGroupRow {
            name: row.get(0)?,
            folder: row.get(1)?,
            trigger_pattern: row.get(2)?,
            requires_trigger: requires_trigger != 0,
            runtime: row.get(4)?,
            model: row.get(5)?,
        })
    })
    .optional()
    .context("failed to query registered_groups")
}

fn ensure_telegram_persistence_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "\
        CREATE TABLE IF NOT EXISTS chats (
          jid TEXT PRIMARY KEY,
          name TEXT,
          last_message_time TEXT,
          channel TEXT,
          is_group INTEGER DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS messages (
          id TEXT,
          chat_jid TEXT,
          sender TEXT,
          sender_name TEXT,
          content TEXT,
          timestamp TEXT,
          is_from_me INTEGER,
          is_bot_message INTEGER DEFAULT 0,
          PRIMARY KEY (id, chat_jid)
        );
        ",
    )
    .context("failed to ensure Telegram sqlite persistence schema")
}

fn persist_chat_metadata(
    conn: &Connection,
    request: &TelegramIngressRequest,
) -> anyhow::Result<()> {
    let name = request.chat_name.as_deref().unwrap_or(&request.chat_jid);
    let is_group = if matches!(request.chat_type.as_deref(), Some("private")) {
        0_i64
    } else {
        1_i64
    };

    conn.execute(
        "\
        INSERT INTO chats (jid, name, last_message_time, channel, is_group)
        VALUES (?1, ?2, ?3, 'telegram', ?4)
        ON CONFLICT(jid) DO UPDATE SET
          name = COALESCE(excluded.name, chats.name),
          last_message_time = MAX(chats.last_message_time, excluded.last_message_time),
          channel = 'telegram',
          is_group = excluded.is_group
        ",
        params![request.chat_jid, name, request.timestamp, is_group],
    )
    .context("failed to persist Telegram chat metadata")?;

    Ok(())
}

fn persist_inbound_message(
    conn: &Connection,
    request: &TelegramIngressRequest,
) -> anyhow::Result<()> {
    let sender_name = request.sender_name.as_deref().unwrap_or("Unknown");
    let sender_id = request.sender_id.as_deref().unwrap_or("");

    conn.execute(
        "\
        INSERT OR REPLACE INTO messages
          (id, chat_jid, sender, sender_name, content, timestamp, is_from_me, is_bot_message)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)
        ",
        params![
            request.message_id,
            request.chat_jid,
            sender_id,
            sender_name,
            request.content,
            request.timestamp
        ],
    )
    .context("failed to persist Telegram inbound message")?;

    Ok(())
}

fn sqlite_has_table(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1 LIMIT 1")?;
    let exists = stmt.query_row([table], |_| Ok(1_i64)).optional()?.is_some();
    Ok(exists)
}

fn sqlite_has_column(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn split_for_telegram_keeps_chunks_within_limit() {
        let text = "a".repeat(9005);
        let chunks = split_for_telegram(&text, TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= TELEGRAM_MAX_TEXT_CHARS)
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.chars().count())
                .sum::<usize>(),
            text.chars().count()
        );
    }

    #[test]
    fn trigger_match_is_case_insensitive() {
        assert!(trigger_matches("@Amtiskaw please help", "@amtiskaw"));
        assert!(!trigger_matches("hello", "@amtiskaw"));
    }

    #[test]
    fn ingress_requires_trigger_for_non_main_group() {
        let tmp = TempDir::new().expect("create tempdir");
        let db_path = tmp.path().join("messages.db");
        let conn = Connection::open(&db_path).expect("open sqlite");
        conn.execute_batch(
            "\
            CREATE TABLE registered_groups (
              jid TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              folder TEXT NOT NULL,
              trigger_pattern TEXT NOT NULL,
              added_at TEXT NOT NULL,
              container_config TEXT,
              requires_trigger INTEGER DEFAULT 1,
              runtime TEXT,
              model TEXT
            );
            INSERT INTO registered_groups
              (jid, name, folder, trigger_pattern, added_at, requires_trigger, runtime)
            VALUES
              ('tg:1', 'Team', 'team', '@Amtiskaw', '2026-01-01T00:00:00Z', 1, 'gemini');
            ",
        )
        .expect("seed groups");
        drop(conn);

        let mut config = IntercomConfig::default();
        config.storage.sqlite_legacy_path = db_path.display().to_string();
        let bridge = TelegramBridge::new(&config);

        let response = bridge
            .route_ingress(
                &config,
                TelegramIngressRequest {
                    chat_jid: "tg:1".to_string(),
                    chat_name: Some("Team".to_string()),
                    chat_type: Some("group".to_string()),
                    message_id: "123".to_string(),
                    sender_id: Some("99".to_string()),
                    sender_name: Some("User".to_string()),
                    content: "hello".to_string(),
                    timestamp: "2026-02-25T00:00:00Z".to_string(),
                    persist: false,
                },
            )
            .expect("route ingress");

        assert!(!response.accepted);
        assert_eq!(response.reason.as_deref(), Some("trigger_required"));
        assert_eq!(response.runtime.as_deref(), Some("gemini"));
        assert_eq!(response.model.as_deref(), Some("gemini-3.1-pro"));
    }
}
