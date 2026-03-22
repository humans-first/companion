use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, RwLock};
use tracing::debug;

use crate::config::McpServerConfig;
use crate::tool::{ToolCallContext, ToolInfo, ToolKey, ToolSchema, ToolSource, ToolSourceFactory};

const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// JSON-RPC types (minimal, just what MCP needs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    id: Option<u64>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// MCP protocol response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct McpToolDef {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct McpToolsListResult {
    tools: Vec<McpToolDef>,
}

#[derive(Debug, Deserialize)]
struct McpToolCallResult {
    content: Vec<serde_json::Value>,
    #[serde(rename = "isError", default)]
    is_error: bool,
}

// ---------------------------------------------------------------------------
// McpClient: manages a single MCP server subprocess
// ---------------------------------------------------------------------------

struct McpClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Reap the child to avoid zombies. `Child::wait` is async, so we
        // spawn a thread to do a blocking waitpid after the kill signal.
        if let Some(pid) = self.child.id() {
            std::thread::spawn(move || {
                extern "C" {
                    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
                }
                // SAFETY: pid was valid moments ago; errors are ignored.
                unsafe { waitpid(pid as i32, std::ptr::null_mut(), 0) };
            });
        }
    }
}

impl McpClient {
    /// Spawn an MCP server process and perform the initialize handshake.
    async fn spawn(config: &McpServerConfig) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn MCP server '{}': {e}", config.command))?;

        let stdin = BufWriter::new(child.stdin.take().ok_or("failed to get MCP server stdin")?);
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("failed to get MCP server stdout")?,
        );

        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };

        // MCP initialize handshake.
        let init_result = client
            .send_request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "harness",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
            )
            .await?;

        debug!(init = ?init_result, "MCP server initialized");

        // Send initialized notification.
        client
            .send_notification("notifications/initialized")
            .await?;

        Ok(client)
    }

    async fn send_request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let mut line =
            serde_json::to_string(&request).map_err(|e| format!("serialize request: {e}"))?;
        line.push('\n');

        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to MCP server: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("flush MCP server stdin: {e}"))?;

        // Read response with a deadline, skipping notifications.
        let deadline = tokio::time::Instant::now() + MCP_REQUEST_TIMEOUT;
        loop {
            let mut response_line = String::new();
            let read_result =
                tokio::time::timeout_at(deadline, self.stdout.read_line(&mut response_line))
                    .await;

            let bytes_read = match read_result {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(format!("read from MCP server: {e}")),
                Err(_) => {
                    return Err(format!(
                        "MCP request '{method}' timed out after {}s",
                        MCP_REQUEST_TIMEOUT.as_secs()
                    ))
                }
            };

            if bytes_read == 0 {
                return Err("MCP server closed stdout".to_string());
            }

            let trimmed = response_line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let response: JsonRpcResponse = serde_json::from_str(trimmed)
                .map_err(|e| format!("parse MCP response: {e} (line: {trimmed})"))?;

            if response.id.is_none() {
                debug!(line = trimmed, "skipping MCP notification");
                continue;
            }

            if response.id != Some(id) {
                return Err(format!(
                    "MCP response id mismatch: expected {id}, got {:?}",
                    response.id
                ));
            }

            if let Some(err) = response.error {
                return Err(format!("MCP error {}: {}", err.code, err.message));
            }

            return response
                .result
                .ok_or_else(|| "MCP response has no result".to_string());
        }
    }

    async fn send_notification(&mut self, method: &str) -> Result<(), String> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
        };

        let mut line = serde_json::to_string(&notification)
            .map_err(|e| format!("serialize notification: {e}"))?;
        line.push('\n');

        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write notification to MCP server: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("flush MCP server stdin: {e}"))?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// McpToolSource: implements ToolSource for a single MCP server
// ---------------------------------------------------------------------------

pub struct McpToolSource {
    source_name: String,
    client: Mutex<McpClient>,
    /// Cached tool definitions from tools/list.
    tool_defs: RwLock<Vec<McpToolDef>>,
}

pub struct McpToolSourceFactory {
    config: McpServerConfig,
}

impl McpToolSourceFactory {
    pub fn new(config: McpServerConfig) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait(?Send)]
impl ToolSourceFactory for McpToolSourceFactory {
    async fn create(&self, source_name: &str) -> Result<Box<dyn ToolSource>, String> {
        Ok(Box::new(McpToolSource::new(source_name, &self.config).await?))
    }
}

impl McpToolSource {
    /// Spawn the MCP server, initialize it, and discover its tools.
    pub async fn new(source_name: &str, config: &McpServerConfig) -> Result<Self, String> {
        let mut client = McpClient::spawn(config).await?;
        let tool_defs = fetch_tool_defs(&mut client).await?;

        debug!(
            source = source_name,
            count = tool_defs.len(),
            "discovered MCP tools"
        );

        Ok(Self {
            source_name: source_name.to_string(),
            client: Mutex::new(client),
            tool_defs: RwLock::new(tool_defs),
        })
    }
}

