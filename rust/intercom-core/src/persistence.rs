use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_postgres::{Client, NoTls};
use tracing::{error, info};

// ---------------------------------------------------------------------------
// Types — mirror the Node.js interfaces from types.ts and db.ts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMessage {
    pub id: String,
    pub chat_jid: String,
    pub sender: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default)]
    pub is_bot_message: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatInfo {
    pub jid: String,
    pub name: String,
    pub last_message_time: String,
    pub channel: Option<String>,
    pub is_group: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
    pub is_bot_message: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub group_folder: String,
    pub chat_jid: String,
    pub prompt: String,
    pub schedule_type: String,
    pub schedule_value: String,
    #[serde(default = "default_context_mode")]
    pub context_mode: String,
    pub next_run: Option<String>,
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    #[serde(default = "default_status")]
    pub status: String,
    pub created_at: String,
}

fn default_context_mode() -> String {
    "isolated".to_string()
}

fn default_status() -> String {
    "active".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunLog {
    pub task_id: String,
    pub run_at: String,
    pub duration_ms: i64,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredGroup {
    pub jid: String,
    pub name: String,
    pub folder: String,
    pub trigger: String,
    pub added_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_trigger: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

// ---------------------------------------------------------------------------
// Pool — reconnecting single-client wrapper
// ---------------------------------------------------------------------------

/// A simple Postgres connection pool that holds a single client behind a
/// RwLock. Reconnects automatically on connection loss.
#[derive(Clone)]
pub struct PgPool {
    dsn: String,
    client: Arc<RwLock<Option<Client>>>,
}

impl PgPool {
    pub fn new(dsn: String) -> Self {
        Self {
            dsn,
            client: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn connect(&self) -> anyhow::Result<()> {
        let client = connect_postgres(&self.dsn).await?;
        ensure_schema(&client).await?;
        *self.client.write().await = Some(client);
        info!("postgres connected and schema ensured");
        Ok(())
    }

    /// Get a reference to the underlying client. Reconnects if necessary.
    async fn get(&self) -> anyhow::Result<tokio::sync::RwLockReadGuard<'_, Option<Client>>> {
        // Fast path: client exists and is alive
        {
            let guard = self.client.read().await;
            if guard.is_some() {
                return Ok(guard);
            }
        }
        // Slow path: reconnect
        self.connect().await?;
        let guard = self.client.read().await;
        if guard.is_some() {
            Ok(guard)
        } else {
            Err(anyhow!("failed to establish postgres connection"))
        }
    }

    /// Get a connected client and execute a closure against it.
    async fn with_client<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: for<'c> FnOnce(&'c Client) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'c>>,
    {
        let guard = self.get().await?;
        let client = guard.as_ref().unwrap();
        f(client).await
    }
}

async fn connect_postgres(dsn: &str) -> anyhow::Result<Client> {
    let (client, connection) = tokio_postgres::connect(dsn, NoTls)
        .await
        .context("failed to connect to postgres")?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            error!(err = %err, "postgres connection error");
        }
    });
    Ok(client)
}

// ---------------------------------------------------------------------------
// Schema — live tables (not the legacy migration tables)
// ---------------------------------------------------------------------------

