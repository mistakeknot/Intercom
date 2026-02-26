# IronClaw Phase 4 — Orchestrator Wiring

Phase 4 connects the independently-built Phase 3 modules into a working orchestrator inside `intercomd`. After Phase 4, intercomd can autonomously poll messages, dispatch containers, run scheduled tasks, and handle slash commands — all without the Node host in the critical path.

## Current state (end of Phase 3)

```
serve() spawns:
  ├── IPC watcher (polls data/ipc/ for container→host messages)
  ├── Group registry sync (fetches registered groups from Node every 10s)
  └── Event consumer (polls ic events, sends push notifications)

Standalone modules (not yet wired):
  ├── scheduler.rs     — run_scheduler_loop() polls PgPool for due tasks, calls TaskCallback
  ├── queue.rs         — GroupQueue with per-group serialization + concurrency cap
  ├── container/       — run_container_agent() spawns Docker containers
  ├── commands.rs      — handle_command() returns CommandResult
  └── db.rs            — 25 HTTP endpoints for Node dual-write
```

## Target state (end of Phase 4)

```
serve() spawns:
  ├── IPC watcher
  ├── Group registry sync → populates shared RegisteredGroups
  ├── Event consumer
  ├── Scheduler loop → polls PgPool → enqueues DueTasks into GroupQueue
  ├── Message loop → polls PgPool → groups by JID → enqueues into GroupQueue
  └── GroupQueue → dequeues → calls run_container_agent() → streams output → sends via Telegram

AppState gains:
  ├── queue: Arc<GroupQueue>       — shared across message loop, scheduler, commands
  ├── groups: Arc<RwLock<Groups>>  — shared registered groups cache
  └── sessions: Arc<RwLock<Sessions>>  — in-memory session cache
```

## Dependency chain

```
4a. Shared state (groups, sessions, queue in AppState)
     │
     ├── 4b. Message loop (polls messages, enqueues)
     │        │
     │        └── 4c. processGroupMessages (dequeue callback → runAgent → container)
     │
     ├── 4d. Scheduler wiring (run_scheduler_loop with real TaskCallback)
     │
     └── 4e. Command wiring (Telegram ingress → command detection → real side effects)
```

4a is a prerequisite for everything. 4b-4e can proceed in sequence after 4a.

## Task 4a — Shared orchestrator state

**Goal**: Add `GroupQueue`, registered groups, and sessions as shared state in `serve()`.

**Changes to `main.rs`**:
1. Create `GroupQueue::new(max_concurrent, data_dir)` in `serve()`.
2. Create `Arc<RwLock<HashMap<String, RegisteredGroup>>>` for registered groups.
3. Create `Arc<RwLock<HashMap<String, String>>>` for sessions (groupFolder → sessionId).
4. Add `queue: Arc<GroupQueue>` and `groups`/`sessions` to `AppState`.
5. Load initial state from Postgres on startup (groups, sessions, router state).

**New config fields** (in `IntercomConfig`):
- `orchestrator.enabled: bool` (default false) — feature flag for Phase 4
- `orchestrator.max_concurrent_containers: usize` (default 3)
- `orchestrator.poll_interval_ms: u64` (default 1000)
- `orchestrator.idle_timeout_ms: u64` (default 300000)
- `orchestrator.main_group_folder: String` (default "main")
- `scheduler.enabled: bool` (default false)
- `scheduler.poll_interval_ms: u64` (default 10000)
- `scheduler.timezone: String` (default "UTC")

**RegisteredGroup struct** (in `intercom-core`):
```rust
pub struct RegisteredGroup {
    pub name: String,
    pub folder: String,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub requires_trigger: bool,
    pub trigger: Option<String>,
}
```

**Estimated size**: ~100 lines config + ~50 lines state wiring.

## Task 4b — Message loop

**Goal**: Replace Node's `startMessageLoop()` with a Rust async poll loop.

**New module**: `intercomd/src/message_loop.rs`

**What it does**:
1. Polls `PgPool::get_new_messages(jids, last_timestamp)` every `poll_interval`.
2. Groups by chat_jid, checks trigger pattern for non-main groups.
3. If container active for group: pipes via `queue.send_message()`.
4. If no container: calls `queue.enqueue_message_check()`.
5. Manages `last_timestamp` (global) and `last_agent_timestamp` (per-group) cursors.
6. On startup: `recover_pending_messages()` re-enqueues any groups with unprocessed messages.

**New persistence functions needed**:
- `PgPool::get_new_messages(jids: &[&str], since: &str, bot_name: &str) -> Vec<NewMessage>` (EXISTS but returns all — need JID filtering)
- `PgPool::get_router_state(key: &str) -> Option<String>` (EXISTS)
- `PgPool::set_router_state(key: &str, value: &str)` (EXISTS)

