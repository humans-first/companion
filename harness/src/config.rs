use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::HarnessError;

#[derive(Debug, Clone, Deserialize)]
pub struct HarnessConfig {
    pub name: String,
    pub system_prompt: String,
    pub model: ModelConfig,
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub mode_tool_sources: HashMap<String, Vec<String>>,
    #[serde(default = "default_tool_policy")]
    pub tool_policy: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub model_id: String,
    #[serde(default)]
    pub available_models: Vec<String>,
    pub base_url: String,
    pub api_key: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub timeout_secs: Option<u64>,
    pub requests_per_minute: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_tool_policy() -> String {
    "permit(principal, action, resource);".to_string()
}

impl HarnessConfig {
    pub fn load(path: &Path) -> Result<Self, HarnessError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| HarnessError::Config(format!("failed to read {}: {e}", path.display())))?;

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("json");

        let config: Self = match ext {
            "yaml" | "yml" => serde_yaml::from_str(&content)
                .map_err(|e| HarnessError::Config(format!("invalid YAML config: {e}")))?,
            _ => serde_json::from_str(&content)
                .map_err(|e| HarnessError::Config(format!("invalid JSON config: {e}")))?,
        };

        for (field, value) in [
            ("name", &config.name),
            ("system_prompt", &config.system_prompt),
            ("model.model_id", &config.model.model_id),
            ("model.base_url", &config.model.base_url),
        ] {
            if value.is_empty() {
                return Err(HarnessError::Config(format!("'{field}' must not be empty")));
            }
        }
        for model_id in &config.model.available_models {
            if model_id.is_empty() {
                return Err(HarnessError::Config(
                    "'model.available_models' must not contain empty strings".into(),
                ));
            }
        }
        for (mode_id, source_names) in &config.mode_tool_sources {
            if mode_id.is_empty() {
                return Err(HarnessError::Config(
                    "'mode_tool_sources' must not contain empty mode ids".into(),
                ));
            }
            for source_name in source_names {
                if source_name.is_empty() {
                    return Err(HarnessError::Config(format!(
                        "'mode_tool_sources.{mode_id}' must not contain empty source names"
                    )));
                }
            }
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_tmp(content: &str, ext: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(ext).tempfile().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    const VALID_JSON: &str = r#"{
        "name": "test",
        "system_prompt": "You are helpful.",
        "model": { "model_id": "gpt-4", "base_url": "https://api.openai.com" }
    }"#;

    const VALID_YAML: &str = "
name: test
system_prompt: You are helpful.
model:
  model_id: gpt-4
  base_url: https://api.openai.com
";

    #[test]
    fn load_valid_json() {
        let f = write_tmp(VALID_JSON, ".json");
        let cfg = HarnessConfig::load(f.path()).unwrap();
        assert_eq!(cfg.name, "test");
        assert_eq!(cfg.system_prompt, "You are helpful.");
        assert_eq!(cfg.model.model_id, "gpt-4");
        assert_eq!(cfg.model.base_url, "https://api.openai.com");
    }

    #[test]
    fn load_valid_yaml() {
        let f = write_tmp(VALID_YAML, ".yaml");
        let cfg = HarnessConfig::load(f.path()).unwrap();
        assert_eq!(cfg.name, "test");
        assert_eq!(cfg.model.model_id, "gpt-4");
    }

    #[test]
    fn default_tool_policy_value() {
        let f = write_tmp(VALID_JSON, ".json");
        let cfg = HarnessConfig::load(f.path()).unwrap();
        assert_eq!(cfg.tool_policy, "permit(principal, action, resource);");
    }

    #[test]
    fn optional_model_fields_absent() {
        let f = write_tmp(VALID_JSON, ".json");
        let cfg = HarnessConfig::load(f.path()).unwrap();
        assert!(cfg.model.api_key.is_none());
        assert!(cfg.model.available_models.is_empty());
        assert!(cfg.mode_tool_sources.is_empty());
        assert!(cfg.model.max_tokens.is_none());
        assert!(cfg.model.temperature.is_none());
        assert!(cfg.model.timeout_secs.is_none());
        assert!(cfg.model.requests_per_minute.is_none());
    }

    #[test]
    fn optional_model_fields_present() {
        let json = r#"{
            "name": "test", "system_prompt": "hi",
            "model": {
                "model_id": "gpt-4", "base_url": "https://api.openai.com",
                "available_models": ["gpt-4", "gpt-4.1"],
                "api_key": "sk-abc", "max_tokens": 512, "temperature": 0.7,
                "timeout_secs": 30, "requests_per_minute": 60
            }
        }"#;
        let f = write_tmp(json, ".json");
        let cfg = HarnessConfig::load(f.path()).unwrap();
        assert_eq!(cfg.model.api_key.as_deref(), Some("sk-abc"));
        assert_eq!(cfg.model.available_models, vec!["gpt-4", "gpt-4.1"]);
        assert_eq!(cfg.model.max_tokens, Some(512));
        assert_eq!(cfg.model.timeout_secs, Some(30));
        assert_eq!(cfg.model.requests_per_minute, Some(60));
        assert!((cfg.model.temperature.unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn error_on_invalid_path() {
        let err = HarnessConfig::load(Path::new("/nonexistent/path/config.json")).unwrap_err();
        assert!(matches!(err, HarnessError::Config(_)));
    }

    #[test]
    fn error_on_malformed_json() {
        let f = write_tmp("{ not valid json }", ".json");
        let err = HarnessConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, HarnessError::Config(_)));
    }

    #[test]
    fn error_on_malformed_yaml() {
        let f = write_tmp(":\n  bad: [yaml", ".yaml");
        let err = HarnessConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, HarnessError::Config(_)));
    }

    #[test]
    fn error_on_empty_name() {
        let json = r#"{"name":"","system_prompt":"hi","model":{"model_id":"m","base_url":"u"}}"#;
        let err = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn error_on_empty_system_prompt() {
        let json = r#"{"name":"n","system_prompt":"","model":{"model_id":"m","base_url":"u"}}"#;
        let err = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap_err();
        assert!(err.to_string().contains("system_prompt"));
    }

    #[test]
    fn error_on_empty_model_id() {
        let json = r#"{"name":"n","system_prompt":"hi","model":{"model_id":"","base_url":"u"}}"#;
        let err = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap_err();
        assert!(err.to_string().contains("model_id"));
    }

    #[test]
    fn error_on_empty_base_url() {
        let json = r#"{"name":"n","system_prompt":"hi","model":{"model_id":"m","base_url":""}}"#;
        let err = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap_err();
        assert!(err.to_string().contains("base_url"));
    }

    #[test]
    fn error_on_empty_available_model() {
        let json = r#"{"name":"n","system_prompt":"hi","model":{"model_id":"m","available_models":[""],"base_url":"u"}}"#;
        let err = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap_err();
        assert!(err.to_string().contains("available_models"));
    }

    #[test]
    fn mode_tool_sources_are_loaded() {
        let json = r#"{
            "name":"n",
            "system_prompt":"hi",
            "model":{"model_id":"m","base_url":"u"},
            "mode_tool_sources":{"architect":["filesystem"],"ask":[]}
        }"#;
        let cfg = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap();
        assert_eq!(
            cfg.mode_tool_sources.get("architect").unwrap(),
            &vec!["filesystem".to_string()]
        );
        assert_eq!(cfg.mode_tool_sources.get("ask").unwrap(), &Vec::<String>::new());
    }

    #[test]
    fn error_on_empty_mode_tool_source_name() {
        let json = r#"{
            "name":"n",
            "system_prompt":"hi",
            "model":{"model_id":"m","base_url":"u"},
            "mode_tool_sources":{"architect":[""]}
        }"#;
        let err = HarnessConfig::load(write_tmp(json, ".json").path()).unwrap_err();
        assert!(err.to_string().contains("mode_tool_sources.architect"));
    }
}
