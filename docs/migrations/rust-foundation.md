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

# Start health endpoints
./rust/target/debug/intercomd serve --config config/intercom.toml
```

## Current HTTP endpoints

- `GET /healthz`
- `GET /readyz`
- `GET /v1/runtime/profiles`

These are scaffolding endpoints for deployment checks and migration wiring.

## Compatibility guarantees in this phase

- No replacement of Node service entrypoint.
- No destructive schema changes.
- No required Postgres dependency to continue running existing Intercom.

## Next phase focus

- Add real Demarch read/write adapters in Rust with command policy enforcement.
- Implement SQLite -> Postgres migrator with idempotent checkpoints.
- Add Telegram production path into Rust runtime while preserving runtime IDs (`claude`, `gemini`, `codex`).