**Key port difference**: The Node loop stores messages first (via channel onMessage callback), then polls. In Rust, Telegram ingress already stores via `/v1/db/messages`. The message loop just polls and dispatches.

**Estimated size**: ~180 lines.

## Task 4c — processGroupMessages callback

**Goal**: Implement the callback that `GroupQueue::run_for_group()` invokes when it's a group's turn.

**What it does** (port of Node's `processGroupMessages`):
1. Fetch pending messages since `lastAgentTimestamp[chatJid]`.
2. Check trigger pattern for non-main groups.
3. Format messages into prompt.
4. Handle model switch context (summary injection — deferred, just use raw history).
5. Call `run_container_agent()` with the prompt.
6. Stream output: route results to Telegram via `/v1/telegram/send`.
7. Store bot responses in Postgres.
8. Manage cursor: advance on success, rollback on error.

**This is the most complex task** — it connects queue → container → Telegram → DB in a single callback.

**Approach**: Build as a `ProcessMessagesFn` closure that captures `PgPool`, `Arc<GroupQueue>`, `Arc<TelegramBridge>`, `IntercomConfig`, and registered groups.

**Estimated size**: ~250 lines.

## Task 4d — Scheduler wiring

**Goal**: Wire `run_scheduler_loop()` into `serve()` with a real `TaskCallback` that enqueues into `GroupQueue`.

**Changes**:
1. Create `SchedulerConfig` from `IntercomConfig`.
2. Build `TaskCallback` closure that calls `queue.enqueue_task()`.
3. The task_fn creates a closure that runs the container agent for the task's group + prompt.
4. After container run: calculate next_run, update task in Postgres, log the run.

**Estimated size**: ~80 lines.

## Task 4e — Command side effects

**Goal**: Wire slash commands to actually kill containers, clear sessions, and switch models.

**Current state**: `commands.rs` returns `CommandResult` text but has no side effects (no queue access, no DB access, no group mutation).

**Changes**:
1. Add `CommandContext` fields: `queue: Arc<GroupQueue>`, `db: Option<PgPool>`, `groups: Arc<RwLock<Groups>>`.
2. `/reset`: Call `queue.kill_group()`, delete session from Postgres and in-memory cache.
3. `/model <name>`: Kill container, clear session, update group model/runtime in Postgres and in-memory.
4. `/status`: Read `queue.is_active()` for real container state.

**Alternative**: Keep `commands.rs` pure (returns intent, not effects) and have the caller in `main.rs` apply side effects based on the result. This is cleaner for testing.

**Recommended**: Intent-based approach — `CommandResult` gains an `effects: Vec<CommandEffect>` field (KillGroup, ClearSession, SwitchModel). The caller applies effects.

**Estimated size**: ~100 lines.

## What stays in Node after Phase 4

- **WhatsApp channel** (Baileys) — no Rust equivalent
- **Stream accumulator** — progressive output display (Rust sends final result only initially)
- **Summarizer** — conversation summary for model switches (deferred)
- **host-callback.ts** — still needed for WhatsApp message sends

## Feature flag approach

Phase 4 is behind `orchestrator.enabled = true` in config. When disabled:
- Message loop, scheduler, and queue don't start
- All current sidecar behavior preserved
- Node remains the orchestrator

When enabled:
- intercomd takes over message dispatch, scheduling, and container lifecycle
- Node's `startMessageLoop()` and `startSchedulerLoop()` should be disabled
- Node keeps WhatsApp channel active, stores messages in both SQLite and Postgres

## Risk assessment

**processGroupMessages callback** (high):
- Cursor management with rollback-on-error is subtle
- Container output streaming must be byte-correct for OUTPUT markers
- Race between piping follow-up messages and container exit

**Message loop** (medium):
- Dual cursor (global + per-group) ordering is fiddly
- Crash recovery re-enqueue must not duplicate

**Command side effects** (low):
- Kill/clear/switch are simple mutations with well-tested queue methods

**Scheduler wiring** (low):
- Just connecting existing interfaces

## Execution order

| # | Task | Depends on | Est. lines | Priority |
|---|------|-----------|-----------|----------|
| 4a | Shared state | — | ~150 | P1 |
| 4b | Message loop | 4a | ~180 | P1 |
| 4c | processGroupMessages | 4a, 4b | ~250 | P1 |
| 4d | Scheduler wiring | 4a | ~80 | P2 |
| 4e | Command side effects | 4a | ~100 | P2 |

Start with 4a (shared state), then 4b+4c (message flow end-to-end), then 4d and 4e in either order.
