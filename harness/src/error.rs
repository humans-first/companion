use agent_client_protocol as acp;

/// Harness-specific error codes in the JSON-RPC server error range (-32000 to -32099).
const UNKNOWN_SESSION: i32 = -32001;
const PROMPT_IN_FLIGHT: i32 = -32002;
const POLICY_DENIED: i32 = -32004;
const LLM_ERROR: i32 = -32010;
const MCP_ERROR: i32 = -32011;
const SANDBOX_ERROR: i32 = -32012;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("config: {0}")]
    Config(String),

    #[error("unknown session: {0}")]
    UnknownSession(String),

    #[error("prompt already in flight: {0}")]
    PromptInFlight(String),

    #[error("cancelled")]
    Cancelled,

    #[error("LLM: {0}")]
    Llm(String),

    #[error("MCP: {0}")]
    Mcp(String),

    #[error("sandbox: {0}")]
    Sandbox(String),

    #[error("policy denied: {0}")]
    PolicyDenied(String),

    #[error("tool: {0}")]
    Tool(String),
}

impl From<HarnessError> for acp::Error {
    fn from(e: HarnessError) -> Self {
        let code = match &e {
            HarnessError::Config(_) | HarnessError::Tool(_) => -32603,
            HarnessError::UnknownSession(_) => UNKNOWN_SESSION,
            HarnessError::PromptInFlight(_) => PROMPT_IN_FLIGHT,
            HarnessError::Cancelled => -32800,
            HarnessError::PolicyDenied(_) => POLICY_DENIED,
            HarnessError::Llm(_) => LLM_ERROR,
            HarnessError::Mcp(_) => MCP_ERROR,
            HarnessError::Sandbox(_) => SANDBOX_ERROR,
        };
        acp::Error::new(code, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol as acp;

    use super::*;

    fn code(e: HarnessError) -> i32 {
        i32::from(acp::Error::from(e).code)
    }

    fn message(e: HarnessError) -> String {
        acp::Error::from(e).message
    }

    #[test]
    fn error_codes() {
        assert_eq!(code(HarnessError::Config("x".into())), -32603);
        assert_eq!(code(HarnessError::Tool("x".into())), -32603);
        assert_eq!(code(HarnessError::UnknownSession("x".into())), -32001);
        assert_eq!(code(HarnessError::PromptInFlight("x".into())), -32002);
        assert_eq!(code(HarnessError::Cancelled), -32800);
        assert_eq!(code(HarnessError::PolicyDenied("x".into())), -32004);
        assert_eq!(code(HarnessError::Llm("x".into())), -32010);
        assert_eq!(code(HarnessError::Mcp("x".into())), -32011);
        assert_eq!(code(HarnessError::Sandbox("x".into())), -32012);
    }

    #[test]
    fn messages_preserved() {
        assert_eq!(
            message(HarnessError::Config("bad cfg".into())),
            "config: bad cfg"
        );
        assert_eq!(
            message(HarnessError::UnknownSession("s1".into())),
            "unknown session: s1"
        );
        assert_eq!(
            message(HarnessError::PromptInFlight("s1".into())),
            "prompt already in flight: s1"
        );
        assert_eq!(message(HarnessError::Cancelled), "cancelled");
        assert_eq!(message(HarnessError::Llm("timeout".into())), "LLM: timeout");
        assert_eq!(
            message(HarnessError::Mcp("conn refused".into())),
            "MCP: conn refused"
        );
        assert_eq!(message(HarnessError::Sandbox("oom".into())), "sandbox: oom");
        assert_eq!(
            message(HarnessError::PolicyDenied("denied".into())),
            "policy denied: denied"
        );
        assert_eq!(
            message(HarnessError::Tool("bad tool".into())),
            "tool: bad tool"
        );
    }

    #[test]
    fn display_formatting() {
        assert_eq!(
            HarnessError::Config("bad cfg".into()).to_string(),
            "config: bad cfg"
        );
        assert_eq!(
            HarnessError::UnknownSession("s1".into()).to_string(),
            "unknown session: s1"
        );
        assert_eq!(
            HarnessError::PromptInFlight("s1".into()).to_string(),
            "prompt already in flight: s1"
        );
        assert_eq!(HarnessError::Cancelled.to_string(), "cancelled");
        assert_eq!(
            HarnessError::Llm("timeout".into()).to_string(),
            "LLM: timeout"
        );
        assert_eq!(
            HarnessError::Mcp("conn refused".into()).to_string(),
            "MCP: conn refused"
        );
        assert_eq!(
            HarnessError::Sandbox("oom".into()).to_string(),
            "sandbox: oom"
        );
        assert_eq!(
            HarnessError::PolicyDenied("denied".into()).to_string(),
            "policy denied: denied"
        );
        assert_eq!(
            HarnessError::Tool("bad tool".into()).to_string(),
            "tool: bad tool"
        );
    }
}
