use cedar_policy::{Authorizer, Context, Entities, EntityUid, PolicySet, Request};
use tracing::{debug, info};

use crate::error::HarnessError;
use crate::tool::ToolKey;

/// Cedar-based authorization engine for tool calls.
pub struct ToolPolicyEngine {
    policy_set: PolicySet,
    authorizer: Authorizer,
}

impl ToolPolicyEngine {
    /// Create a policy engine from an inline Cedar policy string.
    ///
    /// Operates in schemaless mode (no validation).
    pub fn from_policy_str(policy_src: &str) -> Result<Self, HarnessError> {
        let policy_set = policy_src
            .parse::<PolicySet>()
            .map_err(|e| HarnessError::Config(format!("failed to parse tool policy: {e}")))?;

        info!(
            num_policies = policy_set.policies().count(),
            "tool policy engine initialized"
        );

        Ok(Self {
            policy_set,
            authorizer: Authorizer::new(),
        })
    }

    /// Authorize a tool call.
    ///
    /// - Principal: from ACP session _meta, e.g. `User::"telegram|12345"`
    /// - Action: always `Action::"ToolCall"`
    /// - Resource: `Tool::"source:tool_name"`, e.g. `Tool::"github:list_repos"`
    /// - Context: tool parameters as Cedar context
    pub fn authorize_tool_call(
        &self,
        principal_str: &str,
        tool_key: &ToolKey,
        params: &serde_json::Value,
    ) -> Result<(), HarnessError> {
        if principal_str.is_empty() {
            return Err(HarnessError::PolicyDenied(
                "principal must not be empty (expected format: Type::\"id\")".to_string(),
            ));
        }
        let principal: EntityUid = principal_str.parse().map_err(|e| {
            HarnessError::PolicyDenied(format!(
                "invalid principal '{principal_str}': {e} (expected format: Type::\"id\", e.g. User::\"telegram|12345\")"
            ))
        })?;

        let action: EntityUid = r#"Action::"ToolCall""#
            .parse()
            .map_err(|e| HarnessError::PolicyDenied(format!("invalid action: {e}")))?;

        // Resource uses the ToolKey display format: "source:tool_name".
        let resource_str = format!("Tool::\"{}\"", tool_key);
        let resource: EntityUid = resource_str.parse().map_err(|e| {
            HarnessError::PolicyDenied(format!("invalid resource '{resource_str}': {e}"))
        })?;

        let context = Context::from_json_value(params.clone(), None)
            .map_err(|e| HarnessError::PolicyDenied(format!("invalid context: {e}")))?;

        let request = Request::new(principal, action, resource, context, None)
            .map_err(|e| HarnessError::PolicyDenied(format!("invalid request: {e}")))?;

        let entities = Entities::empty();
        let response = self
            .authorizer
            .is_authorized(&request, &self.policy_set, &entities);

        debug!(
            decision = ?response.decision(),
            tool = %tool_key,
            "cedar tool authorization result"
        );

        match response.decision() {
            cedar_policy::Decision::Allow => Ok(()),
            cedar_policy::Decision::Deny => {
                let reasons: Vec<String> = response
                    .diagnostics()
                    .errors()
                    .map(|e| e.to_string())
                    .collect();
                let reason = if reasons.is_empty() {
                    "no matching permit policy".to_string()
                } else {
                    reasons.join("; ")
                };
                Err(HarnessError::PolicyDenied(format!("{tool_key}: {reason}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERMIT_ALL: &str = r#"permit(principal, action == Action::"ToolCall", resource);"#;
    const DENY_ALL: &str = r#"forbid(principal, action == Action::"ToolCall", resource);"#;

    fn tool(source: &str, name: &str) -> ToolKey {
        ToolKey::new(source, name)
    }

    // 1) from_policy_str with valid Cedar policies
    #[test]
    fn from_policy_str_valid() {
        assert!(ToolPolicyEngine::from_policy_str(PERMIT_ALL).is_ok());
        assert!(ToolPolicyEngine::from_policy_str(DENY_ALL).is_ok());
        assert!(ToolPolicyEngine::from_policy_str("").is_ok()); // empty = no policies
    }

    // 2) Invalid policy syntax errors
    #[test]
    fn from_policy_str_invalid_syntax() {
        let result = ToolPolicyEngine::from_policy_str("this is not cedar");
        assert!(matches!(result, Err(HarnessError::Config(_))));
    }

    // 3) authorize_tool_call with permit policy (should succeed)
    #[test]
    fn authorize_permit() {
        let engine = ToolPolicyEngine::from_policy_str(PERMIT_ALL).unwrap();
        let result = engine.authorize_tool_call(
            r#"User::"alice""#,
            &tool("github", "list_repos"),
            &serde_json::json!({}),
        );
        assert!(result.is_ok());
    }

    // 4) authorize_tool_call with deny policy (should fail)
    #[test]
    fn authorize_deny() {
        let engine = ToolPolicyEngine::from_policy_str(DENY_ALL).unwrap();
        let result = engine.authorize_tool_call(
            r#"User::"alice""#,
            &tool("github", "list_repos"),
            &serde_json::json!({}),
        );
        assert!(matches!(result, Err(HarnessError::PolicyDenied(_))));
    }

    // 5) Empty principal validation
    #[test]
    fn authorize_empty_principal() {
        let engine = ToolPolicyEngine::from_policy_str(PERMIT_ALL).unwrap();
        let result =
            engine.authorize_tool_call("", &tool("github", "list_repos"), &serde_json::json!({}));
        assert!(matches!(result, Err(HarnessError::PolicyDenied(_))));
    }

    // 6) Malformed principal / resource strings
    #[test]
    fn authorize_malformed_principal() {
        let engine = ToolPolicyEngine::from_policy_str(PERMIT_ALL).unwrap();
        // Missing quotes around the id — not valid Cedar EntityUid syntax
        let result = engine.authorize_tool_call(
            "User::alice",
            &tool("github", "list_repos"),
            &serde_json::json!({}),
        );
        assert!(matches!(result, Err(HarnessError::PolicyDenied(_))));
    }
}
