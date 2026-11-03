//! MCP stdio server and local IPC transport for the `waku-daemon mcp-agent-tools` bridge.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{Context as _, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use uuid::Uuid;
use waku_protocol::delegation::DelegationBatchRequest;

use super::auth::{DelegationRegistry, SessionBridgeConfig};
use super::scheduler::DelegationScheduler;

#[derive(Serialize, Deserialize)]
pub struct IpcRequest {
    pub token: String,
    pub session_id: String,
    pub method: String,
    pub params: JsonValue,
}

#[derive(Serialize, Deserialize)]
pub struct IpcResponse {
    pub result: Option<JsonValue>,
    pub error: Option<String>,
}

/// Start the local IPC server listening on `registry.directory().join("ipc.sock")`.
pub fn start_ipc_server(
    registry: Arc<DelegationRegistry>,
    scheduler: DelegationScheduler,
) -> anyhow::Result<PathBuf> {
    let socket_path = registry.socket_path().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixListener;
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).with_context(|| {
            format!(
                "could not bind delegation IPC socket {}",
                socket_path.display()
            )
        })?;

        let registry_clone = registry.clone();
        let scheduler_clone = scheduler.clone();
        thread::Builder::new()
            .name("waku-delegation-ipc".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let registry = registry_clone.clone();
                    let scheduler = scheduler_clone.clone();
                    thread::Builder::new()
                        .name("waku-delegation-ipc-client".into())
                        .spawn(move || {
                            handle_ipc_client(stream, &registry, &scheduler);
                        })
                        .ok();
                }
            })?;
    }

    Ok(socket_path)
}

#[cfg(unix)]
fn handle_ipc_client(
    mut stream: std::os::unix::net::UnixStream,
    registry: &DelegationRegistry,
    scheduler: &DelegationScheduler,
) {
    let reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: IpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                let resp = IpcResponse {
                    result: None,
                    error: Some(format!("invalid IPC request JSON: {e}")),
                };
                let _ = serde_json::to_writer(&mut stream, &resp);
                let _ = stream.write_all(b"\n");
                let _ = stream.flush();
                continue;
            }
        };

        // Validate session config and token
        let config = match DelegationRegistry::load_session_config(
            registry.directory(),
            &request.session_id,
        ) {
            Ok(cfg) => cfg,
            Err(e) => {
                let resp = IpcResponse {
                    result: None,
                    error: Some(format!("unauthorized session: {e}")),
                };
                let _ = serde_json::to_writer(&mut stream, &resp);
                let _ = stream.write_all(b"\n");
                let _ = stream.flush();
                continue;
            }
        };

        if subtle::ConstantTimeEq::ct_eq(config.token.as_bytes(), request.token.as_bytes())
            .unwrap_u8()
            != 1
        {
            let resp = IpcResponse {
                result: None,
                error: Some("invalid session token".into()),
            };
            let _ = serde_json::to_writer(&mut stream, &resp);
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
            continue;
        }

        let response = match request.method.as_str() {
            "delegate_batch" => {
                match serde_json::from_value::<DelegationBatchRequest>(request.params) {
                    Ok(batch_req) => {
                        match scheduler.delegate_batch_authorized(&config, batch_req) {
                            Ok(res) => IpcResponse {
                                result: Some(serde_json::to_value(res).unwrap_or(JsonValue::Null)),
                                error: None,
                            },
                            Err(e) => IpcResponse {
                                result: None,
                                error: Some(format!("{e:#}")),
                            },
                        }
                    }
                    Err(e) => IpcResponse {
                        result: None,
                        error: Some(format!("invalid delegate_batch parameters: {e}")),
                    },
                }
            }
            "wait_delegates" => {
                let group_id = request.params.get("group_id").and_then(JsonValue::as_str);
                let timeout_ms = request.params.get("timeout_ms").and_then(JsonValue::as_u64);
                match group_id.and_then(|id| Uuid::parse_str(id).ok()) {
                    Some(gid) => match scheduler.get_group_info(gid).and_then(|group| {
                        let group =
                            group.ok_or_else(|| anyhow!("delegation group {gid} not found"))?;
                        anyhow::ensure!(
                            group.parent_session_id == config.parent_session_id,
                            "delegation group belongs to another session"
                        );
                        scheduler.wait_delegates(gid, timeout_ms)
                    }) {
                        Ok(res) => IpcResponse {
                            result: Some(serde_json::to_value(res).unwrap_or(JsonValue::Null)),
                            error: None,
                        },
                        Err(e) => IpcResponse {
                            result: None,
                            error: Some(format!("{e:#}")),
                        },
                    },
                    None => IpcResponse {
                        result: None,
                        error: Some("group_id is required and must be a valid UUID".into()),
                    },
                }
            }
            other => IpcResponse {
                result: None,
                error: Some(format!("unknown delegation method: {other}")),
            },
        };

        let _ = serde_json::to_writer(&mut stream, &response);
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
}

