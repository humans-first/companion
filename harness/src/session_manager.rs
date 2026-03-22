use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::HarnessConfig;
use crate::error::HarnessError;
use crate::session::{ConversationMessage, SessionData};
use crate::session_store::{SessionListing, SessionStore, SessionStoreCapabilities};
use crate::tool_runtime::{SessionToolRuntime, ToolRuntimeManager};

const DEFAULT_MODE_ID: &str = "code";
const DEFAULT_PRINCIPAL: &str = r#"User::"anonymous""#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub prompt_suffix: &'static str,
}

const MODE_DESCRIPTORS: [ModeDescriptor; 3] = [
    ModeDescriptor {
        id: "code",
        name: "Code",
        description: "Implement changes, use tools when needed, and verify concrete results.",
        prompt_suffix:
            "You are in code mode. Prefer concrete implementation steps, tool use, and verification when the user asks for changes.",
    },
    ModeDescriptor {
        id: "ask",
        name: "Ask",
        description: "Focus on answering questions and investigating without changing code unless asked.",
        prompt_suffix:
            "You are in ask mode. Prioritize explanation and investigation. Avoid making code changes unless the user explicitly requests edits.",
    },
    ModeDescriptor {
        id: "architect",
        name: "Architect",
        description: "Focus on design, tradeoffs, and high-level planning before implementation.",
        prompt_suffix:
            "You are in architect mode. Focus on system design, tradeoffs, and plans before implementation details.",
    },
];

pub struct PromptTurn {
    pub session_id: String,
    pub session: SessionData,
    pub runtime: Rc<SessionToolRuntime>,
    pub cancellation: CancellationToken,
    pub principal: String,
    pub base_history_len: usize,
}

pub struct SessionManager {
    config: HarnessConfig,
    store: Box<dyn SessionStore>,
    tool_runtime: ToolRuntimeManager,
    active_turns: Mutex<HashMap<String, CancellationToken>>,
}

impl SessionManager {
    pub fn new(
        config: HarnessConfig,
        store: Box<dyn SessionStore>,
        tool_runtime: ToolRuntimeManager,
    ) -> Result<Self, HarnessError> {
        let manager = Self {
            config,
            store,
            tool_runtime,
            active_turns: Mutex::new(HashMap::new()),
        };
        manager.validate_mode_tool_sources()?;
        Ok(manager)
    }

    pub fn capabilities(&self) -> SessionStoreCapabilities {
        self.store.capabilities()
    }

    pub fn agent_name(&self) -> &str {
        &self.config.name
    }

    pub fn available_modes(&self) -> Vec<ModeDescriptor> {
        MODE_DESCRIPTORS.to_vec()
    }

    pub fn available_model_ids(&self) -> Vec<String> {
        let mut model_ids = vec![self.config.model.model_id.clone()];
        for model_id in &self.config.model.available_models {
            if !model_ids.iter().any(|candidate| candidate == model_id) {
                model_ids.push(model_id.clone());
            }
        }
        model_ids
    }

    pub fn supports_model(&self, model_id: &str) -> bool {
        self.available_model_ids()
            .iter()
            .any(|candidate| candidate == model_id)
    }

    pub fn mode_definition(&self, mode_id: &str) -> Option<ModeDescriptor> {
        MODE_DESCRIPTORS
            .iter()
            .find(|mode| mode.id == mode_id)
            .cloned()
    }

    pub fn effective_system_prompt(&self, mode_id: &str) -> String {
        let mut prompt = self.config.system_prompt.clone();
        let mode = self
            .mode_definition(mode_id)
            .or_else(|| self.mode_definition(DEFAULT_MODE_ID))
            .expect("default mode definition exists");
        prompt.push_str("\n\n## Session Mode\n");
        prompt.push_str(mode.prompt_suffix);
        prompt
    }

