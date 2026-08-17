//! E2E coverage for the agent-facing `aioncore team` CLI fallback.

use std::process::Stdio;

use aionui_team::mcp::protocol::{read_frame, write_frame};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

fn team_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    command.arg("team");
    command
}

async fn write_stdio_message(stdin: &mut tokio::process::ChildStdin, value: serde_json::Value) {
    stdin.write_all(value.to_string().as_bytes()).await.unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.flush().await.unwrap();
}

async fn read_stdio_message(stdout: &mut BufReader<tokio::process::ChildStdout>) -> serde_json::Value {
    loop {
        let mut line = String::new();
        let bytes = timeout(Duration::from_secs(10), stdout.read_line(&mut line))
            .await
            .expect("timed out waiting for MCP stdio response")
            .expect("failed to read MCP stdio response");
        assert_ne!(bytes, 0, "MCP stdio server closed stdout unexpectedly");
        if let Ok(value) = serde_json::from_str(&line) {
            return value;
        }
    }
}

#[tokio::test]
async fn team_capabilities_prints_contract_without_runtime_env() {
    let output = team_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "team capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["contract"], "agent-facing-team-cli");
    assert_eq!(stdout["data"]["tools"].as_array().unwrap().len(), 13);
    let spawn = stdout["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "team_spawn_agent")
        .unwrap();
    assert_eq!(spawn["lead_only"], true);
    assert!(spawn["stdin_json_schema"]["properties"]["assistant_id"].is_object());
}

#[tokio::test]
async fn team_help_prints_markdown_without_runtime_env() {
    let output = team_command()
        .arg("help")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(output.status.success());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["format"], "markdown");
    assert!(stdout["data"]["text"].as_str().unwrap().contains("team send-message"));
}

#[tokio::test]
async fn tool_command_rejects_forged_identity_fields_before_http_call() {
    let mut child = team_command()
        .args(["send-message"])
        .env("AIONUI_BASE_URL", "http://127.0.0.1:9")
        .env("AIONUI_CONVERSATION_ID", "conv-1")
        .env("AIONUI_USER_ID", "user-1")
        .env("AIONUI_RUNTIME_TOKEN", "token-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"to":"worker-1","message":"hi","team_id":"team-1","slot_id":"lead-1","role":"lead"}"#)
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_SCHEMA_VALIDATION_FAILED"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "schema_validation_failed");
    assert!(stdout["error"]["details"]["expected_schema"].is_object());
}

#[tokio::test]
async fn team_context_requires_runtime_env_and_prints_json_error() {
    let output = team_command()
        .arg("context")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_ENV_MISSING"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "runtime_context_missing");
    assert_eq!(stdout["meta"]["command"], "team context");
}

#[tokio::test]
async fn unknown_team_command_returns_json_error_envelope() {
    let output = team_command().arg("does-not-exist").output().await.unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_UNKNOWN_COMMAND"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "unknown_tool");
    assert_eq!(stdout["meta"]["command"], "team does-not-exist");
}

#[tokio::test]
async fn team_work_list_is_callable_through_real_mcp_stdio_transport() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tcp_server = tokio::spawn(async move {
        for expected_method in ["tools/list", "tools/call"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let init: serde_json::Value = serde_json::from_slice(&read_frame(&mut socket).await.unwrap()).unwrap();
            assert_eq!(init["method"], "initialize");
            assert_eq!(init["params"]["auth_token"], "stdio-e2e-token");
            assert_eq!(init["params"]["slot_id"], "stdio-e2e-slot");
            write_frame(
                &mut socket,
                &serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {}
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            let request: serde_json::Value = serde_json::from_slice(&read_frame(&mut socket).await.unwrap()).unwrap();
            assert_eq!(request["method"], expected_method);
            let result = if expected_method == "tools/list" {
                serde_json::json!({
                    "tools": [{
                        "name": "team_work_list",
                        "description": "Read the authoritative Team Work snapshot.",
                        "input_schema": { "type": "object", "properties": {} }
                    }]
                })
            } else {
                assert_eq!(request["params"]["name"], "team_work_list");
                assert_eq!(request["params"]["arguments"], serde_json::json!({}));
                serde_json::json!({
                    "content": [{ "type": "text", "text": "work snapshot ok" }],
                    "isError": false
                })
            };
            write_frame(
                &mut socket,
                &serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": result
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_aioncore"))
        .arg("mcp-team-stdio")
        .env("TEAM_MCP_PORT", port.to_string())
        .env("TEAM_MCP_TOKEN", "stdio-e2e-token")
        .env("TEAM_AGENT_SLOT_ID", "stdio-e2e-slot")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    write_stdio_message(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "team-stdio-e2e", "version": "1.0.0" }
            }
        }),
    )
    .await;
    let initialize = read_stdio_message(&mut stdout).await;
    assert_eq!(initialize["id"], 1);
    assert!(initialize.get("result").is_some(), "initialize failed: {initialize}");

    write_stdio_message(
        &mut stdin,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    write_stdio_message(
        &mut stdin,
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    let listed = read_stdio_message(&mut stdout).await;
    assert_eq!(listed["id"], 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "team_work_list");

    write_stdio_message(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "team_work_list", "arguments": {} }
        }),
    )
    .await;
    let called = read_stdio_message(&mut stdout).await;
    assert_eq!(called["id"], 3);
    assert!(called.get("error").is_none(), "tools/call failed: {called}");
    assert_eq!(called["result"]["isError"], false);
    assert_eq!(called["result"]["content"][0]["text"], "work snapshot ok");

    tcp_server.await.unwrap();
    drop(stdin);
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("MCP stdio server did not exit after stdin closed")
        .unwrap();
    assert!(status.success(), "MCP stdio server exited with {status}");
}
