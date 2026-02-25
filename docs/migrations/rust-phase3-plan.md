# IronClaw Phase 3 — Container Runner + Persistence

Phase 3 migrates the two largest remaining Node.js subsystems to Rust: the container runner (Docker lifecycle management) and the persistence layer (SQLite → Postgres for live reads/writes). Together these represent ~1500 lines of Node.js and unlock the critical path to making intercomd standalone.

## Dependency chain

```
3a. Postgres persistence layer      3b. Container runner
     (independent)                       (independent)
          │                                    │
          └──────────┬─────────────────────────┘
                     │
              3c. Task scheduler
                   (needs both)
                     │
              3d. Message routing loop
                   (needs scheduler + container runner)
                     │
              3e. Slash command handler
                   (needs queue state)
```

Tasks 3a and 3b can proceed in parallel. Tasks 3c–3e are sequential.

## Task 3a — Postgres persistence layer

**Goal**: Replace all live SQLite reads/writes with Postgres equivalents in Rust.

**New crate**: None — add `persistence` module to `intercom-core`.

**Tables to port** (from `src/db.ts`):

| Table | Rows | Read by | Written by |
|-------|------|---------|------------|
| `chats` | ~200 | Node | Node |
| `messages` | ~50k | Node | Node |
| `scheduled_tasks` | ~20 | Node | Node |
| `task_run_logs` | ~500 | Node (log only) | Node |
| `router_state` | ~3 KV pairs | Node | Node |
| `sessions` | ~10 | Node | Node |
| `registered_groups` | ~8 | Node + Rust (Telegram ingress) | Node |

**Approach**: Use `sqlx` with compile-time checked queries against a Postgres schema. The schema mirrors SQLite but uses proper types (TIMESTAMPTZ, BOOLEAN, SERIAL).

**Migration strategy**:
1. Create Postgres schema matching current SQLite tables (extending existing `intercom-compat` migration tooling).
2. Add `PersistenceLayer` trait in `intercom-core` with SQLite and Postgres implementations.
3. intercomd uses Postgres implementation; Node continues using SQLite during transition.
4. Dual-write phase: Node writes to both SQLite and Postgres (via intercomd HTTP endpoint) so data stays in sync.
5. Cutover: switch reads to Postgres, retire SQLite.

**Key functions to port** (from `src/db.ts`, 719 lines):
- `getNewMessages` / `getMessagesSince` — message polling with cursor
- `storeChatMetadata` / `storeMessage` / `storeMessageDirect` — chat + message storage
- `getDueTasks` / `createTask` / `updateTask` / `updateTaskAfterRun` — task CRUD
- `logTaskRun` — task execution logs
- `getRouterState` / `setRouterState` — KV cursor store
- `getSession` / `setSession` / `getAllSessions` — group session mapping
- `getAllRegisteredGroups` / `setRegisteredGroup` — group registration

**New HTTP endpoints on intercomd**:
- `POST /v1/db/messages` — store message
- `POST /v1/db/chat-metadata` — update chat metadata
- `POST /v1/db/tasks` — task CRUD
- `POST /v1/db/sessions` — session CRUD
- `GET /v1/db/new-messages` — poll new messages for registered groups
- `GET /v1/db/due-tasks` — query due scheduled tasks

**Estimated size**: ~400 lines Rust (schema + queries + trait + endpoints).

## Task 3b — Container runner

**Goal**: Spawn and manage Docker containers from Rust, replacing `src/container-runner.ts` (741 lines).

**New module**: `intercomd/src/container.rs`

**What it does** (from reading container-runner.ts):

1. **Runtime selection**: Resolves `Runtime` (claude/gemini/codex) from group config or model name.
2. **Volume mounts**: Builds mount list based on group type (main vs non-main), runtime, and additional mounts.
3. **Mount security**: Validates additional mounts against external allowlist (`~/.config/intercom/mount-allowlist.json`).
4. **Container args**: Constructs `docker run -i --rm --name nanoclaw-{group}-{ts} ...` with mounts, env vars, user mapping.
5. **Secrets injection**: Reads secrets from `.env` + Claude OAuth from `~/.claude/.credentials.json`, passes via stdin (never on disk).
6. **Stdin/stdout protocol**: Writes JSON input to stdin, streams stdout for `OUTPUT_START_MARKER`/`OUTPUT_END_MARKER` pairs.
7. **Timeout management**: Hard timeout with activity-based reset. Idle containers closed via `_close` sentinel file.
8. **Graceful stop**: `docker stop` with 15s grace, fallback to SIGKILL.
9. **Container logging**: Writes per-run log files to `groups/{folder}/logs/`.
10. **Snapshot writes**: `current_tasks.json` and `available_groups.json` written to IPC dir before each run.

**Submodules**:
- `container/runner.rs` — `runContainerAgent()` equivalent, tokio process management
- `container/mounts.rs` — volume mount builder with runtime-specific logic
- `container/security.rs` — mount allowlist validation (port of `mount-security.ts`)
- `container/secrets.rs` — secrets reader (.env + OAuth token)
- `container/protocol.rs` — OUTPUT marker parsing, ContainerInput/ContainerOutput types

**Key design decisions**:
- Use `tokio::process::Command` for async container spawning.
- Parse stdout incrementally using `tokio::io::BufReader` with marker scanning.
- Secrets passed via stdin pipe, zeroed from memory after write.
- Container names: `nanoclaw-{sanitized_folder}-{epoch_ms}` (matches Node pattern).
- Runner source mounts: claude at `/app/src`, gemini/codex at `/app/{runner}/src`.

