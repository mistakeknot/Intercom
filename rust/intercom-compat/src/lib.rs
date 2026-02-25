use std::fs;
use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

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
}
