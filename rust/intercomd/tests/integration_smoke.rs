//! Smoke integration tests for intercomd.
//!
//! These tests spawn the actual intercomd binary on a random port with a
//! minimal config (no Postgres), then verify HTTP endpoints respond correctly.
//! No Docker, no Postgres, no Telegram — pure HTTP endpoint validation.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

/// Find a free port by binding to :0 and reading the assigned port.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to :0");
    listener.local_addr().unwrap().port()
}

/// Write a minimal config TOML to a temp file (no Postgres, orchestrator disabled).
fn write_test_config(dir: &tempfile::TempDir, port: u16) -> PathBuf {
    let config_path = dir.path().join("test.toml");
    let toml = format!(
        r#"
[server]
bind = "127.0.0.1:{port}"
host_callback_url = "http://127.0.0.1:19999"

[storage]

[runtimes]
default_runtime = "claude"

[runtimes.profiles.claude]
provider = "anthropic"
default_model = "claude-opus-4-6"
required_env = []

[orchestrator]
enabled = false

[scheduler]
enabled = false

[events]
enabled = false

[demarch]
enabled = false
"#
    );
    std::fs::write(&config_path, toml).expect("write test config");
    config_path
}

/// Build the intercomd binary (debug mode) and return its path.
fn intercomd_binary() -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let output = Command::new("cargo")
        .args(["build", "--bin", "intercomd", "--workspace"])
        .current_dir(&workspace_root)
        .output()
        .expect("cargo build");
    assert!(
        output.status.success(),
        "cargo build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    workspace_root.join("target/debug/intercomd")
}

/// Spawn intercomd and wait for it to be ready.
struct TestServer {
    child: Child,
    base_url: String,
}

impl TestServer {
    fn start(config_path: &PathBuf, port: u16) -> Self {
        let binary = intercomd_binary();
        let child = Command::new(&binary)
            .args(["serve", "--config", config_path.to_str().unwrap()])
            .env("RUST_LOG", "warn")
            .env("ASSISTANT_NAME", "TestBot")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn intercomd");

        let base_url = format!("http://127.0.0.1:{port}");

        let server = TestServer { child, base_url };
        server.wait_ready();
        server
    }

    fn wait_ready(&self) {
        let client = reqwest::blocking::Client::new();
        for _ in 0..50 {
            if client
                .get(format!("{}/healthz", self.base_url))
                .timeout(Duration::from_millis(200))
                .send()
                .is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("intercomd did not become ready within 5 seconds");
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Send SIGTERM for graceful shutdown
        #[cfg(unix)]
        {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
        }
        let _ = self.child.wait();
    }
}

#[test]
fn healthz_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_test_config(&dir, port);
    let server = TestServer::start(&config, port);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!("{}/healthz", server.base_url))
        .send()
        .expect("GET /healthz");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "intercomd");
    assert!(body["uptime_seconds"].is_number());
}

#[test]
fn readyz_reports_orchestrator_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_test_config(&dir, port);
    let server = TestServer::start(&config, port);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!("{}/readyz", server.base_url))
        .send()
        .expect("GET /readyz");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["status"], "ready");
    assert_eq!(body["orchestrator_enabled"], false);
    assert_eq!(body["postgres_connected"], false);
    assert_eq!(body["active_containers"], 0);
}

#[test]
fn command_reset_returns_effects() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_test_config(&dir, port);
    let server = TestServer::start(&config, port);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{}/v1/commands", server.base_url))
        .json(&serde_json::json!({
            "chat_jid": "tg:12345",
            "command": "reset",
            "args": "",
            "group_name": "Test Group",
            "group_folder": "test-group",
            "container_active": true
        }))
        .send()
        .expect("POST /v1/commands");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert!(body["text"].as_str().unwrap().contains("Session cleared"));
    let effects = body["effects"].as_array().unwrap();
    assert_eq!(effects.len(), 2);
    assert_eq!(effects[0], "KillContainer");
    assert_eq!(effects[1], "ClearSession");
}

#[test]
fn command_model_switch_returns_effects() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_test_config(&dir, port);
    let server = TestServer::start(&config, port);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{}/v1/commands", server.base_url))
        .json(&serde_json::json!({
            "chat_jid": "tg:12345",
            "command": "model",
            "args": "gemini-3.1-pro",
            "group_name": "Test Group",
            "group_folder": "test-group",
            "current_model": "claude-opus-4-6",
            "container_active": false
        }))
        .send()
        .expect("POST /v1/commands");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert!(body["text"].as_str().unwrap().contains("Switched"));
    let effects = body["effects"].as_array().unwrap();
    assert_eq!(effects.len(), 3);
    assert_eq!(effects[0], "KillContainer");
    assert_eq!(effects[1], "ClearSession");
    // SwitchModel is a struct variant — serialized as object
    assert!(effects[2]["SwitchModel"]["model_id"].as_str().unwrap() == "gemini-3.1-pro");
    assert!(effects[2]["SwitchModel"]["runtime"].as_str().unwrap() == "gemini");
}

#[test]
fn runtime_profiles_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = write_test_config(&dir, port);
    let server = TestServer::start(&config, port);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!("{}/v1/runtime/profiles", server.base_url))
        .send()
        .expect("GET /v1/runtime/profiles");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["default_runtime"], "claude");
    assert!(body["profiles"].as_array().unwrap().contains(&serde_json::json!("claude")));
}
