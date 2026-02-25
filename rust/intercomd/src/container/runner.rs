//! Async container runner: spawns Docker containers and manages their lifecycle.
//!
//! Port of `runContainerAgent()` from container-runner.ts.
//!
//! Uses tokio::process for async spawning, streams stdout for OUTPUT marker
//! pairs, manages activity-based timeouts, and handles graceful stop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use intercom_core::{
    ContainerInput, ContainerOutput, ContainerStatus, RuntimeKind, VolumeMount,
    container_image, extract_output_markers,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, watch};
use tracing::{debug, error, info, warn};

use super::mounts::{GroupInfo, build_volume_mounts, container_name};
use super::secrets::{build_container_args, read_secrets};
use super::security::MountAllowlist;

/// Container runtime binary name.
const CONTAINER_RUNTIME_BIN: &str = "docker";

/// Maximum output buffer size (1 MiB) before truncation.
const MAX_OUTPUT_SIZE: usize = 1_048_576;

/// Default container timeout (5 minutes).
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Default idle timeout (30 minutes).
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 1_800_000;

/// Configuration for running a container agent.
pub struct RunConfig {
    pub project_root: PathBuf,
    pub groups_dir: PathBuf,
    pub data_dir: PathBuf,
    pub timezone: String,
    pub idle_timeout_ms: u64,
    pub allowlist: Option<MountAllowlist>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            project_root: std::env::current_dir().unwrap_or_default(),
            groups_dir: PathBuf::from("groups"),
            data_dir: PathBuf::from("data"),
            timezone: "UTC".to_string(),
            idle_timeout_ms: DEFAULT_IDLE_TIMEOUT_MS,
            allowlist: None,
        }
    }
}

/// Result of a container run.
pub struct RunResult {
    pub output: ContainerOutput,
    pub container_name: String,
    pub duration: Duration,
}

/// Callback for streaming container output as it arrives.
pub type OutputCallback =
    Box<dyn Fn(ContainerOutput) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Run a container agent: spawn, write input, stream output, manage lifecycle.
