# Intercom IronClaw Replatform

Migration tracker for replatforming Intercom from NanoClaw (Node.js/SQLite) to IronClaw (Rust/Postgres).

## Architecture

intercomd runs as a systemd user service alongside the Node host. Current model is **sidecar**: Rust handles IPC polling, Demarch queries, Telegram API, and event notifications. Node handles container lifecycle, task scheduling, message routing, and persistence.

```
                    ┌────────────────────────────────────────┐
                    │          intercomd (Rust :7340)         │
                    │  IPC watcher · Demarch · Telegram API  │
                    │  Event consumer · Registry sync         │
                    └──────────┬──────────┬──────────────────┘
                               │ HTTP     │ HTTP
                    ┌──────────▼──────────▼──────────────────┐
                    │     Node host callback (:7341)          │
                    │  sendMessage · forwardTask · groups     │
                    └──────────┬──────────────────────────────┘
                               │
                    ┌──────────▼──────────────────────────────┐
                    │     Node orchestrator (index.ts)         │
                    │  Containers · Scheduler · Queue · DB    │
                    │  WhatsApp (Baileys) · Telegram (Grammy) │
                    └─────────────────────────────────────────┘
```

## Workspace

Three crates under `rust/`:

- `intercomd` — daemon binary (serve, print-config, inspect-legacy, migrate-legacy, verify-migration)
- `intercom-core` — shared types: config, demarch adapter, IPC types, runtime profiles
- `intercom-compat` — SQLite→Postgres migration helpers

## Commands

```bash
# From apps/intercom
npm run rust:check
npm run rust:build
npm run rust:test

# Direct
cd rust && cargo check --workspace
cd rust && cargo build --workspace --release
cd rust && cargo test --workspace

# Service management
systemctl --user start intercomd
systemctl --user status intercomd
journalctl --user -u intercomd -f
```

## HTTP endpoints

- `GET /healthz` — health check with uptime
- `GET /readyz` — readiness with profile count, feature flags, postgres status
- `GET /v1/runtime/profiles` — configured runtime profiles
- `POST /v1/demarch/read` — Demarch kernel read operations
- `POST /v1/demarch/write` — Demarch kernel write operations (main-group gated)
- `POST /v1/telegram/ingress` — route incoming Telegram messages
- `POST /v1/telegram/send` — send Telegram message via Bot API
- `POST /v1/telegram/edit` — edit Telegram message via Bot API
- `POST /v1/db/*` — 25 Postgres persistence endpoints (chats, messages, tasks, sessions, groups, router state)
- `POST /v1/commands` — slash command handler (help, status, model, reset/new)

## IPC watcher

Polls `data/ipc/{group}/` for container-originated messages, tasks, and Demarch queries.

```
data/ipc/
├── main/
│   ├── messages/       → outbound chat messages (container → host)
│   ├── tasks/          → task management (schedule, pause, resume, cancel)
│   ├── queries/        → Demarch kernel queries ({uuid}.json)
│   ├── responses/      → query responses ({uuid}.json, written by intercomd)
│   └── input/          → follow-up messages piped to active container
├── team-eng/
│   └── ...             → same structure, per-group authorization
└── errors/             → malformed files moved here for debugging
```

Query types: `run_status`, `sprint_phase`, `search_beads`, `spec_lookup`,
`review_summary`, `next_work`, `run_events`, `create_issue`, `update_issue`,
`close_issue`, `start_run`, `approve_gate`.

## Completed — Phase 1 (Foundation)

- Rust workspace scaffolding with three crates.
- `config/intercom.toml.example` for daemon configuration.
- Demarch read/write adapters with allowlist-based command policy enforcement.
- SQLite → Postgres migrator with idempotent checkpoints, dry-run, and parity verification.
- Telegram ingress/egress bridge with chunking, trigger matching, and group lookup.
- IPC watcher with atomic response writes and error quarantine.

## Completed — Phase 2 (Sidecar wiring)

- `HttpDelegate`: IPC messages and tasks forwarded to Node host via HTTP callback bridge.
- Node-side `host-callback.ts` server on port 7341 (send-message, forward-task, registered-groups).
- Event consumer loop: polls `ic events tail --consumer=intercom` for gate, run, budget, phase events.
- `GroupRegistry` with `sync_registry_loop`: thread-safe registry synced from Node every 10s.
- systemd user service (`config/intercomd.service`) with `Before=intercom.service` ordering.

## Compatibility guarantees (current)

- Node service remains the primary entrypoint.
- No destructive schema changes.
- No required Postgres dependency for existing Intercom functionality.
- intercomd can be stopped without affecting core message flow (Node IPC watcher is still active as fallback).

