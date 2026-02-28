//! Volume mount builder: constructs the mount list for container execution.
//!
//! Port of `buildVolumeMounts()` from container-runner.ts.

use std::fs;
use std::path::Path;

use intercom_core::{RuntimeKind, VolumeMount, runner_container_path, runner_dir_name};
use tracing::debug;

use super::security::{ContainerConfig, MountAllowlist, validate_additional_mounts};

/// Registered group information needed for mount building.
pub struct GroupInfo {
    pub folder: String,
    pub name: String,
    pub container_config: Option<ContainerConfig>,
}

/// Build the volume mount list for a container invocation.
///
/// Mount structure:
/// - Main: project root (ro) + group folder (rw) + global (if exists)
/// - Non-main: group folder (rw) + global (ro)
/// - Claude: per-group `.claude/` sessions directory
/// - All: per-group IPC namespace, runner source (ro), shared source (non-Claude)
/// - Additional mounts from group config (validated against allowlist)
pub fn build_volume_mounts(
    group: &GroupInfo,
    is_main: bool,
    runtime: RuntimeKind,
    project_root: &Path,
    groups_dir: &Path,
    data_dir: &Path,
    allowlist: Option<&MountAllowlist>,
) -> Vec<VolumeMount> {
    let mut mounts = Vec::new();
    let group_dir = groups_dir.join(&group.folder);

    if is_main {
        // Main gets the project root read-only.
        mounts.push(VolumeMount {
            host_path: project_root.to_string_lossy().to_string(),
            container_path: "/workspace/project".to_string(),
            readonly: true,
            exclude: vec![],
        });

        // Main also gets its group folder as the working directory.
        fs::create_dir_all(&group_dir).ok();
        mounts.push(VolumeMount {
            host_path: group_dir.to_string_lossy().to_string(),
            container_path: "/workspace/group".to_string(),
            readonly: false,
            exclude: vec![],
        });
    } else {
        // Other groups only get their own folder.
        fs::create_dir_all(&group_dir).ok();
        mounts.push(VolumeMount {
            host_path: group_dir.to_string_lossy().to_string(),
            container_path: "/workspace/group".to_string(),
            readonly: false,
            exclude: vec![],
        });

        // Global memory directory (read-only for non-main).
        let global_dir = groups_dir.join("global");
        if global_dir.exists() {
            mounts.push(VolumeMount {
                host_path: global_dir.to_string_lossy().to_string(),
                container_path: "/workspace/global".to_string(),
                readonly: true,
                exclude: vec![],
            });
        }
    }

    // Claude runtime: per-group .claude/ sessions directory with settings and skills.
    if runtime == RuntimeKind::Claude {
        let sessions_dir = data_dir
            .join("sessions")
            .join(&group.folder)
            .join(".claude");
        fs::create_dir_all(&sessions_dir).ok();

        // Create default settings file if missing.
        let settings_file = sessions_dir.join("settings.json");
        if !settings_file.exists() {
            let default_settings = serde_json::json!({
                "env": {
                    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1",
                    "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD": "1",
                    "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "0"
                }
            });
            fs::write(
                &settings_file,
                serde_json::to_string_pretty(&default_settings).unwrap() + "\n",
            )
            .ok();
        }

        // Sync skills from container/skills/ into each group's .claude/skills/.
        let skills_src = project_root.join("container").join("skills");
        if skills_src.exists() {
            let skills_dst = sessions_dir.join("skills");
            if let Ok(entries) = fs::read_dir(&skills_src) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        let src_dir = entry.path();
                        let dst_dir = skills_dst.join(entry.file_name());
                        copy_dir_recursive(&src_dir, &dst_dir);
                    }
                }
            }
        }

        mounts.push(VolumeMount {
            host_path: sessions_dir.to_string_lossy().to_string(),
            container_path: "/home/node/.claude".to_string(),
            readonly: false,
            exclude: vec![],
        });
    }

    // Per-group IPC namespace.
    let ipc_dir = data_dir.join("ipc").join(&group.folder);
    for sub in &["messages", "tasks", "input", "queries", "responses"] {
        fs::create_dir_all(ipc_dir.join(sub)).ok();
    }
    mounts.push(VolumeMount {
        host_path: ipc_dir.to_string_lossy().to_string(),
        container_path: "/workspace/ipc".to_string(),
        readonly: false,
        exclude: vec![],
    });

    // Mount agent-runner source from host (recompiled on container startup).
    let runner_src = project_root
        .join("container")
        .join(runner_dir_name(runtime))
        .join("src");
    if runner_src.exists() {
        mounts.push(VolumeMount {
            host_path: runner_src.to_string_lossy().to_string(),
            container_path: runner_container_path(runtime),
            readonly: true,
            exclude: vec![],
        });
    }

    // Non-Claude runtimes also need the shared code mounted.
    if runtime != RuntimeKind::Claude {
        let shared_src = project_root.join("container").join("shared");
        if shared_src.exists() {
            mounts.push(VolumeMount {
                host_path: shared_src.to_string_lossy().to_string(),
                container_path: "/app/shared".to_string(),
                readonly: true,
                exclude: vec![],
            });
        }
    }

    // Additional mounts validated against external allowlist.
    if let Some(ref config) = group.container_config {
        if !config.additional_mounts.is_empty() {
            if let Some(allowlist) = allowlist {
                let validated = validate_additional_mounts(
                    &config.additional_mounts,
                    &group.name,
                    is_main,
                    allowlist,
                );
                for vm in validated {
                    mounts.push(VolumeMount {
                        host_path: vm.host_path,
                        container_path: vm.container_path,
                        readonly: vm.readonly,
                        exclude: vm.exclude,
                    });
                }
            } else {
                debug!(
                    group = %group.name,
                    count = config.additional_mounts.len(),
                    "Skipping additional mounts — no allowlist loaded"
                );
            }
        }
    }

    mounts
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).ok();
    if let Ok(entries) = fs::read_dir(src) {
        for entry in entries.flatten() {
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if src_path.is_dir() {
                copy_dir_recursive(&src_path, &dst_path);
            } else {
                fs::copy(&src_path, &dst_path).ok();
            }
        }
    }
}

