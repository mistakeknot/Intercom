# IronClaw (Rust Daemon)

## Crate Structure

| Crate | Purpose |
|-------|---------|
| `intercomd` | Axum HTTP daemon — Telegram poller, bridge, IPC, events, orchestrator, container runner |
| `intercom-core` | Shared types: config, IPC, container protocol, Postgres persistence, Demarch adapter |
| `intercom-compat` | Legacy SQLite inspection and SQLite-to-Postgres migration |

## Configuration (`config/intercom.toml`)

TOML-based config with env var overrides (`INTERCOMD_BIND`, `INTERCOM_POSTGRES_DSN`). Key sections:

- `[server]` — bind address (default `127.0.0.1:7340`)
- `[storage]` — Postgres DSN, groups dir
- `[runtimes]` — runtime profiles (claude/gemini/codex) with provider, default model, required env vars
- `[orchestrator]` — `enabled` flag, max concurrent containers, poll interval, idle timeout, main group folder
- `[scheduler]` — `enabled` flag, poll interval, IANA timezone for cron
- `[events]` — `enabled` flag, poll interval, notification JID for push notifications
- `[demarch]` — `enabled` flag, read/write allowlists for `ic`/`bd` CLI commands

## CLI Subcommands

```bash
intercomd serve --config config/intercom.toml     # Start HTTP service (default)
intercomd print-config --config config/intercom.toml  # Dump effective config as JSON
intercomd inspect-legacy --sqlite store/messages.db   # Inspect legacy SQLite state
intercomd migrate-legacy --sqlite store/messages.db   # Migrate SQLite -> Postgres
intercomd verify-migration --sqlite store/messages.db # Compare counts for parity
```

## HTTP API

| Endpoint | Purpose |
|----------|---------|
| `GET /healthz` | Health check with uptime |
| `GET /readyz` | Readiness: runtime profiles, Postgres, Telegram, orchestrator status |
| `GET /v1/runtime/profiles` | List configured runtime profiles |
| `POST /v1/telegram/send` | Send message via Telegram Bot API (with chunking) |
| `POST /v1/telegram/edit` | Edit existing Telegram message |
| `POST /v1/commands` | Handle slash commands (/help, /status, /model, /reset) |
| `POST /v1/demarch/read` | Execute Demarch read operation (allowlisted `ic`/`bd` commands) |
| `POST /v1/demarch/write` | Execute Demarch write operation (main group only) |
| `POST /v1/db/*` | 24 Postgres persistence endpoints (chats, messages, tasks, sessions, groups) |

## Background Loops

When `serve` is running, these loops run concurrently (shutdown via `tokio::sync::watch`):

1. **Telegram poller** — `getUpdates` long-polling, message/command/callback/media parsing, direct Postgres write + queue enqueue.
2. **IPC watcher** — polls `data/ipc/{group}/` for messages, tasks, queries. Delegates messages/tasks via `TelegramDelegate`, handles Demarch queries natively.
3. **Event consumer** — polls `ic events tail --consumer=intercom`, sends push notifications for `gate.pending`, `run.completed`, `budget.exceeded`, `phase.changed`.
4. **Scheduler** (orchestrator) — polls for due tasks, spawns containers for scheduled prompts.
