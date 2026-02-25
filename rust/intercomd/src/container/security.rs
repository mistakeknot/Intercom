//! Mount security: validates additional container mounts against an external allowlist.
//!
//! The allowlist lives OUTSIDE the project root (`~/.config/intercom/mount-allowlist.json`)
//! so container agents cannot modify security configuration.
//!
//! Port of `src/mount-security.ts`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Default blocked patterns — paths that should never be mounted.
const DEFAULT_BLOCKED_PATTERNS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".gpg",
    ".aws",
    ".azure",
    ".gcloud",
    ".kube",
    ".docker",
    "credentials",
    ".env",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    "private_key",
    ".secret",
    "/wm",
];

/// Paths that are unconditionally blocked regardless of allowlist.
const HARD_BLOCKED_ROOTS: &[&str] = &["/wm"];

/// External mount allowlist configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountAllowlist {
    pub allowed_roots: Vec<AllowedRoot>,
    pub blocked_patterns: Vec<String>,
    pub non_main_read_only: bool,
}

/// A root directory that may be mounted into containers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedRoot {
    pub path: String,
    pub allow_read_write: bool,
    #[serde(default)]
    pub description: Option<String>,
}

/// Additional mount request from group configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalMount {
    pub host_path: String,
    #[serde(default)]
    pub container_path: Option<String>,
    #[serde(default = "default_true")]
    pub readonly: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Container configuration from group registration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfig {
    #[serde(default)]
    pub additional_mounts: Vec<AdditionalMount>,
    pub timeout: Option<u64>,
}

/// Result of validating a single mount.
#[derive(Debug)]
pub struct MountValidationResult {
    pub allowed: bool,
    pub reason: String,
    pub real_host_path: Option<String>,
    pub resolved_container_path: Option<String>,
    pub effective_readonly: Option<bool>,
}

/// Validated mount ready for container arg construction.
#[derive(Debug, Clone)]
pub struct ValidatedMount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
    pub exclude: Vec<String>,
}

/// Default allowlist path.
pub fn default_allowlist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".config/intercom/mount-allowlist.json")
}

/// Load the mount allowlist from the external config location.
pub fn load_allowlist(path: &Path) -> Option<MountAllowlist> {
    if !path.exists() {
        warn!(
            path = %path.display(),
            "Mount allowlist not found — additional mounts will be BLOCKED"
        );
        return None;
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "Failed to read mount allowlist — additional mounts will be BLOCKED"
            );
            return None;
        }
    };

    let mut allowlist: MountAllowlist = match serde_json::from_str(&content) {
        Ok(a) => a,
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "Failed to parse mount allowlist — additional mounts will be BLOCKED"
            );
            return None;
        }
    };

    // Merge default blocked patterns with user-configured ones.
    let mut merged: Vec<String> = DEFAULT_BLOCKED_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for pattern in &allowlist.blocked_patterns {
        if !merged.contains(pattern) {
            merged.push(pattern.clone());
        }
    }
    allowlist.blocked_patterns = merged;

    info!(
        path = %path.display(),
        allowed_roots = allowlist.allowed_roots.len(),
        blocked_patterns = allowlist.blocked_patterns.len(),
        "Mount allowlist loaded"
    );

    Some(allowlist)
}

/// Expand `~` to home directory and resolve to absolute path.
fn expand_path(p: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    if p == "~" {
        PathBuf::from(&home)
    } else if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(&home).join(rest)
    } else {
        PathBuf::from(p).canonicalize().unwrap_or_else(|_| PathBuf::from(p))
    }
}

/// Get the real (canonical) path, resolving symlinks.
fn real_path(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// Check if any path component matches a blocked pattern.
fn matches_blocked_pattern(real: &Path, patterns: &[String]) -> Option<String> {
    let real_str = real.to_string_lossy();
    for pattern in patterns {
        // Check individual path components
        for component in real.components() {
            let part = component.as_os_str().to_string_lossy();
            if part == pattern.as_str() || part.contains(pattern.as_str()) {
                return Some(pattern.clone());
            }
        }
        // Also check full path
        if real_str.contains(pattern.as_str()) {
            return Some(pattern.clone());
        }
    }
    None
}

/// Check if a real path is under a hard-blocked root.
fn is_hard_blocked(real: &Path) -> bool {
    let normalized = real.to_string_lossy();
    HARD_BLOCKED_ROOTS.iter().any(|root| {
        normalized == *root || normalized.starts_with(&format!("{}/", root))
    })
}

/// Check if a real path is under an allowed root.
fn find_allowed_root<'a>(real: &Path, roots: &'a [AllowedRoot]) -> Option<&'a AllowedRoot> {
    for root in roots {
        let expanded = expand_path(&root.path);
        let real_root = match real_path(&expanded) {
            Some(r) => r,
            None => continue,
        };
        if let Ok(relative) = real.strip_prefix(&real_root) {
            // strip_prefix succeeds only when real is under real_root
            let _ = relative; // just need the check to succeed
            return Some(root);
        }
    }
    None
}

