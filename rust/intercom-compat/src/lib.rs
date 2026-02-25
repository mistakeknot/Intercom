use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls, Transaction};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacySnapshot {
    pub chats: u64,
    pub messages: u64,
    pub registered_groups: u64,
    pub sessions: u64,
    pub scheduled_tasks: u64,
    pub task_run_logs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyLayout {
    pub has_env: bool,
    pub group_folders: u64,
    pub has_main_group: bool,
    pub has_global_group: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigratedCounts {
    pub chats: u64,
    pub messages: u64,
    pub registered_groups: u64,
    pub sessions: u64,
    pub scheduled_tasks: u64,
    pub task_run_logs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationOptions {
    pub sqlite_path: PathBuf,
    pub postgres_dsn: String,
    pub dry_run: bool,
    pub checkpoint_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub dry_run: bool,
    pub checkpoint_name: String,
    pub skipped_by_checkpoint: bool,
    pub source: LegacySnapshot,
    pub planned: LegacySnapshot,
    pub migrated: MigratedCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParityReport {
    pub checkpoint_name: Option<String>,
    pub source: LegacySnapshot,
    pub target: MigratedCounts,
    pub matches: bool,
    pub mismatches: Vec<String>,
}

pub fn inspect_legacy_sqlite(path: impl AsRef<Path>) -> anyhow::Result<LegacySnapshot> {
    let path = path.as_ref();
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open sqlite database: {}", path.display()))?;

    Ok(LegacySnapshot {
        chats: count_rows(&conn, "chats")?,
        messages: count_rows(&conn, "messages")?,
        registered_groups: count_rows(&conn, "registered_groups")?,
        sessions: count_rows(&conn, "sessions")?,
        scheduled_tasks: count_rows(&conn, "scheduled_tasks")?,
        task_run_logs: count_rows(&conn, "task_run_logs")?,
    })
}

pub fn inspect_legacy_layout(project_root: impl AsRef<Path>) -> LegacyLayout {
    let project_root = project_root.as_ref();
    let env_file = project_root.join(".env");
    let groups_dir = project_root.join("groups");

    let mut layout = LegacyLayout {
        has_env: env_file.exists(),
        ..LegacyLayout::default()
    };

    if groups_dir.is_dir() {
        let mut folder_count = 0_u64;
        if let Ok(entries) = fs::read_dir(&groups_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    folder_count += 1;
                    if let Some(name) = entry.file_name().to_str() {
                        if name == "main" {
                            layout.has_main_group = true;
                        }
                        if name == "global" {
                            layout.has_global_group = true;
                        }
                    }
                }
            }
        }
        layout.group_folders = folder_count;
    }

    layout
}

pub async fn migrate_legacy_to_postgres(
    options: MigrationOptions,
) -> anyhow::Result<MigrationReport> {
    let source = inspect_legacy_sqlite(&options.sqlite_path)?;

    if options.dry_run {
        return Ok(MigrationReport {
            dry_run: true,
            checkpoint_name: options.checkpoint_name,
            skipped_by_checkpoint: false,
            planned: source.clone(),
            source,
            migrated: MigratedCounts::default(),
        });
    }

    if options.postgres_dsn.trim().is_empty() {
        return Err(anyhow!(
            "postgres DSN is required when running migration without --dry-run"
        ));
    }

    let sqlite = Connection::open(&options.sqlite_path).with_context(|| {
        format!(
            "failed to open sqlite database for migration: {}",
            options.sqlite_path.display()
        )
    })?;

    let mut client = connect_postgres(&options.postgres_dsn).await?;
    ensure_postgres_schema(&client).await?;

    if checkpoint_exists(&client, &options.checkpoint_name).await? {
        return Ok(MigrationReport {
            dry_run: false,
            checkpoint_name: options.checkpoint_name,
            skipped_by_checkpoint: true,
            planned: source.clone(),
            source,
            migrated: MigratedCounts::default(),
        });
    }

    let tx = client.transaction().await?;
    let mut migrated = MigratedCounts::default();

    migrated.chats = migrate_chats(&sqlite, &tx).await?;
    migrated.messages = migrate_messages(&sqlite, &tx).await?;
    migrated.registered_groups = migrate_registered_groups(&sqlite, &tx).await?;
    migrated.sessions = migrate_sessions(&sqlite, &tx).await?;
    migrated.scheduled_tasks = migrate_scheduled_tasks(&sqlite, &tx).await?;
    migrated.task_run_logs = migrate_task_run_logs(&sqlite, &tx).await?;

    let details = serde_json::to_string(&migrated)?;
    tx.execute(
        "\
        INSERT INTO intercom_migration_checkpoints (checkpoint_name, details)
        VALUES ($1, $2::jsonb)
        ON CONFLICT (checkpoint_name)
        DO UPDATE SET completed_at = now(), details = EXCLUDED.details
        ",
        &[&options.checkpoint_name, &details],
    )
    .await?;

    tx.commit().await?;

    Ok(MigrationReport {
        dry_run: false,
        checkpoint_name: options.checkpoint_name,
        skipped_by_checkpoint: false,
        planned: source.clone(),
        source,
        migrated,
    })
}

pub async fn verify_migration_parity(
    sqlite_path: impl AsRef<Path>,
    postgres_dsn: &str,
) -> anyhow::Result<ParityReport> {
    if postgres_dsn.trim().is_empty() {
        return Err(anyhow!("postgres DSN is required for parity verification"));
    }

    let source = inspect_legacy_sqlite(sqlite_path)?;
    let client = connect_postgres(postgres_dsn).await?;

    let target = MigratedCounts {
        chats: count_pg_rows(&client, "intercom_legacy_chats").await?,
        messages: count_pg_rows(&client, "intercom_legacy_messages").await?,
        registered_groups: count_pg_rows(&client, "intercom_legacy_registered_groups").await?,
        sessions: count_pg_rows(&client, "intercom_legacy_sessions").await?,
        scheduled_tasks: count_pg_rows(&client, "intercom_legacy_scheduled_tasks").await?,
        task_run_logs: count_pg_rows(&client, "intercom_legacy_task_run_logs").await?,
    };

    let mut mismatches = Vec::new();
    compare_count("chats", source.chats, target.chats, &mut mismatches);
    compare_count(
        "messages",
        source.messages,
        target.messages,
        &mut mismatches,
    );
    compare_count(
        "registered_groups",
        source.registered_groups,
        target.registered_groups,
        &mut mismatches,
    );
    compare_count(
        "sessions",
        source.sessions,
        target.sessions,
        &mut mismatches,
    );
    compare_count(
        "scheduled_tasks",
        source.scheduled_tasks,
        target.scheduled_tasks,
        &mut mismatches,
    );
    compare_count(
        "task_run_logs",
        source.task_run_logs,
        target.task_run_logs,
        &mut mismatches,
    );

    let checkpoint_name = latest_checkpoint_name(&client).await?;

    Ok(ParityReport {
        checkpoint_name,
        source,
        target,
        matches: mismatches.is_empty(),
        mismatches,
    })
}

fn compare_count(name: &str, source: u64, target: u64, mismatches: &mut Vec<String>) {
    if source != target {
        mismatches.push(format!("{name}: source={source}, target={target}"));
    }
}

fn count_rows(conn: &Connection, table: &str) -> anyhow::Result<u64> {
    let query = format!("SELECT COUNT(*) FROM {table}");
    let mut stmt = match conn.prepare(&query) {
        Ok(stmt) => stmt,
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("no such table") {
                return Ok(0);
            }
            return Err(err)
                .with_context(|| format!("failed to prepare count query for table `{table}`"));
        }
    };

    let count: i64 = stmt
        .query_row([], |row| row.get(0))
        .with_context(|| format!("failed to execute count query for table `{table}`"))?;

    Ok(count.max(0) as u64)
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

async fn connect_postgres(dsn: &str) -> anyhow::Result<Client> {
    let (client, connection) = tokio_postgres::connect(dsn, NoTls)
        .await
        .with_context(|| "failed to connect to postgres")?;

    tokio::spawn(async move {
        if let Err(err) = connection.await {
            eprintln!("postgres connection error: {err}");
        }
    });

    Ok(client)
}

async fn ensure_postgres_schema(client: &Client) -> anyhow::Result<()> {
    client
        .batch_execute(
            "\
            CREATE TABLE IF NOT EXISTS intercom_migration_checkpoints (
              checkpoint_name TEXT PRIMARY KEY,
              completed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
              details JSONB NOT NULL DEFAULT '{}'::jsonb
            );

            CREATE TABLE IF NOT EXISTS intercom_legacy_chats (
              jid TEXT PRIMARY KEY,
              name TEXT,
              last_message_time TEXT,
              channel TEXT,
              is_group BIGINT
            );

            CREATE TABLE IF NOT EXISTS intercom_legacy_messages (
              id TEXT NOT NULL,
              chat_jid TEXT NOT NULL,
              sender TEXT,
              sender_name TEXT,
              content TEXT,
              timestamp TEXT,
              is_from_me BIGINT,
              is_bot_message BIGINT,
              PRIMARY KEY (id, chat_jid)
            );

            CREATE TABLE IF NOT EXISTS intercom_legacy_registered_groups (
              jid TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              folder TEXT NOT NULL,
              trigger_pattern TEXT NOT NULL,
              added_at TEXT NOT NULL,
              container_config TEXT,
              requires_trigger BIGINT,
              runtime TEXT,
              model TEXT
            );

            CREATE TABLE IF NOT EXISTS intercom_legacy_sessions (
              group_folder TEXT PRIMARY KEY,
              session_id TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS intercom_legacy_scheduled_tasks (
              id TEXT PRIMARY KEY,
              group_folder TEXT NOT NULL,
              chat_jid TEXT NOT NULL,
              prompt TEXT NOT NULL,
              schedule_type TEXT NOT NULL,
              schedule_value TEXT NOT NULL,
              next_run TEXT,
              last_run TEXT,
              last_result TEXT,
              status TEXT,
              created_at TEXT,
              context_mode TEXT
            );

            CREATE TABLE IF NOT EXISTS intercom_legacy_task_run_logs (
              id BIGINT PRIMARY KEY,
              task_id TEXT NOT NULL,
              run_at TEXT NOT NULL,
              duration_ms BIGINT,
              status TEXT,
              result TEXT,
              error TEXT
            );
            ",
        )
        .await
        .context("failed to create postgres migration schema")
}

async fn checkpoint_exists(client: &Client, checkpoint_name: &str) -> anyhow::Result<bool> {
    let row = client
        .query_opt(
            "SELECT checkpoint_name FROM intercom_migration_checkpoints WHERE checkpoint_name = $1",
            &[&checkpoint_name],
        )
        .await?;
    Ok(row.is_some())
}

async fn latest_checkpoint_name(client: &Client) -> anyhow::Result<Option<String>> {
    let row = client
        .query_opt(
            "SELECT checkpoint_name FROM intercom_migration_checkpoints ORDER BY completed_at DESC LIMIT 1",
            &[],
        )
        .await
        .or_else(|err| {
            if err.code() == Some(&SqlState::UNDEFINED_TABLE) {
                Ok(None)
            } else {
                Err(err)
            }
        })?;

    Ok(row.map(|row| row.get::<usize, String>(0)))
}

async fn count_pg_rows(client: &Client, table: &str) -> anyhow::Result<u64> {
    let query = format!("SELECT COUNT(*) FROM {table}");
    match client.query_one(&query, &[]).await {
        Ok(row) => {
            let count: i64 = row.get(0);
            Ok(count.max(0) as u64)
        }
        Err(err) if err.code() == Some(&SqlState::UNDEFINED_TABLE) => Ok(0),
        Err(err) => Err(err).context(format!("failed to count rows for table `{table}`")),
    }
}

async fn migrate_chats(sqlite: &Connection, tx: &Transaction<'_>) -> anyhow::Result<u64> {
    if !sqlite_has_table(sqlite, "chats")? {
        return Ok(0);
    }

    let mut stmt =
        sqlite.prepare("SELECT jid, name, last_message_time, channel, is_group FROM chats")?;
    let mut rows = stmt.query([])?;
    let mut count = 0_u64;

    while let Some(row) = rows.next()? {
        let jid: String = row.get(0)?;
        let name: Option<String> = row.get(1)?;
        let last_message_time: Option<String> = row.get(2)?;
        let channel: Option<String> = row.get(3)?;
        let is_group: Option<i64> = row.get(4)?;

        tx.execute(
            "\
            INSERT INTO intercom_legacy_chats (jid, name, last_message_time, channel, is_group)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (jid)
            DO UPDATE SET
              name = EXCLUDED.name,
              last_message_time = EXCLUDED.last_message_time,
              channel = EXCLUDED.channel,
              is_group = EXCLUDED.is_group
            ",
            &[&jid, &name, &last_message_time, &channel, &is_group],
        )
        .await?;

        count += 1;
    }

    Ok(count)
}

async fn migrate_messages(sqlite: &Connection, tx: &Transaction<'_>) -> anyhow::Result<u64> {
    if !sqlite_has_table(sqlite, "messages")? {
        return Ok(0);
    }

    let has_sender_name = sqlite_has_column(sqlite, "messages", "sender_name")?;
    let has_is_bot_message = sqlite_has_column(sqlite, "messages", "is_bot_message")?;

    let sender_name_expr = if has_sender_name {
        "sender_name"
    } else {
        "NULL AS sender_name"
    };
    let is_bot_expr = if has_is_bot_message {
        "is_bot_message"
    } else {
        "0 AS is_bot_message"
    };

    let query = format!(
        "SELECT id, chat_jid, sender, {sender_name_expr}, content, timestamp, is_from_me, {is_bot_expr} FROM messages"
    );

    let mut stmt = sqlite.prepare(&query)?;
    let mut rows = stmt.query([])?;
    let mut count = 0_u64;

    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let chat_jid: String = row.get(1)?;
        let sender: Option<String> = row.get(2)?;
        let sender_name: Option<String> = row.get(3)?;
        let content: Option<String> = row.get(4)?;
        let timestamp: Option<String> = row.get(5)?;
        let is_from_me: Option<i64> = row.get(6)?;
        let is_bot_message: Option<i64> = row.get(7)?;

        tx.execute(
            "\
            INSERT INTO intercom_legacy_messages
              (id, chat_jid, sender, sender_name, content, timestamp, is_from_me, is_bot_message)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id, chat_jid)
            DO UPDATE SET
              sender = EXCLUDED.sender,
              sender_name = EXCLUDED.sender_name,
              content = EXCLUDED.content,
              timestamp = EXCLUDED.timestamp,
              is_from_me = EXCLUDED.is_from_me,
              is_bot_message = EXCLUDED.is_bot_message
            ",
            &[
                &id,
                &chat_jid,
                &sender,
                &sender_name,
                &content,
                &timestamp,
                &is_from_me,
                &is_bot_message,
            ],
        )
        .await?;

        count += 1;
    }

    Ok(count)
}