## Completed — Phase 3a (Postgres persistence)

- `persistence.rs` in `intercom-core`: PgPool, live schema (TIMESTAMPTZ, BOOLEAN, SERIAL, JSONB), all CRUD functions from db.ts.
- `db.rs` in `intercomd`: 25 POST endpoints under `/v1/db/` for Node dual-write during migration.
- Optional Postgres: graceful degradation when DSN unconfigured (503 on DB endpoints).

## Completed — Phase 3b (Container runner)

- `container/` module in `intercomd` with 4 submodules: security, secrets, mounts, runner.
- Full port of `container-runner.ts`, `mount-security.ts`, and env/secrets handling.
- `container.rs` in `intercom-core`: shared protocol types, OUTPUT marker parser.
- 18 unit tests for protocol types, mount security, secrets parsing, runner helpers.

## Completed — Phase 3c (Task scheduler)

- `scheduler.rs` in `intercomd`: `calculate_next_run()` with cron (chrono-tz), interval (ms offset), once support.
- `result_summary()` for task run result formatting (truncation, error prefix).
- `run_scheduler_loop()` async poll loop with `tokio::select!` for graceful shutdown.
- 10 unit tests.

## Completed — Phase 3d (Group queue)

- `queue.rs` in `intercomd`: `GroupQueue` with `Arc<Mutex<Inner>>` for thread-safe state.
- Per-group serialization, global concurrency cap, task priority over messages.
- IPC follow-up message piping, exponential retry backoff, close sentinel for container preemption.
- Graceful shutdown with container detachment. 6 unit tests.

## Completed — Phase 3e (Slash commands)

- `commands.rs` in `intercomd`: model catalog (5 entries), `resolve_model()` with exact/number/substring/prefix inference.
- `handle_command()` dispatcher for help, status, model, reset/new.
- `POST /v1/commands` HTTP endpoint wired to `AppState`.
- 16 unit tests.

## Phase 4 — Orchestrator wiring (complete)

Phase 4 connected the independently-built Phase 3 modules into a working orchestrator inside `intercomd`. Behind `orchestrator.enabled` feature flag.

### 4a. Shared orchestrator state
- `AppState` gains `queue: Arc<GroupQueue>`, `groups: Arc<RwLock<Groups>>`, `sessions: Arc<RwLock<Sessions>>`
- `OrchestratorConfig` and `SchedulerConfig` in `intercom-core/config.rs`
- Groups and sessions loaded from Postgres on startup with graceful degradation
- `readyz` endpoint reports `orchestrator_enabled`, `registered_groups`, `active_containers`

### 4b. Message loop
- `message_loop.rs` — port of `startMessageLoop()` from Node
- Dual-cursor design: global `last_timestamp` + per-group `last_agent_timestamp`
- Polls PgPool for new messages, groups by JID, checks trigger patterns
- Pipes follow-up messages to active containers; enqueues new groups into GroupQueue
- Startup recovery re-enqueues groups with unprocessed messages

### 4c. processGroupMessages callback
- `process_group.rs` — port of `processGroupMessages()` + `runAgent()`
- `build_process_messages_fn()` creates `ProcessMessagesFn` closure for GroupQueue
- Full pipeline: fetch pending → check trigger → format prompt → spawn container → stream output → send via Telegram → store bot responses → manage cursor
- Cursor rollback on error (unless output already sent to user)
- Strips `<internal>...</internal>` blocks from agent output

### 4d. Scheduler wiring
- `scheduler_wiring.rs` — `build_task_callback()` produces `TaskCallback` for scheduler loop
- Dispatches `DueTask` → `queue.enqueue_task()` → `run_container_agent()` → Telegram output
- Logs task runs and advances next_run (cron/interval/once) via PgPool
- Context mode support: `group` shares session, `isolated` gets fresh session

### 4e. Command side effects
- `CommandEffect` enum: `KillContainer`, `ClearSession`, `SwitchModel`
- `/reset` emits `KillContainer` (if active) + `ClearSession`
- `/model` switch emits `KillContainer` + `ClearSession` + `SwitchModel`
- `apply_command_effects()` applies effects via queue, sessions, and Postgres
- Intent-based pattern keeps command handlers pure and testable

### serve() spawn tree (after Phase 4)
```
serve() spawns:
  ├── IPC watcher (polls data/ipc/)
  ├── Group registry sync (fetches from Node)
  ├── Event consumer (polls ic events)
  ├── Message loop (polls PgPool → enqueues into GroupQueue)
  ├── Scheduler loop (polls PgPool → enqueues tasks into GroupQueue)
  └── GroupQueue → dequeues → run_container_agent() → Telegram → Postgres
```

