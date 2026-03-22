use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::{self as acp};
use serde_json::value::RawValue;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::agent_loop;
use crate::error::HarnessError;
use crate::llm::LlmProvider;
use crate::policy::ToolPolicyEngine;
use crate::sandbox::SandboxConfig;
use crate::session::SessionData;
use crate::session_manager::SessionManager;

pub struct QueuedSessionNotification {
    pub notification: acp::SessionNotification,
    pub ack: oneshot::Sender<Result<(), acp::Error>>,
}

#[derive(Clone)]
pub struct SessionNotifier {
    tx: mpsc::UnboundedSender<QueuedSessionNotification>,
}

impl SessionNotifier {
    pub async fn send(&self, notification: acp::SessionNotification) -> Result<(), acp::Error> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(QueuedSessionNotification {
                notification,
                ack: ack_tx,
            })
            .map_err(|_| acp::Error::internal_error().data("session notifier is closed"))?;
        ack_rx
            .await
            .map_err(|_| acp::Error::internal_error().data("session notifier dropped ack"))?
    }
}

pub fn session_notifier_channel() -> (
    SessionNotifier,
    mpsc::UnboundedReceiver<QueuedSessionNotification>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (SessionNotifier { tx }, rx)
}

const MODE_CONFIG_ID: &str = "mode";
const MODEL_CONFIG_ID: &str = "model";

pub struct HarnessAgent {
    sessions: SessionManager,
    llm: Arc<dyn LlmProvider>,
    sandbox_config: SandboxConfig,
    policy: Option<ToolPolicyEngine>,
    notifier: SessionNotifier,
    session_update_guards: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl HarnessAgent {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        sessions: SessionManager,
        sandbox_config: SandboxConfig,
        policy: Option<ToolPolicyEngine>,
        notifier: SessionNotifier,
    ) -> Self {
        Self {
            sessions,
            llm,
            sandbox_config,
            policy,
            notifier,
            session_update_guards: Mutex::new(HashMap::new()),
        }
    }

