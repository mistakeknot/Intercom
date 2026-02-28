# Intercom Roadmap

> Auto-generated from beads on 2026-02-28. Strategic context: [Demarch Roadmap](../../../docs/demarch-roadmap.md)

## Critical Path

```
iv-x2l69 (P1) Build container images ✓ DONE
    ├─► iv-5t62v (P2) Set up Postgres for Rust orchestrator
    └─► (unblocked) Verify Gemini/Codex runtimes

iv-dyy33 (P2) Add WriteOperation variants
    └─► iv-wjbex (P2) Sprint status push notifications
```

## Completed

- ✓ iv-x2l69 [task] Build and test container images on ethics-gradient

## Open Items

### P2 — Core Features
- ○ iv-5t62v [task] Set up Postgres for Rust orchestrator (enables orchestrator.enabled=true)
- ○ iv-dyy33 [feature] Add WriteOperation variants: RejectGate, DeferGate, ExtendBudget, CancelRun (blocks: iv-wjbex)
- ○ iv-4x5pz [task] Callback auth: verify sender_id matches chat owner
- ○ iv-elbnh [task] Session continuity across model switches
- ○ iv-niu3a [task] Discovery triage via messaging
- ○ iv-wjbex [feature] Sprint status push notifications (blocked by: iv-dyy33)

### P3 — Polish & Extensions
- ○ iv-0131e [task] Decouple IpcDelegate from Telegram bridge
