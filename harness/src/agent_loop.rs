use async_openai::types::{
    ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage, FinishReason,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::error::HarnessError;
use crate::llm::{self, LlmProvider, LlmRequestConfig};
use crate::policy::ToolPolicyEngine;
use crate::sandbox::{self, SandboxConfig};
use crate::session::{ConversationMessage, Role, SessionData};
use crate::tool::{ToolKey, ToolRegistry};

const MAX_ITERATIONS: usize = 20;

pub struct RunConfig<'a> {
    pub system_prompt: &'a str,
    pub model_id: &'a str,
    pub principal: &'a str,
}

fn should_offer_tools(session: &SessionData, tools: &ToolRegistry) -> bool {
    let _ = session;
    !tools.all_tools().is_empty()
}

fn build_messages(
    session: &SessionData,
    full_system_prompt: &str,
    include_tool_history: bool,
) -> Vec<ChatCompletionRequestMessage> {
    let mut messages = Vec::new();
    messages.push(llm::system_message(full_system_prompt));

    for msg in &session.history {
        let message = match msg.role {
            Role::User => msg.content.as_deref().map(llm::user_message),
            Role::Assistant => {
                if let Some(ref tool_calls) = msg.tool_calls {
                    if include_tool_history {
                        Some(ChatCompletionRequestMessage::Assistant(
                            ChatCompletionRequestAssistantMessage {
                                content: msg.content.as_ref().map(|c| {
                                    ChatCompletionRequestAssistantMessageContent::Text(c.clone())
                                }),
                                tool_calls: Some(tool_calls.clone()),
                                ..Default::default()
                            },
                        ))
                    } else {
                        msg.content.as_deref().map(llm::assistant_message)
                    }
                } else {
                    msg.content.as_deref().map(llm::assistant_message)
                }
            }
            Role::Tool => {
                if include_tool_history {
                    if let (Some(ref id), Some(ref content)) = (&msg.tool_call_id, &msg.content) {
                        Some(llm::tool_result_message(id, content))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        if let Some(message) = message {
            messages.push(message);
        }
    }

    messages
}

/// Build the full system prompt including the tool catalog.
fn build_system_prompt(base_prompt: &str, tools: &ToolRegistry) -> String {
    let all_tools = tools.all_tools();

    if all_tools.is_empty() {
        return base_prompt.to_string();
    }

    let mut prompt = base_prompt.to_string();
    prompt.push_str("\n\n## Available MCP Tools\n\n");
    prompt.push_str(
        "Only call tools when they are actually needed to complete the user request.\n",
    );
    prompt.push_str(
        "Use discover(tools=[\"source:tool\", ...]) to get full input schemas before coding.\n",
    );
    prompt.push_str(
        "Then use execute(code=\"...\") with Python code that calls tool(\"source:tool\", params).\n\n",
    );

    for info in all_tools {
        prompt.push_str(&format!("- `{}` — {}\n", info.key, info.description));
    }

    prompt
}

/// Run the agent loop: call LLM, handle tool calls, repeat until content response.
pub async fn run<L: LlmProvider + ?Sized>(
    session: &mut SessionData,
    llm: &L,
    tools: &ToolRegistry,
    sandbox_config: &SandboxConfig,
    policy: Option<&ToolPolicyEngine>,
    config: RunConfig<'_>,
    cancellation: &CancellationToken,
) -> Result<String, HarnessError> {
    let llm_tools = if should_offer_tools(session, tools) {
        llm::harness_tool_definitions()
    } else {
        Vec::new()
    };
    let full_system_prompt = build_system_prompt(config.system_prompt, tools);
    let include_tool_history = !llm_tools.is_empty();

    for iteration in 0..MAX_ITERATIONS {
        let messages = build_messages(session, &full_system_prompt, include_tool_history);

        debug!(iteration, messages = messages.len(), "calling LLM");

        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err(HarnessError::Cancelled),
            result = llm.chat_completion(
                messages,
                llm_tools.clone(),
                LlmRequestConfig {
                    model_id: Some(config.model_id.to_string()),
                },
            ) => result,
        }?;

        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| HarnessError::Llm("no choices returned".into()))?;

        let finish_reason = choice.finish_reason;
        let message = choice.message;

        match finish_reason {
            Some(FinishReason::Stop)
            | Some(FinishReason::ToolCalls)
            | Some(FinishReason::FunctionCall)
            | None => {}
            Some(reason) => {
                return Err(HarnessError::Llm(format!(
                    "unexpected finish_reason: {reason:?}"
                )));
            }
        }

        // Check for tool calls.
        if let Some(tool_calls) = message.tool_calls {
            if !tool_calls.is_empty() {
                debug!(count = tool_calls.len(), "LLM requested tool calls");

                // Store the assistant message with tool calls in history.
                session
                    .history
                    .push(ConversationMessage::assistant_tool_calls_with_content(
                        message.content.clone(),
                        tool_calls.clone(),
                    ));

                // Handle each tool call.
                if llm_tools.is_empty() {
                    if let Some(content) = message.content {
                        session
                            .history
                            .push(ConversationMessage::assistant(content.clone()));
                        return Ok(content);
                    }
                    return Err(HarnessError::Llm(
                        "model returned tool calls when no tools were offered".to_string(),
                    ));
                }

                for tool_call in &tool_calls {
                    let result = tokio::select! {
                        _ = cancellation.cancelled() => return Err(HarnessError::Cancelled),
                        result = handle_tool_call(
                            tool_call,
                            tools,
                            sandbox_config,
                            policy,
                            config.principal,
                            &session.cwd,
                            cancellation,
                        ) => result,
                    };
                    session.history.push(ConversationMessage::tool_result(
                        tool_call.id.clone(),
                        result,
                    ));
                }

                continue;
            }
        }

        // No tool calls — LLM returned content.
        let content = message.content.unwrap_or_default();
        session
            .history
            .push(ConversationMessage::assistant(content.clone()));

        debug!(
            iteration,
            finish_reason = ?finish_reason,
            "agent loop complete"
        );

        return Ok(content);
    }

    warn!("agent loop hit max iterations ({MAX_ITERATIONS})");
    Err(HarnessError::Llm(format!(
        "exceeded {MAX_ITERATIONS} iterations"
    )))
}

/// Sanitize params for logging: truncate large values and redact sensitive keys.
fn sanitize_params(args: &str) -> String {
    const MAX_LEN: usize = 200;
    const SENSITIVE_KEYS: &[&str] = &["password", "secret", "token", "key", "credential"];

    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(args) else {
        return "<non-JSON params>".to_string();
    };

    if let Some(obj) = value.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            if SENSITIVE_KEYS.iter().any(|s| k.to_lowercase().contains(s)) {
                *v = serde_json::Value::String("<redacted>".to_string());
            }
        }
    }

    let s = value.to_string();
    if s.len() > MAX_LEN {
        format!("{}…", &s[..MAX_LEN])
    } else {
        s
    }
}

