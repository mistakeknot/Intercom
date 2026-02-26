# IronClaw Phase 6: Cutover, Node Removal, Stream Accumulator

**Bead:** iv-i3oxs
**Complexity:** 3/5 (moderate)
**Depends on:** Phase 5 (iv-u0gmm) — integration tests + flag flip ✓

## Goal

Complete the IronClaw replatform: flip the production flag, remove ~580 lines of dead Node orchestrator code, and port the StreamAccumulator for progressive Telegram output from Rust.

## Context

Phases 1–5 built the full Rust orchestrator (message loop, scheduler, queue, container dispatch, commands) behind `orchestrator.enabled` and `RUST_ORCHESTRATOR` flags. The deployment procedure is documented. This phase executes the cutover and cleans up.

## Tasks

### 6-pre. Pre-cutover bug fixes (blocking 6a)

Two bugs discovered by plan review that must be fixed before flipping the production flag:

**Bug 1 (P0): `write_snapshots()` never called.** `container/runner.rs:573` defines `pub async fn write_snapshots()` but it is called from zero places. Node's `runAgent()` writes `current_tasks.json` and `available_groups.json` before every container spawn. Without this, Rust-spawned containers read stale/missing data.

Fix: Call `write_snapshots()` from `process_group.rs` before `run_container_agent()` and from `scheduler_wiring.rs` before task dispatch. Both have the pool, groups, and tasks in scope.

**Bug 2 (P1): Agent-timestamp double-write race.** `message_loop.rs` holds `AgentTimestamps` in a local `mut` variable (loaded once at startup). `process_group.rs` independently loads from and saves to Postgres per invocation. The message loop's stale copy can overwrite cursor advances from process_group, causing message duplication.

Fix: Move `AgentTimestamps` into `Arc<RwLock<AgentTimestamps>>` in `AppState`. Both message_loop and process_group read/write through the shared lock. No Postgres round-trip on read in process_group (resolves the secondary performance concern too).

### 6a. Production cutover (ops, no code)

Execute the deployment procedure from `docs/migrations/rust-foundation.md`:

1. Pre-flight: verify intercomd `/readyz` shows `postgres_connected: true`, `registered_groups > 0`
2. **Drain check:** verify `active_containers == 0` before restarting Node — otherwise in-flight containers are killed and messages with advanced cursors get no response
3. Add `RUST_ORCHESTRATOR=true` to Node service, restart Node first (drops loops)
4. Enable `orchestrator.enabled = true` + `scheduler.enabled = true` in `config/intercom.toml`
5. Restart intercomd (acquires orchestration)
6. Verify: send test Telegram message, check `journalctl --user -u intercomd -f`
7. Monitor for 1 hour — watch for duplicate messages, missed triggers, scheduler misfires

**Gate:** User confirms production is stable before proceeding to 6b.

### 6b. Remove dead Node orchestrator code (~580 lines)

**Before starting:** Tag trunk for rollback: `git tag v-pre-6b`. After 6b, the config-based rollback (`RUST_ORCHESTRATOR=false`) no longer works since the conditional blocks are deleted. Rollback after 6b requires `git revert` + `npm run build` + restart.

Once production runs on Rust orchestrator, remove:

| Component | File | Lines | Action |
|-----------|------|-------|--------|
| `startMessageLoop()` | src/index.ts | ~98 | Delete function |
| `processGroupMessages()` | src/index.ts | ~200 | Delete function |
| `recoverPendingMessages()` | src/index.ts | ~12 | Delete function |
| `runAgent()` | src/index.ts | ~83 | Delete (Rust handles container dispatch) |
| Message state vars | src/index.ts | ~10 | Delete `lastTimestamp`, `lastAgentTimestamp`, `pendingModelSwitch`, `reportedModels` |
| Scheduler wiring | src/index.ts | ~18 | Delete conditional block |
| Message loop wiring | src/index.ts | ~3 | Delete conditional block |
| `RUST_ORCHESTRATOR` import + conditionals | src/index.ts + config.ts | ~10 | Remove flag (always true) |
| `startSchedulerLoop()` + `runTask()` | src/task-scheduler.ts | 249 | Delete entire file |
| `StreamAccumulator` | src/stream-accumulator.ts | 167 | Keep — port to Rust in 6c, then delete |
| `pendingModelSwitch` + summary injection | src/index.ts | ~60 | Delete (was in processGroupMessages) |

