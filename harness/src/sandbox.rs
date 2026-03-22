use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::error::HarnessError;
use crate::policy::ToolPolicyEngine;
use crate::tool::{ToolCallContext, ToolKey, ToolRegistry};

/// Maximum allowed size for code submitted to the sandbox (~1 MB).
const MAX_CODE_SIZE: usize = 1024 * 1024;
const MAX_IPC_LINE_BYTES: usize = 256 * 1024;
const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_PAYLOAD_BYTES: usize = 128 * 1024;
const MAX_ERROR_LEN: usize = 4096;
const MAX_OPEN_FILES: u64 = 64;
const MAX_FILE_SIZE_BYTES: u64 = 1024 * 1024;
const MAX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;

/// Configuration for the Python sandbox.
pub struct SandboxConfig {
    pub timeout: Duration,
    pub runner_path: PathBuf,
}

/// Result of a sandbox execution.
pub struct SandboxResult {
    pub output: String,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// IPC message types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SandboxMessage {
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        key: String,
        params: serde_json::Value,
    },
    #[serde(rename = "done")]
    Done {
        output: String,
        error: Option<String>,
    },
}

#[derive(Debug, Serialize)]
struct ToolResultMsg {
    r#type: &'static str,
    id: String,
    result: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ToolErrorMsg {
    r#type: &'static str,
    id: String,
    error: String,
}

// ---------------------------------------------------------------------------
// Sandbox execution
// ---------------------------------------------------------------------------

/// Execute Python code in the restricted sandbox.
///
/// For each `tool()` call in the Python code, the sandbox communicates back
/// via IPC. We check the policy and call the tool registry.
pub async fn execute(
    code: &str,
    config: &SandboxConfig,
    tools: &ToolRegistry,
    policy: Option<&ToolPolicyEngine>,
    principal: &str,
    cwd: &Path,
    cancellation: &CancellationToken,
) -> Result<SandboxResult, HarnessError> {
    if code.is_empty() {
        return Err(HarnessError::Sandbox("code must not be empty".into()));
    }
    if code.len() > MAX_CODE_SIZE {
        return Err(HarnessError::Sandbox(format!(
            "code exceeds maximum allowed size of {} bytes",
            MAX_CODE_SIZE
        )));
    }
    if code.contains("__HARNESS_END_") {
        return Err(HarnessError::Sandbox(
            "code contains reserved sentinel pattern".into(),
        ));
    }

    // Generate a unique sentinel per invocation to prevent code injection.
    let sentinel = format!("__HARNESS_END_{}__", uuid::Uuid::now_v7());
    let python_path = resolve_python3()?;
    let sandbox_dir = tempfile::Builder::new()
        .prefix("harness-sandbox-")
        .tempdir()
        .map_err(|e| HarnessError::Sandbox(format!("failed to create sandbox dir: {e}")))?;

    let mut command = tokio::process::Command::new(&python_path);
    command
        .arg("-u")
        .arg("-I")
        .arg("-S")
        .arg(&config.runner_path)
        .current_dir(sandbox_dir.path())
        .env_clear()
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        let timeout = config.timeout;
        // SAFETY: pre_exec runs in the child process just before exec. We only
        // call async-signal-safe libc functions to apply resource limits.
        unsafe {
            command.pre_exec(move || apply_unix_resource_limits(timeout));
        }
    }

