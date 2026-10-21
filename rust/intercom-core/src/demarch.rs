use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::DemarchConfig;

const STANDALONE_MSG: &str =
    "Demarch kernel not available — Intercom is running in standalone mode.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DemarchStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DemarchResponse {
    pub status: DemarchStatus,
    pub result: String,
}

impl DemarchResponse {
    pub fn ok(result: impl Into<String>) -> Self {
        Self {
            status: DemarchStatus::Ok,
            result: result.into(),
        }
    }

    pub fn error(result: impl Into<String>) -> Self {
        Self {
            status: DemarchStatus::Error,
            result: result.into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReadOperation {
    RunStatus {
        run_id: Option<String>,
    },
    /// Phase of `run_id`, or of the project's active run when `None`.
    SprintPhase {
        #[serde(default)]
        run_id: Option<String>,
    },
    SearchBeads {
        id: Option<String>,
        query: Option<String>,
        status: Option<String>,
    },
    /// Artifacts of `run_id` (or the active run); one artifact when
    /// `artifact_id` is set.
    SpecLookup {
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default)]
        artifact_id: Option<String>,
    },
    ReviewSummary,
    NextWork,
    RunEvents {
        limit: Option<u32>,
        since: Option<String>,
    },
    RunTokens {
        run_id: String,
    },
    DispatchList {
        #[serde(default)]
        active_only: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WriteOperation {
    CreateIssue {
        title: String,
        description: Option<String>,
        priority: Option<String>,
        issue_type: Option<String>,
        labels: Option<Vec<String>>,
    },
    UpdateIssue {
        id: String,
        status: Option<String>,
        priority: Option<String>,
        title: Option<String>,
        description: Option<String>,
        notes: Option<String>,
    },
    CloseIssue {
        id: String,
        reason: Option<String>,
    },
    StartRun {
        title: Option<String>,
        description: Option<String>,
    },
    ApproveGate {
        gate_id: Option<String>,
        reason: Option<String>,
    },
    RejectGate {
        gate_id: Option<String>,
        reason: Option<String>,
    },
    DeferGate {
        gate_id: Option<String>,
        reason: Option<String>,
    },
    ExtendBudget {
        run_id: String,
        max_dispatches: Option<u32>,
    },
    CancelRun {
        run_id: String,
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemarchCommandPlan {
    pub bin: &'static str,
    pub signature: &'static str,
    pub args: Vec<String>,
    /// Optional stdin payload (e.g. for `ic state set` which reads JSON from stdin).
    pub stdin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DemarchAdapter {
    config: DemarchConfig,
    project_root: PathBuf,
    /// Root of the Demarch monorepo (where `.beads/` and `.clavain/` live).
    /// `bd` and `ic` must run from here, not from the intercom subdirectory.
    monorepo_root: PathBuf,
}

impl DemarchAdapter {
    pub fn new(config: DemarchConfig, project_root: impl AsRef<Path>) -> Self {
        let project_root = project_root.as_ref().to_path_buf();
        let monorepo_root = Self::find_monorepo_root(&project_root);
        Self {
            config,
            project_root,
            monorepo_root,
        }
    }

    /// Walk up from `start` looking for a directory containing `.beads/`.
    /// Falls back to `start` if not found (standalone mode).
    fn find_monorepo_root(start: &Path) -> PathBuf {
        let mut dir = if start.is_absolute() {
            start.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(start)
        };
        loop {
            if dir.join(".beads").is_dir() {
                return dir;
            }
            if !dir.pop() {
                break;
            }
        }
        start.to_path_buf()
    }

    pub fn execute_read(&self, operation: ReadOperation) -> DemarchResponse {
        if !self.config.enabled {
            return DemarchResponse::error("Demarch integration is disabled.");
        }

        match operation {
            ReadOperation::ReviewSummary => self.handle_review_summary(),
            ReadOperation::SprintPhase { run_id } => self.handle_sprint_phase(run_id),
            ReadOperation::SpecLookup {
                run_id,
                artifact_id,
            } => self.handle_spec_lookup(run_id, artifact_id),
            op => match Self::plan_read(&op) {
                Some(plan) => self.execute_plan(plan, false),
                None => DemarchResponse::error("Read operation is not implemented."),
            },
        }
    }

    pub fn execute_write(&self, operation: WriteOperation, is_main: bool) -> DemarchResponse {
        if !self.config.enabled {
            return DemarchResponse::error("Demarch integration is disabled.");
        }

        if self.config.require_main_group_for_writes && !is_main {
            return DemarchResponse::error("Write operation requires main group privileges.");
        }

        let plan = Self::plan_write(&operation);
        self.execute_plan(plan, true)
    }

    /// Plan the single ic/bd call for a read. Returns `None` for reads that
    /// are not one command: `ReviewSummary`, and `SprintPhase`/`SpecLookup`
    /// without a run id (`execute_read` resolves the active run first).
    pub fn plan_read(operation: &ReadOperation) -> Option<DemarchCommandPlan> {
        match operation {
            ReadOperation::RunStatus { run_id } => {
                let args = if let Some(run_id) = run_id {
                    vec![
                        "run".to_string(),
                        "status".to_string(),
                        run_id.clone(),
                        "--json".to_string(),
                    ]
                } else {
                    vec![
                        "run".to_string(),
                        "current".to_string(),
                        "--json".to_string(),
                    ]
                };

                let signature = if run_id.is_some() {
                    "ic run status --json"
                } else {
                    "ic run current --json"
                };

                Some(DemarchCommandPlan {
                    bin: "ic",
                    signature,
                    args,
                    stdin: None,
                })
            }
            // `ic run phase <id>` prints the bare phase name; it has no JSON
            // form. The signature keeps its historical allowlist spelling.
            ReadOperation::SprintPhase { run_id } => run_id.as_ref().map(|run_id| DemarchCommandPlan {
                bin: "ic",
                signature: "ic run phase --json",
                args: vec!["run".to_string(), "phase".to_string(), run_id.clone()],
                stdin: None,
            }),
            ReadOperation::SearchBeads { id, query, status } => {
                if let Some(id) = id {
                    return Some(DemarchCommandPlan {
                        bin: "bd",
                        signature: "bd show --json",
                        args: vec!["show".to_string(), id.clone(), "--json".to_string()],
                        stdin: None,
                    });
                }

                let mut args = vec!["list".to_string(), "--json".to_string()];
                if let Some(status) = status {
                    args.push(format!("--status={status}"));
                }
                if let Some(query) = query {
                    args.push(format!("--search={query}"));
                }

                Some(DemarchCommandPlan {
                    bin: "bd",
                    signature: "bd list --json",
                    args,
                    stdin: None,
                })
            }
            // ic has no `run artifact get`; a single artifact is selected
            // from this list by `handle_spec_lookup`.
            ReadOperation::SpecLookup { run_id, .. } => run_id.as_ref().map(|run_id| DemarchCommandPlan {
                bin: "ic",
                signature: "ic run artifact list --json",
                args: vec![
                    "run".to_string(),
                    "artifact".to_string(),
                    "list".to_string(),
                    run_id.clone(),
                    "--json".to_string(),
                ],
                stdin: None,
            }),
            ReadOperation::ReviewSummary => None,
            ReadOperation::NextWork => Some(DemarchCommandPlan {
                bin: "bd",
                signature: "bd ready --json",
                args: vec!["ready".to_string(), "--json".to_string()],
                stdin: None,
            }),
            ReadOperation::RunEvents { limit, since } => {
                let mut args = vec![
                    "events".to_string(),
                    "tail".to_string(),
                    "--consumer=intercom".to_string(),
                    "--json".to_string(),
                    format!("--limit={}", limit.unwrap_or(20)),
                ];
                if let Some(since) = since {
                    args.push(format!("--since={since}"));
                }

                Some(DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic events tail --consumer=intercom --json",
                    args,
                    stdin: None,
                })
            }
            ReadOperation::RunTokens { run_id } => Some(DemarchCommandPlan {
                bin: "ic",
                signature: "ic run tokens --json",
                args: vec![
                    "run".to_string(),
                    "tokens".to_string(),
                    run_id.clone(),
                    "--json".to_string(),
                ],
                stdin: None,
            }),
            ReadOperation::DispatchList { active_only } => {
                let mut args = vec![
                    "dispatch".to_string(),
                    "list".to_string(),
                    "--json".to_string(),
                ];
                if *active_only {
                    args.push("--active".to_string());
                }

                Some(DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic dispatch list --json",
                    args,
                    stdin: None,
                })
            }
        }
    }

    pub fn plan_write(operation: &WriteOperation) -> DemarchCommandPlan {
        match operation {
            WriteOperation::CreateIssue {
                title,
                description,
                priority,
                issue_type,
                labels,
            } => {
                let mut args = vec![
                    "create".to_string(),
                    "--title".to_string(),
                    title.clone(),
                    "--json".to_string(),
                ];
                if let Some(description) = description {
                    args.push("--description".to_string());
                    args.push(description.clone());
                }
                if let Some(priority) = priority {
                    args.push("--priority".to_string());
                    args.push(priority.clone());
                }
                if let Some(issue_type) = issue_type {
                    args.push("--type".to_string());
                    args.push(issue_type.clone());
                }
                if let Some(labels) = labels {
                    if !labels.is_empty() {
                        args.push("--labels".to_string());
                        args.push(labels.join(","));
                    }
                }

                DemarchCommandPlan {
                    bin: "bd",
                    signature: "bd create --json",
                    args,
                    stdin: None,
                }
            }
            WriteOperation::UpdateIssue {
                id,
                status,
                priority,
                title,
                description,
                notes,
            } => {
                let mut args = vec!["update".to_string(), id.clone(), "--json".to_string()];
                if let Some(status) = status {
                    args.push("--status".to_string());
                    args.push(status.clone());
                }
                if let Some(priority) = priority {
                    args.push("--priority".to_string());
                    args.push(priority.clone());
                }
                if let Some(title) = title {
                    args.push("--title".to_string());
                    args.push(title.clone());
                }
                if let Some(description) = description {
                    args.push("--description".to_string());
                    args.push(description.clone());
                }
                if let Some(notes) = notes {
                    args.push("--notes".to_string());
                    args.push(notes.clone());
                }

                DemarchCommandPlan {
                    bin: "bd",
                    signature: "bd update --json",
                    args,
                    stdin: None,
                }
            }
            WriteOperation::CloseIssue { id, reason } => {
                let mut args = vec!["close".to_string(), id.clone(), "--json".to_string()];
                if let Some(reason) = reason {
                    args.push("--reason".to_string());
                    args.push(reason.clone());
                }

                DemarchCommandPlan {
                    bin: "bd",
                    signature: "bd close --json",
                    args,
                    stdin: None,
                }
            }
            WriteOperation::StartRun { title, description } => {
                let mut args = vec![
                    "run".to_string(),
                    "create".to_string(),
                    "--json".to_string(),
                ];
                if let Some(title) = title {
                    args.push("--title".to_string());
                    args.push(title.clone());
                }
                if let Some(description) = description {
                    args.push("--description".to_string());
                    args.push(description.clone());
                }

                DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic run create --json",
                    args,
                    stdin: None,
                }
            }
            WriteOperation::ApproveGate { gate_id, reason } => {
                let mut args = vec![
                    "gate".to_string(),
                    "override".to_string(),
                    "--json".to_string(),
                ];
                if let Some(gate_id) = gate_id {
                    args.push(gate_id.clone());
                }
                if let Some(reason) = reason {
                    args.push("--reason".to_string());
                    args.push(reason.clone());
                }

                DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic gate override --json",
                    args,
                    stdin: None,
                }
            }
            WriteOperation::RejectGate { gate_id, reason } => {
                // Gate rejection = record decision via ic state set.
                // The gate stays failed (no override). The state record
                // provides audit trail visible to event consumers.
                let scope = gate_id
                    .as_deref()
                    .unwrap_or("unknown")
                    .to_string();
                let reason_text = reason
                    .as_deref()
                    .unwrap_or("Rejected via Intercom");
                let payload = serde_json::json!({
                    "action": "reject",
                    "reason": reason_text,
                })
                .to_string();

                DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic state set --json",
                    args: vec![
                        "state".to_string(),
                        "set".to_string(),
                        "gate-decision".to_string(),
                        scope,
                        "--json".to_string(),
                    ],
                    stdin: Some(payload),
                }
            }
            WriteOperation::DeferGate { gate_id, reason } => {
                let scope = gate_id
                    .as_deref()
                    .unwrap_or("unknown")
                    .to_string();
                let reason_text = reason
                    .as_deref()
                    .unwrap_or("Deferred via Intercom");
                let payload = serde_json::json!({
                    "action": "defer",
                    "reason": reason_text,
                })
                .to_string();

                DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic state set --json",
                    args: vec![
                        "state".to_string(),
                        "set".to_string(),
                        "gate-decision".to_string(),
                        scope,
                        "--json".to_string(),
                    ],
                    stdin: Some(payload),
                }
            }
            WriteOperation::ExtendBudget {
                run_id,
                max_dispatches,
            } => {
                let mut args = vec![
                    "run".to_string(),
                    "set".to_string(),
                    run_id.clone(),
                    "--json".to_string(),
                ];
                if let Some(n) = max_dispatches {
                    args.push(format!("--max-dispatches={n}"));
                }

                DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic run set --json",
                    args,
                    stdin: None,
                }
            }
            WriteOperation::CancelRun { run_id, reason } => {
                let mut args = vec![
                    "run".to_string(),
                    "cancel".to_string(),
                    run_id.clone(),
                    "--json".to_string(),
                ];
                if let Some(reason) = reason {
                    args.push("--reason".to_string());
                    args.push(reason.clone());
                }

                DemarchCommandPlan {
                    bin: "ic",
                    signature: "ic run cancel --json",
                    args,
                    stdin: None,
                }
            }
        }
    }

    fn execute_plan(&self, plan: DemarchCommandPlan, write: bool) -> DemarchResponse {
        if !self.is_signature_allowed(plan.signature, write) {
            return DemarchResponse::error(format!(
                "Operation blocked by demarch {} allowlist: {}",
                if write { "write" } else { "read" },
                plan.signature
            ));
        }

        if !is_cli_available(plan.bin) {
            return DemarchResponse::error(STANDALONE_MSG);
        }

        match self.exec_cli(plan.bin, &plan.args, plan.stdin.as_deref()) {
            Ok(result) => DemarchResponse::ok(result),
            Err(err) => DemarchResponse::error(err.to_string()),
        }
    }

    fn is_signature_allowed(&self, signature: &str, write: bool) -> bool {
        let allowlist = if write {
            &self.config.write_allowlist
        } else {
            &self.config.read_allowlist
        };

        allowlist.iter().any(|allowed| allowed == signature)
    }

    fn exec_cli(
        &self,
        bin: &str,
        args: &[String],
        stdin_data: Option<&str>,
    ) -> anyhow::Result<String> {
        use std::process::Stdio;

        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(&self.monorepo_root)
            .env("PATH", build_extended_path());

        if stdin_data.is_some() {
            cmd.stdin(Stdio::piped());
        }

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {} with args {:?}", bin, args))?;

        if let Some(data) = stdin_data {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(data.as_bytes())
                    .with_context(|| format!("failed to write stdin to {}", bin))?;
            }
        }

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for {} with args {:?}", bin, args))?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() {
            return Ok(stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stderr.is_empty() {
            return Err(anyhow!(stderr));
        }
        if !stdout.is_empty() {
            return Err(anyhow!(stdout));
        }

        Err(anyhow!(format!(
            "`{}` exited with status {}",
            bin, output.status
        )))
    }

    /// Use `run_id`, or resolve the project's active run with
    /// `ic run current --json`, which emits `{found, id, phase, goal}` and
    /// exits 1 with `{"found":false}` when there is none. Resolving needs
    /// `ic run current --json` in the read allowlist (it is a default); a
    /// custom allowlist without it blocks calls that pass no run id.
    fn resolve_run_id(&self, run_id: Option<String>) -> Result<String, DemarchResponse> {
        if let Some(run_id) = run_id.filter(|id| !id.is_empty()) {
            return Ok(run_id);
        }

        let plan = Self::plan_read(&ReadOperation::RunStatus { run_id: None })
            .expect("run current plan");
        let response = self.execute_plan(plan, false);
        let parsed = serde_json::from_str::<serde_json::Value>(&response.result).ok();
        let found = parsed
            .as_ref()
            .and_then(|v| v.get("found"))
            .and_then(|v| v.as_bool());
        let id = parsed
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty());

        match (&response.status, found, id) {
            (DemarchStatus::Ok, Some(true), Some(id)) => Ok(id.to_string()),
            (_, Some(false), _) => Err(DemarchResponse::error("No active run found.")),
            (DemarchStatus::Error, _, _) => Err(response),
            _ => Err(DemarchResponse::error(format!(
                "Unexpected ic run current output: {}",
                response.result
            ))),
        }
    }

    fn handle_sprint_phase(&self, run_id: Option<String>) -> DemarchResponse {
        let run_id = match self.resolve_run_id(run_id) {
            Ok(run_id) => run_id,
            Err(response) => return response,
        };
        let plan = Self::plan_read(&ReadOperation::SprintPhase {
            run_id: Some(run_id.clone()),
        })
        .expect("run phase plan");

        let response = self.execute_plan(plan, false);
        if response.status == DemarchStatus::Error {
            return response;
        }
        if response.result.is_empty() {
            return DemarchResponse::error(format!("No phase found for run {run_id}."));
        }
        DemarchResponse::ok(
            serde_json::json!({ "run_id": run_id, "phase": response.result }).to_string(),
        )
    }

    fn handle_spec_lookup(
        &self,
        run_id: Option<String>,
        artifact_id: Option<String>,
    ) -> DemarchResponse {
        let run_id = match self.resolve_run_id(run_id) {
            Ok(run_id) => run_id,
            Err(response) => return response,
        };
        let plan = Self::plan_read(&ReadOperation::SpecLookup {
            run_id: Some(run_id.clone()),
            artifact_id: None,
        })
        .expect("artifact list plan");

        let response = self.execute_plan(plan, false);
        if response.status == DemarchStatus::Error {
            return response;
        }
        // `ic run artifact list --json` prints an array, `[]` when the run has none.
        let Ok(artifacts) = serde_json::from_str::<Vec<serde_json::Value>>(&response.result)
        else {
            return DemarchResponse::error(format!(
                "Unexpected ic run artifact list output: {}",
                response.result
            ));
        };
        let Some(artifact_id) = artifact_id else {
            return response;
        };

        let found = artifacts
            .into_iter()
            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(artifact_id.as_str()));
        match found {
            Some(artifact) => DemarchResponse::ok(artifact.to_string()),
            None => DemarchResponse::error(format!(
                "Artifact {artifact_id} not found in run {run_id}."
            )),
        }
    }

    fn handle_review_summary(&self) -> DemarchResponse {
        let search_dirs = [
            self.project_root.join("docs/research/flux-drive"),
            self.project_root.join("docs/research"),
        ];

        for dir in search_dirs {
            if !dir.exists() {
                continue;
            }

            let mut files = match fs::read_dir(&dir) {
                Ok(entries) => entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension().and_then(|ext| ext.to_str()) == Some("json")
                            && path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| name.contains("verdict"))
                    })
                    .collect::<Vec<_>>(),
                Err(_) => continue,
            };

            files.sort_by(|a, b| b.cmp(a));

            let mut verdicts = Vec::new();
            for file in files.into_iter().take(3) {
                if let Ok(content) = fs::read_to_string(file) {
                    verdicts.push(content);
                }
            }

            if !verdicts.is_empty() {
                return DemarchResponse::ok(format!("[{}]", verdicts.join(",")));
            }
        }

        DemarchResponse::error("No review verdicts found.")
    }
}

