# Plan: Warm Container Pool (Long-Running Containers)

**Bead:** iv-7iy1i
**Date:** 2026-03-06
**Approach:** Option A from brainstorm — long-running containers with IPC message delivery
**Memory strategy:** Aggressive — prewarm all registered groups at startup
**Revision:** v2 — incorporates flux-drive review findings (architecture, correctness, safety)

## Context

The container-side message loop already exists (`agent-runner/src/index.ts` lines 557-590): after each `runQuery()`, it calls `waitForIpcMessage()` which polls `/workspace/ipc/input/` for JSON files. The Rust side already has `write_ipc_message()` and `write_close_sentinel()` in `queue.rs`. The missing piece is making the host-side reuse running containers instead of spawning fresh ones per message.

### Current flow (cold start every time)
```
message -> enqueue_message_check() -> process_group_messages()
  -> run_container_agent() -> docker run --rm (3-5s cold start)
  -> write stdin -> read UDS -> container exits
```

### Target flow (warm containers)
```
startup -> spawn containers for all registered groups (prewarm)
message -> enqueue_message_check() -> check GroupState.pool_container
  YES -> write IPC message file (instant), await output via delivery channel
  NO  -> spawn new container via pool_spawn() (cold start only first time)
container idle 30min -> _close sentinel -> container exits -> cleaned up
next message for group -> re-spawn
```

### Key Architectural Decisions (from plan review)

1. **No separate ContainerPool struct** — extend `GroupState` in `queue.rs` with pool fields. Single state owner prevents dual-state-machine invariant violations.
2. **Keep foreground process model** — `docker run` (NOT `-d`). Foreground gives instant crash detection via `Child::wait()`, exit codes, and stderr capture. Detached mode loses all three and requires polling `docker events`.
3. **Channel-based output routing** — replace closure-swap `on_output` with a `tokio::mpsc` channel. Per-message delivery handler receives frames from the channel. Lifecycle observer (notify_idle) is permanent, not swapped.
4. **Explicit "container idle" signal** — container writes `{"type":"idle"}` after completing a query. This is the signal to release the delivery channel, NOT the `Success` output frame (which fires on every chunk).

## Tasks

### Phase 1: Extend GroupState for Pool

- [x] **1.1** Add pool fields to `GroupState` in `queue.rs`
  - `pool_container: Option<PoolContainer>` — holds container handle when warm
  - `PoolContainer` struct: `container_name: String`, `child: Child`, `started_at: Instant`, `last_activity: Instant`, `uds_listener: JoinHandle`, `delivery_tx: mpsc::Sender<OutputFrame>`
  - State is implicitly managed: `Some(pc)` = warm, `None` = cold
  - Reference: `queue.rs` lines 30-50 for existing GroupState

