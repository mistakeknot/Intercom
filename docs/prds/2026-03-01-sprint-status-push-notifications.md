# PRD: Sprint Status Push Notifications

**Bead:** iv-wjbex
**Priority:** P2
**Complexity:** 3/5 (moderate)

## Problem Statement

Intercom users have zero Intercore visibility from Telegram. When sprints stall (gate blocked, run stuck), there's no notification — users discover problems minutes to hours late by manually running `ic run status`.

## Features

### F1: /status Intercore Enrichment
Extend the existing `/status` Telegram command to show active run state: phase, budget, dispatches, gates.

**Acceptance criteria:**
- `/status` shows active run info when an Intercore run exists
- Gracefully degrades when `ic` CLI is unavailable or no run is active
- Shows phase progress (e.g., "executing (5/9)")
- Shows token budget usage as percentage

### F2: Sprint Event Alerts
Add `phase.block` and `run.cancel` event handling to the event consumer for proactive push notifications.

**Acceptance criteria:**
- `block` events produce "Sprint stalled" notification with gate details
- `cancel` events produce "Run cancelled" notification
- Gate block notifications include Approve/Reject/Defer buttons (reuse existing `gate_approval_buttons`)

### F3: Stale Phase Detection
Timer-based alert when a run is stuck in the same phase beyond a threshold.

**Acceptance criteria:**
- Configurable threshold (default: 2 hours, `stale_phase_threshold_secs`)
- Alerts only once per stale window (reset on phase change)
- Respects `auto_advance=false` (suppress stale alerts for paused runs)

## Technical Approach

- **F1**: Add `RunTokens` and `DispatchList` ReadOperations. Call from `handle_status_command()`.
- **F2**: Add match arms in `format_notification()` for `block` and `cancel` event types.
- **F3**: Track `last_phase_change` and `stale_alerted` in EventConsumer state. Check on each poll.

## Out of Scope
- HTTP status endpoint
- Multi-run status display
- Historical status logging
