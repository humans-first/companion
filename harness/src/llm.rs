use std::sync::Mutex;
use std::time::Instant;

use async_openai::config::OpenAIConfig;
use async_openai::types::{
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessageArgs,
    ChatCompletionTool, ChatCompletionToolType, CreateChatCompletionRequestArgs,
    CreateChatCompletionResponse, FunctionObjectArgs,
};
use async_openai::Client;
use async_trait::async_trait;
use tracing::debug;

use crate::config::ModelConfig;
use crate::error::HarnessError;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: Vec<ChatCompletionTool>,
        request_config: LlmRequestConfig,
    ) -> Result<CreateChatCompletionResponse, HarnessError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmRequestConfig {
    pub model_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Token bucket rate limiter
// ---------------------------------------------------------------------------

struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl TokenBucket {
    fn new(requests_per_minute: u32) -> Self {
        let capacity = requests_per_minute as f64;
        Self {
            tokens: capacity,
            capacity,
            refill_rate: capacity / 60.0,
            last_refill: Instant::now(),
        }
    }

    /// Returns how long to wait (if any) before a token is available.
    fn acquire(&mut self) -> Option<std::time::Duration> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let wait_secs = (1.0 - self.tokens) / self.refill_rate;
            Some(std::time::Duration::from_secs_f64(wait_secs))
        }
    }
}

// ---------------------------------------------------------------------------
// LlmClient
// ---------------------------------------------------------------------------

pub struct LlmClient {
    client: Client<OpenAIConfig>,
    model_id: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    timeout_secs: u64,
    rate_limiter: Option<Mutex<TokenBucket>>,
}

impl LlmClient {
    pub fn new(config: &ModelConfig) -> Self {
        let mut openai_config = OpenAIConfig::new().with_api_base(&config.base_url);

        if let Some(ref key) = config.api_key {
            openai_config = openai_config.with_api_key(key);
        } else {
            // Some providers (Ollama) don't need a key; set a dummy to avoid env var lookup.
            openai_config = openai_config.with_api_key("unused");
        }

        Self {
            client: Client::with_config(openai_config),
            model_id: config.model_id.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            timeout_secs: config.timeout_secs.unwrap_or(120),
            rate_limiter: config
                .requests_per_minute
                .map(|rpm| Mutex::new(TokenBucket::new(rpm))),
        }
    }
}

#[async_trait]
impl LlmProvider for LlmClient {
    async fn chat_completion(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: Vec<ChatCompletionTool>,
        request_config: LlmRequestConfig,
    ) -> Result<CreateChatCompletionResponse, HarnessError> {
        let model_id = request_config
            .model_id
            .as_deref()
            .unwrap_or(&self.model_id);
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder.model(model_id).messages(messages);

        if !tools.is_empty() {
            builder.tools(tools);
        }
        if let Some(max_tokens) = self.max_tokens {
            builder.max_tokens(max_tokens);
        }
        if let Some(temperature) = self.temperature {
            builder.temperature(temperature);
        }

        let request = builder
            .build()
            .map_err(|e| HarnessError::Llm(format!("build request: {e}")))?;

        debug!(model = %model_id, "calling LLM");

        if let Some(ref limiter) = self.rate_limiter {
            let wait = limiter.lock().unwrap().acquire();
            if let Some(wait) = wait {
                tokio::time::sleep(wait).await;
            }
        }

        tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.client.chat().create(request),
        )
        .await
        .map_err(|_| HarnessError::Llm(format!("timed out after {}s", self.timeout_secs)))?
        .map_err(|e| HarnessError::Llm(format!("API error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Helper constructors for building messages
// ---------------------------------------------------------------------------

pub fn system_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestSystemMessageArgs::default()
        .content(content)
        .build()
        .expect("system message builder: content is set")
        .into()
}

pub fn user_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestUserMessageArgs::default()
        .content(content)
        .build()
        .expect("user message builder: content is set")
        .into()
}

pub fn assistant_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestAssistantMessageArgs::default()
        .content(content)
        .build()
        .expect("assistant message builder: content is set")
        .into()
}

pub fn tool_result_message(tool_call_id: &str, content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
        tool_call_id: tool_call_id.to_string(),
        content: ChatCompletionRequestToolMessageContent::Text(content.to_string()),
    })
}

