# Intercom Rust Workspace

This workspace is the phase-1 foundation for replatforming Intercom onto an IronClaw-style Rust runtime.

## Crates

- `intercomd`: daemon skeleton (`serve`, `print-config`, `inspect-legacy`)
- `intercom-core`: shared config and runtime domain types
- `intercom-compat`: compatibility helpers for legacy Node/SQLite inspection

## Build

```bash
cargo check --workspace
cargo build --workspace
cargo build --workspace --release
cargo test --workspace
```

Run from this directory, or use top-level npm scripts (`npm run rust:*`).