///
/// This is the Rust equivalent of `runContainerAgent()` from container-runner.ts.
pub async fn run_container_agent(
    group: &GroupInfo,
    input: &ContainerInput,
    runtime: RuntimeKind,
    is_main: bool,
    config: &RunConfig,
    on_output: Option<Arc<OutputCallback>>,
) -> anyhow::Result<RunResult> {
    let start = Instant::now();

    // Ensure group directory exists
    let group_dir = config.groups_dir.join(&group.folder);
    tokio::fs::create_dir_all(&group_dir).await.ok();
    let logs_dir = group_dir.join("logs");
    tokio::fs::create_dir_all(&logs_dir).await.ok();

    // Build mounts and container args
    let mounts = build_volume_mounts(
        group,
        is_main,
        runtime,
        &config.project_root,
        &config.groups_dir,
        &config.data_dir,
        config.allowlist.as_ref(),
    );

    let name = container_name(&group.folder);
    let image = container_image(runtime);
    let container_args = build_container_args(&mounts, &name, image, &config.timezone);

    info!(
        group = %group.name,
        container_name = %name,
        mount_count = mounts.len(),
        is_main,
        runtime = runtime.as_str(),
        "Spawning container agent"
    );

    // Spawn the container process
    let mut child = Command::new(CONTAINER_RUNTIME_BIN)
        .args(&container_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn container: {}", e))?;

    // Write input + secrets to stdin
    let mut stdin_input = input.clone();
    stdin_input.secrets = Some(read_secrets(&config.project_root));
    let input_json = serde_json::to_string(&stdin_input)?;
    // Zero secrets from our copy
    drop(stdin_input);

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input_json.as_bytes()).await?;
        stdin.shutdown().await.ok();
    }

    // Set up timeout management
    let container_timeout = group
        .container_config
        .as_ref()
        .and_then(|c| c.timeout)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    // Grace period: hard timeout must be at least idle_timeout + 30s
    let timeout_ms = container_timeout.max(config.idle_timeout_ms + 30_000);
    let timeout_duration = Duration::from_millis(timeout_ms);

    let (activity_tx, mut activity_rx) = watch::channel(Instant::now());
    let timed_out = Arc::new(Mutex::new(false));
    let had_streaming_output = Arc::new(Mutex::new(false));
    let new_session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Timeout watchdog task
    let timeout_name = name.clone();
    let timeout_flag = timed_out.clone();
    let timeout_handle = tokio::spawn(async move {
        loop {
            let last_activity = *activity_rx.borrow();
            let elapsed = last_activity.elapsed();
            if elapsed >= timeout_duration {
                *timeout_flag.lock().await = true;
                error!(
                    container_name = %timeout_name,
                    "Container timeout, stopping"
                );
                // Graceful stop
                let stop_result = Command::new(CONTAINER_RUNTIME_BIN)
                    .args(["stop", &timeout_name])
                    .output()
                    .await;
                if let Err(e) = stop_result {
                    warn!(
                        container_name = %timeout_name,
                        error = %e,
                        "Graceful stop failed"
                    );
                }
                break;
            }
            let remaining = timeout_duration - elapsed;
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {}
                _ = activity_rx.changed() => {}
            }
        }
    });

    // Stream stdout for OUTPUT markers
    let stdout = child.stdout.take().unwrap();
    let mut stdout_reader = BufReader::new(stdout);
    let mut stdout_buf = String::new();
    let mut stdout_total = String::new();
    let mut stdout_truncated = false;

    let stderr = child.stderr.take().unwrap();
    let mut stderr_reader = BufReader::new(stderr);
    let mut stderr_buf = String::new();
    let mut stderr_total = String::new();
    let mut stderr_truncated = false;

    // Process stdout and stderr concurrently
    let on_output_ref = on_output.clone();
    let had_output_ref = had_streaming_output.clone();
    let session_ref = new_session_id.clone();
    let activity_tx_ref = activity_tx.clone();

    loop {
        tokio::select! {
            result = stdout_reader.read_line(&mut stdout_buf) => {
                match result {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        // Accumulate for logging
                        if !stdout_truncated {
                            let remaining = MAX_OUTPUT_SIZE - stdout_total.len();
                            if stdout_buf.len() > remaining {
                                stdout_total.push_str(&stdout_buf[..remaining]);
                                stdout_truncated = true;
                                warn!(group = %group.name, "Container stdout truncated");
                            } else {
                                stdout_total.push_str(&stdout_buf);
                            }
                        }

                        // Parse OUTPUT markers
                        if on_output_ref.is_some() {
                            let (results, consumed) = extract_output_markers(&stdout_buf);
                            if consumed > 0 {
                                stdout_buf = stdout_buf[consumed..].to_string();
                            }
                            for json_str in results {
                                match serde_json::from_str::<ContainerOutput>(&json_str) {
                                    Ok(parsed) => {
                                        if let Some(ref sid) = parsed.new_session_id {
                                            *session_ref.lock().await = Some(sid.clone());
                                        }
                                        *had_output_ref.lock().await = true;
                                        // Reset activity timer
                                        activity_tx_ref.send(Instant::now()).ok();

                                        if let Some(ref cb) = on_output_ref {
                                            cb(parsed).await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            group = %group.name,
                                            error = %e,
                                            "Failed to parse streamed output chunk"
                                        );
                                    }
                                }
                            }
                        }
                        if consumed_none(&stdout_buf) {
                            stdout_buf.clear();
                        }
                    }
                    Err(e) => {
                        warn!(group = %group.name, error = %e, "Error reading stdout");
                        break;
                    }
                }
            }
            result = stderr_reader.read_line(&mut stderr_buf) => {
                match result {
                    Ok(0) => {} // stderr EOF, keep reading stdout
                    Ok(_) => {
                        let line = stderr_buf.trim();
                        if !line.is_empty() {
                            debug!(container = %group.folder, "{}", line);
                        }
                        if !stderr_truncated {
                            let remaining = MAX_OUTPUT_SIZE - stderr_total.len();
                            if stderr_buf.len() > remaining {
                                stderr_total.push_str(&stderr_buf[..remaining]);
                                stderr_truncated = true;
                                warn!(group = %group.name, "Container stderr truncated");
                            } else {
                                stderr_total.push_str(&stderr_buf);
                            }
                        }
                        stderr_buf.clear();
                    }
                    Err(_) => {} // stderr error, non-fatal
                }
            }
        }
    }

    // Wait for process exit
    let status = child.wait().await?;
    let duration = start.elapsed();

    // Cancel timeout watchdog
    timeout_handle.abort();

    let was_timed_out = *timed_out.lock().await;
    let had_output = *had_streaming_output.lock().await;
    let session_id = new_session_id.lock().await.clone();
    let exit_code = status.code();

    // Write container log
    write_container_log(
        &logs_dir,
        &group.name,
        &name,
        duration,
        exit_code,
        was_timed_out,
        had_output,
        &mounts,
        &stdout_total,
        stdout_truncated,
        &stderr_total,
        stderr_truncated,
    )
    .await;

    // Handle timeout cases
    if was_timed_out {
        if had_output {
            info!(
                group = %group.name,
                container_name = %name,
                duration_ms = duration.as_millis(),
                "Container timed out after output (idle cleanup)"
            );
            return Ok(RunResult {
                output: ContainerOutput {
                    status: ContainerStatus::Success,
                    result: None,
                    new_session_id: session_id,
                    error: None,
                    model: None,
                    event: None,
                },
                container_name: name,
                duration,
            });
        }

        error!(
            group = %group.name,
            container_name = %name,
            duration_ms = duration.as_millis(),
            "Container timed out with no output"
        );
        return Ok(RunResult {
            output: ContainerOutput {
                status: ContainerStatus::Error,
                result: None,
                new_session_id: None,
                error: Some(format!("Container timed out after {}ms", container_timeout)),
                model: None,
                event: None,
            },
            container_name: name,
            duration,
        });
    }

    // Handle error exit
    if !status.success() {
        error!(
            group = %group.name,
            exit_code = ?exit_code,
            duration_ms = duration.as_millis(),
            "Container exited with error"
        );
        let tail = if stderr_total.len() > 200 {
            &stderr_total[stderr_total.len() - 200..]
        } else {
            &stderr_total
        };
        return Ok(RunResult {
            output: ContainerOutput {
                status: ContainerStatus::Error,
                result: None,
                new_session_id: None,
                error: Some(format!(
                    "Container exited with code {}: {}",
                    exit_code.unwrap_or(-1),
                    tail
                )),
                model: None,
                event: None,
            },
            container_name: name,
            duration,
        });
    }

    // Streaming mode: output was already dispatched via callbacks
    if on_output.is_some() {
        info!(
            group = %group.name,
            duration_ms = duration.as_millis(),
            "Container completed (streaming mode)"
        );
        return Ok(RunResult {
            output: ContainerOutput {
                status: ContainerStatus::Success,
                result: None,
                new_session_id: session_id,
                error: None,
                model: None,
                event: None,
            },
            container_name: name,
            duration,
        });
    }

    // Legacy mode: parse the last output marker pair from accumulated stdout
    let (results, _) = extract_output_markers(&stdout_total);
    if let Some(last_json) = results.last() {
        match serde_json::from_str::<ContainerOutput>(last_json) {
            Ok(output) => {
                info!(
                    group = %group.name,
                    duration_ms = duration.as_millis(),
                    status = ?output.status,
                    "Container completed"
                );
                Ok(RunResult {
                    output,
                    container_name: name,
                    duration,
                })
            }
            Err(e) => {
                error!(
                    group = %group.name,
                    error = %e,
                    "Failed to parse container output"
                );
                Ok(RunResult {
                    output: ContainerOutput {
                        status: ContainerStatus::Error,
                        result: None,
                        new_session_id: None,
                        error: Some(format!("Failed to parse container output: {}", e)),
                        model: None,
                        event: None,
                    },
                    container_name: name,
                    duration,
                })
            }
        }
    } else {
        // Fallback: try parsing last non-empty line
        let last_line = stdout_total.trim().lines().last().unwrap_or("");
        match serde_json::from_str::<ContainerOutput>(last_line) {
            Ok(output) => Ok(RunResult {
                output,
                container_name: name,
                duration,
            }),
            Err(e) => Ok(RunResult {
                output: ContainerOutput {
                    status: ContainerStatus::Error,
                    result: None,
                    new_session_id: None,
                    error: Some(format!(
                        "No OUTPUT markers found and failed to parse last line: {}",
                        e
                    )),
                    model: None,
                    event: None,
                },
                container_name: name,
                duration,
            }),
        }
    }
}

