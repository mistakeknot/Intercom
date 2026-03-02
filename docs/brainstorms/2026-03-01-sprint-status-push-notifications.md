# Sprint Status Push Notifications

**Bead:** iv-wjbex
**Date:** 2026-03-01
**Scope:** Full — /status enrichment + proactive push alerts

## Problem

Intercom users have no visibility into Intercore sprint state from Telegram. The `/status` command shows group-local info (model, session, container) but nothing about the active run: which phase it's in, whether gates are blocking, how much budget remains, or whether dispatches are active. When sprints stall (gate blocked, budget exceeded, phase stuck), there's no notification — the user has to manually check `ic run status`.

## Goal

1. **Enrich `/status`** with Intercore run data: current phase, gate status, dispatch count, token spend
2. **Add proactive push alerts** for sprint lifecycle events: gate blocks, cancellations, stale phases, budget warnings

## Current Infrastructure

| Component | Status | Notes |
|-----------|--------|-------|
| Event consumer (`events.rs`) | Exists | Polls `ic events tail`, formats notifications, sends via IpcDelegate |
| `/status` command (`commands.rs`) | Exists | Shows group info only, no Intercore awareness |
| DemarchAdapter read ops | Exists | `RunStatus`, `SprintPhase`, `RunEvents` already available |
| Telegram inline buttons | Exists | Used by gate.pending and budget.exceeded handlers |
| IpcDelegate | Exists | HttpDelegate forwards to Node host |

## Design

### Part 1: /status Enrichment

**Approach:** In `handle_status_command()` (commands.rs), call `DemarchAdapter::execute_read(RunStatus { run_id: None })` to get the current active run. Parse the JSON response and append run info to the status message.

**Data to show:**
- Active run ID and goal (from `ic run current --json`)
- Current phase (from run status)
- Token budget: spent / total (percentage) — from `ic run tokens <id>` and `run.token_budget`
- Active dispatches count — from `ic dispatch list --active --json`
- Pending gates — from `ic gate check <run_id>` (exit code 1 = gate pending)

**Format:**
```
📊 Status
━━━━━━━━━━━━━━━━━━━━━━
Group: Main
Model: claude-opus-4-6
Session: abc12345...
Container: active

📋 Active Run: mq1dvunt
  Goal: Implement safety floors
  Phase: executing (5/9)
  Budget: 45k / 250k (18%)
  Dispatches: 2 active
  Gates: none pending
━━━━━━━━━━━━━━━━━━━━━━
```

**When no active run:** Just show "No active run" after the group info.

**Implementation:** commands.rs `handle_status_command()` already has access to `DemarchAdapter` via the `CommandContext`. Add a `read_run_status()` helper that fetches and formats the run info block. This is a read-only operation — no write permissions needed.

### Part 2: Proactive Push Alerts

**Approach:** Extend the event consumer's `format_notification()` to handle additional event types already emitted by Intercore.

**New event handlers:**

| Event Kind | Trigger | Notification | Buttons |
|------------|---------|-------------|---------|
| `phase.block` / `block` | Gate blocks phase advance | "🚫 Sprint stalled: gate blocked at {from}→{to}\n\nReason: {condition details}" | Approve / Reject / Defer (reuse gate buttons) |
| `run.cancel` / `cancel` | Run cancelled | "🛑 Run {run_id} cancelled" | None |

**Synthetic alerts (timer-based):**

| Alert | Trigger | Notification |
|-------|---------|-------------|
| Stale phase | Run stuck in same phase >2 hours | "⚠️ Run {run_id} has been in phase '{phase}' for {duration}. May need attention." |
| Budget warning | Token spend > budget_warn_pct (default 80%) | Already handled by `budget.exceeded` event — no additional work needed |

**Stale phase detection:** The event consumer polls every `poll_interval_ms`. Add a `last_phase_change` timestamp tracker. On each poll, if `now - last_phase_change > stale_threshold` and there's an active run, emit a stale warning. Only alert once per stale window (use a `stale_alerted` flag, reset on phase change).

**Implementation detail:** The `phase.block` event type is already emitted by Intercore (I see it in the events output). The event consumer's `format_notification()` just needs new match arms. The stale detection is the only truly new logic — it runs inside the existing poll loop.

### New ReadOperations Needed

**For /status enrichment:**
1. `RunTokens { run_id: String }` → `ic run tokens <id> --json` — token aggregation
2. `DispatchList { active_only: bool }` → `ic dispatch list --active --json` — active dispatch count

These need to be added to the `ReadOperation` enum, `plan_read()`, and the read allowlist.

### Configuration

Add to `[events]` in intercom.toml:
```toml
# Stale phase detection threshold (seconds). 0 = disabled.
stale_phase_threshold_secs = 7200  # 2 hours
```

No new config sections needed — everything fits within the existing `[events]` config.

## Files to Modify

| File | Change |
|------|--------|
| `rust/intercom-core/src/demarch.rs` | Add `RunTokens`, `DispatchList` to ReadOperation + plan_read() |
| `rust/intercom-core/src/config.rs` | Add `stale_phase_threshold_secs` to EventsConfig, add read allowlist entries |
| `rust/intercomd/src/commands.rs` | Enrich `/status` with Intercore run data |
| `rust/intercomd/src/events.rs` | Add `block`/`cancel` event handlers, stale phase detection |
| `config/intercom.toml.example` | Add new config entries |

## Risks

- **ic CLI availability:** `/status` enrichment calls `ic` which may not be installed. The DemarchAdapter already handles this (returns "standalone mode" error). We should degrade gracefully: show group info + "Intercore unavailable" instead of failing the whole command.
- **Stale alerts for paused runs:** A run might be intentionally paused. The `auto_advance=false` flag on the run could be checked to suppress stale alerts.
- **Event consumer not enabled:** If `events.enabled=false`, push alerts won't fire. This is expected — the feature is opt-in.

## Not In Scope

- Web dashboard or HTTP status endpoint (Telegram-first for now)
- Historical status tracking / status log table
- Multi-run status (show only the most recent active run)
- Budget extension via /status (already handled by budget.exceeded buttons)