**Port of mount-security.ts** (448 lines):
- External allowlist at `~/.config/intercom/mount-allowlist.json`
- Hard-blocked paths: `/wm`, `.ssh`, `.gnupg`, `.aws`, `.docker`, etc.
- Symlink resolution via `std::fs::canonicalize`
- Non-main groups forced read-only when `nonMainReadOnly` is true
- Container paths prefixed with `/workspace/extra/`

**Estimated size**: ~600 lines Rust.

## Task 3c — Task scheduler

**Goal**: Replace `src/task-scheduler.ts` (249 lines) — poll due tasks and run containers.

**New module**: `intercomd/src/scheduler.rs`

**What it does**:
1. Polls `scheduled_tasks` table for rows where `next_run <= NOW()` and `status = 'active'`.
2. Enqueues each due task into the group queue.
3. After each run, calculates next run time (cron via `cron` crate, interval via addition, once → completed).
4. Logs each run to `task_run_logs` with duration and result.
5. Uses `context_mode` to determine session inheritance: `'group'` shares session, `'isolated'` creates fresh.

**Dependencies**: Requires 3a (Postgres queries) and 3b (container runner).

**Crate dependency**: `cron` for cron expression parsing (replaces Node's `cron-parser`).

**Estimated size**: ~150 lines Rust.

## Task 3d — Message routing loop + group queue

**Goal**: Replace `src/index.ts` message loop + `src/group-queue.ts` (361 lines).

**New modules**: `intercomd/src/queue.rs`, modifications to `main.rs` serve loop.

**GroupQueue semantics** (from reading group-queue.ts):
- Per-group serialization: only one container runs per group at a time.
- Global concurrency cap: `MAX_CONCURRENT_CONTAINERS` (configurable).
- Priority: tasks before messages in drain order.
- Retry with exponential backoff (5s base, max 5 retries).
- Idle detection: `notifyIdle()` + `_close` sentinel for preemption.
- Follow-up messages piped via IPC `input/` directory while container is active.
- Graceful shutdown: containers detached (not killed) — they finish via idle timeout.

**Message loop semantics** (from reading index.ts):
- Polls `getNewMessages()` from DB every `POLL_INTERVAL` (1s).
- Groups by chatJid, checks trigger pattern for non-main groups.
- If container active for group: pipes message via `queue.sendMessage()` → IPC `input/` file.
- If no container: enqueues via `queue.enqueueMessageCheck()`.
- Crash recovery: on startup, checks for unprocessed messages and re-enqueues.
- Cursor management: `lastTimestamp` (global) + `lastAgentTimestamp` (per-group), with rollback on error.

**Design considerations**:
- The queue needs to be integrated with the Telegram/WhatsApp channel abstraction.
- In the Rust model, intercomd already receives Telegram messages via `/v1/telegram/ingress`. The message loop can poll Postgres directly instead of going through channel → SQLite → poll.
- WhatsApp integration remains Node-only (no Rust Baileys equivalent). Messages from WhatsApp will still flow through Node → SQLite → Postgres dual-write → Rust poll.

**Estimated size**: ~350 lines Rust.

## Task 3e — Slash command handler

**Goal**: Handle `/help`, `/status`, `/model`, `/reset`, `/new` in Rust.

**New module**: `intercomd/src/commands.rs`

**Current commands** (from index.ts):
- `/help` — static text, lists all commands
- `/status` — shows model, session, container state, uptime
- `/model` — lists catalog or switches model (kills container, clears session, sets pending model switch flag)
- `/reset` / `/new` — kills container, clears session, clears model switch state
- `/ping` and `/chatid` — handled directly in Telegram channel, not in command handler

**Design**: Commands return `CommandResult { text, parseMode }`. The Telegram ingress handler already routes to intercomd — add command detection before agent invocation.

**Estimated size**: ~120 lines Rust.

## What stays in Node (Phase 4+)

- **WhatsApp channel** (Baileys) — no Rust equivalent. Will remain as a thin adapter that writes to Postgres and receives from intercomd via HTTP.
- **Stream accumulator** — progressive output display. Moves to Rust when container runner moves.
- **Summarizer** — conversation summary generation for model switches. Low priority.

## Execution order

| # | Task | Depends on | Est. lines | Priority |
|---|------|-----------|-----------|----------|
| 3a | Postgres persistence | — | ~400 | P1 |
| 3b | Container runner | — | ~600 | P1 |
| 3c | Task scheduler | 3a, 3b | ~150 | P2 |
| 3d | Message routing + queue | 3a, 3b, 3c | ~350 | P2 |
| 3e | Slash commands | 3d | ~120 | P3 |

**Recommended approach**: Start with 3b (container runner) as the highest-risk item — Docker process management, mount security, and stdin/stdout protocol parsing are the most complex to get right. 3a (Postgres) can be developed in parallel by a second agent or in sequence after 3b.

## Risk assessment

**Container runner** (medium-high risk):
- stdout parsing with OUTPUT markers must be byte-perfect or agents silently lose output.
- Mount security has a hard policy layer — bugs here are security vulnerabilities.
- Secrets handling: OAuth token refresh, memory zeroing, no disk persistence.
- Docker timeout/kill edge cases: idle cleanup vs. real timeout vs. graceful stop.

**Postgres persistence** (low risk):
- Schema is well-defined from SQLite. sqlx compile-time checking catches type errors.
- Dual-write phase means Node can be the safety net during transition.
- Connection pooling (deadpool-postgres or sqlx pool) is straightforward.

**Task scheduler** (low risk):
- Simple poll loop with cron parsing. Well-tested pattern.

**Message routing** (medium risk):
- Cursor management with rollback has subtle ordering concerns.
- Piping follow-up messages to active containers via IPC input/ requires coordination with container lifecycle.
- Crash recovery re-enqueue logic must not cause duplicate processing.
