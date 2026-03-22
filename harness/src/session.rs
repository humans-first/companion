use std::path::PathBuf;

use async_openai::types::ChatCompletionMessageToolCall;

/// Message role in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation history.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: Role,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ChatCompletionMessageToolCall>>,
    pub tool_call_id: Option<String>,
}

impl ConversationMessage {
    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            content: Some(text),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(text: String) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(text),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ChatCompletionMessageToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls_with_content(
        content: Option<String>,
        tool_calls: Vec<ChatCompletionMessageToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: String, content: String) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
        }
    }
}

/// Persisted session state.
#[derive(Debug, Clone)]
pub struct SessionData {
    pub history: Vec<ConversationMessage>,
    pub cwd: PathBuf,
    pub mode_id: String,
    pub model_id: String,
}

impl SessionData {
    pub fn new(cwd: PathBuf, mode_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            history: Vec::new(),
            cwd,
            mode_id: mode_id.into(),
            model_id: model_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use async_openai::types::{ChatCompletionToolType, FunctionCall};

    use super::*;

    fn dummy_tool_call(id: &str) -> ChatCompletionMessageToolCall {
        ChatCompletionMessageToolCall {
            id: id.to_string(),
            r#type: ChatCompletionToolType::Function,
            function: FunctionCall {
                name: "my_fn".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn session_data_new_sets_defaults() {
        let cwd = PathBuf::from("/tmp");
        let session = SessionData::new(cwd.clone(), "code", "model");
        assert!(session.history.is_empty());
        assert_eq!(session.cwd, cwd);
        assert_eq!(session.mode_id, "code");
        assert_eq!(session.model_id, "model");
    }

    #[test]
    fn message_constructors_set_expected_fields() {
        let user = ConversationMessage::user("hello".to_string());
        assert_eq!(user.role, Role::User);
        assert_eq!(user.content.as_deref(), Some("hello"));
        assert!(user.tool_calls.is_none());
        assert!(user.tool_call_id.is_none());

        let assistant = ConversationMessage::assistant("hi".to_string());
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.content.as_deref(), Some("hi"));

        let call = dummy_tool_call("call-1");
        let assistant_tools = ConversationMessage::assistant_tool_calls(vec![call.clone()]);
        assert_eq!(assistant_tools.role, Role::Assistant);
        assert!(assistant_tools.content.is_none());
        assert_eq!(assistant_tools.tool_calls.unwrap()[0].id, "call-1");

        let assistant_tools_with_content = ConversationMessage::assistant_tool_calls_with_content(
            Some("working".to_string()),
            vec![call],
        );
        assert_eq!(
            assistant_tools_with_content.content.as_deref(),
            Some("working")
        );

        let tool = ConversationMessage::tool_result("call-1".to_string(), "done".to_string());
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.content.as_deref(), Some("done"));
        assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    }
}
