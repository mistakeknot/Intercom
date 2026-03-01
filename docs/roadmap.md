# Intercom Roadmap

> Auto-generated from beads on 2026-03-01. Strategic context: [Demarch Roadmap](../../../docs/demarch-roadmap.md)

## Critical Path

```
iv-dyy33 (P2) Add WriteOperation variants
    └─► iv-wjbex (P2) Sprint status push notifications
```

## Completed

- ✓ iv-x2l69 [task] Build and test container images on ethics-gradient
- ✓ iv-5t62v [task] Set up Postgres for Rust orchestrator (Docker, schema auto-created, message loop running)

## Open Items

### P2 — Core Features
- ○ iv-dyy33 [feature] Add WriteOperation variants: RejectGate, DeferGate, ExtendBudget, CancelRun (blocks: iv-wjbex)
- ○ iv-4x5pz [task] Callback auth: verify sender_id matches chat owner
- ○ iv-elbnh [task] Session continuity across model switches
- ○ iv-niu3a [task] Discovery triage via messaging
- ○ iv-wjbex [feature] Sprint status push notifications (blocked by: iv-dyy33)

### P3 — Polish & Extensions
- ○ iv-0131e [task] Decouple IpcDelegate from Telegram bridge
