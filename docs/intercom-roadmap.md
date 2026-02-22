# Intercom Roadmap

**Version:** 0.1
**Last updated:** 2026-02-22

## Where We Are

Intercom is Demarch's messaging gateway with Telegram and WhatsApp channels, multi-runtime container execution, per-group isolation, IPC tool routing, and scheduled tasks. The codebase is operational but still isolated from Intercore/Clavain workflow state.

## Roadmap — Now

- [intercom-now-runtime-hardening] **Stabilize multi-runtime reliability in production** across Claude, Gemini, and Codex container paths.
- [intercom-now-observability] **Add stronger runtime observability** for queue latency, container lifecycle failures, and IPC delivery errors.
- [intercom-now-security-review] **Complete mount and secret handling review** for default-safe behavior in all non-main groups.

## Roadmap — Next

- [intercom-next-kernel-read] **Add read-only Demarch state tools** for run status, sprint phase, and bead visibility from messaging conversations.
- [intercom-next-event-bridge] **Implement event bridge from kernel state to chat notifications** for meaningful workflow updates.
- [intercom-next-gateway-ux] **Unify messaging UX contracts** for status, approvals, and follow-up actions across supported channels.

## Roadmap — Later

- [intercom-later-role-access] **Introduce role-scoped group permissions** for read-only vs control actions per chat context.
- [intercom-later-cross-channel-context] **Enable cross-channel continuity** so project context survives channel switching.
- [intercom-later-team-surface] **Evolve into a team-facing agency surface** with recurring digests and operational reporting.

## Research Agenda

- Evaluate safe approval flows for phase gates over chat.
- Define thin integration boundaries with Clavain intents and Intercore event consumption.
- Determine high-signal message summaries that reduce operator noise.

## Keeping Current

Regenerate with `/interpath:roadmap` and keep this file aligned with `docs/intercom-vision.md`.