async fn migrate_registered_groups(
    sqlite: &Connection,
    tx: &Transaction<'_>,
) -> anyhow::Result<u64> {
    if !sqlite_has_table(sqlite, "registered_groups")? {
        return Ok(0);
    }

    let has_runtime = sqlite_has_column(sqlite, "registered_groups", "runtime")?;
    let has_model = sqlite_has_column(sqlite, "registered_groups", "model")?;

    let runtime_expr = if has_runtime {
        "runtime"
    } else {
        "NULL AS runtime"
    };
    let model_expr = if has_model { "model" } else { "NULL AS model" };

    let query = format!(
        "SELECT jid, name, folder, trigger_pattern, added_at, container_config, COALESCE(requires_trigger, 1), {runtime_expr}, {model_expr} FROM registered_groups"
    );

    let mut stmt = sqlite.prepare(&query)?;
    let mut rows = stmt.query([])?;
    let mut count = 0_u64;

    while let Some(row) = rows.next()? {
        let jid: String = row.get(0)?;
        let name: String = row.get(1)?;
        let folder: String = row.get(2)?;
        let trigger_pattern: String = row.get(3)?;
        let added_at: String = row.get(4)?;
        let container_config: Option<String> = row.get(5)?;
        let requires_trigger: Option<i64> = row.get(6)?;
        let runtime: Option<String> = row.get(7)?;
        let model: Option<String> = row.get(8)?;

        tx.execute(
            "\
            INSERT INTO intercom_legacy_registered_groups
              (jid, name, folder, trigger_pattern, added_at, container_config, requires_trigger, runtime, model)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (jid)
            DO UPDATE SET
              name = EXCLUDED.name,
              folder = EXCLUDED.folder,
              trigger_pattern = EXCLUDED.trigger_pattern,
              added_at = EXCLUDED.added_at,
              container_config = EXCLUDED.container_config,
              requires_trigger = EXCLUDED.requires_trigger,
              runtime = EXCLUDED.runtime,
              model = EXCLUDED.model
            ",
            &[
                &jid,
                &name,
                &folder,
                &trigger_pattern,
                &added_at,
                &container_config,
                &requires_trigger,
                &runtime,
                &model,
            ],
        )
        .await?;

        count += 1;
    }

    Ok(count)
}