/// Handle a single tool call from the LLM.
async fn handle_tool_call(
    tool_call: &ChatCompletionMessageToolCall,
    tools: &ToolRegistry,
    sandbox_config: &SandboxConfig,
    policy: Option<&ToolPolicyEngine>,
    principal: &str,
    cwd: &std::path::Path,
    cancellation: &CancellationToken,
) -> String {
    let name = &tool_call.function.name;
    let args = &tool_call.function.arguments;
    let start = std::time::Instant::now();

    let result = match name.as_str() {
        "discover" => handle_discover(args, tools).await,
        "execute" => {
            handle_execute(
                args,
                sandbox_config,
                tools,
                policy,
                principal,
                cwd,
                cancellation,
            )
            .await
        }
        other => format!("unknown tool: {other}"),
    };

    if result.starts_with("error")
        || result.starts_with("unknown tool")
        || result.starts_with("sandbox execution failed")
    {
        error!(
            tool_key = %name,
            principal = %principal,
            params = %sanitize_params(args),
            duration_ms = %start.elapsed().as_millis(),
            error = %result,
            "tool call failed",
        );
    }

    result
}

/// Handle the discover tool: parse requested tool keys, fetch schemas from registry.
async fn handle_discover(args: &str, tools: &ToolRegistry) -> String {
    #[derive(serde::Deserialize)]
    struct DiscoverArgs {
        tools: Vec<String>,
    }

    let parsed: DiscoverArgs = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return format!("error parsing discover args: {e}"),
    };

    // Parse tool keys.
    let keys: Vec<ToolKey> = parsed
        .tools
        .iter()
        .filter_map(|s| match ToolKey::parse(s) {
            Ok(k) => Some(k),
            Err(e) => {
                warn!(key = s, error = %e, "invalid tool key in discover");
                None
            }
        })
        .collect();

    if keys.is_empty() {
        return "no valid tool keys provided".to_string();
    }

    match tools.get_schemas(&keys).await {
        Ok(schemas) => {
            let result: Vec<serde_json::Value> = schemas
                .iter()
                .map(|s| {
                    let mut obj = serde_json::json!({
                        "key": s.key.to_string(),
                        "description": s.description,
                        "input_schema": s.input_schema,
                    });
                    if let Some(ref output) = s.output_schema {
                        obj["output_schema"] = output.clone();
                    }
                    obj
                })
                .collect();
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| "[]".to_string())
        }
        Err(e) => format!("error fetching schemas: {e}"),
    }
}

