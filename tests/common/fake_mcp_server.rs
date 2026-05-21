//! Fake MCP server binary for conformance tests.
//!
//! Speaks the MCP JSON-RPC 2.0 protocol over stdin/stdout.
//! Behavior controlled via environment variables:
//! - `FAKE_MCP_DROP_AFTER_MS=1500` — exits after N milliseconds
//! - `FAKE_MCP_FAIL_INITIALIZE=1` — rejects `initialize` with an error
//! - `FAKE_MCP_HANG_TOOLS_LIST=1` — never responds to `tools/list`
//! - `FAKE_MCP_TOOL_ERROR=1` — `tools/call` returns isError: true (Story 9.2)
//! - `FAKE_MCP_HANG_CALL_TOOL=1` — `tools/call` never responds (Story 9.2)
//! - `FAKE_MCP_EMIT_LIST_CHANGED_AFTER_MS=500` — emit list_changed notification
//!   after N ms and switch to a different tool list (Story 9.2)

use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() {
    let drop_after_ms: Option<u64> = std::env::var("FAKE_MCP_DROP_AFTER_MS")
        .ok()
        .and_then(|v| v.parse().ok());

    if let Some(ms) = drop_after_ms {
        let _ = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            std::process::exit(0);
        });
    }

    let fail_init = std::env::var("FAKE_MCP_FAIL_INITIALIZE").as_deref() == Ok("1");
    let hang_tools = std::env::var("FAKE_MCP_HANG_TOOLS_LIST").as_deref() == Ok("1");
    let tool_error = std::env::var("FAKE_MCP_TOOL_ERROR").as_deref() == Ok("1");
    let hang_call = std::env::var("FAKE_MCP_HANG_CALL_TOOL").as_deref() == Ok("1");
    let emit_list_changed_ms: Option<u64> = std::env::var("FAKE_MCP_EMIT_LIST_CHANGED_AFTER_MS")
        .ok()
        .and_then(|v| v.parse().ok());

    let list_changed = Arc::new(AtomicBool::new(false));

    // Spawn list_changed notifier if configured
    if let Some(ms) = emit_list_changed_ms {
        let changed = Arc::clone(&list_changed);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            changed.store(true, Ordering::SeqCst);
            // The notification will be emitted on the next I/O loop iteration
        });
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        // Emit pending list_changed notification before processing the next message
        if list_changed.load(Ordering::SeqCst) {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": {}
            });
            writeln!(stdout, "{}", notification).unwrap();
            stdout.flush().unwrap();
            list_changed.store(false, Ordering::SeqCst);
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" if !fail_init => {
                let id = id.unwrap_or(serde_json::json!(1));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": "fake-mcp-server", "version": "0.1.0" }
                    }
                })
            }
            "initialize" => {
                let id = id.unwrap_or(serde_json::json!(1));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32603, "message": "initialize rejected by FAKE_MCP_FAIL_INITIALIZE" }
                })
            }
            "tools/list" if !hang_tools => {
                let id = id.unwrap_or(serde_json::json!(2));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "echo",
                                "description": "Echoes back the input text",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "text": { "type": "string" }
                                    }
                                },
                                "annotations": { "readOnlyHint": false }
                            },
                            {
                                "name": "add",
                                "description": "Adds two numbers",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "a": { "type": "number" },
                                        "b": { "type": "number" }
                                    }
                                },
                                "annotations": { "readOnlyHint": true }
                            }
                        ]
                    }
                })
            }
            "tools/call" if !hang_call => {
                let id = id.unwrap_or(serde_json::json!(3));
                let params = msg.get("params").cloned().unwrap_or_default();
                let tool_name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));

                if tool_error {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [
                                { "type": "text", "text": format!("error from {}", tool_name) }
                            ],
                            "isError": true
                        }
                    })
                } else {
                    let echo = match tool_name {
                        "echo" => {
                            let txt = arguments.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            format!("echo: {}", txt)
                        }
                        "add" => {
                            let a = arguments.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let b = arguments.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            format!("{}", a + b)
                        }
                        other => format!("unknown tool: {}", other),
                    };
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [
                                { "type": "text", "text": echo }
                            ],
                            "isError": false
                        }
                    })
                }
            }
            "notifications/initialized" => {
                continue;
            }
            _ => {
                let id = id.unwrap_or(serde_json::json!(0));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })
            }
        };

        writeln!(stdout, "{}", response).unwrap();
        stdout.flush().unwrap();
    }
}