async fn migrate_sessions(sqlite: &Connection, tx: &Transaction<'_>) -> anyhow::Result<u64> {
    if !sqlite_has_table(sqlite, "sessions")? {
        return Ok(0);
    }

    let mut stmt = sqlite.prepare("SELECT group_folder, session_id FROM sessions")?;
    let mut rows = stmt.query([])?;
    let mut count = 0_u64;

    while let Some(row) = rows.next()? {
        let group_folder: String = row.get(0)?;
        let session_id: String = row.get(1)?;

        tx.execute(
            "\
            INSERT INTO intercom_legacy_sessions (group_folder, session_id)
            VALUES ($1, $2)
            ON CONFLICT (group_folder)
            DO UPDATE SET session_id = EXCLUDED.session_id
            ",
            &[&group_folder, &session_id],
        )
        .await?;

        count += 1;
    }

    Ok(count)
}

async fn migrate_scheduled_tasks(sqlite: &Connection, tx: &Transaction<'_>) -> anyhow::Result<u64> {
    if !sqlite_has_table(sqlite, "scheduled_tasks")? {
        return Ok(0);
    }

    let has_context_mode = sqlite_has_column(sqlite, "scheduled_tasks", "context_mode")?;
    let context_expr = if has_context_mode {
        "context_mode"
    } else {
        "NULL AS context_mode"
    };

    let query = format!(
        "SELECT id, group_folder, chat_jid, prompt, schedule_type, schedule_value, next_run, last_run, last_result, status, created_at, {context_expr} FROM scheduled_tasks"
    );

    let mut stmt = sqlite.prepare(&query)?;
    let mut rows = stmt.query([])?;
    let mut count = 0_u64;

    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let group_folder: String = row.get(1)?;
        let chat_jid: String = row.get(2)?;
        let prompt: String = row.get(3)?;
        let schedule_type: String = row.get(4)?;
        let schedule_value: String = row.get(5)?;
        let next_run: Option<String> = row.get(6)?;
        let last_run: Option<String> = row.get(7)?;
        let last_result: Option<String> = row.get(8)?;
        let status: Option<String> = row.get(9)?;
        let created_at: Option<String> = row.get(10)?;
        let context_mode: Option<String> = row.get(11)?;

        tx.execute(
            "\
            INSERT INTO intercom_legacy_scheduled_tasks
              (id, group_folder, chat_jid, prompt, schedule_type, schedule_value, next_run, last_run, last_result, status, created_at, context_mode)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (id)
            DO UPDATE SET
              group_folder = EXCLUDED.group_folder,
              chat_jid = EXCLUDED.chat_jid,
              prompt = EXCLUDED.prompt,
              schedule_type = EXCLUDED.schedule_type,
              schedule_value = EXCLUDED.schedule_value,
              next_run = EXCLUDED.next_run,
              last_run = EXCLUDED.last_run,
              last_result = EXCLUDED.last_result,
              status = EXCLUDED.status,
              created_at = EXCLUDED.created_at,
              context_mode = EXCLUDED.context_mode
            ",
            &[
                &id,
                &group_folder,
                &chat_jid,
                &prompt,
                &schedule_type,
                &schedule_value,
                &next_run,
                &last_run,
                &last_result,
                &status,
                &created_at,
                &context_mode,
            ],
        )
        .await?;

        count += 1;
    }

    Ok(count)
}

