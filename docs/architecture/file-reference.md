# File Reference

## Host (`src/`)

| File | Purpose |
|------|---------|
| `index.ts` | Orchestrator: message loop, state management, agent dispatch |
| `config.ts` | Runtime selection, trigger pattern, paths, engine toggle |
| `types.ts` | RegisteredGroup, Channel, NewMessage, ScheduledTask interfaces |
| `container-runner.ts` | Container spawning, volume mounts, output streaming |
| `container-runtime.ts` | Docker/Podman detection, orphan cleanup (with CLEANUP_EXCLUDE for infra containers) |
| `mount-security.ts` | Allowlist-based mount validation |
| `group-queue.ts` | Per-group message queue with global concurrency limit |
| `channels/telegram.ts` | Telegram Bot API via Grammy |
| `channels/whatsapp.ts` | WhatsApp Web via Baileys |
| `host-callback.ts` | HTTP callback server for intercomd delegation (incl. `/v1/ipc/set-typing`) |
| `intercomd-client.ts` | Client for intercomd bridge endpoints |
| `query-handlers.ts` | Demarch CLI query handlers (`ic`/`bd` via execFileSync) |
| `stream-accumulator.ts` | Real-time Telegram message editing with tool call streaming |
| `summarizer.ts` | Conversation summary caching (GPT-5.3 Codex) |
| `ipc.ts` | IPC watcher: messages, tasks, group registration |
| `router.ts` | Message formatting, channel selection, outbound routing |
| `task-scheduler.ts` | Cron/interval/once task scheduling and execution |
| `pg-writer.ts` | Direct Postgres outbox writer (INSERT into `message_outbox`, pg.Pool max 1 conn) |
| `db.ts` | SQLite: messages, groups, sessions, state, tasks |

## Rust (`rust/`)

| File | Purpose |
|------|---------|
| `intercomd/src/main.rs` | Axum server, CLI, route wiring, shutdown coordination |
| `intercomd/src/telegram.rs` | Telegram bridge (ingress routing, send with chunking, edit, typing indicator) |
| `intercomd/src/ipc.rs` | IPC watcher, IpcDelegate trait, HttpDelegate (incl. set_typing), group registry |
| `intercomd/src/events.rs` | Kernel event consumer (gate, run, budget, phase notifications) |
| `intercomd/src/commands.rs` | Slash commands (/help, /status, /model, /reset) with model catalog |
| `intercomd/src/db.rs` | Postgres route handlers (24 endpoints) |
| `intercomd/src/queue.rs` | Group queue with concurrency limiting |
| `intercomd/src/message_loop.rs` | Message poll loop (orchestrator, legacy — replaced by outbox when `use_outbox=true`) |
| `intercomd/src/outbox.rs` | Outbox drain loop, LISTEN/NOTIFY loop, outbox cleanup loop |
| `intercomd/src/process_group.rs` | Container dispatch per group (with typing indicator lifecycle) |
| `intercomd/src/scheduler.rs` | Task scheduler loop |
| `intercomd/src/scheduler_wiring.rs` | Scheduler callback wiring |
| `intercomd/src/container/runner.rs` | Async container spawning with OUTPUT marker streaming, two-phase timeout watchdog |
| `intercomd/src/container/mounts.rs` | Volume mount builder |
| `intercomd/src/container/secrets.rs` | Secret injection into containers |
| `intercomd/src/container/security.rs` | Mount allowlist validation |
| `intercom-core/src/config.rs` | TOML config with env overrides |
| `intercom-core/src/persistence.rs` | Postgres persistence (tokio-postgres) |
| `intercom-core/src/demarch.rs` | Demarch kernel adapter (ic/bd CLI execution) |
| `intercom-core/src/ipc.rs` | IPC types (IpcMessage, IpcTask, IpcQuery) |
| `intercom-core/src/container.rs` | Container protocol types and helpers |
| `intercom-compat/src/lib.rs` | SQLite inspection, migration, parity verification |

## Container (`container/`)

| File | Purpose |
|------|---------|
| `Dockerfile` / `Dockerfile.gemini` / `Dockerfile.codex` | Runtime images |
| `build.sh` | Multi-runtime build script |
| `agent-runner/src/index.ts` | Claude agent loop (Agent SDK) |
| `gemini-runner/src/index.ts` | Gemini agent loop (Code Assist API) |
| `codex-runner/src/index.ts` | Codex agent loop (codex exec CLI) |
| `shared/protocol.ts` | ContainerInput/Output types, OUTPUT markers |
| `shared/executor.ts` | Tool execution: shell, file, grep, glob |
| `shared/ipc-tools.ts` | IPC tools: send_message, schedule_task, register_group |
| `shared/ipc-input.ts` | IPC polling: drainIpcInput(), waitForIpcMessage() |
| `shared/session-base.ts` | Conversation archival (markdown transcripts) |
| `shared/system-prompt.ts` | System prompt builder |