#[async_trait::async_trait(?Send)]
impl ToolSource for McpToolSource {
    async fn refresh(&self) -> Result<(), String> {
        let mut client = self.client.lock().await;
        let tool_defs = fetch_tool_defs(&mut client).await?;
        *self.tool_defs.write().await = tool_defs;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
        let tool_defs = self.tool_defs.read().await;
        Ok(tool_defs
            .iter()
            .map(|def| ToolInfo {
                key: ToolKey::new(&self.source_name, &def.name),
                description: def.description.clone().unwrap_or_default(),
            })
            .collect())
    }

    async fn get_schemas(&self, tools: &[String]) -> Result<Vec<ToolSchema>, String> {
        let requested: std::collections::HashSet<&str> = tools.iter().map(|s| s.as_str()).collect();

        let tool_defs = self.tool_defs.read().await;
        Ok(tool_defs
            .iter()
            .filter(|def| requested.contains(def.name.as_str()))
            .map(|def| ToolSchema {
                key: ToolKey::new(&self.source_name, &def.name),
                description: def.description.clone().unwrap_or_default(),
                input_schema: def.input_schema.clone().unwrap_or(serde_json::json!({})),
                output_schema: None, // MCP spec doesn't define output schemas yet.
            })
            .collect())
    }

    async fn call_tool(
        &self,
        tool: &str,
        params: serde_json::Value,
        _context: &ToolCallContext,
    ) -> Result<serde_json::Value, String> {
        let mut client = self.client.lock().await;
        let result = client
            .send_request(
                "tools/call",
                Some(serde_json::json!({
                    "name": tool,
                    "arguments": params
                })),
            )
            .await?;

        let call_result: McpToolCallResult =
            serde_json::from_value(result).map_err(|e| format!("parse tools/call result: {e}"))?;
        decode_tool_call_result(call_result)
    }
}

async fn fetch_tool_defs(client: &mut McpClient) -> Result<Vec<McpToolDef>, String> {
    let result = client.send_request("tools/list", None).await?;
    let tools_result: McpToolsListResult =
        serde_json::from_value(result).map_err(|e| format!("parse tools/list: {e}"))?;
    Ok(tools_result.tools)
}

fn block_type(block: &serde_json::Value) -> Option<&str> {
    block.get("type").and_then(serde_json::Value::as_str)
}

fn block_text(block: &serde_json::Value) -> Option<&str> {
    block.get("text").and_then(serde_json::Value::as_str)
}

fn decode_tool_call_result(call_result: McpToolCallResult) -> Result<serde_json::Value, String> {
    if call_result.is_error {
        let texts: Vec<&str> = call_result.content.iter().filter_map(block_text).collect();
        let error_text = if texts.is_empty() {
            serde_json::Value::Array(call_result.content).to_string()
        } else {
            texts.join("\n")
        };
        return Err(format!("MCP tool error: {error_text}"));
    }

    let texts: Vec<&str> = call_result
        .content
        .iter()
        .filter(|block| block_type(block) == Some("text"))
        .filter_map(block_text)
        .collect();

    if call_result.content.len() == 1 && texts.len() == 1 {
        if let Ok(json) = serde_json::from_str(texts[0]) {
            return Ok(json);
        }
        return Ok(serde_json::Value::String(texts[0].to_string()));
    }

    if texts.len() == call_result.content.len() {
        return Ok(serde_json::Value::String(texts.join("\n")));
    }

    Ok(serde_json::Value::Array(call_result.content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_single_text_json_payload() {
        let result = decode_tool_call_result(McpToolCallResult {
            content: vec![serde_json::json!({"type": "text", "text": "{\"ok\":true}"})],
            is_error: false,
        })
        .unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[test]
    fn decode_mixed_content_preserves_structure() {
        let result = decode_tool_call_result(McpToolCallResult {
            content: vec![
                serde_json::json!({"type": "text", "text": "hello"}),
                serde_json::json!({"type": "image", "mimeType": "image/png", "data": "abc"}),
            ],
            is_error: false,
        })
        .unwrap();
        assert!(matches!(result, serde_json::Value::Array(_)));
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[test]
    fn decode_error_without_text_includes_raw_blocks() {
        let err = decode_tool_call_result(McpToolCallResult {
            content: vec![serde_json::json!({"type": "resource", "uri": "file:///tmp/x"})],
            is_error: true,
        })
        .unwrap_err();
        assert!(err.contains("resource"));
        assert!(err.contains("file:///tmp/x"));
    }
}