    async fn send_agent_message(
        &self,
        session_id: acp::SessionId,
        content: String,
    ) -> Result<(), acp::Error> {
        if content.is_empty() {
            return Ok(());
        }

        self.notifier
            .send(acp::SessionNotification::new(
                session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(content.into())),
            ))
            .await
    }

    async fn send_current_mode_update(
        &self,
        session_id: acp::SessionId,
        mode_id: String,
    ) -> Result<(), acp::Error> {
        self.notifier
            .send(acp::SessionNotification::new(
                session_id,
                acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(mode_id)),
            ))
            .await
    }

    async fn send_config_option_update(
        &self,
        session_id: acp::SessionId,
        config_options: Vec<acp::SessionConfigOption>,
    ) -> Result<(), acp::Error> {
        self.notifier
            .send(acp::SessionNotification::new(
                session_id,
                acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                    config_options,
                )),
            ))
            .await
    }

    async fn session_update_lock(&self, session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let guard = {
            let mut guards = self.session_update_guards.lock().await;
            guards
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        guard.lock_owned().await
    }

    async fn rollback_mode_change(&self, session_id: &str, previous_mode_id: &str) {
        if let Err(err) = self.sessions.set_mode(session_id, previous_mode_id).await {
            warn!(
                session_id,
                previous_mode_id,
                error = %err,
                "failed to roll back session mode change after notification error"
            );
        }
    }

    async fn rollback_model_change(&self, session_id: &str, previous_model_id: &str) {
        if let Err(err) = self.sessions.set_model(session_id, previous_model_id).await {
            warn!(
                session_id,
                previous_model_id,
                error = %err,
                "failed to roll back session model change after notification error"
            );
        }
    }

    async fn apply_mode_change(
        &self,
        session_id: acp::SessionId,
        mode_id: &str,
    ) -> Result<SessionData, acp::Error> {
        let sid = session_id.0.to_string();
        let _guard = self.session_update_lock(&sid).await;
        let previous = self
            .sessions
            .load_session(&sid, None)
            .await
            .map_err(acp::Error::from)?;
        let updated = self
            .sessions
            .set_mode(&sid, mode_id)
            .await
            .map_err(acp::Error::from)?;

        if let Err(err) = self
            .send_current_mode_update(session_id.clone(), updated.mode_id.clone())
            .await
        {
            self.rollback_mode_change(&sid, &previous.mode_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .send_config_option_update(
                session_id,
                self.session_config_options(&updated.mode_id, &updated.model_id),
            )
            .await
        {
            self.rollback_mode_change(&sid, &previous.mode_id).await;
            return Err(err);
        }

        Ok(updated)
    }

    async fn apply_model_change(
        &self,
        session_id: acp::SessionId,
        model_id: &str,
    ) -> Result<SessionData, acp::Error> {
        let sid = session_id.0.to_string();
        let _guard = self.session_update_lock(&sid).await;
        let previous = self
            .sessions
            .load_session(&sid, None)
            .await
            .map_err(acp::Error::from)?;
        let updated = self
            .sessions
            .set_model(&sid, model_id)
            .await
            .map_err(acp::Error::from)?;

        if let Err(err) = self
            .send_config_option_update(
                session_id,
                self.session_config_options(&updated.mode_id, &updated.model_id),
            )
            .await
        {
            self.rollback_model_change(&sid, &previous.model_id).await;
            return Err(err);
        }

        Ok(updated)
    }

    fn available_modes(&self) -> Vec<acp::SessionMode> {
        self.sessions
            .available_modes()
            .into_iter()
            .map(|mode| {
                acp::SessionMode::new(mode.id, mode.name).description(mode.description.to_string())
            })
            .collect()
    }

    fn session_mode_state(&self, mode_id: &str) -> acp::SessionModeState {
        acp::SessionModeState::new(mode_id.to_string(), self.available_modes())
    }

    fn session_model_state(&self, model_id: &str) -> acp::SessionModelState {
        let available_models = self
            .sessions
            .available_model_ids()
            .into_iter()
            .map(|candidate| acp::ModelInfo::new(candidate.clone(), candidate))
            .collect();
        acp::SessionModelState::new(model_id.to_string(), available_models)
    }

    fn session_config_options(
        &self,
        mode_id: &str,
        model_id: &str,
    ) -> Vec<acp::SessionConfigOption> {
        let mode_options = self
            .sessions
            .available_modes()
            .into_iter()
            .map(|mode| {
                acp::SessionConfigSelectOption::new(mode.id, mode.name)
                    .description(mode.description.to_string())
            })
            .collect::<Vec<_>>();
        let model_options = self
            .sessions
            .available_model_ids()
            .into_iter()
            .map(|candidate| acp::SessionConfigSelectOption::new(candidate.clone(), candidate))
            .collect::<Vec<_>>();

        vec![
            acp::SessionConfigOption::select(MODE_CONFIG_ID, "Mode", mode_id.to_string(), mode_options)
                .description("Controls how the harness approaches the session.".to_string())
                .category(acp::SessionConfigOptionCategory::Mode),
            acp::SessionConfigOption::select(
                MODEL_CONFIG_ID,
                "Model",
                model_id.to_string(),
                model_options,
            )
            .description("Controls which language model is used for prompts in this session.".to_string())
            .category(acp::SessionConfigOptionCategory::Model),
        ]
    }

    fn new_session_response(
        &self,
        session_id: acp::SessionId,
        data: &SessionData,
    ) -> acp::NewSessionResponse {
        acp::NewSessionResponse::new(session_id)
            .modes(self.session_mode_state(&data.mode_id))
            .models(self.session_model_state(&data.model_id))
            .config_options(self.session_config_options(&data.mode_id, &data.model_id))
    }

    fn load_session_response(&self, data: &SessionData) -> acp::LoadSessionResponse {
        acp::LoadSessionResponse::new()
            .modes(self.session_mode_state(&data.mode_id))
            .models(self.session_model_state(&data.model_id))
            .config_options(self.session_config_options(&data.mode_id, &data.model_id))
    }

    fn fork_session_response(
        &self,
        session_id: acp::SessionId,
        data: &SessionData,
    ) -> acp::ForkSessionResponse {
        acp::ForkSessionResponse::new(session_id)
            .modes(self.session_mode_state(&data.mode_id))
            .models(self.session_model_state(&data.model_id))
            .config_options(self.session_config_options(&data.mode_id, &data.model_id))
    }

    fn resume_session_response(&self, data: &SessionData) -> acp::ResumeSessionResponse {
        acp::ResumeSessionResponse::new()
            .modes(self.session_mode_state(&data.mode_id))
            .models(self.session_model_state(&data.model_id))
            .config_options(self.session_config_options(&data.mode_id, &data.model_id))
    }

    fn initialize_capabilities(&self) -> acp::AgentCapabilities {
        let store_capabilities = self.sessions.capabilities();
        let mut session_capabilities = acp::SessionCapabilities::new();
        if store_capabilities.list {
            session_capabilities = session_capabilities.list(acp::SessionListCapabilities::new());
        }
        if store_capabilities.fork {
            session_capabilities = session_capabilities.fork(acp::SessionForkCapabilities::new());
        }
        if store_capabilities.resume {
            session_capabilities =
                session_capabilities.resume(acp::SessionResumeCapabilities::new());
        }
        if store_capabilities.close {
            session_capabilities =
                session_capabilities.close(acp::SessionCloseCapabilities::new());
        }

        acp::AgentCapabilities::new()
            .load_session(store_capabilities.load)
            .session_capabilities(session_capabilities)
    }

    fn ensure_supported(&self, supported: bool) -> Result<(), acp::Error> {
        if supported {
            Ok(())
        } else {
            Err(acp::Error::method_not_found())
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for HarnessAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        debug!(name = %self.sessions.agent_name(), "initializing harness agent");
        Ok(acp::InitializeResponse::new(args.protocol_version)
            .agent_capabilities(self.initialize_capabilities())
            .agent_info(
                acp::Implementation::new("harness", env!("CARGO_PKG_VERSION"))
                    .title(self.sessions.agent_name()),
            ))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let (sid, data) = self
            .sessions
            .new_session(args.cwd)
            .await
            .map_err(acp::Error::from)?;
        let session_id = acp::SessionId::new(sid);
        Ok(self.new_session_response(session_id, &data))
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        self.ensure_supported(self.sessions.capabilities().load)?;
        let data = self
            .sessions
            .load_session(args.session_id.0.as_ref(), Some(args.cwd))
            .await
            .map_err(acp::Error::from)?;
        Ok(self.load_session_response(&data))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let sid = args.session_id.0.to_string();
        let principal = args
            .meta
            .as_ref()
            .and_then(|m| m.get("principal"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        let user_text = extract_text_from_prompt(&args.prompt).map_err(acp::Error::from)?;

        let mut turn = self
            .sessions
            .begin_prompt(&sid, principal, user_text)
            .await
            .map_err(acp::Error::from)?;
        let system_prompt = self.sessions.effective_system_prompt(&turn.session.mode_id);
        let model_id = turn.session.model_id.clone();

        let result = {
            let tools = turn.runtime.tools.lock().await;
            agent_loop::run(
                &mut turn.session,
                self.llm.as_ref(),
                &tools,
                &self.sandbox_config,
                self.policy.as_ref(),
                agent_loop::RunConfig {
                    system_prompt: &system_prompt,
                    model_id: &model_id,
                    principal: &turn.principal,
                },
                &turn.cancellation,
            )
            .await
        };

        self.sessions.finish_prompt(&sid).await;

        match result {
            Ok(content) => {
                self.send_agent_message(args.session_id, content).await?;
                self.sessions
                    .commit_prompt(&sid, turn.base_history_len, turn.session)
                    .await
                    .map_err(acp::Error::from)?;
                Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
            }
            Err(HarnessError::Cancelled) => Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
            Err(err) => Err(err.into()),
        }
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        self.sessions.cancel(args.session_id.0.as_ref()).await;
        Ok(())
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        self.apply_mode_change(args.session_id, args.mode_id.0.as_ref())
            .await?;
        Ok(acp::SetSessionModeResponse::new())
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        self.apply_model_change(args.session_id, args.model_id.0.as_ref())
            .await?;
        Ok(acp::SetSessionModelResponse::new())
    }

    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        let updated = match args.config_id.0.as_ref() {
            MODE_CONFIG_ID => {
                self.apply_mode_change(args.session_id.clone(), args.value.0.as_ref())
                    .await?
            }
            MODEL_CONFIG_ID => {
                self.apply_model_change(args.session_id.clone(), args.value.0.as_ref())
                    .await?
            }
            other => {
                return Err(acp::Error::invalid_params().data(format!("unknown config id: {other}")))
            }
        };

        let config_options = self.session_config_options(&updated.mode_id, &updated.model_id);
        Ok(acp::SetSessionConfigOptionResponse::new(config_options))
    }

    async fn close_session(
        &self,
        args: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, acp::Error> {
        self.ensure_supported(self.sessions.capabilities().close)?;
        self.sessions
            .close_session(args.session_id.0.as_ref())
            .await
            .map_err(acp::Error::from)?;
        Ok(acp::CloseSessionResponse::new())
    }

    async fn list_sessions(
        &self,
        _args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        self.ensure_supported(self.sessions.capabilities().list)?;
        let sessions = self
            .sessions
            .list_sessions()
            .await
            .map_err(acp::Error::from)?
            .into_iter()
            .map(|session| acp::SessionInfo::new(acp::SessionId::new(session.id), session.cwd))
            .collect();
        Ok(acp::ListSessionsResponse::new(sessions))
    }

    async fn fork_session(
        &self,
        args: acp::ForkSessionRequest,
    ) -> Result<acp::ForkSessionResponse, acp::Error> {
        self.ensure_supported(self.sessions.capabilities().fork)?;
        let (sid, data) = self
            .sessions
            .fork_session(args.session_id.0.as_ref(), args.cwd)
            .await
            .map_err(acp::Error::from)?;
        Ok(self.fork_session_response(acp::SessionId::new(sid), &data))
    }

    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        self.ensure_supported(self.sessions.capabilities().resume)?;
        let data = self
            .sessions
            .resume_session(args.session_id.0.as_ref(), Some(args.cwd))
            .await
            .map_err(acp::Error::from)?;
        Ok(self.resume_session_response(&data))
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        if args.method.as_ref() == "harness/status" {
            let json = serde_json::json!({
                "status": "ok",
                "name": self.sessions.agent_name(),
                "tools": self.sessions.active_tool_count().await,
                "active_runtimes": self.sessions.active_runtime_count().await,
                "configured_sources": self.sessions.configured_tool_sources(),
            });
            let raw: Arc<RawValue> = RawValue::from_string(json.to_string())
                .map_err(|_| acp::Error::internal_error())?
                .into();
            return Ok(acp::ExtResponse::new(raw));
        }

        if args.method.as_ref() == "harness/refresh_tools" {
            let json = match self.sessions.refresh_tool_runtimes().await {
                Ok(()) => serde_json::json!({"status": "ok"}),
                Err(e) => serde_json::json!({"status": "error", "error": e.to_string()}),
            };
            let raw: Arc<RawValue> = RawValue::from_string(json.to_string())
                .map_err(|_| acp::Error::internal_error())?
                .into();
            return Ok(acp::ExtResponse::new(raw));
        }

        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

/// Extract plain text from ACP prompt content blocks.
fn extract_text_from_prompt(prompt: &[acp::ContentBlock]) -> Result<String, HarnessError> {
    let texts: Vec<&str> = prompt
        .iter()
        .filter_map(|block| {
            if let acp::ContentBlock::Text(text_block) = block {
                Some(text_block.text.as_str())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        return Err(HarnessError::Tool(
            "prompt must contain at least one text block".into(),
        ));
    }
    Ok(texts.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use agent_client_protocol::Agent as _;
    use async_openai::types::{
        ChatCompletionRequestMessage, ChatCompletionTool, CreateChatCompletionResponse,
    };
    use tokio::sync::Notify;

    use super::*;
    use crate::config::{HarnessConfig, ModelConfig};
    use crate::llm::LlmRequestConfig;
    use crate::sandbox::SandboxConfig;
    use crate::session_store::{InMemorySessionStore, SessionStoreCapabilities};
    use crate::tool_runtime::ToolRuntimeManager;

    struct ImmediateProvider {
        response: CreateChatCompletionResponse,
        requested_models: Arc<StdMutex<Vec<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ImmediateProvider {
        async fn chat_completion(
            &self,
            _messages: Vec<ChatCompletionRequestMessage>,
            _tools: Vec<ChatCompletionTool>,
            request_config: LlmRequestConfig,
        ) -> Result<CreateChatCompletionResponse, HarnessError> {
            self.requested_models
                .lock()
                .unwrap()
                .push(request_config.model_id);
            Ok(self.response.clone())
        }
    }

    struct BlockingProvider {
        started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for BlockingProvider {
        async fn chat_completion(
            &self,
            _messages: Vec<ChatCompletionRequestMessage>,
            _tools: Vec<ChatCompletionTool>,
            _request_config: LlmRequestConfig,
        ) -> Result<CreateChatCompletionResponse, HarnessError> {
            self.started.notify_one();
            tokio::time::sleep(Duration::from_secs(60)).await;
            unreachable!("cancelled prompt should drop the provider future");
        }
    }

    fn test_config() -> HarnessConfig {
        HarnessConfig {
            name: "test".to_string(),
            system_prompt: "You are helpful.".to_string(),
            model: ModelConfig {
                model_id: "test-model".to_string(),
                available_models: vec!["backup-model".to_string()],
                base_url: "http://localhost".to_string(),
                api_key: None,
                max_tokens: None,
                temperature: None,
                timeout_secs: Some(1),
                requests_per_minute: None,
            },
            mcp_servers: Default::default(),
            mode_tool_sources: Default::default(),
            tool_policy: "permit(principal, action, resource);".to_string(),
        }
    }

    fn sandbox_config() -> SandboxConfig {
        SandboxConfig {
            timeout: Duration::from_secs(1),
            runner_path: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("sandbox")
                .join("runner.py"),
        }
    }

    fn response_with_text(text: &str) -> CreateChatCompletionResponse {
        serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test-model",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": text
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        }))
        .unwrap()
    }

    fn response_with_unexpected_tool_call(text: Option<&str>) -> CreateChatCompletionResponse {
        serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test-model",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": text,
                        "tool_calls": [
                            {
                                "id": "call-1",
                                "type": "function",
                                "function": {
                                    "name": "execute",
                                    "arguments": "{\"code\":\"print(1)\"}"
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        }))
        .unwrap()
    }

    fn immediate_provider(text: &str) -> (ImmediateProvider, Arc<StdMutex<Vec<Option<String>>>>) {
        let requested_models = Arc::new(StdMutex::new(Vec::new()));
        (
            ImmediateProvider {
                response: response_with_text(text),
                requested_models: requested_models.clone(),
            },
            requested_models,
        )
    }

    fn agent_with_store_capabilities(
        provider: Arc<dyn LlmProvider>,
        capabilities: SessionStoreCapabilities,
        notifier: SessionNotifier,
    ) -> HarnessAgent {
        let sessions = SessionManager::new(
            test_config(),
            Box::new(InMemorySessionStore::with_capabilities(capabilities)),
            ToolRuntimeManager::new(),
        )
        .unwrap();
        HarnessAgent::new(provider, sessions, sandbox_config(), None, notifier)
    }

    fn agent_with_provider(provider: Arc<dyn LlmProvider>, notifier: SessionNotifier) -> HarnessAgent {
        agent_with_store_capabilities(
            provider,
            SessionStoreCapabilities {
                list: true,
                load: true,
                resume: true,
                fork: true,
                close: true,
            },
            notifier,
        )
    }

    fn start_notifier_pump(
        mut rx: mpsc::UnboundedReceiver<QueuedSessionNotification>,
    ) -> Arc<StdMutex<Vec<acp::SessionNotification>>> {
        let notifications = Arc::new(StdMutex::new(Vec::new()));
        let sink = notifications.clone();
        tokio::task::spawn_local(async move {
            while let Some(envelope) = rx.recv().await {
                sink.lock().unwrap().push(envelope.notification);
                let _ = envelope.ack.send(Ok(()));
            }
        });
        notifications
    }

    async fn new_session(agent: &HarnessAgent) -> acp::SessionId {
        agent
            .new_session(acp::NewSessionRequest::new(std::env::temp_dir()))
            .await
            .unwrap()
            .session_id
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_derives_capabilities_from_store() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let (provider, _) = immediate_provider("unused");
                let agent = agent_with_store_capabilities(
                    Arc::new(provider),
                    SessionStoreCapabilities {
                        list: true,
                        load: false,
                        resume: false,
                        fork: true,
                        close: false,
                    },
                    notifier,
                );

                let response = agent
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .unwrap();

                let capabilities = response.agent_capabilities;
                assert!(!capabilities.load_session);
                let session_capabilities = capabilities.session_capabilities;
                assert!(session_capabilities.list.is_some());
                assert!(session_capabilities.fork.is_some());
                assert!(session_capabilities.resume.is_none());
                assert!(session_capabilities.close.is_none());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_emits_agent_message_chunk_and_persists_history() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let notifications = start_notifier_pump(rx);
                let (provider, _requested_models) = immediate_provider("hello world");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                let response = agent
                    .prompt(acp::PromptRequest::new(
                        session_id.clone(),
                        vec!["say hi".into()],
                    ))
                    .await
                    .unwrap();

                assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
                {
                    let notifications = notifications.lock().unwrap();
                    assert_eq!(notifications.len(), 1);
                    match &notifications[0].update {
                        acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                            acp::ContentBlock::Text(text) => assert_eq!(text.text, "hello world"),
                            other => panic!("unexpected content block: {other:?}"),
                        },
                        other => panic!("unexpected session update: {other:?}"),
                    }
                }

                let stored = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(stored.history.len(), 2);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn second_prompt_on_same_session_is_rejected() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let started = Arc::new(Notify::new());
                let agent = Rc::new(agent_with_provider(
                    Arc::new(BlockingProvider {
                        started: started.clone(),
                    }),
                    notifier,
                ));

                let session_id = new_session(&agent).await;
                let first_agent = agent.clone();
                let first_session = session_id.clone();
                let prompt_handle = tokio::task::spawn_local(async move {
                    first_agent
                        .prompt(acp::PromptRequest::new(first_session, vec!["first".into()]))
                        .await
                });

                started.notified().await;
                let err = agent
                    .prompt(acp::PromptRequest::new(
                        session_id.clone(),
                        vec!["second".into()],
                    ))
                    .await
                    .unwrap_err();

                assert!(err.message.contains("prompt already in flight"));
                agent
                    .cancel(acp::CancelNotification::new(session_id))
                    .await
                    .unwrap();
                let response = prompt_handle.await.unwrap().unwrap();
                assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_session_advertises_modes_models_and_config_options() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let (provider, _requested_models) = immediate_provider("unused");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let response = agent
                    .new_session(acp::NewSessionRequest::new(std::env::temp_dir()))
                    .await
                    .unwrap();

                assert_eq!(response.modes.as_ref().unwrap().current_mode_id.0.as_ref(), "code");
                assert_eq!(response.modes.as_ref().unwrap().available_modes.len(), 3);
                assert_eq!(
                    response.models.as_ref().unwrap().current_model_id.0.as_ref(),
                    "test-model"
                );
                assert_eq!(response.models.as_ref().unwrap().available_models.len(), 2);
                let config_ids: Vec<&str> = response
                    .config_options
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|option| option.id.0.as_ref())
                    .collect();
                assert_eq!(config_ids, vec![MODE_CONFIG_ID, MODEL_CONFIG_ID]);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_and_resume_update_cwd_from_request() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let (provider, _requested_models) = immediate_provider("unused");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                agent
                    .load_session(acp::LoadSessionRequest::new(
                        session_id.clone(),
                        "/tmp/loaded",
                    ))
                    .await
                    .unwrap();
                let stored = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(stored.cwd, std::path::PathBuf::from("/tmp/loaded"));

                agent
                    .resume_session(acp::ResumeSessionRequest::new(
                        session_id.clone(),
                        "/tmp/resumed",
                    ))
                    .await
                    .unwrap();
                let stored = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(stored.cwd, std::path::PathBuf::from("/tmp/resumed"));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_session_mode_emits_updates_and_changes_session_state() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let notifications = start_notifier_pump(rx);
                let (provider, _requested_models) = immediate_provider("unused");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                agent
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        session_id.clone(),
                        "ask",
                    ))
                    .await
                    .unwrap();

                let loaded = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(loaded.mode_id, "ask");

                let notifications = notifications.lock().unwrap();
                assert_eq!(notifications.len(), 2);
                match &notifications[0].update {
                    acp::SessionUpdate::CurrentModeUpdate(update) => {
                        assert_eq!(update.current_mode_id.0.as_ref(), "ask");
                    }
                    other => panic!("unexpected session update: {other:?}"),
                }
                match &notifications[1].update {
                    acp::SessionUpdate::ConfigOptionUpdate(update) => {
                        let mode_option = update
                            .config_options
                            .iter()
                            .find(|option| option.id.0.as_ref() == MODE_CONFIG_ID)
                            .unwrap();
                        match &mode_option.kind {
                            acp::SessionConfigKind::Select(select) => {
                                assert_eq!(select.current_value.0.as_ref(), "ask");
                            }
                            other => panic!("unexpected config kind: {other:?}"),
                        }
                    }
                    other => panic!("unexpected session update: {other:?}"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_session_model_changes_prompt_model_override() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let notifications = start_notifier_pump(rx);
                let (provider, requested_models) = immediate_provider("from backup");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                agent
                    .set_session_model(acp::SetSessionModelRequest::new(
                        session_id.clone(),
                        "backup-model",
                    ))
                    .await
                    .unwrap();
                agent
                    .prompt(acp::PromptRequest::new(
                        session_id.clone(),
                        vec!["say hi".into()],
                    ))
                    .await
                    .unwrap();

                assert_eq!(
                    requested_models.lock().unwrap().as_slice(),
                    &[Some("backup-model".to_string())]
                );
                let loaded = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(loaded.model_id, "backup-model");

                let notifications = notifications.lock().unwrap();
                assert!(matches!(
                    notifications[0].update,
                    acp::SessionUpdate::ConfigOptionUpdate(_)
                ));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_session_mode_rolls_back_when_notification_delivery_fails() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                drop(rx);
                let (provider, _requested_models) = immediate_provider("unused");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                let _err = agent
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        session_id.clone(),
                        "ask",
                    ))
                    .await
                    .unwrap_err();
                let loaded = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(loaded.mode_id, "code");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_session_model_rolls_back_when_notification_delivery_fails() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                drop(rx);
                let (provider, _requested_models) = immediate_provider("unused");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                let _err = agent
                    .set_session_model(acp::SetSessionModelRequest::new(
                        session_id.clone(),
                        "backup-model",
                    ))
                    .await
                    .unwrap_err();
                let loaded = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(loaded.model_id, "test-model");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_session_config_option_updates_model() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let (provider, _requested_models) = immediate_provider("unused");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                let response = agent
                    .set_session_config_option(acp::SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        MODEL_CONFIG_ID,
                        "backup-model",
                    ))
                    .await
                    .unwrap();

                let model_option = response
                    .config_options
                    .iter()
                    .find(|option| option.id.0.as_ref() == MODEL_CONFIG_ID)
                    .unwrap();
                match &model_option.kind {
                    acp::SessionConfigKind::Select(select) => {
                        assert_eq!(select.current_value.0.as_ref(), "backup-model");
                    }
                    other => panic!("unexpected config kind: {other:?}"),
                }

                let resumed = agent
                    .resume_session(acp::ResumeSessionRequest::new(
                        session_id,
                        std::env::temp_dir(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    resumed.models.unwrap().current_model_id.0.as_ref(),
                    "backup-model"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_session_clones_history_mode_model_and_uses_new_cwd() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let (provider, _requested_models) = immediate_provider("hello");
                let agent = agent_with_provider(Arc::new(provider), notifier);

                let session_id = new_session(&agent).await;
                agent
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        session_id.clone(),
                        "architect",
                    ))
                    .await
                    .unwrap();
                agent
                    .set_session_model(acp::SetSessionModelRequest::new(
                        session_id.clone(),
                        "backup-model",
                    ))
                    .await
                    .unwrap();
                agent
                    .prompt(acp::PromptRequest::new(
                        session_id.clone(),
                        vec!["say hi".into()],
                    ))
                    .await
                    .unwrap();

                let forked = agent
                    .fork_session(acp::ForkSessionRequest::new(
                        session_id.clone(),
                        std::env::temp_dir().join("forked"),
                    ))
                    .await
                    .unwrap();

                assert_eq!(forked.modes.unwrap().current_mode_id.0.as_ref(), "architect");
                assert_eq!(
                    forked.models.unwrap().current_model_id.0.as_ref(),
                    "backup-model"
                );

                let source_snapshot = agent
                    .sessions
                    .load_session(session_id.0.as_ref(), None)
                    .await
                    .unwrap();
                let forked_snapshot = agent
                    .sessions
                    .load_session(forked.session_id.0.as_ref(), None)
                    .await
                    .unwrap();

                assert_eq!(source_snapshot.history.len(), forked_snapshot.history.len());
                assert_eq!(forked_snapshot.mode_id, "architect");
                assert_eq!(forked_snapshot.model_id, "backup-model");
                assert!(forked_snapshot.cwd.ends_with("forked"));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_ignores_unexpected_tool_calls_when_no_tools_were_offered() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let notifications = start_notifier_pump(rx);
                let agent = agent_with_provider(
                    Arc::new(ImmediateProvider {
                        response: response_with_unexpected_tool_call(Some("plain reply")),
                        requested_models: Arc::new(StdMutex::new(Vec::new())),
                    }),
                    notifier,
                );

                let session_id = new_session(&agent).await;
                let response = agent
                    .prompt(acp::PromptRequest::new(session_id, vec!["hello".into()]))
                    .await
                    .unwrap();

                assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
                let notifications = notifications.lock().unwrap();
                assert_eq!(notifications.len(), 1);
                match &notifications[0].update {
                    acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                        acp::ContentBlock::Text(text) => assert_eq!(text.text, "plain reply"),
                        other => panic!("unexpected content block: {other:?}"),
                    },
                    other => panic!("unexpected session update: {other:?}"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_errors_when_no_tools_were_offered_and_no_content_is_available() {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async {
                let (notifier, rx) = session_notifier_channel();
                let _notifications = start_notifier_pump(rx);
                let agent = agent_with_provider(
                    Arc::new(ImmediateProvider {
                        response: response_with_unexpected_tool_call(None),
                        requested_models: Arc::new(StdMutex::new(Vec::new())),
                    }),
                    notifier,
                );

                let session_id = new_session(&agent).await;
                let err = agent
                    .prompt(acp::PromptRequest::new(session_id, vec!["hello".into()]))
                    .await
                    .unwrap_err();

                assert!(err.message.contains("model returned tool calls when no tools were offered"));
            })
            .await;
    }
}
