# Security Model

- Agents run in Docker containers with filesystem isolation
- Each group gets its own IPC namespace (no cross-group message injection)
- Secrets passed via stdin, never written to mounted volumes
- Shell commands have secrets stripped from environment
- Additional mounts validated against external allowlist (`~/.config/intercom/mount-allowlist.json`)
- Non-main groups can be forced read-only via allowlist
- Hard policy block: `/wm` paths rejected for additional mounts
- Demarch writes restricted to main group by default (`require_main_group_for_writes`)
- Query handlers use `execFileSync` (no shell) to prevent command injection from container-supplied params
- Demarch read/write commands validated against allowlists in `intercom.toml`