    let mut child = command
        .spawn()
        .map_err(|e| HarnessError::Sandbox(format!("failed to spawn Python sandbox: {e}")))?;

    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Sandbox("failed to get sandbox stdin".into()))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Sandbox("failed to get sandbox stdout".into()))?;
    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| HarnessError::Sandbox("failed to get sandbox stderr".into()))?;

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stderr);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line).await {
            if n == 0 {
                break;
            }
            warn!(target: "sandbox", "{}", line.trim_end());
            line.clear();
        }
    });

    let mut stdin = BufWriter::new(child_stdin);
    let mut stdout = BufReader::new(child_stdout);

    // Protocol: sentinel line, code, sentinel line.
    write_line(&mut stdin, &sentinel).await?;
    stdin
        .write_all(code.as_bytes())
        .await
        .map_err(|e| HarnessError::Sandbox(format!("write code: {e}")))?;
    if !code.ends_with('\n') {
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| HarnessError::Sandbox(format!("write newline: {e}")))?;
    }
    write_line(&mut stdin, &sentinel).await?;
    stdin
        .flush()
        .await
        .map_err(|e| HarnessError::Sandbox(format!("flush: {e}")))?;

    // IPC loop with timeout and cancellation.
    let tool_context = ToolCallContext::new(cwd.to_path_buf(), principal.to_string());
    let result = tokio::time::timeout(config.timeout, async {
        let mut tool_calls_seen = 0usize;
        loop {
            let line = tokio::select! {
                _ = cancellation.cancelled() => return Err(HarnessError::Cancelled),
                result = read_limited_line(&mut stdout, MAX_IPC_LINE_BYTES) => result?,
            };

            let Some(line) = line else {
                return Err(HarnessError::Sandbox(
                    "sandbox process closed stdout unexpectedly".into(),
                ));
            };

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let msg: SandboxMessage = serde_json::from_str(line)
                .map_err(|e| HarnessError::Sandbox(format!("parse sandbox message: {e}")))?;

            match msg {
                SandboxMessage::ToolCall { id, key, params } => {
                    debug!(id = %id, key = %key, "sandbox tool call");
                    tool_calls_seen += 1;
                    if tool_calls_seen > MAX_TOOL_CALLS {
                        return Err(HarnessError::Sandbox(format!(
                            "tool call limit exceeded ({MAX_TOOL_CALLS})"
                        )));
                    }

                    // Parse the tool key.
                    let tool_key = match ToolKey::parse(&key) {
                        Ok(k) => k,
                        Err(e) => {
                            send_tool_error(&mut stdin, &id, &e).await?;
                            continue;
                        }
                    };

                    if json_size(&params)? > MAX_TOOL_PAYLOAD_BYTES {
                        send_tool_error(
                            &mut stdin,
                            &id,
                            "tool call params exceed maximum allowed size",
                        )
                        .await?;
                        continue;
                    }

                    // Check Cedar policy before calling the tool.
                    if let Some(policy_engine) = policy {
                        if let Err(e) =
                            policy_engine.authorize_tool_call(principal, &tool_key, &params)
                        {
                            warn!(id = %id, key = %key, error = %e, "tool call denied by policy");
                            send_tool_error(&mut stdin, &id, &e.to_string()).await?;
                            continue;
                        }
                    }

                    // Call the tool via the registry.
                    match tokio::select! {
                        _ = cancellation.cancelled() => return Err(HarnessError::Cancelled),
                        result = tools.call_tool(&tool_key, params, &tool_context) => result,
                    } {
                        Ok(result) => {
                            if json_size(&result)? > MAX_TOOL_PAYLOAD_BYTES {
                                send_tool_error(
                                    &mut stdin,
                                    &id,
                                    "tool call result exceeds maximum allowed size",
                                )
                                .await?;
                            } else {
                                send_tool_result(&mut stdin, &id, result).await?;
                            }
                        }
                        Err(e) => {
                            warn!(id = %id, key = %key, error = %e, "tool call failed");
                            send_tool_error(&mut stdin, &id, &e).await?;
                        }
                    }
                }
                SandboxMessage::Done { output, error } => {
                    return Ok(SandboxResult { output, error });
                }
            }
        }
    })
    .await;

    let outcome = match result {
        Ok(inner) => inner,
        Err(_) => Err(HarnessError::Sandbox(format!(
            "execution timed out after {:?}",
            config.timeout
        ))),
    };

    if outcome.is_err() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
    let _ = stderr_task.await;
    outcome
}

async fn write_line(
    stdin: &mut BufWriter<tokio::process::ChildStdin>,
    content: &str,
) -> Result<(), HarnessError> {
    stdin
        .write_all(content.as_bytes())
        .await
        .map_err(|e| HarnessError::Sandbox(format!("write: {e}")))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|e| HarnessError::Sandbox(format!("write newline: {e}")))?;
    Ok(())
}

