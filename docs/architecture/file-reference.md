# File Reference

## Rust (`rust/`)

| File | Purpose |
|------|---------|
| `intercomd/src/main.rs` | Axum server, CLI, route wiring, shutdown coordination |
| `intercomd/src/telegram_poller.rs` | Telegram `getUpdates` polling, message/media/command/callback parsing |
| `intercomd/src/telegram.rs` | Telegram bridge (send with chunking, edit, typing indicator, buttons) |
| `intercomd/src/ipc.rs` | IPC watcher, IpcDelegate trait, TelegramDelegate, group registry |
| `intercomd/src/events.rs` | Kernel event consumer (gate, run, budget, phase notifications) |
| `intercomd/src/commands.rs` | Slash commands (/help, /status, /model, /reset) with model catalog |
| `intercomd/src/db.rs` | Postgres route handlers (24 endpoints) |
| `intercomd/src/queue.rs` | Group queue with concurrency limiting |
| `intercomd/src/process_group.rs` | Container dispatch per group (with typing indicator lifecycle) |
| `intercomd/src/scheduler.rs` | Task scheduler loop |
| `intercomd/src/scheduler_wiring.rs` | Scheduler callback wiring |
| `intercomd/src/container/runner.rs` | Async container spawning with UDS output, two-phase timeout watchdog |
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

## Archived: Node Host (`src/`)

The Node host process is archived — kept for potential WhatsApp (Baileys) revival but not running in production. See `src/` for the original source. Key files were: `index.ts` (orchestrator), `channels/telegram.ts` (grammY), `host-callback.ts` (HTTP bridge), `db.ts` (SQLite), `pg-writer.ts` (outbox writer).
