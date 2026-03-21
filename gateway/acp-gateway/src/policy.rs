use std::path::Path;

use cedar_policy::{Authorizer, Context, Entities, EntityUid, PolicySet, Request, Schema};
use tracing::{debug, info, warn};

use crate::error;

/// Cedar-based authorization engine for ACP prompts.
pub struct PolicyEngine {
    policy_set: PolicySet,
    schema: Option<Schema>,
    authorizer: Authorizer,
}

impl PolicyEngine {
    /// Load Cedar policies from a directory, optionally validating against a schema.
    pub fn load(policy_dir: &Path, schema_file: Option<&Path>) -> Result<Self, String> {
        let schema = if let Some(path) = schema_file {
            let schema_src = std::fs::read_to_string(path)
                .map_err(|e| format!("failed to read schema {}: {e}", path.display()))?;
            let (schema, warnings) = Schema::from_cedarschema_str(&schema_src)
                .map_err(|e| format!("failed to parse schema: {e}"))?;
            for w in warnings {
                warn!("schema warning: {w}");
            }
            Some(schema)
        } else {
            None
        };

        let mut policy_src = String::new();
        let entries = std::fs::read_dir(policy_dir)
            .map_err(|e| format!("failed to read policy dir {}: {e}", policy_dir.display()))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "cedar") {
                let src = std::fs::read_to_string(&path)
                    .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
                policy_src.push_str(&src);
                policy_src.push('\n');
                info!(file = %path.display(), "loaded cedar policy");
            }
        }

        let policy_set = policy_src
            .parse::<PolicySet>()
            .map_err(|e| format!("failed to parse policies: {e}"))?;

        if let Some(ref schema) = schema {
            let validator = cedar_policy::Validator::new(schema.clone());
            let result = validator.validate(&policy_set, cedar_policy::ValidationMode::Strict);
            if !result.validation_passed() {
                let errors: Vec<String> = result.validation_errors().map(|e| e.to_string()).collect();
                return Err(format!("policy validation failed: {}", errors.join("; ")));
            }
        }

        info!(
            num_policies = policy_set.policies().count(),
            "cedar policy engine initialized"
        );

        Ok(Self {
            policy_set,
            schema,
            authorizer: Authorizer::new(),
        })
    }

    /// Authorize a prompt request using the `_meta` field.
    ///
    /// Expected _meta structure:
    /// ```json
    /// {
    ///   "principal": "User::\"alice\"",
    ///   "action": "Action::\"prompt\"",
    ///   "resource": "Agent::\"kiro\""
    /// }
    /// ```
    pub fn authorize(&self, meta: &Option<serde_json::Map<String, serde_json::Value>>) -> Result<(), agent_client_protocol::Error> {
        let meta_obj = meta.as_ref().ok_or_else(|| error::authz_denied("missing _meta"))?;

        let principal_str = meta_obj
            .get("principal")
            .and_then(|v| v.as_str())
            .ok_or_else(|| error::authz_denied("missing principal in _meta"))?;
        let action_str = meta_obj
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| error::authz_denied("missing action in _meta"))?;
        let resource_str = meta_obj
            .get("resource")
            .and_then(|v| v.as_str())
            .ok_or_else(|| error::authz_denied("missing resource in _meta"))?;

        let principal: EntityUid = principal_str
            .parse()
            .map_err(|e| error::authz_denied(&format!("invalid principal: {e}")))?;
        let action: EntityUid = action_str
            .parse()
            .map_err(|e| error::authz_denied(&format!("invalid action: {e}")))?;
        let resource: EntityUid = resource_str
            .parse()
            .map_err(|e| error::authz_denied(&format!("invalid resource: {e}")))?;

        let context = if let Some(ctx_val) = meta_obj.get("context") {
            Context::from_json_value(ctx_val.clone(), self.schema.as_ref().map(|s| (s, &action)))
                .map_err(|e| error::authz_denied(&format!("invalid context: {e}")))?
        } else {
            Context::empty()
        };

        let request = Request::new(
            principal,
            action,
            resource,
            context,
            self.schema.as_ref(),
        )
        .map_err(|e| error::authz_denied(&format!("invalid request: {e}")))?;

        let entities = Entities::empty();
        let response = self.authorizer.is_authorized(&request, &self.policy_set, &entities);

        debug!(decision = ?response.decision(), "cedar authorization result");

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
                Err(error::authz_denied(&reason))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_meta(principal: &str, action: &str, resource: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
        let mut map = serde_json::Map::new();
        map.insert("principal".to_string(), serde_json::Value::String(principal.to_string()));
        map.insert("action".to_string(), serde_json::Value::String(action.to_string()));
        map.insert("resource".to_string(), serde_json::Value::String(resource.to_string()));
        Some(map)
    }

    fn write_policy(dir: &Path, filename: &str, content: &str) {
        let path = dir.join(filename);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_allow_with_permit_policy() {
        let dir = tempdir();
        write_policy(
            &dir,
            "allow.cedar",
            r#"permit(principal, action, resource);"#,
        );

        let engine = PolicyEngine::load(&dir, None).unwrap();
        let meta = make_meta(
            r#"User::"alice""#,
            r#"Action::"prompt""#,
            r#"Agent::"kiro""#,
        );
        assert!(engine.authorize(&meta).is_ok());
    }

    #[test]
    fn test_deny_default() {
        let dir = tempdir();
        // No policies → default deny.
        write_policy(&dir, "empty.cedar", "");

        let engine = PolicyEngine::load(&dir, None).unwrap();
        let meta = make_meta(
            r#"User::"alice""#,
            r#"Action::"prompt""#,
            r#"Agent::"kiro""#,
        );
        let result = engine.authorize(&meta);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            agent_client_protocol::ErrorCode::Other(crate::error::AUTHZ_DENIED)
        );
    }

    #[test]
    fn test_deny_missing_meta() {
        let dir = tempdir();
        write_policy(
            &dir,
            "allow.cedar",
            r#"permit(principal, action, resource);"#,
        );
        let engine = PolicyEngine::load(&dir, None).unwrap();
        let result = engine.authorize(&None);
        assert!(result.is_err());
    }

    #[test]
    fn test_deny_missing_principal() {
        let dir = tempdir();
        write_policy(
            &dir,
            "allow.cedar",
            r#"permit(principal, action, resource);"#,
        );
        let engine = PolicyEngine::load(&dir, None).unwrap();
        let mut map = serde_json::Map::new();
        map.insert("action".to_string(), serde_json::Value::String(r#"Action::"prompt""#.to_string()));
        map.insert("resource".to_string(), serde_json::Value::String(r#"Agent::"kiro""#.to_string()));
        let meta = Some(map);
        let result = engine.authorize(&meta);
        assert!(result.is_err());
    }

    #[test]
    fn test_forbid_policy() {
        let dir = tempdir();
        write_policy(
            &dir,
            "policy.cedar",
            r#"
            permit(principal, action, resource);
            forbid(principal == User::"banned", action, resource);
            "#,
        );

        let engine = PolicyEngine::load(&dir, None).unwrap();

        // allowed user
        let meta = make_meta(
            r#"User::"alice""#,
            r#"Action::"prompt""#,
            r#"Agent::"kiro""#,
        );
        assert!(engine.authorize(&meta).is_ok());

        // banned user
        let meta = make_meta(
            r#"User::"banned""#,
            r#"Action::"prompt""#,
            r#"Agent::"kiro""#,
        );
        assert!(engine.authorize(&meta).is_err());
    }

    // Use thread_local to hold TempDir so it lives as long as the test.
    // TempDir is deleted on drop, so we need it to outlive the test function body.
    thread_local! {
        static TEMP_DIRS: std::cell::RefCell<Vec<tempfile::TempDir>> = std::cell::RefCell::new(Vec::new());
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        TEMP_DIRS.with(|dirs| dirs.borrow_mut().push(dir));
        path
    }
}