async fn ensure_schema(client: &Client) -> anyhow::Result<()> {
    client
        .batch_execute(
            "\
            CREATE TABLE IF NOT EXISTS chats (
              jid TEXT PRIMARY KEY,
              name TEXT,
              last_message_time TIMESTAMPTZ,
              channel TEXT,
              is_group BOOLEAN DEFAULT FALSE
            );

            CREATE TABLE IF NOT EXISTS messages (
              id TEXT NOT NULL,
              chat_jid TEXT NOT NULL,
              sender TEXT,
              sender_name TEXT,
              content TEXT,
              timestamp TIMESTAMPTZ NOT NULL,
              is_from_me BOOLEAN DEFAULT FALSE,
              is_bot_message BOOLEAN DEFAULT FALSE,
              PRIMARY KEY (id, chat_jid)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);

            CREATE TABLE IF NOT EXISTS scheduled_tasks (
              id TEXT PRIMARY KEY,
              group_folder TEXT NOT NULL,
              chat_jid TEXT NOT NULL,
              prompt TEXT NOT NULL,
              schedule_type TEXT NOT NULL,
              schedule_value TEXT NOT NULL,
              context_mode TEXT DEFAULT 'isolated',
              next_run TIMESTAMPTZ,
              last_run TIMESTAMPTZ,
              last_result TEXT,
              status TEXT DEFAULT 'active',
              created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_next_run ON scheduled_tasks(next_run);
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON scheduled_tasks(status);

            CREATE TABLE IF NOT EXISTS task_run_logs (
              id SERIAL PRIMARY KEY,
              task_id TEXT NOT NULL REFERENCES scheduled_tasks(id) ON DELETE CASCADE,
              run_at TIMESTAMPTZ NOT NULL,
              duration_ms INTEGER NOT NULL,
              status TEXT NOT NULL,
              result TEXT,
              error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_task_run_logs_task ON task_run_logs(task_id, run_at);

            CREATE TABLE IF NOT EXISTS router_state (
              key TEXT PRIMARY KEY,
              value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
              group_folder TEXT PRIMARY KEY,
              session_id TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS registered_groups (
              jid TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              folder TEXT NOT NULL UNIQUE,
              trigger_pattern TEXT NOT NULL,
              added_at TIMESTAMPTZ NOT NULL,
              container_config JSONB,
              requires_trigger BOOLEAN DEFAULT TRUE,
              runtime TEXT,
              model TEXT
            );
            ",
        )
        .await
        .context("failed to create postgres schema")
}

// ---------------------------------------------------------------------------
// Query functions — chat operations
// ---------------------------------------------------------------------------

impl PgPool {
    pub async fn store_chat_metadata(
        &self,
        jid: &str,
        timestamp: &str,
        name: Option<&str>,
        channel: Option<&str>,
        is_group: Option<bool>,
    ) -> anyhow::Result<()> {
        self.with_client(|client| {
            let jid = jid.to_string();
            let timestamp = timestamp.to_string();
            let name = name.map(|s| s.to_string());
            let channel = channel.map(|s| s.to_string());
            Box::pin(async move {
                let display_name = name.as_deref().unwrap_or(&jid);
                client
                    .execute(
                        "\
                        INSERT INTO chats (jid, name, last_message_time, channel, is_group)
                        VALUES ($1, $2, $3::timestamptz, $4, $5)
                        ON CONFLICT (jid) DO UPDATE SET
                          name = COALESCE(NULLIF(EXCLUDED.name, EXCLUDED.jid), chats.name),
                          last_message_time = GREATEST(chats.last_message_time, EXCLUDED.last_message_time),
                          channel = COALESCE(EXCLUDED.channel, chats.channel),
                          is_group = COALESCE(EXCLUDED.is_group, chats.is_group)
                        ",
                        &[&jid, &display_name, &timestamp, &channel, &is_group],
                    )
                    .await
                    .context("store_chat_metadata")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn update_chat_name(&self, jid: &str, name: &str) -> anyhow::Result<()> {
        self.with_client(|client| {
            let jid = jid.to_string();
            let name = name.to_string();
            Box::pin(async move {
                let now = chrono_now();
                client
                    .execute(
                        "\
                        INSERT INTO chats (jid, name, last_message_time)
                        VALUES ($1, $2, $3::timestamptz)
                        ON CONFLICT (jid) DO UPDATE SET name = EXCLUDED.name
                        ",
                        &[&jid, &name, &now],
                    )
                    .await
                    .context("update_chat_name")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_all_chats(&self) -> anyhow::Result<Vec<ChatInfo>> {
        self.with_client(|client| {
            Box::pin(async move {
                let rows = client
                    .query(
                        "SELECT jid, name, last_message_time, channel, is_group \
                         FROM chats ORDER BY last_message_time DESC",
                        &[],
                    )
                    .await
                    .context("get_all_chats")?;
                Ok(rows
                    .iter()
                    .map(|r| ChatInfo {
                        jid: r.get("jid"),
                        name: r.get::<_, Option<String>>("name").unwrap_or_default(),
                        last_message_time: format_ts(r.get("last_message_time")),
                        channel: r.get("channel"),
                        is_group: r.get::<_, Option<bool>>("is_group").unwrap_or(false),
                    })
                    .collect())
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Message operations
    // -----------------------------------------------------------------------

    pub async fn store_message(&self, msg: &NewMessage) -> anyhow::Result<()> {
        self.with_client(|client| {
            let msg = msg.clone();
            Box::pin(async move {
                client
                    .execute(
                        "\
                        INSERT INTO messages (id, chat_jid, sender, sender_name, content, timestamp, is_from_me, is_bot_message)
                        VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8)
                        ON CONFLICT (id, chat_jid) DO UPDATE SET
                          content = EXCLUDED.content,
                          is_bot_message = EXCLUDED.is_bot_message
                        ",
                        &[
                            &msg.id,
                            &msg.chat_jid,
                            &msg.sender,
                            &msg.sender_name,
                            &msg.content,
                            &msg.timestamp,
                            &msg.is_from_me,
                            &msg.is_bot_message,
                        ],
                    )
                    .await
                    .context("store_message")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_recent_conversation(
        &self,
        chat_jid: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ConversationMessage>> {
        self.with_client(|client| {
            let chat_jid = chat_jid.to_string();
            Box::pin(async move {
                let rows = client
                    .query(
                        "\
                        SELECT sender_name, content, timestamp, is_bot_message
                        FROM messages
                        WHERE chat_jid = $1 AND content != '' AND content IS NOT NULL
                        ORDER BY timestamp DESC
                        LIMIT $2
                        ",
                        &[&chat_jid, &limit],
                    )
                    .await
                    .context("get_recent_conversation")?;
                let mut result: Vec<ConversationMessage> = rows
                    .iter()
                    .map(|r| ConversationMessage {
                        sender_name: r.get::<_, Option<String>>("sender_name").unwrap_or_default(),
                        content: r.get::<_, Option<String>>("content").unwrap_or_default(),
                        timestamp: format_ts(r.get("timestamp")),
                        is_bot_message: r.get::<_, Option<bool>>("is_bot_message").unwrap_or(false),
                    })
                    .collect();
                result.reverse(); // Return in chronological order
                Ok(result)
            })
        })
        .await
    }

    pub async fn get_new_messages(
        &self,
        jids: &[String],
        last_timestamp: &str,
        bot_prefix: &str,
    ) -> anyhow::Result<(Vec<NewMessage>, String)> {
        if jids.is_empty() {
            return Ok((vec![], last_timestamp.to_string()));
        }
        self.with_client(|client| {
            let jids = jids.to_vec();
            let last_timestamp = last_timestamp.to_string();
            let bot_prefix = format!("{}:%", bot_prefix);
            Box::pin(async move {
                // Build dynamic IN clause
                let mut params: Vec<Box<dyn tokio_postgres::types::ToSql + Send + Sync>> =
                    Vec::with_capacity(jids.len() + 2);
                params.push(Box::new(last_timestamp.clone()));
                for jid in &jids {
                    params.push(Box::new(jid.clone()));
                }
                params.push(Box::new(bot_prefix));

                let placeholders: Vec<String> = (0..jids.len())
                    .map(|i| format!("${}", i + 2))
                    .collect();
                let bot_idx = jids.len() + 2;

                let sql = format!(
                    "SELECT id, chat_jid, sender, sender_name, content, timestamp \
                     FROM messages \
                     WHERE timestamp > $1::timestamptz AND chat_jid IN ({}) \
                       AND is_bot_message = FALSE AND content NOT LIKE ${} \
                       AND content != '' AND content IS NOT NULL \
                     ORDER BY timestamp",
                    placeholders.join(", "),
                    bot_idx,
                );

                let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    params.iter().map(|p| p.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
                let rows = client
                    .query(&sql, &param_refs)
                    .await
                    .context("get_new_messages")?;

                let mut new_timestamp = last_timestamp;
                let messages: Vec<NewMessage> = rows
                    .iter()
                    .map(|r| {
                        let ts = format_ts(r.get("timestamp"));
                        if ts > new_timestamp {
                            new_timestamp = ts.clone();
                        }
                        NewMessage {
                            id: r.get("id"),
                            chat_jid: r.get("chat_jid"),
                            sender: r.get::<_, Option<String>>("sender").unwrap_or_default(),
                            sender_name: r.get::<_, Option<String>>("sender_name").unwrap_or_default(),
                            content: r.get::<_, Option<String>>("content").unwrap_or_default(),
                            timestamp: ts,
                            is_from_me: false,
                            is_bot_message: false,
                        }
                    })
                    .collect();

                Ok((messages, new_timestamp))
            })
        })
        .await
    }

    pub async fn get_messages_since(
        &self,
        chat_jid: &str,
        since_timestamp: &str,
        bot_prefix: &str,
    ) -> anyhow::Result<Vec<NewMessage>> {
        self.with_client(|client| {
            let chat_jid = chat_jid.to_string();
            let since_timestamp = since_timestamp.to_string();
            let bot_prefix = format!("{}:%", bot_prefix);
            Box::pin(async move {
                let rows = client
                    .query(
                        "\
                        SELECT id, chat_jid, sender, sender_name, content, timestamp
                        FROM messages
                        WHERE chat_jid = $1 AND timestamp > $2::timestamptz
                          AND is_bot_message = FALSE AND content NOT LIKE $3
                          AND content != '' AND content IS NOT NULL
                        ORDER BY timestamp
                        ",
                        &[&chat_jid, &since_timestamp, &bot_prefix],
                    )
                    .await
                    .context("get_messages_since")?;
                Ok(rows
                    .iter()
                    .map(|r| NewMessage {
                        id: r.get("id"),
                        chat_jid: r.get("chat_jid"),
                        sender: r.get::<_, Option<String>>("sender").unwrap_or_default(),
                        sender_name: r.get::<_, Option<String>>("sender_name").unwrap_or_default(),
                        content: r.get::<_, Option<String>>("content").unwrap_or_default(),
                        timestamp: format_ts(r.get("timestamp")),
                        is_from_me: false,
                        is_bot_message: false,
                    })
                    .collect())
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Scheduled task operations
    // -----------------------------------------------------------------------

    pub async fn create_task(&self, task: &ScheduledTask) -> anyhow::Result<()> {
        self.with_client(|client| {
            let task = task.clone();
            Box::pin(async move {
                client
                    .execute(
                        "\
                        INSERT INTO scheduled_tasks
                          (id, group_folder, chat_jid, prompt, schedule_type, schedule_value, context_mode, next_run, status, created_at)
                        VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9, $10::timestamptz)
                        ",
                        &[
                            &task.id,
                            &task.group_folder,
                            &task.chat_jid,
                            &task.prompt,
                            &task.schedule_type,
                            &task.schedule_value,
                            &task.context_mode,
                            &task.next_run,
                            &task.status,
                            &task.created_at,
                        ],
                    )
                    .await
                    .context("create_task")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_task_by_id(&self, id: &str) -> anyhow::Result<Option<ScheduledTask>> {
        self.with_client(|client| {
            let id = id.to_string();
            Box::pin(async move {
                let row = client
                    .query_opt(
                        "SELECT * FROM scheduled_tasks WHERE id = $1",
                        &[&id],
                    )
                    .await
                    .context("get_task_by_id")?;
                Ok(row.map(|r| row_to_task(&r)))
            })
        })
        .await
    }

    pub async fn get_tasks_for_group(&self, group_folder: &str) -> anyhow::Result<Vec<ScheduledTask>> {
        self.with_client(|client| {
            let group_folder = group_folder.to_string();
            Box::pin(async move {
                let rows = client
                    .query(
                        "SELECT * FROM scheduled_tasks WHERE group_folder = $1 ORDER BY created_at DESC",
                        &[&group_folder],
                    )
                    .await
                    .context("get_tasks_for_group")?;
                Ok(rows.iter().map(|r| row_to_task(r)).collect())
            })
        })
        .await
    }

    pub async fn get_all_tasks(&self) -> anyhow::Result<Vec<ScheduledTask>> {
        self.with_client(|client| {
            Box::pin(async move {
                let rows = client
                    .query(
                        "SELECT * FROM scheduled_tasks ORDER BY created_at DESC",
                        &[],
                    )
                    .await
                    .context("get_all_tasks")?;
                Ok(rows.iter().map(|r| row_to_task(r)).collect())
            })
        })
        .await
    }

    pub async fn update_task(&self, id: &str, updates: &TaskUpdate) -> anyhow::Result<()> {
        // All task fields are strings — collect into Vec<String> for easy ownership transfer.
        let mut fields = Vec::new();
        let mut params: Vec<String> = Vec::new();
        let mut idx = 1usize;

        if let Some(ref prompt) = updates.prompt {
            fields.push(format!("prompt = ${idx}"));
            params.push(prompt.clone());
            idx += 1;
        }
        if let Some(ref schedule_type) = updates.schedule_type {
            fields.push(format!("schedule_type = ${idx}"));
            params.push(schedule_type.clone());
            idx += 1;
        }
        if let Some(ref schedule_value) = updates.schedule_value {
            fields.push(format!("schedule_value = ${idx}"));
            params.push(schedule_value.clone());
            idx += 1;
        }
        if let Some(ref next_run) = updates.next_run {
            fields.push(format!("next_run = ${idx}::timestamptz"));
            params.push(next_run.clone());
            idx += 1;
        }
        if let Some(ref status) = updates.status {
            fields.push(format!("status = ${idx}"));
            params.push(status.clone());
            idx += 1;
        }

        if fields.is_empty() {
            return Ok(());
        }

        params.push(id.to_string());
        let sql = format!(
            "UPDATE scheduled_tasks SET {} WHERE id = ${idx}",
            fields.join(", ")
        );

        self.with_client(|client| {
            Box::pin(async move {
                let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    params.iter().map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
                client.execute(&sql, &param_refs).await.context("update_task")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn delete_task(&self, id: &str) -> anyhow::Result<()> {
        self.with_client(|client| {
            let id = id.to_string();
            Box::pin(async move {
                // task_run_logs has ON DELETE CASCADE, but be explicit
                client
                    .execute("DELETE FROM task_run_logs WHERE task_id = $1", &[&id])
                    .await
                    .context("delete_task_logs")?;
                client
                    .execute("DELETE FROM scheduled_tasks WHERE id = $1", &[&id])
                    .await
                    .context("delete_task")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_due_tasks(&self) -> anyhow::Result<Vec<ScheduledTask>> {
        self.with_client(|client| {
            Box::pin(async move {
                let rows = client
                    .query(
                        "\
                        SELECT * FROM scheduled_tasks
                        WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= now()
                        ORDER BY next_run
                        ",
                        &[],
                    )
                    .await
                    .context("get_due_tasks")?;
                Ok(rows.iter().map(|r| row_to_task(r)).collect())
            })
        })
        .await
    }

    pub async fn update_task_after_run(
        &self,
        id: &str,
        next_run: Option<&str>,
        last_result: &str,
    ) -> anyhow::Result<()> {
        self.with_client(|client| {
            let id = id.to_string();
            let next_run = next_run.map(|s| s.to_string());
            let last_result = last_result.to_string();
            Box::pin(async move {
                let now = chrono_now();
                // If next_run is None, mark task as completed
                let new_status = if next_run.is_none() {
                    "completed"
                } else {
                    "active"
                };
                client
                    .execute(
                        "\
                        UPDATE scheduled_tasks
                        SET next_run = $1::timestamptz, last_run = $2::timestamptz,
                            last_result = $3,
                            status = CASE WHEN $1 IS NULL THEN 'completed' ELSE status END
                        WHERE id = $4
                        ",
                        &[&next_run, &now, &last_result, &id],
                    )
                    .await
                    .context("update_task_after_run")?;
                let _ = new_status; // used in the CASE expression via $1 IS NULL
                Ok(())
            })
        })
        .await
    }

    pub async fn log_task_run(&self, log: &TaskRunLog) -> anyhow::Result<()> {
        self.with_client(|client| {
            let log = log.clone();
            Box::pin(async move {
                client
                    .execute(
                        "\
                        INSERT INTO task_run_logs (task_id, run_at, duration_ms, status, result, error)
                        VALUES ($1, $2::timestamptz, $3, $4, $5, $6)
                        ",
                        &[
                            &log.task_id,
                            &log.run_at,
                            &(log.duration_ms as i32),
                            &log.status,
                            &log.result,
                            &log.error,
                        ],
                    )
                    .await
                    .context("log_task_run")?;
                Ok(())
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Router state operations
    // -----------------------------------------------------------------------

    pub async fn get_router_state(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.with_client(|client| {
            let key = key.to_string();
            Box::pin(async move {
                let row = client
                    .query_opt(
                        "SELECT value FROM router_state WHERE key = $1",
                        &[&key],
                    )
                    .await
                    .context("get_router_state")?;
                Ok(row.map(|r| r.get("value")))
            })
        })
        .await
    }

    pub async fn set_router_state(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.with_client(|client| {
            let key = key.to_string();
            let value = value.to_string();
            Box::pin(async move {
                client
                    .execute(
                        "\
                        INSERT INTO router_state (key, value) VALUES ($1, $2)
                        ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value
                        ",
                        &[&key, &value],
                    )
                    .await
                    .context("set_router_state")?;
                Ok(())
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Session operations
    // -----------------------------------------------------------------------

    pub async fn get_session(&self, group_folder: &str) -> anyhow::Result<Option<String>> {
        self.with_client(|client| {
            let group_folder = group_folder.to_string();
            Box::pin(async move {
                let row = client
                    .query_opt(
                        "SELECT session_id FROM sessions WHERE group_folder = $1",
                        &[&group_folder],
                    )
                    .await
                    .context("get_session")?;
                Ok(row.map(|r| r.get("session_id")))
            })
        })
        .await
    }

    pub async fn set_session(&self, group_folder: &str, session_id: &str) -> anyhow::Result<()> {
        self.with_client(|client| {
            let group_folder = group_folder.to_string();
            let session_id = session_id.to_string();
            Box::pin(async move {
                client
                    .execute(
                        "\
                        INSERT INTO sessions (group_folder, session_id) VALUES ($1, $2)
                        ON CONFLICT (group_folder) DO UPDATE SET session_id = EXCLUDED.session_id
                        ",
                        &[&group_folder, &session_id],
                    )
                    .await
                    .context("set_session")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_all_sessions(&self) -> anyhow::Result<HashMap<String, String>> {
        self.with_client(|client| {
            Box::pin(async move {
                let rows = client
                    .query("SELECT group_folder, session_id FROM sessions", &[])
                    .await
                    .context("get_all_sessions")?;
                let mut result = HashMap::new();
                for row in &rows {
                    result.insert(
                        row.get::<_, String>("group_folder"),
                        row.get::<_, String>("session_id"),
                    );
                }
                Ok(result)
            })
        })
        .await
    }

    pub async fn delete_session(&self, group_folder: &str) -> anyhow::Result<()> {
        self.with_client(|client| {
            let group_folder = group_folder.to_string();
            Box::pin(async move {
                client
                    .execute(
                        "DELETE FROM sessions WHERE group_folder = $1",
                        &[&group_folder],
                    )
                    .await
                    .context("delete_session")?;
                Ok(())
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Registered group operations
    // -----------------------------------------------------------------------

    pub async fn get_registered_group(&self, jid: &str) -> anyhow::Result<Option<RegisteredGroup>> {
        self.with_client(|client| {
            let jid = jid.to_string();
            Box::pin(async move {
                let row = client
                    .query_opt(
                        "SELECT * FROM registered_groups WHERE jid = $1",
                        &[&jid],
                    )
                    .await
                    .context("get_registered_group")?;
                Ok(row.map(|r| row_to_registered_group(&r)))
            })
        })
        .await
    }

    pub async fn set_registered_group(&self, group: &RegisteredGroup) -> anyhow::Result<()> {
        self.with_client(|client| {
            let group = group.clone();
            Box::pin(async move {
                let config_json: Option<serde_json::Value> = group.container_config.clone();
                let requires_trigger = group.requires_trigger.unwrap_or(true);
                client
                    .execute(
                        "\
                        INSERT INTO registered_groups
                          (jid, name, folder, trigger_pattern, added_at, container_config, requires_trigger, runtime, model)
                        VALUES ($1, $2, $3, $4, $5::timestamptz, $6, $7, $8, $9)
                        ON CONFLICT (jid) DO UPDATE SET
                          name = EXCLUDED.name,
                          folder = EXCLUDED.folder,
                          trigger_pattern = EXCLUDED.trigger_pattern,
                          container_config = EXCLUDED.container_config,
                          requires_trigger = EXCLUDED.requires_trigger,
                          runtime = EXCLUDED.runtime,
                          model = EXCLUDED.model
                        ",
                        &[
                            &group.jid,
                            &group.name,
                            &group.folder,
                            &group.trigger,
                            &group.added_at,
                            &config_json,
                            &requires_trigger,
                            &group.runtime,
                            &group.model,
                        ],
                    )
                    .await
                    .context("set_registered_group")?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_all_registered_groups(&self) -> anyhow::Result<HashMap<String, RegisteredGroup>> {
        self.with_client(|client| {
            Box::pin(async move {
                let rows = client
                    .query("SELECT * FROM registered_groups", &[])
                    .await
                    .context("get_all_registered_groups")?;
                let mut result = HashMap::new();
                for row in &rows {
                    let group = row_to_registered_group(row);
                    result.insert(group.jid.clone(), group);
                }
                Ok(result)
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    // Format as ISO 8601 to match Node's new Date().toISOString()
    let secs = now.as_secs();
    let millis = now.as_millis() % 1000;
    // Use a simple formatter — no chrono dependency needed
    let dt = time_from_epoch(secs, millis as u32);
    dt
}

fn time_from_epoch(secs: u64, millis: u32) -> String {
    // Convert epoch seconds to UTC datetime string
    // Days since epoch
    let days = secs / 86400;
    let rem = secs % 86400;
    let hours = rem / 3600;
    let minutes = (rem % 3600) / 60;
    let seconds = rem % 60;

    // Gregorian calendar from days since 1970-01-01
    let (year, month, day) = days_to_date(days);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hours, minutes, seconds, millis
    )
}

fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Format a Postgres TIMESTAMPTZ value as ISO 8601 string.
/// tokio-postgres returns TIMESTAMPTZ as `SystemTime` when no chrono feature.
fn format_ts(ts: std::time::SystemTime) -> String {
    let dur = ts
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    time_from_epoch(dur.as_secs(), (dur.as_millis() % 1000) as u32)
}

fn row_to_task(r: &tokio_postgres::Row) -> ScheduledTask {
    ScheduledTask {
        id: r.get("id"),
        group_folder: r.get("group_folder"),
        chat_jid: r.get("chat_jid"),
        prompt: r.get("prompt"),
        schedule_type: r.get("schedule_type"),
        schedule_value: r.get("schedule_value"),
        context_mode: r
            .get::<_, Option<String>>("context_mode")
            .unwrap_or_else(|| "isolated".to_string()),
        next_run: r.get::<_, Option<std::time::SystemTime>>("next_run").map(format_ts),
        last_run: r.get::<_, Option<std::time::SystemTime>>("last_run").map(format_ts),
        last_result: r.get("last_result"),
        status: r
            .get::<_, Option<String>>("status")
            .unwrap_or_else(|| "active".to_string()),
        created_at: format_ts(r.get("created_at")),
    }
}

fn row_to_registered_group(r: &tokio_postgres::Row) -> RegisteredGroup {
    RegisteredGroup {
        jid: r.get("jid"),
        name: r.get("name"),
        folder: r.get("folder"),
        trigger: r.get("trigger_pattern"),
        added_at: format_ts(r.get("added_at")),
        container_config: r.get("container_config"),
        requires_trigger: r.get::<_, Option<bool>>("requires_trigger"),
        runtime: r.get("runtime"),
        model: r.get("model"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_now_format() {
        let ts = chrono_now();
        // Should be ISO 8601 with Z suffix
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {ts}");
        assert!(ts.contains('T'), "timestamp should contain T: {ts}");
        assert_eq!(ts.len(), 24, "expected YYYY-MM-DDTHH:MM:SS.mmmZ format: {ts}");
    }

    #[test]
    fn time_from_epoch_known_date() {
        // 2024-01-15T12:30:45.123Z = epoch 1705321845
        let ts = time_from_epoch(1705321845, 123);
        assert_eq!(ts, "2024-01-15T12:30:45.123Z");
    }

    #[test]
    fn days_to_date_epoch() {
        let (y, m, d) = days_to_date(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_date_known() {
        // 2024-01-15 is day 19737 since epoch
        let (y, m, d) = days_to_date(19737);
        assert_eq!((y, m, d), (2024, 1, 15));
    }

    #[test]
    fn default_serde_values() {
        let json = r#"{"id":"t1","group_folder":"g1","chat_jid":"j1","prompt":"p","schedule_type":"once","schedule_value":"2024-01-01","created_at":"2024-01-01T00:00:00Z"}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.context_mode, "isolated");
        assert_eq!(task.status, "active");
        assert!(task.next_run.is_none());
    }

    #[test]
    fn registered_group_serde_roundtrip() {
        let group = RegisteredGroup {
            jid: "tg:12345".to_string(),
            name: "Test Group".to_string(),
            folder: "test-group".to_string(),
            trigger: "!ai".to_string(),
            added_at: "2024-01-01T00:00:00.000Z".to_string(),
            container_config: Some(serde_json::json!({"additionalMounts": []})),
            requires_trigger: Some(true),
            runtime: Some("claude".to_string()),
            model: None,
        };
        let json = serde_json::to_string(&group).unwrap();
        let parsed: RegisteredGroup = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.jid, "tg:12345");
        assert_eq!(parsed.runtime, Some("claude".to_string()));
        // model should be absent from JSON (skip_serializing_if)
        assert!(!json.contains("\"model\""));
    }

    #[test]
    fn pg_pool_new() {
        let pool = PgPool::new("postgres://localhost/test".to_string());
        assert_eq!(pool.dsn, "postgres://localhost/test");
    }
}