async fn migrate_task_run_logs(sqlite: &Connection, tx: &Transaction<'_>) -> anyhow::Result<u64> {
    if !sqlite_has_table(sqlite, "task_run_logs")? {
        return Ok(0);
    }

    let mut stmt = sqlite.prepare(
        "SELECT id, task_id, run_at, duration_ms, status, result, error FROM task_run_logs",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0_u64;

    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let task_id: String = row.get(1)?;
        let run_at: String = row.get(2)?;
        let duration_ms: Option<i64> = row.get(3)?;
        let status: Option<String> = row.get(4)?;
        let result: Option<String> = row.get(5)?;
        let error: Option<String> = row.get(6)?;

        tx.execute(
            "\
            INSERT INTO intercom_legacy_task_run_logs
              (id, task_id, run_at, duration_ms, status, result, error)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (id)
            DO UPDATE SET
              task_id = EXCLUDED.task_id,
              run_at = EXCLUDED.run_at,
              duration_ms = EXCLUDED.duration_ms,
              status = EXCLUDED.status,
              result = EXCLUDED.result,
              error = EXCLUDED.error
            ",
            &[
                &id,
                &task_id,
                &run_at,
                &duration_ms,
                &status,
                &result,
                &error,
            ],
        )
        .await?;

        count += 1;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn count_defaults_to_zero_for_missing_tables() {
        let conn = Connection::open_in_memory().expect("open in memory sqlite");
        let rows = count_rows(&conn, "does_not_exist").expect("count missing table");
        assert_eq!(rows, 0);
    }

    #[test]
    fn inspect_sqlite_counts_known_tables() {
        let tmp = TempDir::new().expect("create tempdir");
        let db_path = tmp.path().join("messages.db");
        let conn = Connection::open(&db_path).expect("open sqlite");

        conn.execute_batch(
            "\
            CREATE TABLE chats (jid TEXT PRIMARY KEY);\
            CREATE TABLE registered_groups (jid TEXT PRIMARY KEY);\
            INSERT INTO chats (jid) VALUES ('a');\
            INSERT INTO chats (jid) VALUES ('b');\
            INSERT INTO registered_groups (jid) VALUES ('g1');\
            ",
        )
        .expect("seed tables");

        drop(conn);

        let snapshot = inspect_legacy_sqlite(&db_path).expect("inspect sqlite");
        assert_eq!(snapshot.chats, 2);
        assert_eq!(snapshot.registered_groups, 1);
        assert_eq!(snapshot.messages, 0);
        assert_eq!(snapshot.scheduled_tasks, 0);
    }

    #[tokio::test]
    async fn dry_run_migration_uses_sqlite_only() {
        let tmp = TempDir::new().expect("create tempdir");
        let db_path = tmp.path().join("messages.db");
        let conn = Connection::open(&db_path).expect("open sqlite");

        conn.execute_batch(
            "\
            CREATE TABLE chats (jid TEXT PRIMARY KEY);\
            INSERT INTO chats (jid) VALUES ('a');\
            ",
        )
        .expect("seed tables");

        drop(conn);

        let report = migrate_legacy_to_postgres(MigrationOptions {
            sqlite_path: db_path,
            postgres_dsn: "postgres://unused".to_string(),
            dry_run: true,
            checkpoint_name: "test_checkpoint".to_string(),
        })
        .await
        .expect("dry-run migration");

        assert!(report.dry_run);
        assert_eq!(report.source.chats, 1);
        assert_eq!(report.planned.chats, 1);
        assert_eq!(report.migrated.chats, 0);
    }
}