- 129 unit tests + 5 integration tests = 134 total across workspace

## Phase 5 — Integration testing + flag flip (complete)

### Config updates
- `config/intercom.toml.example` now includes `[orchestrator]` and `[scheduler]` sections with all fields documented
- Both default to `enabled = false` (safe default)

### Integration test harness
- `rust/intercomd/tests/integration_smoke.rs` — 5 tests that spawn the real binary on a random port
- Tests: healthz, readyz (orchestrator flag), command reset effects, model switch effects, runtime profiles
- Uses `reqwest::blocking` for synchronous HTTP against the spawned process
- `TestServer` struct manages lifecycle with SIGTERM on drop

### Node orchestrator toggle
- `RUST_ORCHESTRATOR=true` env var disables Node's `startMessageLoop()`, `startSchedulerLoop()`, and `queue.setProcessMessagesFn()`
- Node continues handling WhatsApp channel, IPC watcher, and host-callback server
- IPC watcher, group registry sync, and host-callback remain active regardless of toggle

### Deployment procedure

**Pre-flight:**
1. Verify intercomd is running: `systemctl --user status intercomd`
2. Check `/readyz`: `curl -s http://localhost:7340/readyz | jq`
3. Confirm `postgres_connected: true` and `registered_groups > 0`

**Enable Rust orchestrator:**

Order matters — disable Node loops first to prevent dual-dispatch duplicates.

1. Add `Environment=RUST_ORCHESTRATOR=true` to `intercom.service` override
2. Restart Node first: `systemctl --user restart intercom`
   (Node now runs without message loop/scheduler — safe, intercomd still in sidecar mode)
3. Add to `config/intercom.toml`:
   ```toml
   [orchestrator]
   enabled = true
   max_concurrent_containers = 3
   poll_interval_ms = 1000
   idle_timeout_ms = 300000
   main_group_folder = "main"

   [scheduler]
   enabled = true
   poll_interval_ms = 10000
   timezone = "UTC"
   ```
4. Restart intercomd: `systemctl --user restart intercomd`
   (Rust orchestrator takes over message dispatch)
5. Verify: send a test Telegram message, check `journalctl --user -u intercomd -f` for message loop activity

**Rollback (before Phase 6b):**
1. Remove `Environment=RUST_ORCHESTRATOR=true` from intercom.service
2. Set `orchestrator.enabled = false` in `config/intercom.toml`
3. Restart both: `systemctl --user restart intercomd intercom`

**Rollback (after Phase 6b):** Config rollback no longer works — Node orchestrator code is removed. Use `git revert` to the `v-pre-6b` tag, then `npm run build && systemctl --user restart intercom`.

## Phase 6b — Node orchestrator code removal (complete)

Removed dead Node orchestrator code after Rust cutover. The `RUST_ORCHESTRATOR` flag and its conditional blocks are gone — Rust always handles orchestration.

### Removed
- `processGroupMessages()` (~200 lines) — Rust `process_group.rs` replaces this
- `runAgent()` (~83 lines) — Rust handles container dispatch
- `startMessageLoop()` (~100 lines) — Rust `message_loop.rs` replaces this
- `recoverPendingMessages()` (~12 lines) — Rust `recover_pending_messages()` replaces this
- `loadState()` / `saveState()` cursor management — Rust holds cursors in `Arc<RwLock<AgentTimestamps>>`
- `src/task-scheduler.ts` (249 lines) — deleted entirely, Rust `scheduler_wiring.rs` replaces
- `src/task-scheduler.test.ts` — deleted with the module
- `RUST_ORCHESTRATOR` flag from `config.ts` and `readEnvFile` list
- `POLL_INTERVAL` and `SCHEDULER_POLL_INTERVAL` constants from `config.ts`
- Summarizer integration from `handleModel` / `handleReset` (context carryover on model switch — future Rust feature)
- `pendingModelSwitch` and `lastAgentTimestamp` state variables

### Kept intact
- `GroupQueue` (`src/group-queue.ts`) — container lifecycle management
- `host-callback.ts` — intercomd delegates container spawning to Node
- `container-runner.ts` — spawns containers
- `ipc.ts` — container→host IPC watcher
- Command handlers (`handleHelp`, `handleStatus`, `handleModel`, `handleReset`)
- Channel code (WhatsApp, Telegram)
- `router.ts` — message formatting for channels

### Line count
- `src/index.ts`: 919 → 423 lines (−496)
- `src/task-scheduler.ts`: deleted (−249)
- `src/config.ts`: 172 → 163 lines (−9)

### Rollback tag
`v-pre-6b` — tagged at trunk before code removal. Rollback: `git revert`, `npm run build`, restart.
