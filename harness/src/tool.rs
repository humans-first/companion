use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use crate::error::HarnessError;

/// Key for addressing a specific tool: "source:tool_name".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolKey {
    pub source: String,
    pub tool: String,
}

impl ToolKey {
    pub fn new(source: &str, tool: &str) -> Self {
        Self {
            source: source.to_string(),
            tool: tool.to_string(),
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let (source, tool) = s
            .split_once(':')
            .ok_or_else(|| format!("invalid tool key '{s}': expected 'source:tool' format"))?;
        if source.is_empty() || tool.is_empty() {
            return Err(format!(
                "invalid tool key '{s}': source and tool must be non-empty"
            ));
        }
        Ok(Self {
            source: source.to_string(),
            tool: tool.to_string(),
        })
    }
}

impl fmt::Display for ToolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source, self.tool)
    }
}

/// Summary info for a tool (returned by list_tools).
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub key: ToolKey,
    pub description: String,
}

/// Full schema for a tool (returned by get_schemas).
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub key: ToolKey,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
}

/// Out-of-band context supplied with a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallContext {
    pub cwd: PathBuf,
    pub principal: String,
}

impl ToolCallContext {
    pub fn new(cwd: impl Into<PathBuf>, principal: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            principal: principal.into(),
        }
    }
}

/// Trait for a source of tools. Implemented by McpToolSource, future native sources, etc.
///
/// All methods take `&self`; implementations that need mutability should use interior
/// mutability (e.g., `RefCell` in single-threaded contexts).
#[async_trait::async_trait(?Send)]
pub trait ToolSource {
    /// Refresh any cached metadata from the underlying source.
    async fn refresh(&self) -> Result<(), String> {
        Ok(())
    }

    /// List all tools available from this source (name + description only).
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, String>;

    /// Get full schemas for the specified tools (batch).
    async fn get_schemas(&self, tools: &[String]) -> Result<Vec<ToolSchema>, String>;

    /// Call a tool with the given parameters.
    async fn call_tool(
        &self,
        tool: &str,
        params: serde_json::Value,
        context: &ToolCallContext,
    ) -> Result<serde_json::Value, String>;
}

#[async_trait::async_trait(?Send)]
pub trait ToolSourceFactory {
    async fn create(&self, source_name: &str) -> Result<Box<dyn ToolSource>, String>;
}

