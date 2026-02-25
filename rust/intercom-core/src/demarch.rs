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
    SprintPhase,
    SearchBeads {
        id: Option<String>,
        query: Option<String>,
        status: Option<String>,
    },
    SpecLookup {
        artifact_id: Option<String>,
    },
    ReviewSummary,
    NextWork,
    RunEvents {
        limit: Option<u32>,
        since: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemarchCommandPlan {
    pub bin: &'static str,
    pub signature: &'static str,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DemarchAdapter {
    config: DemarchConfig,
    project_root: PathBuf,
}

impl DemarchAdapter {
    pub fn new(config: DemarchConfig, project_root: impl AsRef<Path>) -> Self {
        Self {
            config,
            project_root: project_root.as_ref().to_path_buf(),
        }
    }

    pub fn execute_read(&self, operation: ReadOperation) -> DemarchResponse {
        if !self.config.enabled {
            return DemarchResponse::error("Demarch integration is disabled.");
        }

        match operation {
            ReadOperation::ReviewSummary => self.handle_review_summary(),
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
                })
            }
            ReadOperation::SprintPhase => Some(DemarchCommandPlan {
                bin: "ic",
                signature: "ic run phase --json",
                args: vec!["run".to_string(), "phase".to_string(), "--json".to_string()],
            }),
            ReadOperation::SearchBeads { id, query, status } => {
                if let Some(id) = id {
                    return Some(DemarchCommandPlan {
                        bin: "bd",
                        signature: "bd show --json",
                        args: vec!["show".to_string(), id.clone(), "--json".to_string()],
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
                })
            }
            ReadOperation::SpecLookup { artifact_id } => {
                let args = if let Some(artifact_id) = artifact_id {
                    vec![
                        "run".to_string(),
                        "artifact".to_string(),
                        "get".to_string(),
                        artifact_id.clone(),
                        "--json".to_string(),
                    ]
                } else {
                    vec![
                        "run".to_string(),
                        "artifact".to_string(),
                        "list".to_string(),
                        "--json".to_string(),
                    ]
                };

                let signature = if artifact_id.is_some() {
                    "ic run artifact get --json"
                } else {
                    "ic run artifact list --json"
                };

                Some(DemarchCommandPlan {
                    bin: "ic",
                    signature,
                    args,
                })
            }
            ReadOperation::ReviewSummary => None,
            ReadOperation::NextWork => Some(DemarchCommandPlan {
                bin: "bd",
                signature: "bd ready --json",
                args: vec!["ready".to_string(), "--json".to_string()],
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

        match self.exec_cli(plan.bin, &plan.args) {
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

    fn exec_cli(&self, bin: &str, args: &[String]) -> anyhow::Result<String> {
        let output = Command::new(bin)
            .args(args)
            .current_dir(&self.project_root)
            .output()
            .with_context(|| format!("failed to execute {} with args {:?}", bin, args))?;

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

fn is_cli_available(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
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
}
