use agent_client_protocol::{self as acp};
use tracing::{debug, info};

use crate::config::Strategy;
use crate::error;
use crate::policy::PolicyEngine;
use crate::pool::AgentPool;

// ---------------------------------------------------------------------------
// Prompt guard — RAII drop guard that clears in_flight_prompts on drop.
// ---------------------------------------------------------------------------

struct PromptGuard {
    pool: AgentPool,
    external_sid: String,
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.pool.end_prompt(&self.external_sid);
    }
}

// ---------------------------------------------------------------------------
// GatewayAgent: implements acp::Agent, faces the upstream client (e.g. Zed)
// ---------------------------------------------------------------------------

pub struct GatewayAgent {
    pool: AgentPool,
    policy: Option<PolicyEngine>,
}

impl GatewayAgent {
    pub fn new(pool: AgentPool, policy: Option<PolicyEngine>) -> Self {
        Self { pool, policy }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for GatewayAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        // Store for replay to future Dedicated backends.
        self.pool.store_init_request(args.clone());

        let slots = self.pool.alive_slots();

        if slots.is_empty() {
            if self.pool.strategy() == Strategy::Dedicated {
                // Spawn one backend, initialize it for real, assign it to first session.
                info!("dedicated mode: spawning first backend for initialization");
                self.pool.spawn_backend_process(0).await.map_err(|e| {
                    acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
                })?;
                let conn = self.pool.backend_conn(0)?;
                return conn.initialize(args).await;
            }
            return Err(acp::Error::internal_error().data("no alive backends"));
        }

        info!("scatter-initializing all backends");

        // Scatter: send initialize to all alive backends.
        let mut futs = Vec::new();
        for slot_idx in &slots {
            let conn = self.pool.backend_conn(*slot_idx)?;
            futs.push(conn.initialize(args.clone()));
        }

        let results = futures::future::join_all(futs).await;

        // Return first success, or last error.
        let mut last_err = None;
        for result in results {
            match result {
                Ok(resp) => return Ok(resp),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(acp::Error::internal_error))
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        debug!("forwarding authenticate");
        // Send to first alive backend.
        let slots = self.pool.alive_slots();
        let slot_idx = slots.first().ok_or_else(acp::Error::internal_error)?;
        self.pool.backend_conn(*slot_idx)?.authenticate(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let cwd = args.cwd.clone();
        let slot_idx = self.pool.select_slot()?;

        // For Dedicated mode, spawn a backend if the slot is empty.
        // (The first backend may already be populated from initialize.)
        if self.pool.strategy() == Strategy::Dedicated && !self.pool.is_slot_populated(slot_idx) {
            self.pool.spawn_dedicated_backend(slot_idx).await.map_err(|e| {
                acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
            })?;
        }

        let conn = self.pool.backend_conn(slot_idx)?;
        let mut response = conn.new_session(args).await?;

        let external_sid = AgentPool::new_external_sid();
        let backend_sid = response.session_id.0.to_string();
        self.pool.register_session(&external_sid, &backend_sid, slot_idx, cwd);
        response.session_id = acp::SessionId::new(external_sid);
        Ok(response)
    }

    async fn load_session(
        &self,
        mut args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let (conn, backend_sid) = self.pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.load_session(args).await
    }

    async fn prompt(
        &self,
        mut args: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let external_sid = args.session_id.0.to_string();

        // Cedar authorization.
        if let Some(ref policy) = self.policy {
            policy.authorize(&args.meta)?;
        }

        // Prompt guard.
        self.pool.begin_prompt(&external_sid)?;
        let _guard = PromptGuard {
            pool: self.pool.clone(),
            external_sid: external_sid.clone(),
        };

        // Try to resolve session; if evicted, reload transparently.
        let (conn, backend_sid) = match self.pool.backend_for_session(&external_sid) {
            Ok(pair) => pair,
            Err(_) => {
                // Check if evicted.
                if let Some(evicted) = self.pool.take_evicted(&external_sid) {
                    info!(external_sid = %external_sid, "transparently reloading evicted session");
                    let slot_idx = self.pool.select_slot()?;

                    // For Dedicated mode, spawn a backend if the slot is empty.
                    if self.pool.strategy() == Strategy::Dedicated && !self.pool.is_slot_populated(slot_idx) {
                        self.pool.spawn_dedicated_backend(slot_idx).await.map_err(|e| {
                            acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
                        })?;
                    }

                    let conn = self.pool.backend_conn(slot_idx)?;
                    let _load_resp = conn
                        .load_session(acp::LoadSessionRequest::new(
                            acp::SessionId::new(evicted.backend_sid.as_str()),
                            evicted.cwd.clone(),
                        ))
                        .await?;
                    self.pool.register_session(&external_sid, &evicted.backend_sid, slot_idx, evicted.cwd);
                    (conn, acp::SessionId::new(evicted.backend_sid.as_str()))
                } else {
                    return Err(error::unknown_session(&external_sid));
                }
            }
        };

        self.pool.touch_session(&external_sid);
        debug!(external = %external_sid, backend = %backend_sid.0, "forwarding prompt");
        args.session_id = backend_sid;
        conn.prompt(args).await
    }

    async fn cancel(&self, mut args: acp::CancelNotification) -> Result<(), acp::Error> {
        let (conn, backend_sid) = self.pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.cancel(args).await
    }

    async fn set_session_mode(
        &self,
        mut args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        let (conn, backend_sid) = self.pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.set_session_mode(args).await
    }

    async fn set_session_config_option(
        &self,
        mut args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        let (conn, backend_sid) = self.pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.set_session_config_option(args).await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        info!("scatter-listing sessions from all backends");
        let slots = self.pool.alive_slots();
        if slots.is_empty() {
            return Ok(acp::ListSessionsResponse::new(vec![]));
        }

        let mut futs = Vec::new();
        for slot_idx in &slots {
            let conn = self.pool.backend_conn(*slot_idx)?;
            futs.push(conn.list_sessions(args.clone()));
        }

        let results = futures::future::join_all(futs).await;
        let mut all_sessions = Vec::new();
        for result in results {
            if let Ok(mut resp) = result {
                for session in &mut resp.sessions {
                    session.session_id = self.pool.to_external(&session.session_id);
                }
                all_sessions.extend(resp.sessions);
            }
        }
        Ok(acp::ListSessionsResponse::new(all_sessions))
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        let slots = self.pool.alive_slots();
        let slot_idx = slots.first().ok_or_else(acp::Error::internal_error)?;
        self.pool.backend_conn(*slot_idx)?.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), acp::Error> {
        let slots = self.pool.alive_slots();
        let slot_idx = slots.first().ok_or_else(acp::Error::internal_error)?;
        self.pool.backend_conn(*slot_idx)?.ext_notification(args).await
    }
}

// ---------------------------------------------------------------------------
// GatewayClient: implements acp::Client, faces the backend agent
// ---------------------------------------------------------------------------

pub struct GatewayClient {
    pool: AgentPool,
}

impl GatewayClient {
    pub fn new(pool: AgentPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for GatewayClient {
    async fn request_permission(
        &self,
        mut args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.request_permission(args).await
    }

    async fn session_notification(&self, mut args: acp::SessionNotification) -> acp::Result<()> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.session_notification(args).await
    }

    async fn read_text_file(
        &self,
        mut args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.read_text_file(args).await
    }

    async fn write_text_file(
        &self,
        mut args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.write_text_file(args).await
    }

    async fn create_terminal(
        &self,
        mut args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.create_terminal(args).await
    }

    async fn terminal_output(
        &self,
        mut args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.terminal_output(args).await
    }

    async fn release_terminal(
        &self,
        mut args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.release_terminal(args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        mut args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.wait_for_terminal_exit(args).await
    }

    async fn kill_terminal(
        &self,
        mut args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        args.session_id = self.pool.to_external(&args.session_id);
        self.pool.upstream_conn()?.kill_terminal(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        self.pool.upstream_conn()?.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        self.pool.upstream_conn()?.ext_notification(args).await
    }
}