async fn send_tool_result(
    stdin: &mut BufWriter<tokio::process::ChildStdin>,
    id: &str,
    result: serde_json::Value,
) -> Result<(), HarnessError> {
    let msg = ToolResultMsg {
        r#type: "tool_result",
        id: id.to_string(),
        result,
    };
    let mut line = serde_json::to_string(&msg)
        .map_err(|e| HarnessError::Sandbox(format!("serialize: {e}")))?;
    if line.len() > MAX_IPC_LINE_BYTES {
        return Err(HarnessError::Sandbox(
            "tool result message exceeds maximum IPC line size".into(),
        ));
    }
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| HarnessError::Sandbox(format!("write tool result: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| HarnessError::Sandbox(format!("flush tool result: {e}")))?;
    Ok(())
}

async fn send_tool_error(
    stdin: &mut BufWriter<tokio::process::ChildStdin>,
    id: &str,
    error: &str,
) -> Result<(), HarnessError> {
    let error = truncate_utf8(error, MAX_ERROR_LEN, "... (truncated)");
    let msg = ToolErrorMsg {
        r#type: "tool_error",
        id: id.to_string(),
        error,
    };
    let mut line = serde_json::to_string(&msg)
        .map_err(|e| HarnessError::Sandbox(format!("serialize: {e}")))?;
    if line.len() > MAX_IPC_LINE_BYTES {
        return Err(HarnessError::Sandbox(
            "tool error message exceeds maximum IPC line size".into(),
        ));
    }
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| HarnessError::Sandbox(format!("write tool error: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| HarnessError::Sandbox(format!("flush tool error: {e}")))?;
    Ok(())
}

fn json_size(value: &serde_json::Value) -> Result<usize, HarnessError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|e| HarnessError::Sandbox(format!("serialize JSON size: {e}")))
}

fn truncate_utf8(input: &str, max_bytes: usize, suffix: &str) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = 0usize;
    for (idx, ch) in input.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }

    let mut truncated = input[..end].to_string();
    truncated.push_str(suffix);
    truncated
}

async fn read_limited_line(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    max_len: usize,
) -> Result<Option<String>, HarnessError> {
    let mut buffer = Vec::new();

    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| HarnessError::Sandbox(format!("read from sandbox: {e}")))?;

        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            return Err(HarnessError::Sandbox(
                "sandbox process closed stdout mid-message".into(),
            ));
        }

        if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
            let take = pos + 1;
            if buffer.len() + take > max_len {
                return Err(HarnessError::Sandbox(format!(
                    "sandbox message exceeds maximum line size of {max_len} bytes"
                )));
            }
            buffer.extend_from_slice(&available[..take]);
            reader.consume(take);
            break;
        }

        if buffer.len() + available.len() > max_len {
            return Err(HarnessError::Sandbox(format!(
                "sandbox message exceeds maximum line size of {max_len} bytes"
            )));
        }

        let take = available.len();
        buffer.extend_from_slice(available);
        reader.consume(take);
    }

    String::from_utf8(buffer)
        .map(Some)
        .map_err(|e| HarnessError::Sandbox(format!("sandbox message is not valid UTF-8: {e}")))
}

fn resolve_python3() -> Result<PathBuf, HarnessError> {
    static PYTHON_PATH: OnceLock<PathBuf> = OnceLock::new();

    if let Some(path) = PYTHON_PATH.get() {
        return Ok(path.clone());
    }

    let resolved = std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("python3"))
                .find(|candidate| candidate.is_file())
        })
        .or_else(|| {
            let fallback = PathBuf::from("/usr/bin/python3");
            fallback.is_file().then_some(fallback)
        })
        .ok_or_else(|| HarnessError::Sandbox("unable to resolve python3 executable".into()))?;

    let canonical = resolved.canonicalize().unwrap_or(resolved);
    let _ = PYTHON_PATH.set(canonical.clone());
    Ok(canonical)
}

