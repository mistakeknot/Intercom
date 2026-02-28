# Intercom Roadmap

> Auto-generated from beads on 2026-02-28. Strategic context: [Demarch Roadmap](../../../docs/demarch-roadmap.md)

## Critical Path

```
intercom-8ok (P1) Build container images
    ├─► intercom-e5a (P2) Enable Rust orchestrator
    │       └─► intercom-7lv (P3) Enable scheduler
    └─► intercom-tqf (P3) Verify Gemini/Codex runtimes

intercom-f2t (P2) Add WriteOperation variants
    └─► intercom-9rc (P3) Enable event consumer
```

## Open Items

### P1 — Blockers
- ○ intercom-8ok [task] Build and test container images on ethics-gradient (blocks: intercom-e5a, intercom-tqf)

### P2 — Core Features
- ○ intercom-f2t [feature] Add WriteOperation variants: RejectGate, DeferGate, ExtendBudget, CancelRun (blocks: intercom-9rc)
- ○ intercom-e5a [feature] Enable Rust orchestrator and validate message loop (blocked by: intercom-8ok, blocks: intercom-7lv)
- ○ intercom-c6t [task] Callback auth: verify sender_id matches chat owner

### P3 — Polish & Extensions
- ○ intercom-9rc [feature] Enable event consumer with push notifications to Telegram (blocked by: intercom-f2t)
- ○ intercom-7lv [feature] Enable scheduler and validate cron/interval task execution (blocked by: intercom-e5a)
- ○ intercom-tqf [task] Verify Gemini and Codex runtime credentials and test end-to-end (blocked by: intercom-8ok)
- ○ intercom-o45 [task] Decouple IpcDelegate from Telegram bridge
- ○ intercom-9n0 [task] Fix AGENTS.md documentation drift (orchestrator.enabled, runtime setup)
