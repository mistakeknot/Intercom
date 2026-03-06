# Warm Container Pool for Faster Response Latency

**Bead:** iv-7iy1i
**Date:** 2026-03-06

## Problem

Every Intercom message triggers a fresh `docker run` which incurs:
1. **Container creation** (~1-2s): Docker creates the container filesystem, sets up namespaces
2. **Node.js startup** (~1-2s): Node boots, loads agent-runner/gemini-runner/codex-runner, parses deps
3. **Agent SDK init** (~0.5-1s): Claude Agent SDK connects, sets up tools

Total cold-start: **~3-5 seconds** before the agent even starts thinking. For a chat UX, this is noticeably sluggish.

## Current Architecture

```
Message arrives → process_group.rs → run_container_agent()
  → docker run --rm intercom-agent:latest  (cold start every time)
  → write stdin (ContainerInput JSON)
  → read UDS output / stdout markers
  → container exits (--rm cleans up)
```

Each container is ephemeral: created, runs once, destroyed. No reuse.

## Design Options

### Option A: Long-Running Containers (Recommended)

Instead of `docker run --rm` per message, keep containers running and send work to them via the existing UDS/IPC mechanism.

**How it works:**
1. On group registration or first message, start a container for the group that stays alive
2. Send prompts via the existing `/workspace/ipc/input/` file-based IPC (already built for mid-session messages)
3. Container processes messages in a loop instead of exiting after one
4. Container idles between messages; reclaimed after configurable idle timeout

**Pros:**
- Eliminates cold start entirely after first message
- Reuses existing IPC infrastructure (ipc-input.ts, ipc-tools.ts already support message polling)
- Natural session continuity — agent memory persists in the running process
- Docker container stays warm with cached node_modules, compiled TS, etc.

**Cons:**
- Memory usage: idle containers consume RAM (~100-200MB each for Node + Chromium)
- Complexity: need lifecycle management (health checks, restart on crash, graceful shutdown)
- Session management changes: currently sessions reset per container; need explicit reset
- Container config changes: can't use `--rm`; need to manage container cleanup

**Implementation sketch:**
- `container/runner.rs`: Add `ContainerPool` that manages long-running containers per group
- Container side: `agent-runner/src/index.ts` already has `drainIpcInput()` loop — extend to be the main processing loop
- Idle timeout: reap containers after N minutes of no messages (configurable per group)
- Crash recovery: detect container exit, auto-restart on next message

### Option B: Container Snapshots (CRIU/checkpoint)

Use Docker checkpoint/restore to freeze a warmed container and restore it instantly.

**Pros:**
- Near-instant restore (~200ms)
- Zero memory cost when not running

**Cons:**
- Requires `docker checkpoint` (CRIU) which needs privileged mode or specific kernel support
- Not widely supported: experimental in Docker, no podman support
- Fragile: checkpoint/restore fails with open network connections, timers, etc.
- Node.js + Chromium state restoration is unreliable

**Verdict:** Too fragile and operationally complex. Not recommended.

### Option C: Pre-Spawned Container Pool

Maintain N idle containers ready to accept work. When a message arrives, claim one from the pool.

**Pros:**
- Eliminates cold start for the first N concurrent requests
- Simpler than long-running: containers are still single-use, just pre-created

**Cons:**
- Wasted resources: idle containers for groups that may not receive messages
- Still has per-message overhead: claim, write input, read output, release
- Doesn't help with session continuity
- Pool sizing is a guessing game

**Verdict:** Partial solution. Better than nothing but Option A is strictly superior.

### Option D: Volume-Cached Node Startup

Mount `node_modules` as a Docker volume instead of having it inside the image. Pre-populate the volume.

**Pros:**
- Faster npm install / module resolution
- Simple to implement

**Cons:**
- Only saves ~0.5s of the 3-5s total
- Doesn't address Docker container creation or agent SDK init
- Introduces volume management complexity

**Verdict:** Marginal improvement. Could combine with Option A but not worth standalone.

## Recommendation: Option A (Long-Running Containers)

The existing IPC infrastructure (`drainIpcInput()`, `/workspace/ipc/input/`, UDS output) was designed for exactly this pattern. The transition is:

1. **Phase 1:** Make the container runner support "attach to existing" — if a container for the group is already running, send work via IPC instead of spawning new
2. **Phase 2:** Start containers eagerly on group registration or first message
3. **Phase 3:** Add idle timeout reaping and crash recovery

### Key Design Decisions

- **One container per group** (not per message, not shared across groups) — maintains isolation
- **Idle timeout default: 30 minutes** — matches current `DEFAULT_IDLE_TIMEOUT_MS`
- **Graceful shutdown:** On intercomd shutdown, signal all containers to finish current work then exit
- **Health check:** Periodic liveness probe (write sentinel to IPC input, expect ACK within 5s)
- **Memory budget:** Track per-container RSS; warn if total exceeds configurable threshold

### Risks

1. **Container crash during message processing** — need retry logic (re-spawn + re-send)
2. **Session state corruption** — long-running process accumulates state; need periodic fresh restart
3. **Resource leak** — containers that lose their IPC connection but stay running (zombie detection)
4. **Concurrent messages to same group** — currently serialized by GroupQueue; must stay serialized with long-running containers too

## Questions for User

1. Is the ~3-5s cold start the primary latency concern, or is API response time (Claude/Gemini) also a target?
2. Memory budget preference: aggressive (prewarm all registered groups) or conservative (warm on first message, reap aggressively)?
3. Should we support container restart without session loss (save/restore session ID across container restarts)?
