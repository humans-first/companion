use std::sync::Arc;

use agent_client_protocol::{self as acp};
use serde_json::value::RawValue;
use tracing::{debug, info};

use crate::config::Strategy;
use crate::error;
use crate::pool::AgentPool;
use crate::service::GatewayService;

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
    service: GatewayService,
    frontend_id: String,
}

impl GatewayAgent {
    pub fn new(service: GatewayService, frontend_id: impl Into<String>) -> Self {
        Self {
            service,
            frontend_id: frontend_id.into(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for GatewayAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        // Store for replay to future Dedicated backends.
        let pool = self.service.pool();
        pool.store_init_request(args.clone());

        let slots = pool.alive_slots();

        if slots.is_empty() {
            if pool.strategy() == Strategy::Dedicated {
                // Spawn one backend, initialize it for real, assign it to first session.
                info!("dedicated mode: spawning first backend for initialization");
                pool.spawn_backend_process(&self.service, 0).await.map_err(|e| {
                    acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
                })?;
                let conn = pool.backend_conn(0)?;
                return conn.initialize(args).await;
            }
            return Err(acp::Error::internal_error().data("no alive backends"));
        }

        info!("scatter-initializing all backends");

        // Scatter: send initialize to all alive backends.
        let mut futs = Vec::new();
        for slot_idx in &slots {
            let conn = pool.backend_conn(*slot_idx)?;
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
        let pool = self.service.pool();
        let slots = pool.alive_slots();
        let slot_idx = slots.first().ok_or_else(acp::Error::internal_error)?;
        pool.backend_conn(*slot_idx)?.authenticate(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let pool = self.service.pool();
        let cwd = args.cwd.clone();
        let slot_idx = pool.select_slot()?;

        // For Dedicated mode, spawn a backend if the slot is empty.
        // (The first backend may already be populated from initialize.)
        if pool.strategy() == Strategy::Dedicated && !pool.is_slot_populated(slot_idx) {
            pool.spawn_dedicated_backend(&self.service, slot_idx).await.map_err(|e| {
                acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
            })?;
        }

        let conn = pool.backend_conn(slot_idx)?;
        let mut response = conn.new_session(args).await?;

        let external_sid = AgentPool::new_external_sid();
        let backend_sid = response.session_id.0.to_string();
        pool.register_session_for_sender(
            &external_sid,
            &backend_sid,
            slot_idx,
            cwd,
            &self.frontend_id,
        );
        response.session_id = acp::SessionId::new(external_sid);
        Ok(response)
    }

    async fn load_session(
        &self,
        mut args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let pool = self.service.pool();
        let external_sid = args.session_id.0.to_string();
        pool.record_sender(&external_sid, &self.frontend_id);

        let (conn, backend_sid) = match pool.backend_for_session(&external_sid) {
            Ok(pair) => pair,
            Err(_) => {
                // Check if evicted — reload transparently.
                if let Some(evicted) = pool.take_evicted(&external_sid) {
                    info!(external_sid = %external_sid, "transparently reloading evicted session via load_session");
                    let slot_idx = pool.select_slot()?;

                    if pool.strategy() == Strategy::Dedicated && !pool.is_slot_populated(slot_idx) {
                        pool.spawn_dedicated_backend(&self.service, slot_idx).await.map_err(|e| {
                            acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
                        })?;
                    }

                    let conn = pool.backend_conn(slot_idx)?;
                    let resp = conn
                        .load_session(acp::LoadSessionRequest::new(
                            acp::SessionId::new(evicted.backend_sid.as_str()),
                            evicted.cwd.clone(),
                        ))
                        .await?;
                    pool.register_session_for_sender(
                        &external_sid,
                        &evicted.backend_sid,
                        slot_idx,
                        evicted.cwd,
                        &self.frontend_id,
                    );
                    return Ok(resp);
                }
                return Err(error::unknown_session(&external_sid));
            }
        };

        args.session_id = backend_sid;
        conn.load_session(args).await
    }

    async fn prompt(
        &self,
        mut args: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let pool = self.service.pool();
        let external_sid = args.session_id.0.to_string();

        // Cedar authorization.
        if let Some(ref policy) = self.service.policy() {
            policy.authorize(&args.meta)?;
        }

        // Prompt guard.
        pool.record_sender(&external_sid, &self.frontend_id);
        pool.begin_prompt_for_sender(&external_sid, &self.frontend_id)?;
        let _guard = PromptGuard {
            pool: pool.clone(),
            external_sid: external_sid.clone(),
        };

        // Try to resolve session; if evicted, reload transparently.
        let (conn, backend_sid) = match pool.backend_for_session(&external_sid) {
            Ok(pair) => pair,
            Err(_) => {
                // Check if evicted.
                if let Some(evicted) = pool.take_evicted(&external_sid) {
                    info!(external_sid = %external_sid, "transparently reloading evicted session");
                    let slot_idx = pool.select_slot()?;

                    // For Dedicated mode, spawn a backend if the slot is empty.
                    if pool.strategy() == Strategy::Dedicated && !pool.is_slot_populated(slot_idx) {
                        pool.spawn_dedicated_backend(&self.service, slot_idx).await.map_err(|e| {
                            acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
                        })?;
                    }

                    let conn = pool.backend_conn(slot_idx)?;
                    let _load_resp = conn
                        .load_session(acp::LoadSessionRequest::new(
                            acp::SessionId::new(evicted.backend_sid.as_str()),
                            evicted.cwd.clone(),
                        ))
                        .await?;
                    pool.register_session_for_sender(
                        &external_sid,
                        &evicted.backend_sid,
                        slot_idx,
                        evicted.cwd,
                        &self.frontend_id,
                    );
                    (conn, acp::SessionId::new(evicted.backend_sid.as_str()))
                } else {
                    return Err(error::unknown_session(&external_sid));
                }
            }
        };

        pool.touch_session(&external_sid);
        debug!(external = %external_sid, backend = %backend_sid.0, "forwarding prompt");
        args.session_id = backend_sid;
        conn.prompt(args).await
    }

    async fn cancel(&self, mut args: acp::CancelNotification) -> Result<(), acp::Error> {
        let pool = self.service.pool();
        pool.record_sender(&args.session_id.0, &self.frontend_id);
        let (conn, backend_sid) = pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.cancel(args).await
    }

    async fn set_session_mode(
        &self,
        mut args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        let pool = self.service.pool();
        pool.record_sender(&args.session_id.0, &self.frontend_id);
        let (conn, backend_sid) = pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.set_session_mode(args).await
    }

    async fn set_session_config_option(
        &self,
        mut args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        let pool = self.service.pool();
        pool.record_sender(&args.session_id.0, &self.frontend_id);
        let (conn, backend_sid) = pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.set_session_config_option(args).await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        info!("scatter-listing sessions from all backends");
        let pool = self.service.pool();
        let slots = pool.alive_slots();
        if slots.is_empty() {
            return Ok(acp::ListSessionsResponse::new(vec![]));
        }

        let mut futs = Vec::new();
        for slot_idx in &slots {
            let conn = pool.backend_conn(*slot_idx)?;
            futs.push(conn.list_sessions(args.clone()));
        }

        let results = futures::future::join_all(futs).await;
        let mut all_sessions = Vec::new();
        for mut resp in results.into_iter().flatten() {
            for session in &mut resp.sessions {
                session.session_id = pool.to_external(&session.session_id);
            }
            all_sessions.extend(resp.sessions);
        }
        Ok(acp::ListSessionsResponse::new(all_sessions))
    }

    #[cfg(feature = "unstable_session_model")]
    async fn set_session_model(
        &self,
        mut args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let pool = self.service.pool();
        pool.record_sender(&args.session_id.0, &self.frontend_id);
        let (conn, backend_sid) = pool.backend_for_session(&args.session_id.0)?;
        args.session_id = backend_sid;
        conn.set_session_model(args).await
    }

    #[cfg(feature = "unstable_session_fork")]
    async fn fork_session(
        &self,
        mut args: acp::ForkSessionRequest,
    ) -> Result<acp::ForkSessionResponse, acp::Error> {
        let pool = self.service.pool();
        let external_sid = args.session_id.0.to_string();
        let cwd = args.cwd.clone();
        pool.record_sender(&external_sid, &self.frontend_id);
        let (conn, backend_sid) = pool.backend_for_session(&external_sid)?;
        let (_, slot_index, _) = pool.resolve_session(&external_sid)
            .ok_or_else(|| error::unknown_session(&external_sid))?;
        args.session_id = backend_sid;
        let mut response = conn.fork_session(args).await?;

        // Register the forked session with a new external ID on the same slot.
        let new_external_sid = AgentPool::new_external_sid();
        let new_backend_sid = response.session_id.0.to_string();
        pool.register_session_for_sender(
            &new_external_sid,
            &new_backend_sid,
            slot_index,
            cwd,
            &self.frontend_id,
        );
        response.session_id = acp::SessionId::new(new_external_sid);
        Ok(response)
    }

    #[cfg(feature = "unstable_session_resume")]
    async fn resume_session(
        &self,
        mut args: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        let pool = self.service.pool();
        let external_sid = args.session_id.0.to_string();
        pool.record_sender(&external_sid, &self.frontend_id);

        let (conn, backend_sid) = match pool.backend_for_session(&external_sid) {
            Ok(pair) => pair,
            Err(_) => {
                // Check if evicted — reload transparently.
                if let Some(evicted) = pool.take_evicted(&external_sid) {
                    info!(external_sid = %external_sid, "transparently reloading evicted session via resume_session");
                    let slot_idx = pool.select_slot()?;
                    if pool.strategy() == Strategy::Dedicated && !pool.is_slot_populated(slot_idx) {
                        pool.spawn_dedicated_backend(&self.service, slot_idx).await.map_err(|e| {
                            acp::Error::internal_error().data(format!("failed to spawn backend: {e}"))
                        })?;
                    }
                    let conn = pool.backend_conn(slot_idx)?;
                    let resp = conn
                        .resume_session(acp::ResumeSessionRequest::new(
                            acp::SessionId::new(evicted.backend_sid.as_str()),
                            evicted.cwd.clone(),
                        ))
                        .await?;
                    pool.register_session_for_sender(
                        &external_sid,
                        &evicted.backend_sid,
                        slot_idx,
                        evicted.cwd,
                        &self.frontend_id,
                    );
                    return Ok(resp);
                }
                return Err(error::unknown_session(&external_sid));
            }
        };

        args.session_id = backend_sid;
        conn.resume_session(args).await
    }

    #[cfg(feature = "unstable_session_close")]
    async fn close_session(
        &self,
        mut args: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, acp::Error> {
        let pool = self.service.pool();
        let external_sid = args.session_id.0.to_string();
        pool.record_sender(&external_sid, &self.frontend_id);
        let (conn, backend_sid) = pool.backend_for_session(&external_sid)?;
        args.session_id = backend_sid;
        let response = conn.close_session(args).await?;
        pool.remove_session(&external_sid);
        Ok(response)
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        // Handle gateway-specific extension methods.
        if args.method.as_ref() == "gateway/status" {
            let status = self.service.pool().pool_status();
            let json = status.to_string();
            let raw: Arc<RawValue> = RawValue::from_string(json)
                .map_err(|_| acp::Error::internal_error())?
                .into();
            return Ok(acp::ExtResponse::new(raw));
        }

        let pool = self.service.pool();
        let slots = pool.alive_slots();
        let slot_idx = slots.first().ok_or_else(acp::Error::internal_error)?;
        pool.backend_conn(*slot_idx)?.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), acp::Error> {
        let pool = self.service.pool();
        let slots = pool.alive_slots();
        let slot_idx = slots.first().ok_or_else(acp::Error::internal_error)?;
        pool.backend_conn(*slot_idx)?.ext_notification(args).await
    }
}

// ---------------------------------------------------------------------------
// GatewayClient: implements acp::Client, faces the backend agent
// ---------------------------------------------------------------------------

pub struct GatewayClient {
    service: GatewayService,
}

impl GatewayClient {
    pub fn new(service: GatewayService) -> Self {
        Self { service }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for GatewayClient {
    async fn request_permission(
        &self,
        mut args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.request_permission(args).await
    }

    async fn session_notification(&self, mut args: acp::SessionNotification) -> acp::Result<()> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.session_notification(args).await
    }

    async fn read_text_file(
        &self,
        mut args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.read_text_file(args).await
    }

    async fn write_text_file(
        &self,
        mut args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.write_text_file(args).await
    }

    async fn create_terminal(
        &self,
        mut args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.create_terminal(args).await
    }

    async fn terminal_output(
        &self,
        mut args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.terminal_output(args).await
    }

    async fn release_terminal(
        &self,
        mut args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.release_terminal(args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        mut args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.wait_for_terminal_exit(args).await
    }

    async fn kill_terminal(
        &self,
        mut args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        let (conn, external_sid) = self.service.frontend_for_backend_session(&args.session_id.0)?;
        args.session_id = external_sid;
        conn.kill_terminal(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        self.service.frontend_for_unscoped_callback()?.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        self.service.frontend_for_unscoped_callback()?.ext_notification(args).await
    }
}