/// Build the two tool definitions exposed to the LLM: discover and execute.
pub fn harness_tool_definitions() -> Vec<ChatCompletionTool> {
    vec![
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObjectArgs::default()
                .name("discover")
                .description(
                    "Get full input/output schemas for MCP tools before using them in code. \
                     Call this with the tool keys you want to use.",
                )
                .parameters(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tools": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "List of tool keys in 'source:tool_name' format"
                        }
                    },
                    "required": ["tools"]
                }))
                .build()
                .expect("discover tool builder: name and description are set"),
        },
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObjectArgs::default()
                .name("execute")
                .description(
                    "Execute Python code that can call MCP tools via tool(key, params). \
                     The tool() function is pre-imported. key is 'source:tool_name' format. \
                     Do not use os, subprocess, or other restricted modules.",
                )
                .parameters(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "Python code to execute"
                        }
                    },
                    "required": ["code"]
                }))
                .build()
                .expect("execute tool builder: name and description are set"),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use async_openai::types::{
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessage, ChatCompletionRequestSystemMessageContent,
        ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
        ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
        ChatCompletionToolType, CreateChatCompletionResponse,
    };
    use async_trait::async_trait;

    use crate::error::HarnessError;

    use super::{
        assistant_message, harness_tool_definitions, system_message, tool_result_message,
        user_message, LlmProvider, LlmRequestConfig, TokenBucket,
    };

    // -----------------------------------------------------------------------
    // MockLlmProvider
    // -----------------------------------------------------------------------

    struct MockLlmProvider {
        response: Result<CreateChatCompletionResponse, HarnessError>,
    }

    impl MockLlmProvider {
        fn failing(msg: &str) -> Self {
            Self {
                response: Err(HarnessError::Llm(msg.to_string())),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockLlmProvider {
        async fn chat_completion(
            &self,
            _messages: Vec<ChatCompletionRequestMessage>,
            _tools: Vec<async_openai::types::ChatCompletionTool>,
            _request_config: LlmRequestConfig,
        ) -> Result<CreateChatCompletionResponse, HarnessError> {
            match &self.response {
                Ok(_) => unreachable!("success path not needed in these tests"),
                Err(e) => Err(HarnessError::Llm(e.to_string())),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Message builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn system_message_content() {
        let msg = system_message("You are helpful.");
        match msg {
            ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(text),
                ..
            }) => assert_eq!(text, "You are helpful."),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn user_message_content() {
        let msg = user_message("Hello!");
        match msg {
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(text),
                ..
            }) => assert_eq!(text, "Hello!"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn assistant_message_content() {
        let msg = assistant_message("I can help.");
        match msg {
            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                content: Some(content),
                ..
            }) => {
                let text = match content {
                    async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(t) => t,
                    other => panic!("unexpected content variant: {other:?}"),
                };
                assert_eq!(text, "I can help.");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn tool_result_message_fields() {
        let msg = tool_result_message("call-123", "result text");
        match msg {
            ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
                tool_call_id,
                content: ChatCompletionRequestToolMessageContent::Text(text),
            }) => {
                assert_eq!(tool_call_id, "call-123");
                assert_eq!(text, "result text");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn message_builders_empty_string() {
        // Builders must not panic on empty content.
        let _ = system_message("");
        let _ = user_message("");
        let _ = assistant_message("");
        let _ = tool_result_message("", "");
    }

    // -----------------------------------------------------------------------
    // harness_tool_definitions tests
    // -----------------------------------------------------------------------

    #[test]
    fn harness_tools_count() {
        assert_eq!(harness_tool_definitions().len(), 2);
    }

    #[test]
    fn harness_tools_names_and_types() {
        let tools = harness_tool_definitions();
        assert_eq!(tools[0].function.name, "discover");
        assert_eq!(tools[1].function.name, "execute");
        assert!(matches!(tools[0].r#type, ChatCompletionToolType::Function));
        assert!(matches!(tools[1].r#type, ChatCompletionToolType::Function));
    }

    #[test]
    fn harness_tools_required_params() {
        let tools = harness_tool_definitions();

        let discover_params = tools[0].function.parameters.as_ref().unwrap();
        let required = &discover_params["required"];
        assert_eq!(required[0], "tools");

        let execute_params = tools[1].function.parameters.as_ref().unwrap();
        let required = &execute_params["required"];
        assert_eq!(required[0], "code");
    }

    #[test]
    fn harness_tools_param_types() {
        let tools = harness_tool_definitions();

        let discover_params = tools[0].function.parameters.as_ref().unwrap();
        assert_eq!(discover_params["properties"]["tools"]["type"], "array");

        let execute_params = tools[1].function.parameters.as_ref().unwrap();
        assert_eq!(execute_params["properties"]["code"]["type"], "string");
    }

    // -----------------------------------------------------------------------
    // TokenBucket tests
    // -----------------------------------------------------------------------

    fn bucket_with_tokens(tokens: f64, capacity: f64) -> TokenBucket {
        TokenBucket {
            tokens,
            capacity,
            refill_rate: capacity / 60.0,
            last_refill: Instant::now(),
        }
    }

    #[test]
    fn token_bucket_new_starts_full() {
        let mut b = TokenBucket::new(60);
        // First acquire should succeed immediately (bucket starts full).
        assert!(b.acquire().is_none());
    }

    #[test]
    fn token_bucket_acquire_decrements() {
        let mut b = TokenBucket::new(60);
        // Drain all 60 tokens.
        for _ in 0..60 {
            assert!(b.acquire().is_none());
        }
        // Next acquire must return a wait duration.
        assert!(b.acquire().is_some());
    }

    #[test]
    fn token_bucket_empty_returns_wait() {
        let mut b = bucket_with_tokens(0.0, 60.0);
        let wait = b.acquire();
        assert!(wait.is_some());
        // Wait should be positive.
        assert!(wait.unwrap().as_secs_f64() > 0.0);
    }

    #[test]
    fn token_bucket_wait_bounded_by_capacity() {
        let mut b = bucket_with_tokens(0.0, 60.0);
        let wait = b.acquire().unwrap();
        // Maximum wait for one token at 1 token/sec is 1 second.
        assert!(wait.as_secs_f64() <= 1.0 + f64::EPSILON);
    }

    #[test]
    fn token_bucket_partial_tokens_no_wait() {
        // 1.5 tokens available — should grant immediately and leave 0.5.
        let mut b = bucket_with_tokens(1.5, 60.0);
        assert!(b.acquire().is_none());
        // Now 0.5 tokens remain — next acquire needs to wait.
        assert!(b.acquire().is_some());
    }

    #[test]
    fn token_bucket_does_not_exceed_capacity() {
        // Start with capacity already full; elapsed time should not push above capacity.
        let mut b = TokenBucket::new(10);
        // Simulate a long idle by setting last_refill far in the past.
        b.last_refill = Instant::now() - std::time::Duration::from_secs(3600);
        b.acquire(); // triggers refill
        assert!(b.tokens <= b.capacity);
    }

    // -----------------------------------------------------------------------
    // MockLlmProvider trait dispatch test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mock_provider_returns_error() {
        let provider = MockLlmProvider::failing("test error");
        let result = provider
            .chat_completion(vec![], vec![], LlmRequestConfig::default())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test error"));
    }

    #[tokio::test]
    async fn mock_provider_usable_as_trait_object() {
        let provider: Box<dyn LlmProvider> = Box::new(MockLlmProvider::failing("oops"));
        let result = provider
            .chat_completion(vec![], vec![], LlmRequestConfig::default())
            .await;
        assert!(result.is_err());
    }
}
