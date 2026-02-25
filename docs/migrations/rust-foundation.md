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
- `GET /readyz` — readiness with profile count, feature flags
- `GET /v1/runtime/profiles` — configured runtime profiles
- `POST /v1/demarch/read` — Demarch kernel read operations
- `POST /v1/demarch/write` — Demarch kernel write operations (main-group gated)
- `POST /v1/telegram/ingress` — route incoming Telegram messages
- `POST /v1/telegram/send` — send Telegram message via Bot API
- `POST /v1/telegram/edit` — edit Telegram message via Bot API

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

## Phase 3 — Container runner + persistence (planned)

See `rust-phase3-plan.md` for the detailed implementation plan.
