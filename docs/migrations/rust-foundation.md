# Intercom Rust Foundation (Phase 1)

This document tracks the first executable migration slice for replatforming Intercom from NanoClaw internals to an IronClaw-based foundation.

## What this phase adds

- A Rust workspace at `rust/` with three crates:
  - `intercomd`: daemon/service skeleton
  - `intercom-core`: shared config + runtime/domain types
  - `intercom-compat`: legacy Node/SQLite inspection helpers
- `config/intercom.toml.example` for Rust daemon configuration.
- Optional setup integration to compile Rust during service setup when Cargo is available.

The existing TypeScript runtime (`dist/index.js`) remains the default service runtime in this phase.

## Commands

From `apps/intercom`:

```bash
npm run rust:check
npm run rust:build
npm run rust:test
```

Or directly:

```bash
cd rust
cargo check --workspace
cargo build --workspace --release
cargo test --workspace
```

## intercomd commands

```bash
# Print effective config (loads config/intercom.toml if present)
./rust/target/debug/intercomd print-config --config config/intercom.toml

# Inspect legacy SQLite + project layout before migration
./rust/target/debug/intercomd inspect-legacy --sqlite store/messages.db --project-root .

# Dry-run migration plan (no Postgres writes)
./rust/target/debug/intercomd migrate-legacy --sqlite store/messages.db --dry-run

# Apply migration with checkpointing
./rust/target/debug/intercomd migrate-legacy \
  --sqlite store/messages.db \
  --postgres-dsn postgres://intercom:intercom@localhost:5432/intercom \
  --checkpoint sqlite_to_postgres_v1

# Verify source/target count parity
./rust/target/debug/intercomd verify-migration \
  --sqlite store/messages.db \
  --postgres-dsn postgres://intercom:intercom@localhost:5432/intercom

# Start health endpoints
./rust/target/debug/intercomd serve --config config/intercom.toml
```

## Current HTTP endpoints

- `GET /healthz`
- `GET /readyz`
- `GET /v1/runtime/profiles`
- `POST /v1/demarch/read`
- `POST /v1/demarch/write`
- `POST /v1/telegram/ingress`
- `POST /v1/telegram/send`
- `POST /v1/telegram/edit`

These are scaffolding endpoints for deployment checks and migration wiring.

## IPC watcher

The `serve` command also starts a filesystem IPC watcher that polls `data/ipc/` for
container-originated messages, tasks, and Demarch kernel queries. This is the Rust
equivalent of `src/ipc.ts` + `src/query-handlers.ts` in the Node host.

```
data/ipc/
├── main/
│   ├── messages/       → outbound chat messages (container → Telegram)
│   ├── tasks/          → task management (schedule, pause, resume, cancel)
│   ├── queries/        → Demarch kernel queries ({uuid}.json)
│   └── responses/      → query responses ({uuid}.json, written by intercomd)
├── team-eng/
│   └── ...             → same structure, per-group authorization
└── errors/             → malformed files moved here for debugging
```

Query types supported: `run_status`, `sprint_phase`, `search_beads`, `spec_lookup`,
`review_summary`, `next_work`, `run_events`, `create_issue`, `update_issue`,
`close_issue`, `start_run`, `approve_gate`.

When `INTERCOM_ENGINE=rust`, the Node Telegram channel can proxy ingress/egress
through these endpoints, with automatic fallback to the existing Node channel path
if `intercomd` is unavailable.

Demarch write operations currently implemented in Rust:

- `create_issue`
- `update_issue`
- `close_issue`
- `start_run`
- `approve_gate`

## Compatibility guarantees in this phase

- No replacement of Node service entrypoint.
- No destructive schema changes.
- No required Postgres dependency to continue running existing Intercom.

## Completed in Phase 1

- Demarch read/write adapters in Rust with allowlist-based command policy enforcement.
- SQLite → Postgres migrator with idempotent checkpoints, dry-run, and parity verification.
- Telegram ingress/egress bridge through intercomd with chunking and trigger matching.
- Filesystem IPC watcher — polls `data/ipc/{group}/` for messages, tasks, and Demarch queries.
  - Queries dispatched to DemarchAdapter (reads + writes with main-group authorization).
  - Messages authorized per-group (main can send anywhere, others restricted).
  - Tasks forwarded via IpcDelegate trait (currently LogOnlyDelegate; wired to Node host next).
  - Bad JSON moved to `errors/` directory for debugging.
  - Atomic response writes (tmp + rename) matching Node.js behavior.

## Next phase focus

- Wire IpcDelegate to Node host via HTTP (intercomd → Node bridge for message sending + task management).
- Add event consumer loop (`ic events tail --consumer=intercom`) for push notifications.
- Implement registered-groups state in Rust (currently placeholder — non-main authorization defaults to reject).
- Add systemd unit for running intercomd alongside Node host.