/// Handle the execute tool: run Python code in the sandbox.
async fn handle_execute(
    args: &str,
    sandbox_config: &SandboxConfig,
    tools: &ToolRegistry,
    policy: Option<&ToolPolicyEngine>,
    principal: &str,
    cwd: &std::path::Path,
    cancellation: &CancellationToken,
) -> String {
    #[derive(serde::Deserialize)]
    struct ExecuteArgs {
        code: String,
    }

    let parsed: ExecuteArgs = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return format!("error parsing execute args: {e}"),
    };

    debug!(code_len = parsed.code.len(), "executing sandbox code");

    match sandbox::execute(
        &parsed.code,
        sandbox_config,
        tools,
        policy,
        principal,
        cwd,
        cancellation,
    )
    .await
    {
        Ok(result) => {
            let mut output = result.output;
            if let Some(err) = result.error {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&format!("Error: {err}"));
            }
            if output.is_empty() {
                "execution completed (no output)".to_string()
            } else {
                output
            }
        }
        Err(e) => format!("sandbox execution failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolCallContext, ToolInfo, ToolKey, ToolRegistry, ToolSchema, ToolSource};

    struct FakeSource(Vec<ToolInfo>);

    #[async_trait::async_trait(?Send)]
    impl ToolSource for FakeSource {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            Ok(self.0.clone())
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
            Ok(serde_json::Value::Null)
        }
    }

    fn registry_with(tools: Vec<ToolInfo>) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register("src".to_string(), Box::new(FakeSource(tools)));
        r
    }

    // --- build_system_prompt ---

    #[test]
    fn build_system_prompt_empty_tools_returns_base() {
        let reg = ToolRegistry::new();
        assert_eq!(build_system_prompt("base", &reg), "base");
    }

    #[tokio::test]
    async fn build_system_prompt_with_refreshed_tools() {
        let tools = vec![
            ToolInfo {
                key: ToolKey::new("src", "alpha"),
                description: "does alpha".to_string(),
            },
            ToolInfo {
                key: ToolKey::new("src", "beta"),
                description: "does beta".to_string(),
            },
        ];
        let mut reg = registry_with(tools);
        reg.refresh_tools().await.unwrap();

        let result = build_system_prompt("base", &reg);
        assert!(result.starts_with("base\n\n## Available MCP Tools"));
        assert!(result.contains("src:alpha"));
        assert!(result.contains("does alpha"));
        assert!(result.contains("src:beta"));
        assert!(result.contains("does beta"));
        assert!(result.contains("Only call tools when they are actually needed"));
    }

    // --- build_messages ---

    fn session_with_tool_history() -> SessionData {
        let mut session = SessionData::new(std::path::PathBuf::from("/tmp"), "code", "model");
        session
            .history
            .push(ConversationMessage::user("find something".to_string()));
        session.history.push(ConversationMessage::assistant_tool_calls_with_content(
            Some("Let me inspect that.".to_string()),
            vec![ChatCompletionMessageToolCall {
                id: "call-1".to_string(),
                r#type: async_openai::types::ChatCompletionToolType::Function,
                function: async_openai::types::FunctionCall {
                    name: "discover".to_string(),
                    arguments: "{\"tools\":[\"src:alpha\"]}".to_string(),
                },
            }],
        ));
        session.history.push(ConversationMessage::tool_result(
            "call-1".to_string(),
            "[]".to_string(),
        ));
        session
            .history
            .push(ConversationMessage::assistant("Done".to_string()));
        session
    }

    #[test]
    fn build_messages_omits_tool_protocol_when_tools_are_unavailable() {
        let messages = build_messages(&session_with_tool_history(), "base", false);
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0], ChatCompletionRequestMessage::System(_)));
        assert!(matches!(messages[1], ChatCompletionRequestMessage::User(_)));
        assert!(matches!(messages[2], ChatCompletionRequestMessage::Assistant(_)));
        assert!(matches!(messages[3], ChatCompletionRequestMessage::Assistant(_)));
    }

    #[test]
    fn build_messages_preserves_tool_protocol_when_tools_are_available() {
        let messages = build_messages(&session_with_tool_history(), "base", true);
        assert_eq!(messages.len(), 5);
        assert!(matches!(messages[0], ChatCompletionRequestMessage::System(_)));
        assert!(matches!(messages[1], ChatCompletionRequestMessage::User(_)));
        assert!(matches!(messages[2], ChatCompletionRequestMessage::Assistant(_)));
        assert!(matches!(messages[3], ChatCompletionRequestMessage::Tool(_)));
        assert!(matches!(messages[4], ChatCompletionRequestMessage::Assistant(_)));
    }

    // --- tool offering ---

    #[test]
    fn should_not_offer_tools_when_registry_is_empty() {
        let mut session = SessionData::new(std::path::PathBuf::from("/tmp"), "code", "model");
        session
            .history
            .push(ConversationMessage::user("hello".to_string()));

        assert!(!should_offer_tools(&session, &ToolRegistry::new()));
    }

    #[tokio::test]
    async fn should_offer_tools_when_tools_exist() {
        let tools = vec![ToolInfo {
            key: ToolKey::new("src", "alpha"),
            description: "does alpha".to_string(),
        }];
        let mut reg = registry_with(tools);
        reg.refresh_tools().await.unwrap();

        let mut session = SessionData::new(std::path::PathBuf::from("/tmp"), "code", "model");
        session
            .history
            .push(ConversationMessage::user("hello".to_string()));

        assert!(should_offer_tools(&session, &reg));
    }

    // --- sanitize_params ---

    #[test]
    fn sanitize_params_non_json_returns_placeholder() {
        assert_eq!(sanitize_params("not json"), "<non-JSON params>");
    }

    #[test]
    fn sanitize_params_redacts_sensitive_keys() {
        let input = r#"{"password":"s3cr3t","token":"abc","user":"alice"}"#;
        let out: serde_json::Value = serde_json::from_str(&sanitize_params(input)).unwrap();
        assert_eq!(out["password"], "<redacted>");
        assert_eq!(out["token"], "<redacted>");
        assert_eq!(out["user"], "alice");
    }

    #[test]
    fn sanitize_params_redacts_keys_containing_sensitive_substrings() {
        let input = r#"{"api_key":"xyz","my_secret":"shh","normal":"ok"}"#;
        let out: serde_json::Value = serde_json::from_str(&sanitize_params(input)).unwrap();
        assert_eq!(out["api_key"], "<redacted>");
        assert_eq!(out["my_secret"], "<redacted>");
        assert_eq!(out["normal"], "ok");
    }

    #[test]
    fn sanitize_params_truncates_long_output() {
        // Build a JSON object whose serialized form exceeds 200 chars.
        let long_val = "x".repeat(300);
        let input = format!(r#"{{"data":"{long_val}"}}"#);
        let out = sanitize_params(&input);
        assert!(out.len() <= 201 + 3); // 200 chars + ellipsis (multi-byte "…" = 3 bytes)
        assert!(out.ends_with('…'));
    }

    #[test]
    fn sanitize_params_short_json_passes_through() {
        let input = r#"{"x":1}"#;
        assert_eq!(sanitize_params(input), input);
    }

    // --- constants ---

    #[test]
    fn max_iterations_constant() {
        assert_eq!(MAX_ITERATIONS, 20);
    }
}