/// Helper: check if the buffer contains no OUTPUT markers (nothing was consumed).
fn consumed_none(buf: &str) -> bool {
    !buf.contains(intercom_core::OUTPUT_START_MARKER)
}

/// Write a container run log to the logs directory.
async fn write_container_log(
    logs_dir: &Path,
    group_name: &str,
    container_name: &str,
    duration: Duration,
    exit_code: Option<i32>,
    timed_out: bool,
    had_output: bool,
    mounts: &[VolumeMount],
    stdout: &str,
    stdout_truncated: bool,
    stderr: &str,
    stderr_truncated: bool,
) {
    let timestamp = chrono_timestamp();
    let log_file = logs_dir.join(format!("container-{}.log", timestamp));
    let is_error = exit_code.unwrap_or(0) != 0 || timed_out;

    let mut lines = vec![
        format!(
            "=== Container Run Log{} ===",
            if timed_out { " (TIMEOUT)" } else { "" }
        ),
        format!("Timestamp: {}", timestamp),
        format!("Group: {}", group_name),
        format!("Container: {}", container_name),
        format!("Duration: {}ms", duration.as_millis()),
        format!("Exit Code: {:?}", exit_code),
        format!("Had Streaming Output: {}", had_output),
        String::new(),
    ];

    if is_error {
        lines.push("=== Mounts ===".to_string());
        for m in mounts {
            lines.push(format!(
                "{} -> {}{}",
                m.host_path,
                m.container_path,
                if m.readonly { " (ro)" } else { "" }
            ));
        }
        lines.push(String::new());
        lines.push(format!(
            "=== Stderr{} ===",
            if stderr_truncated { " (TRUNCATED)" } else { "" }
        ));
        lines.push(stderr.to_string());
        lines.push(String::new());
        lines.push(format!(
            "=== Stdout{} ===",
            if stdout_truncated { " (TRUNCATED)" } else { "" }
        ));
        lines.push(stdout.to_string());
    } else {
        lines.push("=== Mounts ===".to_string());
        for m in mounts {
            lines.push(format!(
                "{}{}",
                m.container_path,
                if m.readonly { " (ro)" } else { "" }
            ));
        }
    }

    let content = lines.join("\n");
    if let Err(e) = tokio::fs::write(&log_file, &content).await {
        warn!(
            log_file = %log_file.display(),
            error = %e,
            "Failed to write container log"
        );
    } else {
        debug!(log_file = %log_file.display(), "Container log written");
    }
}