    pub async fn new_session(&self, cwd: PathBuf) -> Result<(String, SessionData), HarnessError> {
        let session_id = uuid::Uuid::now_v7().to_string();
        let data = SessionData::new(cwd, DEFAULT_MODE_ID, self.config.model.model_id.clone());
        self.store.create(&session_id, data.clone()).await?;
        Ok((session_id, data))
    }

    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: Option<PathBuf>,
    ) -> Result<SessionData, HarnessError> {
        self.load_with_cwd(session_id, cwd).await
    }

    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: Option<PathBuf>,
    ) -> Result<SessionData, HarnessError> {
        self.load_with_cwd(session_id, cwd).await
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionListing>, HarnessError> {
        self.store.list().await
    }

    pub async fn close_session(&self, session_id: &str) -> Result<(), HarnessError> {
        self.cancel(session_id).await;
        self.tool_runtime.remove_runtime(session_id).await;
        self.store.delete(session_id).await
    }

    pub async fn fork_session(
        &self,
        source_session_id: &str,
        cwd: PathBuf,
    ) -> Result<(String, SessionData), HarnessError> {
        let mut data = self.store.load(source_session_id).await?;
        let session_id = uuid::Uuid::now_v7().to_string();
        data.cwd = cwd;
        self.store.create(&session_id, data.clone()).await?;
        Ok((session_id, data))
    }

    pub async fn set_mode(
        &self,
        session_id: &str,
        mode_id: &str,
    ) -> Result<SessionData, HarnessError> {
        self.ensure_no_prompt_in_flight(session_id).await?;
        if self.mode_definition(mode_id).is_none() {
            return Err(HarnessError::Tool(format!("unknown mode id: {mode_id}")));
        }

        let mut data = self.store.load(session_id).await?;
        data.mode_id = mode_id.to_string();
        let selected_sources = self.tool_sources_for_mode(mode_id);
        let _ = self
            .tool_runtime
            .replace_runtime(session_id, &selected_sources)
            .await?;
        self.store.save(session_id, data.clone()).await?;
        Ok(data)
    }

    pub async fn set_model(
        &self,
        session_id: &str,
        model_id: &str,
    ) -> Result<SessionData, HarnessError> {
        self.ensure_no_prompt_in_flight(session_id).await?;
        if !self.supports_model(model_id) {
            return Err(HarnessError::Tool(format!("unknown model id: {model_id}")));
        }

        let mut data = self.store.load(session_id).await?;
        data.model_id = model_id.to_string();
        self.store.save(session_id, data.clone()).await?;
        Ok(data)
    }

    pub async fn begin_prompt(
        &self,
        session_id: &str,
        principal: Option<String>,
        user_text: String,
    ) -> Result<PromptTurn, HarnessError> {
        let mut session = self.store.load(session_id).await?;
        let cancellation = self.begin_turn(session_id).await?;
        let base_history_len = session.history.len();
        session.history.push(ConversationMessage::user(user_text));
        let runtime_result = self
            .tool_runtime
            .get_or_activate(session_id, &self.tool_sources_for_mode(&session.mode_id))
            .await;
        let runtime = match runtime_result {
            Ok(runtime) => runtime,
            Err(err) => {
                self.finish_prompt(session_id).await;
                return Err(err);
            }
        };
        Ok(PromptTurn {
            session_id: session_id.to_string(),
            session,
            runtime,
            cancellation,
            principal: principal.unwrap_or_else(|| DEFAULT_PRINCIPAL.to_string()),
            base_history_len,
        })
    }

    pub async fn commit_prompt(
        &self,
        session_id: &str,
        base_history_len: usize,
        session: SessionData,
    ) -> Result<(), HarnessError> {
        if session.history.len() < base_history_len {
            return Err(HarnessError::Tool(
                "prompt session history regressed during commit".to_string(),
            ));
        }

        let mut current = self.store.load(session_id).await?;
        current
            .history
            .extend(session.history.into_iter().skip(base_history_len));
        self.store.save(session_id, current).await
    }

    pub async fn finish_prompt(&self, session_id: &str) {
        self.active_turns.lock().await.remove(session_id);
    }

    pub async fn cancel(&self, session_id: &str) {
        if let Some(token) = self.active_turns.lock().await.get(session_id).cloned() {
            token.cancel();
        }
    }

    pub async fn refresh_tool_runtimes(&self) -> Result<(), HarnessError> {
        self.tool_runtime.refresh_all().await
    }

    pub async fn active_runtime_count(&self) -> usize {
        self.tool_runtime.active_runtime_count().await
    }

    pub async fn active_tool_count(&self) -> usize {
        self.tool_runtime.active_tool_count().await
    }

    pub fn configured_tool_sources(&self) -> Vec<String> {
        self.tool_runtime.configured_sources()
    }

    async fn begin_turn(&self, session_id: &str) -> Result<CancellationToken, HarnessError> {
        let mut active_turns = self.active_turns.lock().await;
        if active_turns.contains_key(session_id) {
            return Err(HarnessError::PromptInFlight(session_id.to_string()));
        }

        let token = CancellationToken::new();
        active_turns.insert(session_id.to_string(), token.clone());
        Ok(token)
    }

    async fn ensure_no_prompt_in_flight(&self, session_id: &str) -> Result<(), HarnessError> {
        if self.active_turns.lock().await.contains_key(session_id) {
            return Err(HarnessError::PromptInFlight(session_id.to_string()));
        }
        Ok(())
    }

    fn tool_sources_for_mode(&self, mode_id: &str) -> Vec<String> {
        self.config
            .mode_tool_sources
            .get(mode_id)
            .cloned()
            .unwrap_or_else(|| self.tool_runtime.configured_sources())
    }

    async fn load_with_cwd(
        &self,
        session_id: &str,
        cwd: Option<PathBuf>,
    ) -> Result<SessionData, HarnessError> {
        let mut data = self.store.load(session_id).await?;
        if let Some(cwd) = cwd {
            if data.cwd != cwd {
                data.cwd = cwd;
                self.store.save(session_id, data.clone()).await?;
            }
        }
        Ok(data)
    }

    fn validate_mode_tool_sources(&self) -> Result<(), HarnessError> {
        let known_modes = MODE_DESCRIPTORS
            .iter()
            .map(|mode| mode.id)
            .collect::<HashSet<_>>();
        let configured_sources = self
            .tool_runtime
            .configured_sources()
            .into_iter()
            .collect::<HashSet<_>>();

        for (mode_id, source_names) in &self.config.mode_tool_sources {
            if !known_modes.contains(mode_id.as_str()) {
                return Err(HarnessError::Config(format!(
                    "unknown mode id in 'mode_tool_sources': {mode_id}"
                )));
            }

            let mut seen = HashSet::new();
            for source_name in source_names {
                if !configured_sources.contains(source_name) {
                    return Err(HarnessError::Config(format!(
                        "unknown tool source '{source_name}' in 'mode_tool_sources.{mode_id}'"
                    )));
                }
                if !seen.insert(source_name) {
                    return Err(HarnessError::Config(format!(
                        "duplicate tool source '{source_name}' in 'mode_tool_sources.{mode_id}'"
                    )));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::tool::{ToolCallContext, ToolInfo, ToolKey, ToolSchema, ToolSource, ToolSourceFactory};

    use super::*;

    struct FakeSource {
        source_name: String,
    }

    #[async_trait::async_trait(?Send)]
    impl ToolSource for FakeSource {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, String> {
            Ok(vec![ToolInfo {
                key: ToolKey::new(&self.source_name, "ping"),
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
            Ok(serde_json::Value::Null)
        }
    }

    struct FakeFactory;

    #[async_trait::async_trait(?Send)]
    impl ToolSourceFactory for FakeFactory {
        async fn create(&self, source_name: &str) -> Result<Box<dyn ToolSource>, String> {
            Ok(Box::new(FakeSource {
                source_name: source_name.to_string(),
            }))
        }
    }

    fn config() -> HarnessConfig {
        config_with_mode_sources(HashMap::new())
    }

    fn config_with_mode_sources(mode_tool_sources: HashMap<String, Vec<String>>) -> HarnessConfig {
        HarnessConfig {
            name: "test".to_string(),
            system_prompt: "You are helpful.".to_string(),
            model: crate::config::ModelConfig {
                model_id: "test-model".to_string(),
                available_models: vec!["backup-model".to_string()],
                base_url: "http://localhost".to_string(),
                api_key: None,
                max_tokens: None,
                temperature: None,
                timeout_secs: None,
                requests_per_minute: None,
            },
            mcp_servers: HashMap::new(),
            mode_tool_sources,
            tool_policy: "permit(principal, action, resource);".to_string(),
        }
    }

    fn manager() -> SessionManager {
        manager_with_config(config())
    }

    fn manager_with_config(config: HarnessConfig) -> SessionManager {
        let mut runtime = ToolRuntimeManager::new();
        runtime.register_factory("alpha".to_string(), Box::new(FakeFactory));
        runtime.register_factory("beta".to_string(), Box::new(FakeFactory));
        SessionManager::new(
            config,
            Box::new(crate::session_store::InMemorySessionStore::new()),
            runtime,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn principal_is_per_prompt_not_persisted() {
        let manager = manager();
        let (session_id, _) = manager.new_session(PathBuf::from("/tmp")).await.unwrap();

        let turn = manager
            .begin_prompt(&session_id, Some("alice".to_string()), "hello".to_string())
            .await
            .unwrap();
        assert_eq!(turn.principal, "alice");
        manager.finish_prompt(&session_id).await;

        let loaded = manager.load_session(&session_id, None).await.unwrap();
        assert!(loaded.history.is_empty());
    }

    #[tokio::test]
    async fn load_session_updates_cwd_when_requested() {
        let manager = manager();
        let (session_id, _) = manager.new_session(PathBuf::from("/tmp")).await.unwrap();

        let loaded = manager
            .load_session(&session_id, Some(PathBuf::from("/workspace")))
            .await
            .unwrap();

        assert_eq!(loaded.cwd, PathBuf::from("/workspace"));
        let resumed = manager.resume_session(&session_id, None).await.unwrap();
        assert_eq!(resumed.cwd, PathBuf::from("/workspace"));
    }

    #[tokio::test]
    async fn set_mode_restarts_runtime() {
        let manager = manager_with_config(config_with_mode_sources(HashMap::from([(
            "ask".to_string(),
            vec!["beta".to_string()],
        )])));
        let (session_id, _) = manager.new_session(PathBuf::from("/tmp")).await.unwrap();

        let first = manager
            .begin_prompt(&session_id, None, "hello".to_string())
            .await
            .unwrap()
            .runtime;
        assert_eq!(
            first.active_sources(),
            &["alpha".to_string(), "beta".to_string()]
        );
        manager.finish_prompt(&session_id).await;

        let updated = manager.set_mode(&session_id, "ask").await.unwrap();
        assert_eq!(updated.mode_id, "ask");

        let second = manager
            .begin_prompt(&session_id, None, "again".to_string())
            .await
            .unwrap()
            .runtime;
        manager.finish_prompt(&session_id).await;

        assert!(!Rc::ptr_eq(&first, &second));
        assert_eq!(second.active_sources(), &["beta".to_string()]);
    }

    #[tokio::test]
    async fn set_mode_and_model_are_rejected_while_prompt_is_in_flight() {
        let manager = manager();
        let (session_id, _) = manager.new_session(PathBuf::from("/tmp")).await.unwrap();

        let turn = manager
            .begin_prompt(&session_id, None, "hello".to_string())
            .await
            .unwrap();

        let mode_err = manager.set_mode(&session_id, "ask").await.unwrap_err();
        assert!(matches!(mode_err, HarnessError::PromptInFlight(_)));

        let model_err = manager
            .set_model(&session_id, "backup-model")
            .await
            .unwrap_err();
        assert!(matches!(model_err, HarnessError::PromptInFlight(_)));

        manager.finish_prompt(&turn.session_id).await;
    }

    #[tokio::test]
    async fn commit_prompt_merges_history_onto_latest_session_state() {
        let manager = manager();
        let (session_id, _) = manager.new_session(PathBuf::from("/tmp")).await.unwrap();

        let mut turn = manager
            .begin_prompt(&session_id, None, "hello".to_string())
            .await
            .unwrap();
        turn.session
            .history
            .push(ConversationMessage::assistant("hi".to_string()));

        let mut stored = manager.store.load(&session_id).await.unwrap();
        stored.cwd = PathBuf::from("/workspace");
        stored.model_id = "backup-model".to_string();
        manager.store.save(&session_id, stored).await.unwrap();

        manager
            .commit_prompt(&session_id, turn.base_history_len, turn.session)
            .await
            .unwrap();
        manager.finish_prompt(&session_id).await;

        let loaded = manager.load_session(&session_id, None).await.unwrap();
        assert_eq!(loaded.cwd, PathBuf::from("/workspace"));
        assert_eq!(loaded.model_id, "backup-model");
        assert_eq!(loaded.history.len(), 2);
        assert_eq!(loaded.history[0].content.as_deref(), Some("hello"));
        assert_eq!(loaded.history[1].content.as_deref(), Some("hi"));
    }

    #[test]
    fn rejects_unknown_mode_tool_source_mode() {
        let err = match SessionManager::new(
            config_with_mode_sources(HashMap::from([(
                "unknown".to_string(),
                vec!["alpha".to_string()],
            )])),
            Box::new(crate::session_store::InMemorySessionStore::new()),
            {
                let mut runtime = ToolRuntimeManager::new();
                runtime.register_factory("alpha".to_string(), Box::new(FakeFactory));
                runtime
            },
        ) {
            Ok(_) => panic!("expected invalid mode config to be rejected"),
            Err(err) => err,
        };

        assert!(err
            .to_string()
            .contains("unknown mode id in 'mode_tool_sources'"));
    }

    #[test]
    fn rejects_unknown_mode_tool_source_name() {
        let err = match SessionManager::new(
            config_with_mode_sources(HashMap::from([(
                "architect".to_string(),
                vec!["missing".to_string()],
            )])),
            Box::new(crate::session_store::InMemorySessionStore::new()),
            {
                let mut runtime = ToolRuntimeManager::new();
                runtime.register_factory("alpha".to_string(), Box::new(FakeFactory));
                runtime
            },
        ) {
            Ok(_) => panic!("expected invalid source config to be rejected"),
            Err(err) => err,
        };

        assert!(err
            .to_string()
            .contains("unknown tool source 'missing' in 'mode_tool_sources.architect'"));
    }
}