/// Execute the stdio MCP JSON-RPC protocol server for the agent-tools bridge.
pub fn serve_mcp_stdio() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() {
            continue;
        }

        let message: JsonValue = match serde_json::from_str(line_trimmed) {
            Ok(msg) => msg,
            Err(e) => {
                write_json_rpc_error(
                    &mut stdout,
                    JsonValue::Null,
                    -32700,
                    &format!("invalid JSON: {e}"),
                )?;
                continue;
            }
        };

        let method = message
            .get("method")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let id = message.get("id").cloned().unwrap_or(JsonValue::Null);

        match method {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "waku-agent-tools",
                        "version": "1.0.0"
                    }
                });
                write_json_rpc_result(&mut stdout, id, result)?;
            }
            "ping" => {
                write_json_rpc_result(&mut stdout, id, json!({}))?;
            }
            "tools/list" => {
                let tools = json!([
                    {
                        "name": "delegate_batch",
                        "description": "Delegate independent code review subtasks to specified models in parallel.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "request_key": {
                                    "type": "string",
                                    "description": "Optional stable deduplication key for this batch. Repeating it with the same tasks reuses the existing group. When omitted, Waku derives one from the tasks, so an identical retry is still deduplicated."
                                },
                                "tasks": {
                                    "type": "array",
                                    "description": "List of review subtasks",
                                    "minItems": 1,
                                    "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "task_key": {
                                                "type": "string",
                                                "minLength": 1,
                                                "description": "Nonempty key unique across the entire batch, even when the same instructions go to different targets. For three models, use distinct keys such as books-target-1, books-target-2, and books-target-3. The returned task_ids map uses these exact keys."
                                            },
                                            "target_ref": {
                                                "type": "string",
                                                "description": "Reference ID or model ID from the allowed models"
                                            },
                                            "instructions": {
                                                "type": "string",
                                                "description": "Independent review instructions for this subtask"
                                            }
                                        },
                                        "required": ["task_key", "target_ref", "instructions"]
                                    }
                                }
                            },
                            "required": ["tasks"]
                        }
                    },
                    {
                        "name": "wait_delegates",
                        "description": "Wait for delegated subtasks in a group to finish and retrieve their independent conclusions.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "group_id": {
                                    "type": "string",
                                    "description": "The group_id returned by delegate_batch"
                                },
                                "timeout_ms": {
                                    "type": "integer",
                                    "minimum": 0,
                                    "description": "Time to wait in milliseconds. Defaults to 25000; larger values are capped to 25000 by the server. Use 0 to return progress immediately, or call again to keep waiting."
                                }
                            },
                            "required": ["group_id"]
                        }
                    }
                ]);
                write_json_rpc_result(&mut stdout, id, json!({ "tools": tools }))?;
            }
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(JsonValue::Null);
                let tool_name = params
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                let (session_config, socket_path) = match resolve_session_and_socket() {
                    Ok(res) => res,
                    Err(e) => {
                        let content = json!([{
                            "type": "text",
                            "text": format!("Error: could not resolve session bridge: {e:#}")
                        }]);
                        write_json_rpc_result(
                            &mut stdout,
                            id,
                            json!({ "content": content, "isError": true }),
                        )?;
                        continue;
                    }
                };

                let ipc_req = IpcRequest {
                    token: session_config.token,
                    session_id: session_config.parent_session_id.to_string(),
                    method: tool_name.to_owned(),
                    params: arguments,
                };

                match send_ipc_request(&socket_path, &ipc_req) {
                    Ok(ipc_resp) => {
                        if let Some(err) = ipc_resp.error {
                            let content =
                                json!([{ "type": "text", "text": format!("Error: {err}") }]);
                            write_json_rpc_result(
                                &mut stdout,
                                id,
                                json!({ "content": content, "isError": true }),
                            )?;
                        } else {
                            let text = serde_json::to_string_pretty(
                                &ipc_resp.result.unwrap_or(JsonValue::Null),
                            )
                            .unwrap_or_default();
                            let content = json!([{ "type": "text", "text": text }]);
                            write_json_rpc_result(&mut stdout, id, json!({ "content": content }))?;
                        }
                    }
                    Err(e) => {
                        let content =
                            json!([{ "type": "text", "text": format!("IPC failure: {e:#}") }]);
                        write_json_rpc_result(
                            &mut stdout,
                            id,
                            json!({ "content": content, "isError": true }),
                        )?;
                    }
                }
            }
            _ => {
                if id != JsonValue::Null {
                    write_json_rpc_error(
                        &mut stdout,
                        id,
                        -32601,
                        &format!("unsupported method: {method}"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn resolve_session_and_socket() -> anyhow::Result<(SessionBridgeConfig, PathBuf)> {
    if let Ok(config_path_str) = std::env::var("WAKU_DELEGATION_SESSION_PATH") {
        let path = PathBuf::from(config_path_str);
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "no delegation registration at {} — this provider's delegation capability is no longer active",
                path.display()
            )
        })?;
        let config: SessionBridgeConfig = serde_json::from_slice(&bytes)?;
        let socket_path = PathBuf::from(&config.endpoint);
        return Ok((config, socket_path));
    }

    bail!("WAKU_DELEGATION_SESSION_PATH is not set")
}

#[cfg(unix)]
fn send_ipc_request(socket_path: &Path, request: &IpcRequest) -> anyhow::Result<IpcResponse> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path).with_context(|| {
        format!(
            "could not connect to delegation IPC socket {}",
            socket_path.display()
        )
    })?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: IpcResponse = serde_json::from_str(&line)?;
    Ok(response)
}

#[cfg(not(unix))]
fn send_ipc_request(_socket_path: &Path, _request: &IpcRequest) -> anyhow::Result<IpcResponse> {
    bail!("IPC is currently supported on Unix platforms only")
}

fn write_json_rpc_result(
    writer: &mut impl Write,
    id: JsonValue,
    result: JsonValue,
) -> std::io::Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    });
    serde_json::to_writer(&mut *writer, &payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn write_json_rpc_error(
    writer: &mut impl Write,
    id: JsonValue,
    code: i64,
    message: &str,
) -> std::io::Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    });
    serde_json::to_writer(&mut *writer, &payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