#[cfg(unix)]
fn apply_unix_resource_limits(timeout: Duration) -> std::io::Result<()> {
    let cpu_limit = std::cmp::max(1, timeout.as_secs().saturating_add(1)) as libc::rlim_t;

    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let file_size_limit = libc::rlimit {
        rlim_cur: MAX_FILE_SIZE_BYTES as libc::rlim_t,
        rlim_max: MAX_FILE_SIZE_BYTES as libc::rlim_t,
    };
    let open_files_limit = libc::rlimit {
        rlim_cur: MAX_OPEN_FILES as libc::rlim_t,
        rlim_max: MAX_OPEN_FILES as libc::rlim_t,
    };
    let cpu_time_limit = libc::rlimit {
        rlim_cur: cpu_limit,
        rlim_max: cpu_limit,
    };

    macro_rules! set_limit {
        ($resource:expr, $limit:expr, $best_effort:expr) => {{
            // SAFETY: setrlimit is called in the child process before exec.
            if unsafe { libc::setrlimit($resource, $limit) } != 0 {
                let err = std::io::Error::last_os_error();
                if !($best_effort
                    && matches!(err.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOTSUP)))
                {
                    return Err(err);
                }
            }
        }};
    }

    set_limit!(libc::RLIMIT_CORE, &core_limit, false);
    set_limit!(libc::RLIMIT_FSIZE, &file_size_limit, false);
    set_limit!(libc::RLIMIT_NOFILE, &open_files_limit, false);
    set_limit!(libc::RLIMIT_CPU, &cpu_time_limit, false);
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        let memory_limit = libc::rlimit {
            rlim_cur: MAX_MEMORY_BYTES as libc::rlim_t,
            rlim_max: MAX_MEMORY_BYTES as libc::rlim_t,
        };
        set_limit!(libc::RLIMIT_AS, &memory_limit, true);
    }

    Ok(())
}