/// Generate a safe container name from group folder and timestamp.
pub fn container_name(group_folder: &str) -> String {
    let safe_name: String = group_folder
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("intercom-{}-{}", safe_name, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_project_dirs(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let project_root = tmp.path().join("project");
        let groups_dir = tmp.path().join("groups");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&groups_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        (project_root, groups_dir, data_dir)
    }

    #[test]
    fn main_group_gets_project_root_and_group_dir() {
        let tmp = TempDir::new().unwrap();
        let (project_root, groups_dir, data_dir) = setup_project_dirs(&tmp);

        let group = GroupInfo {
            folder: "main".to_string(),
            name: "Main Group".to_string(),
            container_config: None,
        };

        let mounts = build_volume_mounts(
            &group,
            true,
            RuntimeKind::Claude,
            &project_root,
            &groups_dir,
            &data_dir,
            None,
        );

        // Should have project root (ro), group dir (rw), .claude sessions, IPC
        let project_mount = mounts.iter().find(|m| m.container_path == "/workspace/project");
        assert!(project_mount.is_some());
        assert!(project_mount.unwrap().readonly);

        let group_mount = mounts.iter().find(|m| m.container_path == "/workspace/group");
        assert!(group_mount.is_some());
        assert!(!group_mount.unwrap().readonly);
    }

    #[test]
    fn non_main_group_gets_global_memory() {
        let tmp = TempDir::new().unwrap();
        let (project_root, groups_dir, data_dir) = setup_project_dirs(&tmp);

        // Create global directory
        fs::create_dir_all(groups_dir.join("global")).unwrap();

        let group = GroupInfo {
            folder: "team-eng".to_string(),
            name: "Engineering".to_string(),
            container_config: None,
        };

        let mounts = build_volume_mounts(
            &group,
            false,
            RuntimeKind::Claude,
            &project_root,
            &groups_dir,
            &data_dir,
            None,
        );

        let global_mount = mounts.iter().find(|m| m.container_path == "/workspace/global");
        assert!(global_mount.is_some());
        assert!(global_mount.unwrap().readonly);

        // Non-main should NOT have project root mount
        let project_mount = mounts.iter().find(|m| m.container_path == "/workspace/project");
        assert!(project_mount.is_none());
    }

    #[test]
    fn claude_runtime_creates_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let (project_root, groups_dir, data_dir) = setup_project_dirs(&tmp);

        let group = GroupInfo {
            folder: "main".to_string(),
            name: "Main".to_string(),
            container_config: None,
        };

        let mounts = build_volume_mounts(
            &group,
            true,
            RuntimeKind::Claude,
            &project_root,
            &groups_dir,
            &data_dir,
            None,
        );

        let claude_mount = mounts.iter().find(|m| m.container_path == "/home/node/.claude");
        assert!(claude_mount.is_some());

        // Settings file should have been created
        let settings_path = data_dir.join("sessions/main/.claude/settings.json");
        assert!(settings_path.exists());
    }

    #[test]
    fn non_claude_runtime_skips_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let (project_root, groups_dir, data_dir) = setup_project_dirs(&tmp);

        let group = GroupInfo {
            folder: "main".to_string(),
            name: "Main".to_string(),
            container_config: None,
        };

        let mounts = build_volume_mounts(
            &group,
            true,
            RuntimeKind::Gemini,
            &project_root,
            &groups_dir,
            &data_dir,
            None,
        );

        let claude_mount = mounts.iter().find(|m| m.container_path == "/home/node/.claude");
        assert!(claude_mount.is_none());
    }

    #[test]
    fn ipc_directories_created() {
        let tmp = TempDir::new().unwrap();
        let (project_root, groups_dir, data_dir) = setup_project_dirs(&tmp);

        let group = GroupInfo {
            folder: "main".to_string(),
            name: "Main".to_string(),
            container_config: None,
        };

        build_volume_mounts(
            &group,
            true,
            RuntimeKind::Claude,
            &project_root,
            &groups_dir,
            &data_dir,
            None,
        );

        let ipc_base = data_dir.join("ipc/main");
        assert!(ipc_base.join("messages").exists());
        assert!(ipc_base.join("tasks").exists());
        assert!(ipc_base.join("input").exists());
        assert!(ipc_base.join("queries").exists());
        assert!(ipc_base.join("responses").exists());
    }

    #[test]
    fn container_name_sanitizes_folder() {
        let name = container_name("team.eng/special");
        assert!(name.starts_with("intercom-team-eng-special-"));
        assert!(!name.contains('.'));
        assert!(!name.contains('/'));
    }
}