fn chrono_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // ISO-ish format suitable for filenames
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    format!("{}-{:03}", secs, millis)
}

/// Write task and group snapshots to the IPC directory for container consumption.
pub async fn write_snapshots(
    data_dir: &Path,
    group_folder: &str,
    is_main: bool,
    tasks_json: &str,
    groups_json: &str,
) {
    let ipc_dir = data_dir.join("ipc").join(group_folder);
    tokio::fs::create_dir_all(&ipc_dir).await.ok();

    if let Err(e) = tokio::fs::write(ipc_dir.join("current_tasks.json"), tasks_json).await {
        warn!(error = %e, "Failed to write tasks snapshot");
    }
    if let Err(e) = tokio::fs::write(ipc_dir.join("available_groups.json"), groups_json).await {
        warn!(error = %e, "Failed to write groups snapshot");
    }
}

/// Stop a container by name (graceful docker stop).
pub async fn stop_container(container_name: &str) -> bool {
    match Command::new(CONTAINER_RUNTIME_BIN)
        .args(["stop", container_name])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            info!(container_name, "Container stopped");
            true
        }
        Ok(output) => {
            warn!(
                container_name,
                stderr = String::from_utf8_lossy(&output.stderr).as_ref(),
                "Failed to stop container"
            );
            false
        }
        Err(e) => {
            warn!(container_name, error = %e, "Failed to execute docker stop");
            false
        }
    }
}

/// Check if the container runtime is available.
pub async fn ensure_runtime_available() -> anyhow::Result<()> {
    let output = Command::new(CONTAINER_RUNTIME_BIN)
        .args(["info"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("Container runtime not found: {}", e))?;

    if !output.status.success() {
        anyhow::bail!("Container runtime is not running. Ensure Docker is installed and started.");
    }

    debug!("Container runtime available");
    Ok(())
}

/// Kill orphaned intercom containers from previous runs.
pub async fn cleanup_orphans() {
    let output = match Command::new(CONTAINER_RUNTIME_BIN)
        .args(["ps", "--filter", "name=nanoclaw-", "--format", "{{.Names}}"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "Failed to list orphaned containers");
            return;
        }
    };

    let names: Vec<&str> = std::str::from_utf8(&output.stdout)
        .unwrap_or("")
        .trim()
        .split('\n')
        .filter(|s| !s.is_empty())
        .collect();

    for name in &names {
        let _ = Command::new(CONTAINER_RUNTIME_BIN)
            .args(["stop", name])
            .output()
            .await;
    }

    if !names.is_empty() {
        info!(count = names.len(), "Stopped orphaned containers");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_timestamp_format() {
        let ts = chrono_timestamp();
        // Should be "{seconds}-{millis}" format
        assert!(ts.contains('-'));
        let parts: Vec<&str> = ts.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].parse::<u64>().is_ok());
        assert!(parts[1].parse::<u32>().is_ok());
    }

    #[test]
    fn consumed_none_detects_no_markers() {
        assert!(consumed_none("just some output"));
        assert!(!consumed_none(&format!("prefix{}suffix", intercom_core::OUTPUT_START_MARKER)));
    }
}
