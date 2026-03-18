package db

import (
	"context"

	"github.com/jackc/pgx/v5"
)

// ensureSchema creates all tables and indexes if they don't exist.
// Matches the Rust ensure_schema exactly — same DDL, same indexes, same triggers.
func ensureSchema(ctx context.Context, conn *pgx.Conn) error {
	_, err := conn.Exec(ctx, schemaDDL)
	return err
}

const schemaDDL = `
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
CREATE INDEX IF NOT EXISTS idx_tasks_status_next_run
  ON scheduled_tasks(status, next_run)
  WHERE status = 'active' AND next_run IS NOT NULL;

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

CREATE TABLE IF NOT EXISTS message_outbox (
  id BIGSERIAL PRIMARY KEY,
  chat_jid TEXT NOT NULL,
  payload_type TEXT NOT NULL,
  payload JSONB NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  delivered_at TIMESTAMPTZ,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);
CREATE INDEX IF NOT EXISTS idx_outbox_pending
  ON message_outbox(status, created_at)
  WHERE status = 'pending';

CREATE OR REPLACE FUNCTION notify_outbox_insert() RETURNS TRIGGER AS $fn$
BEGIN
  PERFORM pg_notify('intercom_outbox', NEW.id::TEXT);
  RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;

DO $do$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_trigger WHERE tgname = 'trg_outbox_notify'
  ) THEN
    CREATE TRIGGER trg_outbox_notify
      AFTER INSERT ON message_outbox
      FOR EACH ROW EXECUTE FUNCTION notify_outbox_insert();
  END IF;
END;
$do$;
`
