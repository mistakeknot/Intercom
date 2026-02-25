//! Secrets reader: loads credentials from `.env` file and Claude OAuth token.
//!
//! Secrets are injected via container stdin and never written to disk.
//! Port of `readSecrets()` and `readEnvFile()` from container-runner.ts / env.ts.

use std::collections::HashMap;
use std::path::Path;

use tracing::debug;

/// Secret key names for each runtime.
const SECRET_KEYS: &[&str] = &[
    // Claude
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    // Gemini (Code Assist API)
    "GEMINI_REFRESH_TOKEN",
    "GEMINI_OAUTH_CLIENT_ID",
    "GEMINI_OAUTH_CLIENT_SECRET",
    // Codex/OpenAI
    "CODEX_OAUTH_ACCESS_TOKEN",
    "CODEX_OAUTH_REFRESH_TOKEN",
    "CODEX_OAUTH_ID_TOKEN",
    "CODEX_OAUTH_ACCOUNT_ID",
];

/// Parse a `.env` file and return values for requested keys.
/// Does NOT load into process env — callers decide what to do with values.
fn read_env_file(env_path: &Path, keys: &[&str]) -> HashMap<String, String> {
    let content = match std::fs::read_to_string(env_path) {
        Ok(c) => c,
        Err(_) => {
            debug!(path = %env_path.display(), ".env file not found");
            return HashMap::new();
        }
    };

    let wanted: std::collections::HashSet<&str> = keys.iter().copied().collect();
    let mut result = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let eq_idx = match trimmed.find('=') {
            Some(i) => i,
            None => continue,
        };
        let key = trimmed[..eq_idx].trim();
        if !wanted.contains(key) {
            continue;
        }
        let mut value = trimmed[eq_idx + 1..].trim().to_string();
        // Strip surrounding quotes
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if !value.is_empty() {
            result.insert(key.to_string(), value);
        }
    }

    result
}

/// Read the Claude OAuth token from `~/.claude/.credentials.json`.
/// Claude Code auto-refreshes this file, so we always get a valid token.
fn read_claude_oauth_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let cred_path = Path::new(&home).join(".claude/.credentials.json");
    let content = std::fs::read_to_string(&cred_path).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;
    let token = data
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()?
        .to_string();
    if token.is_empty() {
        return None;
    }
    debug!("Read Claude OAuth token from credentials file");
    Some(token)
}

/// Read all runtime secrets from `.env` and Claude OAuth credentials.
///
/// For Claude: if neither `CLAUDE_CODE_OAUTH_TOKEN` nor `ANTHROPIC_API_KEY`
/// is in `.env`, falls back to reading from `~/.claude/.credentials.json`.
pub fn read_secrets(project_root: &Path) -> HashMap<String, String> {
    let env_path = project_root.join(".env");
    let mut secrets = read_env_file(&env_path, SECRET_KEYS);

    // Auto-refresh: read Claude OAuth from credentials file if not in .env
    if !secrets.contains_key("CLAUDE_CODE_OAUTH_TOKEN")
        && !secrets.contains_key("ANTHROPIC_API_KEY")
    {
        if let Some(token) = read_claude_oauth_token() {
            secrets.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), token);
        }
    }

    secrets
}