/// Find the runner.py script. Looks next to the binary first, then in common paths.
pub fn find_runner(binary_dir: Option<&Path>) -> Result<PathBuf, HarnessError> {
    // Try relative to binary directory.
    if let Some(dir) = binary_dir {
        let candidate = dir.join("sandbox").join("runner.py");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // Try relative to current working directory.
    let cwd_candidate = PathBuf::from("sandbox/runner.py");
    if cwd_candidate.exists() {
        return Ok(cwd_candidate.canonicalize().unwrap_or(cwd_candidate));
    }

    // Try relative to the cargo manifest dir (for development).
    let manifest_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("sandbox")
        .join("runner.py");
    if manifest_candidate.exists() {
        return Ok(manifest_candidate);
    }

    Err(HarnessError::Config("cannot find sandbox/runner.py".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolCallContext, ToolInfo, ToolKey, ToolRegistry, ToolSchema, ToolSource};

    struct EchoSource;

    struct ContextSource;

    #[async_trait::async_trait(?Send)]
    impl ToolSource for EchoSource {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            Ok(vec![ToolInfo {
                key: ToolKey::new("echo", "echo"),
                description: "Echo params".to_string(),
            }])
        }

        async fn get_schemas(&self, _tools: &[String]) -> Result<Vec<ToolSchema>, String> {
            Ok(vec![ToolSchema {
                key: ToolKey::new("echo", "echo"),
                description: "Echo params".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
            }])
        }

        async fn call_tool(
            &self,
            _tool: &str,
            params: serde_json::Value,
            _context: &ToolCallContext,
        ) -> Result<serde_json::Value, String> {
            Ok(params)
        }
    }

    #[async_trait::async_trait(?Send)]
    impl ToolSource for ContextSource {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            Ok(vec![ToolInfo {
                key: ToolKey::new("context", "inspect"),
                description: "Echo execution context".to_string(),
            }])
        }

        async fn get_schemas(&self, _tools: &[String]) -> Result<Vec<ToolSchema>, String> {
            Ok(vec![])
        }

        async fn call_tool(
            &self,
            _tool: &str,
            _params: serde_json::Value,
            context: &ToolCallContext,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({
                "cwd": context.cwd,
                "principal": context.principal,
            }))
        }
    }

    fn sandbox_config(timeout: Duration) -> SandboxConfig {
        SandboxConfig {
            timeout,
            runner_path: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("sandbox")
                .join("runner.py"),
        }
    }

    fn manifest_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[tokio::test]
    async fn execute_captures_stdout_without_runner_noise() {
        let registry = ToolRegistry::new();
        let token = CancellationToken::new();
        let result = execute(
            "print('hi')",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"test""#,
            &manifest_dir(),
            &token,
        )
        .await
        .unwrap();

        assert_eq!(result.output, "hi\n");
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn execute_blocks_private_attribute_escapes() {
        let registry = ToolRegistry::new();
        let token = CancellationToken::new();
        let result = execute(
            "print(tool.__class__)",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"test""#,
            &manifest_dir(),
            &token,
        )
        .await
        .unwrap();

        assert!(result.output.is_empty());
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("private attribute"));
    }

    #[tokio::test]
    async fn execute_preserves_tool_bridge_functionality() {
        let mut registry = ToolRegistry::new();
        registry.register("echo".to_string(), Box::new(EchoSource));
        registry.refresh_tools().await.unwrap();

        let token = CancellationToken::new();
        let result = execute(
            "print(tool('echo:echo', {'value': 'hi'})['value'])",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"test""#,
            &manifest_dir(),
            &token,
        )
        .await
        .unwrap();

        assert_eq!(result.output, "hi\n");
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn execute_captures_module_stdout() {
        let registry = ToolRegistry::new();
        let token = CancellationToken::new();
        let result = execute(
            "import pprint\npprint.pprint({'a': 1})",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"test""#,
            &manifest_dir(),
            &token,
        )
        .await
        .unwrap();

        assert_eq!(result.output, "{'a': 1}\n");
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn execute_passes_logical_context_to_tools() {
        let mut registry = ToolRegistry::new();
        registry.register("context".to_string(), Box::new(ContextSource));
        registry.refresh_tools().await.unwrap();

        let logical_cwd = manifest_dir().join("logical-project");
        let token = CancellationToken::new();
        let result = execute(
            "result = tool('context:inspect', {})\nprint(result['cwd'])\nprint(result['principal'])",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"alice""#,
            &logical_cwd,
            &token,
        )
        .await
        .unwrap();

        assert!(result.output.contains(&logical_cwd.display().to_string()));
        assert!(result.output.contains(r#"User::"alice""#));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn execute_truncates_large_output() {
        let registry = ToolRegistry::new();
        let token = CancellationToken::new();
        let result = execute(
            "print('x' * 200000)",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"test""#,
            &manifest_dir(),
            &token,
        )
        .await
        .unwrap();

        assert!(result.output.contains("output truncated"));
        assert!(result.output.len() < 140_000);
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn execute_rejects_oversized_tool_results() {
        struct HugeSource;

        #[async_trait::async_trait(?Send)]
        impl ToolSource for HugeSource {
            async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
                Ok(vec![ToolInfo {
                    key: ToolKey::new("huge", "blob"),
                    description: "Return a huge result".to_string(),
                }])
            }

            async fn get_schemas(&self, _tools: &[String]) -> Result<Vec<ToolSchema>, String> {
                Ok(vec![])
            }

            async fn call_tool(
                &self,
                _tool: &str,
                _params: serde_json::Value,
                _context: &ToolCallContext,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({"blob": "x".repeat(MAX_TOOL_PAYLOAD_BYTES + 1024)}))
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register("huge".to_string(), Box::new(HugeSource));
        registry.refresh_tools().await.unwrap();

        let token = CancellationToken::new();
        let result = execute(
            "try:\n    tool('huge:blob', {})\nexcept Exception as exc:\n    print(exc)",
            &sandbox_config(Duration::from_secs(2)),
            &registry,
            None,
            r#"User::"test""#,
            &manifest_dir(),
            &token,
        )
        .await
        .unwrap();

        assert!(result
            .output
            .contains("tool call result exceeds maximum allowed size"));
        assert!(result.error.is_none());
    }
}