/// Registry that holds multiple tool sources, keyed by source name.
///
/// # Thread safety
///
/// `ToolRegistry` is not `Sync` because `ToolSource` is `?Send`.  It is
/// intended to be used exclusively from a single-threaded async runtime.  If
/// multi-threaded access is ever required, `ToolSource` must become `Send +
/// Sync` and this registry should be wrapped in `Arc<tokio::sync::RwLock<…>>`.
pub struct ToolRegistry {
    sources: HashMap<String, Box<dyn ToolSource>>,
    /// Cached tool listings (populated on init).
    tools_cache: Vec<ToolInfo>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            tools_cache: Vec::new(),
        }
    }

    pub fn register(&mut self, name: String, source: Box<dyn ToolSource>) {
        self.sources.insert(name, source);
    }

    /// Initialize all sources and cache their tool listings.
    pub async fn refresh_tools(&mut self) -> Result<(), HarnessError> {
        self.tools_cache.clear();
        for (source_name, source) in &self.sources {
            source
                .refresh()
                .await
                .map_err(|e| HarnessError::Tool(format!("{source_name}: {e}")))?;
            let tools = source
                .list_tools()
                .await
                .map_err(|e| HarnessError::Tool(format!("{source_name}: {e}")))?;
            for mut info in tools {
                info.key.source = source_name.clone();
                self.tools_cache.push(info);
            }
        }
        Ok(())
    }

    /// Get a flat list of all tools across all sources.
    pub fn all_tools(&self) -> &[ToolInfo] {
        &self.tools_cache
    }

    /// Batch-fetch schemas for the given tool keys.
    /// Groups by source and fans out to each source's get_schemas.
    pub async fn get_schemas(&self, keys: &[ToolKey]) -> Result<Vec<ToolSchema>, String> {
        let mut by_source: HashMap<&str, Vec<String>> = HashMap::new();
        for key in keys {
            by_source
                .entry(&key.source)
                .or_default()
                .push(key.tool.clone());
        }

        let mut results = Vec::new();
        for (source_name, tool_names) in by_source {
            let source = self
                .sources
                .get(source_name)
                .ok_or_else(|| format!("unknown tool source: {source_name}"))?;
            let mut schemas = source.get_schemas(&tool_names).await?;
            for schema in &mut schemas {
                schema.key.source = source_name.to_string();
            }
            results.extend(schemas);
        }
        Ok(results)
    }

    /// Call a tool by its key.
    pub async fn call_tool(
        &self,
        key: &ToolKey,
        params: serde_json::Value,
        context: &ToolCallContext,
    ) -> Result<serde_json::Value, String> {
        let source = self.sources.get(&key.source).ok_or_else(|| {
            format!(
                "unknown tool source '{}' for tool '{}'",
                key.source, key.tool
            )
        })?;
        source.call_tool(&key.tool, params, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- ToolKey::parse ---

    #[test]
    fn parse_valid() {
        let key = ToolKey::parse("mysource:mytool").unwrap();
        assert_eq!(key.source, "mysource");
        assert_eq!(key.tool, "mytool");
    }

    #[test]
    fn parse_no_colon() {
        assert!(ToolKey::parse("noseparator").is_err());
    }

    #[test]
    fn parse_empty_source() {
        assert!(ToolKey::parse(":tool").is_err());
    }

    #[test]
    fn parse_empty_tool() {
        assert!(ToolKey::parse("source:").is_err());
    }

    #[test]
    fn parse_both_empty() {
        assert!(ToolKey::parse(":").is_err());
    }

    // --- ToolKey::Display ---

    #[test]
    fn display_formats_correctly() {
        let key = ToolKey::new("src", "tool");
        assert_eq!(key.to_string(), "src:tool");
    }

    // --- ToolKey equality / hashing ---

    #[test]
    fn equality() {
        assert_eq!(ToolKey::new("a", "b"), ToolKey::new("a", "b"));
        assert_ne!(ToolKey::new("a", "b"), ToolKey::new("a", "c"));
    }

    #[test]
    fn hashing() {
        let mut set = HashSet::new();
        set.insert(ToolKey::new("s", "t"));
        assert!(set.contains(&ToolKey::new("s", "t")));
        assert!(!set.contains(&ToolKey::new("s", "x")));
    }

    // --- ToolRegistry::new ---

    #[test]
    fn registry_new_is_empty() {
        let reg = ToolRegistry::new();
        assert!(reg.all_tools().is_empty());
        assert!(reg.sources.is_empty());
    }

    // --- Mock ToolSource ---

    struct MockSource {
        tools: Vec<ToolInfo>,
    }

    #[async_trait::async_trait(?Send)]
    impl ToolSource for MockSource {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            Ok(self.tools.clone())
        }

        async fn get_schemas(&self, tools: &[String]) -> Result<Vec<ToolSchema>, String> {
            Ok(tools
                .iter()
                .map(|t| ToolSchema {
                    key: ToolKey::new("mock", t),
                    description: String::new(),
                    input_schema: serde_json::Value::Null,
                    output_schema: None,
                })
                .collect())
        }

        async fn call_tool(
            &self,
            tool: &str,
            _params: serde_json::Value,
            _context: &ToolCallContext,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"called": tool}))
        }
    }

    struct RefreshingSource {
        first: std::sync::Mutex<bool>,
    }

    #[async_trait::async_trait(?Send)]
    impl ToolSource for RefreshingSource {
        async fn refresh(&self) -> Result<(), String> {
            *self.first.lock().unwrap() = false;
            Ok(())
        }

        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            let is_first = *self.first.lock().unwrap();
            let name = if is_first {
                "before_refresh"
            } else {
                "after_refresh"
            };
            Ok(vec![ToolInfo {
                key: ToolKey::new("refreshing", name),
                description: "changes after refresh".to_string(),
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
            Ok(serde_json::Value::Null)
        }
    }

    #[tokio::test]
    async fn registry_refresh_and_list() {
        let mut reg = ToolRegistry::new();
        reg.register(
            "mock".to_string(),
            Box::new(MockSource {
                tools: vec![ToolInfo {
                    key: ToolKey::new("mock", "do_thing"),
                    description: "does a thing".to_string(),
                }],
            }),
        );
        reg.refresh_tools().await.unwrap();
        let tools = reg.all_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].key.tool, "do_thing");
    }

    #[tokio::test]
    async fn registry_call_tool() {
        let mut reg = ToolRegistry::new();
        reg.register("mock".to_string(), Box::new(MockSource { tools: vec![] }));
        let key = ToolKey::new("mock", "greet");
        let result = reg
            .call_tool(
                &key,
                serde_json::Value::Null,
                &ToolCallContext::new("/tmp", r#"User::"alice""#),
            )
            .await
            .unwrap();
        assert_eq!(result["called"], "greet");
    }

    #[tokio::test]
    async fn registry_refresh_invokes_source_refresh() {
        let mut reg = ToolRegistry::new();
        reg.register(
            "refreshing".to_string(),
            Box::new(RefreshingSource {
                first: std::sync::Mutex::new(true),
            }),
        );
        reg.refresh_tools().await.unwrap();
        let tools = reg.all_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].key.tool, "after_refresh");
    }
}
