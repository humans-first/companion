use std::collections::HashMap;
use std::rc::Rc;

use tokio::sync::Mutex;

use crate::error::HarnessError;
use crate::tool::{ToolRegistry, ToolSourceFactory};

pub struct SessionToolRuntime {
    pub tools: Mutex<ToolRegistry>,
    active_sources: Vec<String>,
}

impl SessionToolRuntime {
    fn new(tools: ToolRegistry, active_sources: Vec<String>) -> Self {
        Self {
            tools: Mutex::new(tools),
            active_sources,
        }
    }

    pub fn active_sources(&self) -> &[String] {
        &self.active_sources
    }
}

pub struct ToolRuntimeManager {
    factories: HashMap<String, Box<dyn ToolSourceFactory>>,
    runtimes: Mutex<HashMap<String, Rc<SessionToolRuntime>>>,
}

impl Default for ToolRuntimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRuntimeManager {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
            runtimes: Mutex::new(HashMap::new()),
        }
    }

    pub fn register_factory(&mut self, name: String, factory: Box<dyn ToolSourceFactory>) {
        self.factories.insert(name, factory);
    }

    pub fn configured_sources(&self) -> Vec<String> {
        let mut names = self.factories.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    pub async fn get_or_activate(
        &self,
        session_id: &str,
        active_sources: &[String],
    ) -> Result<Rc<SessionToolRuntime>, HarnessError> {
        if let Some(runtime) = self.current_runtime(session_id).await {
            if runtime.active_sources() == active_sources {
                return Ok(runtime);
            }
        }

        self.replace_runtime(session_id, active_sources).await
    }

    pub async fn replace_runtime(
        &self,
        session_id: &str,
        active_sources: &[String],
    ) -> Result<Rc<SessionToolRuntime>, HarnessError> {
        let runtime = Rc::new(self.build_runtime(active_sources).await?);
        self.runtimes
            .lock()
            .await
            .insert(session_id.to_string(), runtime.clone());
        Ok(runtime)
    }

    pub async fn current_runtime(&self, session_id: &str) -> Option<Rc<SessionToolRuntime>> {
        self.runtimes.lock().await.get(session_id).cloned()
    }

    pub async fn remove_runtime(&self, session_id: &str) {
        self.runtimes.lock().await.remove(session_id);
    }

    pub async fn refresh_all(&self) -> Result<(), HarnessError> {
        let runtimes = self.runtimes.lock().await.values().cloned().collect::<Vec<_>>();
        for runtime in runtimes {
            runtime.tools.lock().await.refresh_tools().await?;
        }
        Ok(())
    }

    pub async fn active_runtime_count(&self) -> usize {
        self.runtimes.lock().await.len()
    }

    pub async fn active_tool_count(&self) -> usize {
        let runtimes = self.runtimes.lock().await.values().cloned().collect::<Vec<_>>();
        let mut count = 0;
        for runtime in runtimes {
            count += runtime.tools.lock().await.all_tools().len();
        }
        count
    }

    async fn build_runtime(
        &self,
        active_sources: &[String],
    ) -> Result<SessionToolRuntime, HarnessError> {
        let mut registry = ToolRegistry::new();

        for source_name in active_sources {
            let factory = self
                .factories
                .get(source_name)
                .ok_or_else(|| HarnessError::Tool(format!("unknown tool source: {source_name}")))?;
            let source = factory
                .create(source_name)
                .await
                .map_err(|e| HarnessError::Tool(format!("{source_name}: {e}")))?;
            registry.register(source_name.clone(), source);
        }

        registry.refresh_tools().await?;
        Ok(SessionToolRuntime::new(registry, active_sources.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::tool::{ToolCallContext, ToolInfo, ToolKey, ToolSchema, ToolSource};

    use super::*;

    struct FakeSource {
        name: String,
    }

    #[async_trait::async_trait(?Send)]
    impl ToolSource for FakeSource {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            Ok(vec![ToolInfo {
                key: ToolKey::new(&self.name, "ping"),
                description: "ping".to_string(),
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
            Ok(serde_json::json!({"ok": true}))
        }
    }

    struct FakeFactory {
        created: Rc<RefCell<usize>>,
    }

    #[async_trait::async_trait(?Send)]
    impl ToolSourceFactory for FakeFactory {
        async fn create(&self, source_name: &str) -> Result<Box<dyn ToolSource>, String> {
            *self.created.borrow_mut() += 1;
            Ok(Box::new(FakeSource {
                name: source_name.to_string(),
            }))
        }
    }

    #[tokio::test]
    async fn reuses_runtime_when_source_set_matches() {
        let created = Rc::new(RefCell::new(0));
        let mut manager = ToolRuntimeManager::new();
        manager.register_factory(
            "tools".to_string(),
            Box::new(FakeFactory {
                created: created.clone(),
            }),
        );

        let sources = vec!["tools".to_string()];
        let first = manager.get_or_activate("s1", &sources).await.unwrap();
        let second = manager.get_or_activate("s1", &sources).await.unwrap();

        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(*created.borrow(), 1);
    }

    #[tokio::test]
    async fn replace_runtime_rebuilds_sources() {
        let created = Rc::new(RefCell::new(0));
        let mut manager = ToolRuntimeManager::new();
        manager.register_factory(
            "tools".to_string(),
            Box::new(FakeFactory {
                created: created.clone(),
            }),
        );

        let sources = vec!["tools".to_string()];
        let first = manager.replace_runtime("s1", &sources).await.unwrap();
        let second = manager.replace_runtime("s1", &sources).await.unwrap();

        assert!(!Rc::ptr_eq(&first, &second));
        assert_eq!(*created.borrow(), 2);
    }

    #[tokio::test]
    async fn remove_runtime_drops_mapping() {
        let created = Rc::new(RefCell::new(0));
        let mut manager = ToolRuntimeManager::new();
        manager.register_factory(
            "tools".to_string(),
            Box::new(FakeFactory {
                created: created.clone(),
            }),
        );

        manager
            .replace_runtime("s1", &["tools".to_string()])
            .await
            .unwrap();
        assert_eq!(manager.active_runtime_count().await, 1);
        manager.remove_runtime("s1").await;
        assert_eq!(manager.active_runtime_count().await, 0);
    }
}
