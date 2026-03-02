# Plan: Sprint Status Push Notifications

**Bead:** iv-wjbex
**PRD:** docs/prds/2026-03-01-sprint-status-push-notifications.md

## Tasks

### 1. Add ReadOperation variants for run tokens and dispatch list
- [x] Add `RunTokens { run_id: String }` to `ReadOperation` enum in `demarch.rs`
- [x] Add `DispatchList { active_only: bool }` to `ReadOperation` enum
- [x] Implement `plan_read()` for both: `ic run tokens <id> --json`, `ic dispatch list --active --json`
- [x] Add signatures to `read_allowlist` default in `config.rs`
- [x] Add signatures to `intercom.toml.example`

**Files:** `rust/intercom-core/src/demarch.rs`, `rust/intercom-core/src/config.rs`, `config/intercom.toml.example`
**Pattern:** Follow existing `RunStatus` / `RunEvents` plan_read implementations

### 2. Enrich /status command with Intercore run data
- [x] Add `read_run_status()` helper to `commands.rs` that calls `DemarchAdapter::execute_read(RunStatus { run_id: None })`
- [x] Parse JSON response to extract: run_id, goal, phase, phases array, token_budget
- [x] Call `RunTokens` to get token spend
- [x] Call `DispatchList` to get active dispatch count
- [x] Format and append run info block to status output
- [x] Degrade gracefully: if DemarchAdapter returns error, show "Intercore: unavailable"
- [x] Add phase progress indicator: "phase (N/total)"

**Files:** `rust/intercomd/src/commands.rs`
**Pattern:** Follow existing `handle_status_command()` structure, add to the response string builder

### 3. Add sprint event alert handlers
- [x] Add `block` match arm in `format_notification()` — parse `from_state`, `to_state`, `reason` from event
- [x] For block events with gate conditions in reason, extract gate_id and show gate approval buttons
- [x] Add `cancel` match arm — format "Run cancelled" notification
- [x] Test with existing event JSON structure (source=phase, type=block/cancel)

**Files:** `rust/intercomd/src/events.rs`
**Pattern:** Follow existing `gate.pending` handler format

### 4. Add stale phase detection
- [x] Add `stale_phase_threshold_secs` to `EventsConfig` (default: 7200)
- [x] Add `last_phase_change: Instant` and `stale_alerted: bool` fields to `EventConsumer`
- [x] On each poll, after processing events: check if any event was a phase advance → reset timer
- [x] If `now - last_phase_change > threshold` and not already alerted: send stale notification
- [x] Check active run status — suppress stale alert if no active run
- [x] Reset `stale_alerted` on phase change
- [x] Add config entry to `intercom.toml.example`

**Files:** `rust/intercomd/src/events.rs`, `rust/intercom-core/src/config.rs`, `config/intercom.toml.example`

### 5. Tests
- [x] Test `plan_read()` for `RunTokens` and `DispatchList` in demarch.rs tests
- [x] Test `format_notification()` for `block` and `cancel` events in events.rs tests
- [x] Test stale phase detection logic (timer reset, alert-once behavior)
- [x] Run full test suite (`cargo test`) — 155 tests pass

**Files:** `rust/intercom-core/src/demarch.rs`, `rust/intercomd/src/events.rs`