/// Validate container path to prevent escaping `/workspace/extra/`.
fn is_valid_container_path(p: &str) -> bool {
    !p.is_empty() && !p.contains("..") && !p.starts_with('/')
}

/// Validate a single additional mount against the allowlist.
pub fn validate_mount(
    mount: &AdditionalMount,
    is_main: bool,
    allowlist: &MountAllowlist,
) -> MountValidationResult {
    // Derive container path from host path basename if not specified
    let container_path = mount
        .container_path
        .as_deref()
        .unwrap_or_else(|| {
            Path::new(&mount.host_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("mount")
        })
        .to_string();

    if !is_valid_container_path(&container_path) {
        return MountValidationResult {
            allowed: false,
            reason: format!(
                "Invalid container path: \"{}\" — must be relative, non-empty, and not contain \"..\"",
                container_path
            ),
            real_host_path: None,
            resolved_container_path: None,
            effective_readonly: None,
        };
    }

    let expanded = expand_path(&mount.host_path);
    if is_hard_blocked(&expanded) {
        return MountValidationResult {
            allowed: false,
            reason: format!("Path \"{}\" is blocked by hard policy", expanded.display()),
            real_host_path: None,
            resolved_container_path: None,
            effective_readonly: None,
        };
    }

    let real = match real_path(&expanded) {
        Some(r) => r,
        None => {
            return MountValidationResult {
                allowed: false,
                reason: format!(
                    "Host path does not exist: \"{}\" (expanded: \"{}\")",
                    mount.host_path,
                    expanded.display()
                ),
                real_host_path: None,
                resolved_container_path: None,
                effective_readonly: None,
            };
        }
    };

    if is_hard_blocked(&real) {
        return MountValidationResult {
            allowed: false,
            reason: format!("Path \"{}\" is blocked by hard policy", real.display()),
            real_host_path: None,
            resolved_container_path: None,
            effective_readonly: None,
        };
    }

    if let Some(pattern) = matches_blocked_pattern(&real, &allowlist.blocked_patterns) {
        return MountValidationResult {
            allowed: false,
            reason: format!(
                "Path matches blocked pattern \"{}\": \"{}\"",
                pattern,
                real.display()
            ),
            real_host_path: None,
            resolved_container_path: None,
            effective_readonly: None,
        };
    }

    let allowed_root = match find_allowed_root(&real, &allowlist.allowed_roots) {
        Some(r) => r,
        None => {
            let roots_list: Vec<String> = allowlist
                .allowed_roots
                .iter()
                .map(|r| expand_path(&r.path).display().to_string())
                .collect();
            return MountValidationResult {
                allowed: false,
                reason: format!(
                    "Path \"{}\" is not under any allowed root. Allowed: {}",
                    real.display(),
                    roots_list.join(", ")
                ),
                real_host_path: None,
                resolved_container_path: None,
                effective_readonly: None,
            };
        }
    };

    // Determine effective readonly status
    let requested_read_write = !mount.readonly;
    let effective_readonly = if requested_read_write {
        if !is_main && allowlist.non_main_read_only {
            info!(mount = %mount.host_path, "Mount forced to read-only for non-main group");
            true
        } else if !allowed_root.allow_read_write {
            info!(
                mount = %mount.host_path,
                root = %allowed_root.path,
                "Mount forced to read-only — root does not allow read-write"
            );
            true
        } else {
            false
        }
    } else {
        true
    };

    MountValidationResult {
        allowed: true,
        reason: format!(
            "Allowed under root \"{}\"{}",
            allowed_root.path,
            allowed_root
                .description
                .as_deref()
                .map(|d| format!(" ({})", d))
                .unwrap_or_default()
        ),
        real_host_path: Some(real.to_string_lossy().to_string()),
        resolved_container_path: Some(container_path),
        effective_readonly: Some(effective_readonly),
    }
}

/// Validate all additional mounts for a group.
/// Returns only mounts that passed validation.
pub fn validate_additional_mounts(
    mounts: &[AdditionalMount],
    group_name: &str,
    is_main: bool,
    allowlist: &MountAllowlist,
) -> Vec<ValidatedMount> {
    let mut validated = Vec::new();

    for mount in mounts {
        let result = validate_mount(mount, is_main, allowlist);

        if result.allowed {
            validated.push(ValidatedMount {
                host_path: result.real_host_path.unwrap(),
                container_path: format!(
                    "/workspace/extra/{}",
                    result.resolved_container_path.unwrap()
                ),
                readonly: result.effective_readonly.unwrap(),
                exclude: mount.exclude.clone(),
            });
        } else {
            warn!(
                group = group_name,
                requested_path = %mount.host_path,
                reason = %result.reason,
                "Additional mount REJECTED"
            );
        }
    }

    validated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn test_allowlist(tmp: &TempDir) -> MountAllowlist {
        MountAllowlist {
            allowed_roots: vec![AllowedRoot {
                path: tmp.path().to_string_lossy().to_string(),
                allow_read_write: true,
                description: Some("test root".to_string()),
            }],
            blocked_patterns: DEFAULT_BLOCKED_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            non_main_read_only: true,
        }
    }

    #[test]
    fn allows_path_under_allowed_root() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("project");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: Some("project".to_string()),
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(result.allowed, "reason: {}", result.reason);
        assert_eq!(result.resolved_container_path.as_deref(), Some("project"));
    }

    #[test]
    fn blocks_path_not_under_allowed_root() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let sub = other.path().join("secret");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: None,
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(!result.allowed);
        assert!(result.reason.contains("not under any allowed root"));
    }

    #[test]
    fn blocks_ssh_directory() {
        let tmp = TempDir::new().unwrap();
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir_all(&ssh_dir).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: ssh_dir.to_string_lossy().to_string(),
            container_path: None,
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(!result.allowed);
        assert!(result.reason.contains(".ssh"));
    }

    #[test]
    fn blocks_path_traversal_in_container_path() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("ok");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: Some("../../etc/passwd".to_string()),
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(!result.allowed);
        assert!(result.reason.contains(".."));
    }

    #[test]
    fn non_main_forced_read_only() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("data");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: Some("data".to_string()),
            readonly: false, // requests read-write
            exclude: vec![],
        };

        let result = validate_mount(&mount, false, &allowlist);
        assert!(result.allowed);
        assert_eq!(result.effective_readonly, Some(true)); // forced read-only
    }

    #[test]
    fn main_gets_read_write() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("data");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: Some("data".to_string()),
            readonly: false,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(result.allowed);
        assert_eq!(result.effective_readonly, Some(false));
    }

    #[test]
    fn nonexistent_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: "/nonexistent/path/to/nowhere".to_string(),
            container_path: None,
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(!result.allowed);
        assert!(result.reason.contains("does not exist"));
    }

    #[test]
    fn validate_additional_mounts_filters_invalid() {
        let tmp = TempDir::new().unwrap();
        let good = tmp.path().join("good");
        fs::create_dir_all(&good).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mounts = vec![
            AdditionalMount {
                host_path: good.to_string_lossy().to_string(),
                container_path: Some("good".to_string()),
                readonly: true,
                exclude: vec![],
            },
            AdditionalMount {
                host_path: "/nonexistent".to_string(),
                container_path: None,
                readonly: true,
                exclude: vec![],
            },
        ];

        let validated = validate_additional_mounts(&mounts, "test-group", true, &allowlist);
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].container_path, "/workspace/extra/good");
    }

    #[test]
    fn container_path_defaults_to_basename() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("my-project");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: None,
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(result.allowed);
        assert_eq!(result.resolved_container_path.as_deref(), Some("my-project"));
    }

    #[test]
    fn absolute_container_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("ok");
        fs::create_dir_all(&sub).unwrap();
        let allowlist = test_allowlist(&tmp);

        let mount = AdditionalMount {
            host_path: sub.to_string_lossy().to_string(),
            container_path: Some("/etc/bad".to_string()),
            readonly: true,
            exclude: vec![],
        };

        let result = validate_mount(&mount, true, &allowlist);
        assert!(!result.allowed);
    }
}