**Keep intact:**
- `GroupQueue` (src/group-queue.ts) — container lifecycle still Node-side
- `host-callback.ts` — intercomd delegates container spawning to Node
- `container-runner.ts` — spawns containers
- `ipc.ts` — container→host IPC
- Command handlers — still needed for Node-side command processing
- `router.ts` — message formatting used by channels + host callback
- Channel code (WhatsApp, Telegram) — always Node-side

**Summarizer entanglement:** `handleModel` calls `generateSummary()` and sets `pendingModelSwitch` — but the consumer (`processGroupMessages`) is being deleted. Remove both producer and consumer: strip `pendingModelSwitch`, `generateSummary` call from `handleModel`, and `clearCachedSummary` from `handleReset`. Context carryover on model switches becomes a future Rust-side feature. Keep `summarizer.ts` file (not imported from index.ts anymore).

**Also update:**
- Remove `import { startSchedulerLoop }`
- Remove `import { StreamAccumulator }` (after 6c)
- Remove `import { generateSummary, getCachedSummary, clearCachedSummary }` from summarizer
- Remove `RUST_ORCHESTRATOR` flag from config.ts (always true, no conditional needed)
- Remove `SCHEDULER_POLL_INTERVAL` from config.ts (only used by Node scheduler)
- Update `CLAUDE.md` / `AGENTS.md` to reflect new architecture
- Update `docs/migrations/rust-foundation.md` with Phase 6 completion

### 6c. Port StreamAccumulator to Rust

Build `stream_accumulator.rs` in `intercomd` that progressively updates Telegram messages as the container streams output.

**Current gap:** Rust's `process_group.rs` waits for full container output, then sends a single Telegram message. Node's `StreamAccumulator` sends an initial message, then edits it with progressive updates (tool starts, text deltas).

**Design:**
- New module `stream_accumulator.rs` in intercomd
- State machine: `Idle → Accumulating → Sent → Finalizing`
- Accumulates `StreamEvent::ToolStart` and `StreamEvent::TextDelta` events
- Debounce timer (500ms) — flush to Telegram on timer or on finalize
- First flush: `POST /v1/telegram/send` (get message ID back)
- Subsequent flushes: `POST /v1/telegram/edit` (update existing message)
- Finalize: edit with full result text, strip `<internal>` blocks
- Graceful fallback: if channel doesn't support editing, buffer and send once

**Integration points:**
- `process_group.rs` `build_process_messages_fn()` — wire accumulator into output callback
- `telegram.rs` — add `edit_message()` endpoint (currently only has `send_message`)
- Container runner output callback — already parses `StreamEvent` but ignores non-result events

**Tests:**
- Unit: accumulator state transitions, debounce timing, fallback behavior
- Integration: verify `/v1/telegram/edit` endpoint responds

## Execution Order

```
6-pre (bug fixes) → 6a (ops cutover) → [user gate] → 6b (cleanup) → 6c (streaming) → ship
```

6-pre fixes two blocking bugs before the production flip. 6a is manual ops. 6b and 6c are independent but 6b should go first (smaller diff, validates the cleanup). 6c builds on the cleaned-up codebase.

## Risks

- **6a: Duplicate messages during cutover** — mitigated by deployment order (Node drops loops first)
- **6a: In-flight containers killed** — mitigated by drain check (active_containers == 0) before restart
- **6b: Rollback dead after code removal** — mitigated by `git tag v-pre-6b`; rollback is `git revert` + build + restart
- **6b: Removing too much** — mitigated by keeping GroupQueue, host-callback, container-runner intact
- **6b: Summarizer entanglement** — `handleModel`/`handleReset` use summarizer; resolved by removing the summary pre-generation (Rust-side future feature)
- **6c: Telegram rate limits on edits** — mitigated by 500ms debounce; Telegram allows ~30 edits/min per chat
- **6c: Message ID tracking** — Telegram `sendMessage` returns `message_id`; need to propagate it back from Bot API response

## Verification

- `npm run build` passes after 6b (TypeScript compiles)
- `npm run rust:test` passes after 6c
- Send test message → see progressive updates in Telegram (6c validation)
- `wc -l src/index.ts` decreases by ~400+ lines after 6b