- [x] **1.2** Create `pool_spawn()` function in `container/runner.rs`
  - Sibling to existing `run_container_agent()`, shares `RunConfig` resolution
  - Spawns `docker run` (foreground, NOT `--rm`, NOT `-d`)
  - Returns `PoolContainer` handle with child process, UDS listener task
  - UDS listener is persistent — stays open across messages (don't delete socket between messages)
  - Fix: do NOT call `remove_file(&uds_socket_path)` for pool containers (only on final cleanup)

- [x] **1.3** Implement output routing via channel
  - `pool_spawn()` creates `mpsc::channel<OutputFrame>` for delivery
  - UDS listener task reads frames, sends to channel
  - Permanent lifecycle observer: on `Success` frame with `idle` signal, mark container idle
  - Per-message: caller creates `delivery_rx` receiver, processes frames for Postgres persist + Telegram delivery
  - When message completes: drop receiver, delivery_tx stays alive for next message

- [x] **1.4** Implement `get_or_spawn()` on GroupState
  - If `pool_container.is_some()` and container is ready: write IPC message file, return delivery channel
  - If `pool_container.is_none()`: call `pool_spawn()`, write input to stdin (first message), set `pool_container`
  - On container exit (detected via `Child::wait()` in background task): set `pool_container = None`, log error
  - GroupQueue serialization guarantees no concurrent access per group

- [x] **1.5** Remove `--rm` from container args for pool-managed containers
  - Find where `--rm` is added in `container/mounts.rs` or `container/secrets.rs`
  - Add `pool_managed: bool` to `RunConfig` — when true, omit `--rm`
  - Non-pool containers keep `--rm` for backward compat

### Phase 2: Integrate with Message Processing

- [x] **2.1** Modify `process_group_messages()` in `process_group.rs`
  - Before calling `run_container_agent()`, check `group_state.pool_container`
  - If warm: write IPC message file via `write_ipc_message()`, create delivery receiver, await output
  - If not warm: call `pool_spawn()` + stdin write (first message cold start), set pool_container
  - Pool entry is created on first successful spawn
  - Fix F2: delivery channel is per-message — no callback swap race. Old receiver is dropped before new one created (serialized by GroupQueue).

- [x] **2.2** Split on_output into delivery + lifecycle
  - Today (lines 260-335): `on_output` closure captures chat_jid, telegram, pool, typing_cancel
  - New: `delivery_handler` function takes `(frame, chat_jid, telegram, pool)` — called from channel receiver loop
  - New: `lifecycle_observer` — permanent task that monitors container exit (via Child::wait()) and sets pool_container = None
  - `notify_idle()` only fires when container signals idle, not on every Success frame (fix F8)

- [x] **2.3** Wire pool into `build_process_messages_fn()`
  - No new `Arc<ContainerPool>` parameter needed — pool state lives in GroupState
  - GroupQueue already passed to process_group_messages — pool_container is accessible
  - Create pool_spawn config in `main.rs` alongside queue setup

- [ ] **2.4** Wire pool into scheduler (`scheduler_wiring.rs`)
  - Scheduled tasks can also use warm containers
  - Modify `run_scheduled_task()` to check group_state.pool_container before spawning

### Phase 3: Idle Reaping & Shutdown

- [x] **3.1** Implement idle reaping
  - Track `last_activity` on PoolContainer (updated on each IPC message write)
  - Background task: every 60s, iterate GroupStates, check for containers idle > 30min
  - Reap sequence: write `_close` sentinel, wait for container exit (via Child::wait()), cleanup
  - Fix F3: reaping acquires GroupQueue lock for the group — prevents race with concurrent message write
  - On intercomd shutdown: iterate all groups, send `_close`, wait up to 30s, then force-kill remaining

- [x] **3.2** Handle IPC cleanup between container lifetimes
  - Fix F4: when container exits (expected or crash), purge stale files from `/ipc/input/` for that group
  - Only purge AFTER container process has fully exited (Child::wait() completed)
  - Log purged file count for debugging

### Phase 4: Prewarming

- [ ] **4.1** Add prewarming at startup
  - After groups are loaded from Postgres in `main.rs`, iterate registered groups
  - For each group: spawn container via `pool_spawn()` in background task
  - Enforce `max_containers` cap (configurable, default 20) — skip if at limit
  - Log: "Prewarming N containers for N registered groups"
  - Don't block startup — prewarm is best-effort

- [ ] **4.2** Prewarm on group registration
  - When a new group is registered (via IPC or Telegram), prewarm its container
  - `ipc.rs` group registration handler checks group_state and triggers pool_spawn if cold

### Phase 5: Health & Recovery

- [x] **5.1** Crash detection and auto-restart
  - Background task per pool container: `child.wait()` in tokio::spawn
  - On unexpected exit: log error with exit code + stderr, set pool_container = None
  - Next message for that group triggers fresh spawn (automatic via `get_or_spawn`)
  - No polling or docker events needed — foreground model gives instant notification

- [ ] **5.2** Health check liveness probe
  - Every 5 min: write `{"type":"ping"}` to IPC input, expect container to ACK within 5s
  - If no ACK: mark unhealthy, send SIGTERM to child, wait for exit, set pool_container = None
  - Container side: add ping handler to `drainIpcInput()` (write `{"type":"pong"}` to output)

- [x] **5.3** Shutdown safety
  - On intercomd crash/SIGKILL: containers are left running (no `--rm`)
  - On next startup: `cleanup_orphans()` in runner.rs already handles this — finds containers with intercom labels, stops them
  - Document in threat model: crashed intercomd leaves credentialed containers for up to idle_timeout duration

### Phase 6: Container-Side Changes

- [x] **6.1** Add idle signal to agent-runner (existing protocol already emits `success` with `result: null`)
  - After `runQuery()` completes and before `waitForIpcMessage()`: write `{"type":"idle"}` to output
  - This signals the host that the container is ready for next message
  - Host uses this to release the per-message delivery channel

- [x] **6.2** Add ping/pong handler to `drainIpcInput()`
  - In `ipc-input.ts`: detect `{"type":"ping"}` messages, respond with `{"type":"pong"}`
  - Do not pass ping messages to the agent — handle at IPC layer

### Phase 7: Config & Tests

- [x] **7.1** Add pool config to `intercom.toml`
  - `[pool]` section: `enabled`, `prewarm`, `idle_timeout_secs`, `max_containers`, `memory_warn_mb`
  - Default: enabled=true, prewarm=true, idle_timeout_secs=1800, max_containers=20, memory_warn_mb=4096

- [x] **7.2** Unit tests for pool lifecycle
  - Test `get_or_spawn` routing logic (warm vs cold)
  - Test idle reaping with GroupQueue lock (no race with message write)
  - Test crash recovery (simulate child exit)
  - Test delivery channel: message A output goes to chat A, message B output goes to chat B
  - Test IPC cleanup between container lifetimes (no stale file replay)

- [ ] **7.3** Integration test: warm container message delivery
  - Spawn a pool container, send message via IPC, verify output arrives via delivery channel
  - Verify idle reaping after timeout
  - Verify prewarm on startup

## Key Files to Modify

| File | Change |
|------|--------|
| `rust/intercomd/src/queue.rs` | Add `PoolContainer` struct, pool fields to `GroupState` |
| `rust/intercomd/src/container/runner.rs` | Add `pool_spawn()` function, fix UDS socket lifecycle |
| `rust/intercomd/src/process_group.rs` | Check pool before spawning, channel-based output routing |
| `rust/intercomd/src/scheduler_wiring.rs` | Check pool before spawning |
| `rust/intercomd/src/main.rs` | Wire pool config, prewarm loop, shutdown handler |
| `rust/intercom-core/src/config.rs` | Add `[pool]` config section |
| `container/agent-runner/src/index.ts` | Add idle signal, ping/pong handler |
| `container/shared/ipc-input.ts` | Add ping message type |

## Review Findings Addressed

| Finding | Resolution | Task |
|---------|-----------|------|
| M1: Dual state machines | Pool state in GroupState, not separate struct | 1.1 |
| M2: on_output callback split | Channel-based delivery + permanent lifecycle observer | 1.3, 2.2 |
| M3: docker run -d loses diagnostics | Keep foreground model with Child::wait() | 1.2, 5.1 |
| F1: UDS socket deleted between messages | Don't remove_file for pool containers | 1.2 |
| F2: Callback swap race | Channel per-message, not closure swap | 2.1 |
| F3: Reaper races with message write | Reaper acquires GroupQueue lock | 3.1 |
| F4: Stale IPC files replayed | Purge /ipc/input/ on container exit | 3.2 |
| S1: Secrets in container memory | Accept as tradeoff, document in threat model | 5.3 |
| S2: Cross-user message routing | Same fix as F2 — channel-based routing | 2.1 |

## Risks

1. **UDS listener lifecycle** — Persistent listener must NOT delete socket between messages. Only cleanup on final container exit.
2. **Channel backpressure** — If delivery receiver is slow, channel fills up. Use bounded channel with reasonable capacity (1000 frames).
3. **Container isolation** — Without `--rm`, containers accumulate filesystem state. Mitigated by idle reaping (30min max).
4. **Concurrent access** — Serialized per group by GroupQueue. Pool state is only accessed through GroupState.
5. **Foreground process accumulation** — Each warm container is a foreground child process. Bounded by max_containers config (default 20).