/// Build the Docker CLI args for running a container.
///
/// Constructs `docker run -i --rm --name {name} -e TZ=... --user ... -v ... {image}`.
pub fn build_container_args(
    mounts: &[intercom_core::VolumeMount],
    container_name: &str,
    image: &str,
    timezone: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-i".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name.to_string(),
    ];

    // Pass host timezone
    args.push("-e".to_string());
    args.push(format!("TZ={}", timezone));

    // Run as host user so bind-mounted files are accessible.
    // Skip when running as root (uid 0) or the container's node user (uid 1000).
    #[cfg(unix)]
    {
        let uid = nix_uid();
        let gid = nix_gid();
        if uid != 0 && uid != 1000 {
            args.push("--user".to_string());
            args.push(format!("{}:{}", uid, gid));
            args.push("-e".to_string());
            args.push("HOME=/home/node".to_string());
        }
    }

    for mount in mounts {
        if mount.readonly {
            args.push("-v".to_string());
            args.push(format!("{}:{}:ro", mount.host_path, mount.container_path));
        } else {
            args.push("-v".to_string());
            args.push(format!("{}:{}", mount.host_path, mount.container_path));
        }

        // Overlay excluded subdirectories with empty tmpfs
        for subdir in &mount.exclude {
            args.push("--mount".to_string());
            args.push(format!(
                "type=tmpfs,destination={}/{},tmpfs-size=0",
                mount.container_path, subdir
            ));
        }
    }

    args.push(image.to_string());

    args
}

#[cfg(unix)]
fn nix_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(unix)]
fn nix_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn read_env_file_parses_key_value_pairs() {
        let tmp = TempDir::new().unwrap();
        let env_path = tmp.path().join(".env");
        fs::write(
            &env_path,
            "# comment\nANTHROPIC_API_KEY=sk-test-123\nIRRELEVANT=ignored\n",
        )
        .unwrap();

        let result = read_env_file(&env_path, &["ANTHROPIC_API_KEY"]);
        assert_eq!(result.get("ANTHROPIC_API_KEY").map(|s| s.as_str()), Some("sk-test-123"));
        assert!(!result.contains_key("IRRELEVANT"));
    }

    #[test]
    fn read_env_file_strips_quotes() {
        let tmp = TempDir::new().unwrap();
        let env_path = tmp.path().join(".env");
        fs::write(&env_path, "KEY1=\"quoted\"\nKEY2='single'\n").unwrap();

        let result = read_env_file(&env_path, &["KEY1", "KEY2"]);
        assert_eq!(result.get("KEY1").map(|s| s.as_str()), Some("quoted"));
        assert_eq!(result.get("KEY2").map(|s| s.as_str()), Some("single"));
    }

    #[test]
    fn read_env_file_missing_file_returns_empty() {
        let result = read_env_file(Path::new("/nonexistent/.env"), &["KEY"]);
        assert!(result.is_empty());
    }

    #[test]
    fn read_env_file_skips_empty_values() {
        let tmp = TempDir::new().unwrap();
        let env_path = tmp.path().join(".env");
        fs::write(&env_path, "EMPTY=\nVALID=yes\n").unwrap();

        let result = read_env_file(&env_path, &["EMPTY", "VALID"]);
        assert!(!result.contains_key("EMPTY"));
        assert_eq!(result.get("VALID").map(|s| s.as_str()), Some("yes"));
    }

    #[test]
    fn build_container_args_includes_mounts_and_excludes() {
        use intercom_core::VolumeMount;

        let mounts = vec![
            VolumeMount {
                host_path: "/home/mk/project".to_string(),
                container_path: "/workspace/project".to_string(),
                readonly: true,
                exclude: vec!["node_modules".to_string()],
            },
            VolumeMount {
                host_path: "/home/mk/data".to_string(),
                container_path: "/workspace/group".to_string(),
                readonly: false,
                exclude: vec![],
            },
        ];

        let args = build_container_args(&mounts, "test-container", "nanoclaw-agent:latest", "UTC");

        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"--name".to_string()));
        assert!(args.contains(&"test-container".to_string()));
        assert!(args.contains(&"TZ=UTC".to_string()));
        assert!(args.contains(&"/home/mk/project:/workspace/project:ro".to_string()));
        assert!(args.contains(&"/home/mk/data:/workspace/group".to_string()));
        assert!(args.contains(&"type=tmpfs,destination=/workspace/project/node_modules,tmpfs-size=0".to_string()));
        assert!(args.last() == Some(&"nanoclaw-agent:latest".to_string()));
    }
}
