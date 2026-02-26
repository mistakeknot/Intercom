# IronClaw Phase 5 — Integration Testing + Flag Flip

**Bead:** iv-u0gmm
**Goal:** Validate the Phase 4 orchestrator end-to-end, then enable it in production.

## Context

Phase 4 wired scheduler, message loop, queue, and commands into `serve()` behind `orchestrator.enabled`. All 129 unit tests pass, but nothing has been tested with real containers or live Postgres. The example config is also missing the `[orchestrator]` and `[scheduler]` sections.

## Tasks

### 5a. Update config example with orchestrator/scheduler sections
**Files:** `config/intercom.toml.example`
- Add `[orchestrator]` section with all fields and comments
- Add `[scheduler]` section with all fields and comments
- Both default to `enabled = false` (safe default)
- ~20 lines

### 5b. Integration test harness
**Files:** `rust/intercomd/tests/integration_orchestrator.rs`
- Create `tests/` directory for integration tests (Rust convention: `tests/` at crate root)
- Test 1: `serve_starts_with_orchestrator_disabled` — boot intercomd without Postgres, verify `/readyz` returns `orchestrator_enabled: false`
- Test 2: `healthz_and_readyz_endpoints` — verify both endpoints return valid JSON with expected fields
- Test 3: `command_effects_reset` — POST to `/v1/commands` with reset, verify response has `ClearSession` effect
- Test 4: `command_effects_model_switch` — POST to `/v1/commands` with model switch, verify response has `SwitchModel` effect
- Uses `axum::test` (or `reqwest` against a spawned server on a random port) — no real Postgres needed for these
- ~120 lines

### 5c. Enable orchestrator in production config
**Files:** `config/intercom.toml` (gitignored, must be edited on server)
- Add `[orchestrator]` section with `enabled = true`
- Add `[scheduler]` section with `enabled = true`
- Set `main_group_folder = "main"`
- This is a manual step documented in a checklist, not a code change

### 5d. Disable Node orchestrator when Rust is enabled
**Files:** `src/index.ts`
- Add env var check: `RUST_ORCHESTRATOR=true` skips `startMessageLoop()` and `startSchedulerLoop()`
- Node continues handling WhatsApp channel and host-callback
- Allows dual-run testing: set env var to toggle which orchestrator is active
- ~10 lines

### 5e. Deployment checklist
**Files:** `docs/migrations/rust-foundation.md` (update)
- Document the flag flip procedure
- Pre-flight: verify Postgres connected, groups loaded, sessions loaded (check `/readyz`)
- Flip: add `[orchestrator] enabled = true` to production config, restart intercomd
- Verify: send a test message to Telegram, check `journalctl --user -u intercomd -f` for message loop activity
- Rollback: set `orchestrator.enabled = false`, restart intercomd, unset `RUST_ORCHESTRATOR` in Node service

## Execution Order

5a → 5b → 5d → 5e (code changes)
5c is manual (production server, after deploy)

## Risk

- **Low**: Config and test changes are purely additive
- **Medium**: 5d modifies Node orchestrator startup — bad env var check could break Node
- **Mitigation**: Node only skips loops when `RUST_ORCHESTRATOR=true` is explicitly set; default behavior unchanged
