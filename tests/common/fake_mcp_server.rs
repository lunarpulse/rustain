//! Fake MCP server binary for conformance tests.
//!
//! Speaks the MCP JSON-RPC 2.0 protocol over stdin/stdout.
//! Behavior controlled via environment variables:
//! - `FAKE_MCP_DROP_AFTER_MS=1500` — exits after N milliseconds
//! - `FAKE_MCP_FAIL_INITIALIZE=1` — rejects `initialize` with an error
//! - `FAKE_MCP_HANG_TOOLS_LIST=1` — never responds to `tools/list`

use std::io::{self, BufRead, Write};
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

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
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
                        "capabilities": { "tools": { "listChanged": false } },
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
                                "name": "fake_tool_a",
                                "description": "First fake tool for testing",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "fake_tool_b",
                                "description": "Second fake tool for testing",
                                "inputSchema": { "type": "object", "properties": {} }
                            }
                        ]
                    }
                })
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