/// Check if a CLI tool is available, searching both the system PATH and
/// common user-local bin directories that may not be in the service's PATH
/// (e.g. when intercomd runs as a systemd service with a minimal environment).
fn is_cli_available(bin: &str) -> bool {
    let extended_path = build_extended_path();
    // Use `command -v` (POSIX) instead of `which` — `which` is not installed
    // on all systems (e.g., minimal Ubuntu 24.04 images).
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .env("PATH", &extended_path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Build an extended PATH that includes common user-local bin directories.
/// Prepends ~/.local/bin and ~/.cargo/bin so user-installed versions take
/// precedence over stale system-wide copies (e.g. /usr/local/bin/bd v0.50
/// vs ~/.local/bin/bd v0.58).
fn build_extended_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let prepend_dirs = [
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
    ];
    let has_usr_local = base.split(':').any(|p| p == "/usr/local/bin");
    let mut parts: Vec<String> = Vec::new();
    for dir in &prepend_dirs {
        if !base.split(':').any(|p| p == dir.as_str()) {
            parts.push(dir.clone());
        }
    }
    parts.push(base);
    if !has_usr_local {
        parts.push("/usr/local/bin".to_string());
    }
    parts.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> DemarchAdapter {
        DemarchAdapter::new(DemarchConfig::default(), ".")
    }

    #[test]
    fn write_requires_main_group_by_default() {
        let response = adapter().execute_write(
            WriteOperation::CreateIssue {
                title: "x".to_string(),
                description: None,
                priority: None,
                issue_type: None,
                labels: None,
            },
            false,
        );

        assert_eq!(response.status, DemarchStatus::Error);
        assert!(response.result.contains("main group"));
    }

    #[test]
    fn create_issue_plan_contains_expected_flags() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::CreateIssue {
            title: "test title".to_string(),
            description: Some("desc".to_string()),
            priority: Some("1".to_string()),
            issue_type: Some("task".to_string()),
            labels: Some(vec!["a".to_string(), "b".to_string()]),
        });

        assert_eq!(plan.bin, "bd");
        assert_eq!(plan.signature, "bd create --json");
        assert!(plan.args.contains(&"--title".to_string()));
        assert!(plan.args.contains(&"--priority".to_string()));
        assert!(plan.args.contains(&"--type".to_string()));
        assert!(plan.args.contains(&"--labels".to_string()));
        assert!(plan.args.contains(&"a,b".to_string()));
    }

    #[test]
    fn run_events_plan_uses_consumer_and_default_limit() {
        let plan = DemarchAdapter::plan_read(&ReadOperation::RunEvents {
            limit: None,
            since: None,
        })
        .expect("plan");

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic events tail --consumer=intercom --json");
        assert!(plan.args.contains(&"--consumer=intercom".to_string()));
        assert!(plan.args.contains(&"--limit=20".to_string()));
    }

    #[test]
    fn reject_gate_plan_uses_state_set_with_stdin() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::RejectGate {
            gate_id: Some("gate-review".to_string()),
            reason: Some("Not ready".to_string()),
        });

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic state set --json");
        assert!(plan.args.contains(&"gate-decision".to_string()));
        assert!(plan.args.contains(&"gate-review".to_string()));
        let stdin = plan.stdin.expect("stdin should be set");
        assert!(stdin.contains("\"action\":\"reject\""));
        assert!(stdin.contains("Not ready"));
    }

    #[test]
    fn defer_gate_plan_uses_state_set_with_stdin() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::DeferGate {
            gate_id: Some("gate-ship".to_string()),
            reason: None,
        });

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic state set --json");
        let stdin = plan.stdin.expect("stdin should be set");
        assert!(stdin.contains("\"action\":\"defer\""));
        assert!(stdin.contains("Deferred via Intercom"));
    }

    #[test]
    fn extend_budget_plan_uses_run_set() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::ExtendBudget {
            run_id: "run-abc".to_string(),
            max_dispatches: Some(10),
        });

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic run set --json");
        assert!(plan.args.contains(&"run-abc".to_string()));
        assert!(plan.args.contains(&"--max-dispatches=10".to_string()));
        assert!(plan.stdin.is_none());
    }

    #[test]
    fn cancel_run_plan_uses_run_cancel() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::CancelRun {
            run_id: "run-xyz".to_string(),
            reason: Some("User requested".to_string()),
        });

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic run cancel --json");
        assert!(plan.args.contains(&"run-xyz".to_string()));
        assert!(plan.args.contains(&"--reason".to_string()));
        assert!(plan.args.contains(&"User requested".to_string()));
        assert!(plan.stdin.is_none());
    }

    #[test]
    fn cancel_run_plan_without_reason() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::CancelRun {
            run_id: "run-123".to_string(),
            reason: None,
        });

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic run cancel --json");
        assert!(plan.args.contains(&"run-123".to_string()));
        assert!(!plan.args.contains(&"--reason".to_string()));
    }

    #[test]
    fn extend_budget_plan_without_max_dispatches() {
        let plan = DemarchAdapter::plan_write(&WriteOperation::ExtendBudget {
            run_id: "run-456".to_string(),
            max_dispatches: None,
        });

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic run set --json");
        assert!(plan.args.contains(&"run-456".to_string()));
        // Only run_id and --json, no --max-dispatches flag
        assert_eq!(plan.args.len(), 4);
    }

    #[test]
    fn run_tokens_plan_includes_run_id() {
        let plan = DemarchAdapter::plan_read(&ReadOperation::RunTokens {
            run_id: "run-abc".to_string(),
        })
        .expect("plan");

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic run tokens --json");
        assert!(plan.args.contains(&"run-abc".to_string()));
        assert!(plan.args.contains(&"--json".to_string()));
    }

    #[test]
    fn dispatch_list_plan_active_only() {
        let plan = DemarchAdapter::plan_read(&ReadOperation::DispatchList { active_only: true })
            .expect("plan");

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic dispatch list --json");
        assert!(plan.args.contains(&"--active".to_string()));
    }

    #[test]
    fn sprint_phase_plan_passes_run_id() {
        let plan = DemarchAdapter::plan_read(&ReadOperation::SprintPhase {
            run_id: Some("run-1".to_string()),
        })
        .expect("plan");

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic run phase --json");
        assert_eq!(plan.args, vec!["run", "phase", "run-1"]);
    }

    #[test]
    fn spec_lookup_plan_lists_run_artifacts() {
        let plan = DemarchAdapter::plan_read(&ReadOperation::SpecLookup {
            run_id: Some("run-1".to_string()),
            artifact_id: Some("art-1".to_string()),
        })
        .expect("plan");

        assert_eq!(plan.signature, "ic run artifact list --json");
        assert_eq!(plan.args, vec!["run", "artifact", "list", "run-1", "--json"]);
    }

    #[test]
    fn run_scoped_reads_without_run_id_have_no_single_plan() {
        assert!(DemarchAdapter::plan_read(&ReadOperation::SprintPhase { run_id: None }).is_none());
        assert!(DemarchAdapter::plan_read(&ReadOperation::SpecLookup {
            run_id: None,
            artifact_id: None,
        })
        .is_none());
    }

    #[test]
    fn sprint_phase_deserializes_without_run_id() {
        let op: ReadOperation = serde_json::from_str(r#"{"op":"sprint_phase"}"#).unwrap();
        assert_eq!(op, ReadOperation::SprintPhase { run_id: None });
        let op: ReadOperation = serde_json::from_str(r#"{"op":"spec_lookup"}"#).unwrap();
        assert_eq!(
            op,
            ReadOperation::SpecLookup {
                run_id: None,
                artifact_id: None
            }
        );
    }

    #[test]
    fn run_scoped_reads_need_run_current_in_allowlist_without_run_id() {
        let config = DemarchConfig {
            read_allowlist: vec![
                "ic run phase --json".to_string(),
                "ic run artifact list --json".to_string(),
            ],
            ..DemarchConfig::default()
        };
        let adapter = DemarchAdapter::new(config, std::env::temp_dir());

        let phase = adapter.execute_read(ReadOperation::SprintPhase { run_id: None });
        assert_eq!(phase.status, DemarchStatus::Error);
        assert!(phase.result.contains("ic run current --json"), "{}", phase.result);

        let list = adapter.execute_read(ReadOperation::SpecLookup {
            run_id: None,
            artifact_id: None,
        });
        assert_eq!(list.status, DemarchStatus::Error);
        assert!(list.result.contains("ic run current --json"), "{}", list.result);
    }

    fn ic(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("ic")
            .args(args)
            .current_dir(dir)
            .env("PATH", build_extended_path())
            .output()
            .expect("spawn ic");
        assert!(
            out.status.success(),
            "ic {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn json_id(raw: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(raw).expect("ic json");
        v["id"].as_str().expect("id").to_string()
    }

    /// Runs `SprintPhase` and `SpecLookup` against the installed ic in a
    /// throwaway intercore database. Skips when ic is not on PATH.
    #[test]
    fn run_scoped_reads_against_real_ic() {
        if !is_cli_available("ic") {
            eprintln!("skipping run_scoped_reads_against_real_ic: ic not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().canonicalize().expect("canonicalize");
        ic(&dir, &["init"]);
        let run_id = json_id(&ic(
            &dir,
            &[
                "run",
                "create",
                &format!("--project={}", dir.display()),
                "--goal=intercom reader test",
                "--json",
            ],
        ));
        fs::write(dir.join("spec.md"), "# spec\n").unwrap();
        let artifact_id = json_id(&ic(
            &dir,
            &[
                "run",
                "artifact",
                "add",
                &run_id,
                "--phase=brainstorm",
                "--path=spec.md",
                "--type=prd",
                "--json",
            ],
        ));
        let expected_phase = ic(&dir, &["run", "phase", &run_id]);

        // No .beads above a temp dir, so ic runs from `dir` itself.
        let adapter = DemarchAdapter::new(DemarchConfig::default(), &dir);

        let phase = adapter.execute_read(ReadOperation::SprintPhase { run_id: None });
        assert_eq!(phase.status, DemarchStatus::Ok, "{}", phase.result);
        let phase: serde_json::Value = serde_json::from_str(&phase.result).unwrap();
        assert_eq!(phase["run_id"], run_id.as_str());
        assert_eq!(phase["phase"], expected_phase.as_str());

        let phase = adapter.execute_read(ReadOperation::SprintPhase {
            run_id: Some(run_id.clone()),
        });
        assert_eq!(phase.status, DemarchStatus::Ok, "{}", phase.result);

        let list = adapter.execute_read(ReadOperation::SpecLookup {
            run_id: None,
            artifact_id: None,
        });
        assert_eq!(list.status, DemarchStatus::Ok, "{}", list.result);
        let list: Vec<serde_json::Value> = serde_json::from_str(&list.result).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], artifact_id.as_str());
        assert_eq!(list[0]["run_id"], run_id.as_str());

        let one = adapter.execute_read(ReadOperation::SpecLookup {
            run_id: None,
            artifact_id: Some(artifact_id.clone()),
        });
        assert_eq!(one.status, DemarchStatus::Ok, "{}", one.result);
        let one: serde_json::Value = serde_json::from_str(&one.result).unwrap();
        assert_eq!(one["path"], "spec.md");

        let missing = adapter.execute_read(ReadOperation::SpecLookup {
            run_id: None,
            artifact_id: Some("no-such-artifact".to_string()),
        });
        assert_eq!(missing.status, DemarchStatus::Error);

        ic(&dir, &["run", "cancel", &run_id]);
        let none = adapter.execute_read(ReadOperation::SprintPhase { run_id: None });
        assert_eq!(none.status, DemarchStatus::Error);
        assert!(none.result.contains("No active run"), "{}", none.result);
    }

    #[test]
    fn dispatch_list_plan_all() {
        let plan = DemarchAdapter::plan_read(&ReadOperation::DispatchList { active_only: false })
            .expect("plan");

        assert_eq!(plan.bin, "ic");
        assert_eq!(plan.signature, "ic dispatch list --json");
        assert!(!plan.args.contains(&"--active".to_string()));
    }
}
