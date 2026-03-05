# IronClaw (Rust Daemon)

## Crate Structure

| Crate | Purpose |
|-------|---------|
| `intercomd` | Axum HTTP daemon — Telegram bridge, IPC, events, orchestrator, container runner |
| `intercom-core` | Shared types: config, IPC, container protocol, Postgres persistence, Demarch adapter |
| `intercom-compat` | Legacy SQLite inspection and SQLite-to-Postgres migration |

## Configuration (`config/intercom.toml`)

TOML-based config with env var overrides (`INTERCOMD_BIND`, `INTERCOM_POSTGRES_DSN`, `HOST_CALLBACK_URL`). Key sections:

- `[server]` — bind address (default `127.0.0.1:7340`), host callback URL (default `http://127.0.0.1:7341`)
- `[storage]` — Postgres DSN, legacy SQLite path, groups dir
- `[runtimes]` — runtime profiles (claude/gemini/codex) with provider, default model, required env vars
- `[orchestrator]` — `enabled` flag, max concurrent containers, poll interval, idle timeout, `use_outbox` flag (enables outbox drain + LISTEN/NOTIFY, replaces message poll loop; requires `INTERCOM_POSTGRES_DSN` on both Node and Rust sides)
- `[scheduler]` — `enabled` flag, poll interval, IANA timezone for cron
- `[events]` — `enabled` flag, poll interval, notification JID for push notifications
- `[demarch]` — `enabled` flag, read/write allowlists for `ic`/`bd` CLI commands

## CLI Subcommands

```bash
intercomd serve --config config/intercom.toml     # Start HTTP service (default)
intercomd print-config --config config/intercom.toml  # Dump effective config as JSON
intercomd inspect-legacy --sqlite store/messages.db   # Inspect legacy SQLite state
intercomd migrate-legacy --sqlite store/messages.db   # Migrate SQLite → Postgres
intercomd verify-migration --sqlite store/messages.db # Compare counts for parity
```

## HTTP API

| Endpoint | Purpose |
|----------|---------|
| `GET /healthz` | Health check with uptime |
| `GET /readyz` | Readiness: runtime profiles, Postgres, Telegram, orchestrator status |
| `GET /v1/runtime/profiles` | List configured runtime profiles |
| `POST /v1/telegram/ingress` | Route inbound Telegram message (trigger check, group lookup) |
| `POST /v1/telegram/send` | Send message via Telegram Bot API (with chunking) |
| `POST /v1/telegram/edit` | Edit existing Telegram message |
| `POST /v1/commands` | Handle slash commands (/help, /status, /model, /reset) |
| `POST /v1/demarch/read` | Execute Demarch read operation (allowlisted `ic`/`bd` commands) |
| `POST /v1/demarch/write` | Execute Demarch write operation (main group only) |
| `POST /v1/db/*` | 24 Postgres persistence endpoints (chats, messages, tasks, sessions, groups) |
| `POST /v1/ipc/set-typing` | Set typing indicator on a chat JID (Node host callback) |

## Background Loops

When `serve` is running, these loops run concurrently (shutdown via `tokio::sync::watch`):

1. **IPC watcher** — polls `data/ipc/{group}/` for messages, tasks, queries. Delegates messages/tasks to Node via `HttpDelegate`, handles Demarch queries natively.
2. **Group registry sync** — periodically fetches registered groups from Node host callback.
3. **Event consumer** — polls `ic events tail --consumer=intercom`, sends push notifications for `gate.pending`, `run.completed`, `budget.exceeded`, `phase.changed`.
4. **Message loop** (orchestrator, legacy mode) — polls Postgres for pending messages, dispatches to group queue. Only active when `use_outbox=false`.
5. **Scheduler** (orchestrator) — polls for due tasks, spawns containers for scheduled prompts.
6. **Outbox drain** (orchestrator, outbox mode) — waits for LISTEN signal or 30s fallback, claims `message_outbox` rows in batches of 10, stores to destination tables, enqueues processing. Recovers stale `processing` rows every 5 minutes.
7. **LISTEN/NOTIFY loop** (orchestrator, outbox mode) — maintains a dedicated Postgres connection for `LISTEN intercom_outbox`. Signals the drain loop on notification. Auto-reconnects with exponential backoff (1s–30s).
8. **Outbox cleanup** (orchestrator, outbox mode) — runs hourly, deletes delivered outbox rows older than 7 days.
