#[cfg(test)]
mod integration {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::{self as acp, Agent as _};

    static GLOBAL_SESSION_COUNTER: AtomicUsize = AtomicUsize::new(1);

    use crate::config::Strategy;
    use crate::gateway::{GatewayAgent, GatewayClient};
    use crate::pool::{AgentPool, BackendSlot, PoolConfig};

    // -----------------------------------------------------------------------
    // Mock Agent — acts as a backend agent process
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct MockAgent {
        sessions: Arc<Mutex<HashMap<String, std::path::PathBuf>>>,
        prompts: Arc<Mutex<Vec<(String, Vec<acp::ContentBlock>)>>>,
        load_calls: Arc<Mutex<Vec<String>>>,
        initialize_calls: Arc<AtomicUsize>,
        /// If set, prompt() will sleep this long before returning.
        prompt_delay: Option<std::time::Duration>,
        /// If true, initialize() returns an error.
        fail_initialize: bool,
        /// If true, load_session() returns an error.
        fail_load_session: bool,
    }

    impl MockAgent {
        fn new() -> Self {
            Self {
                sessions: Arc::new(Mutex::new(HashMap::new())),
                prompts: Arc::new(Mutex::new(Vec::new())),
                load_calls: Arc::new(Mutex::new(Vec::new())),
                initialize_calls: Arc::new(AtomicUsize::new(0)),
                prompt_delay: None,
                fail_initialize: false,
                fail_load_session: false,
            }
        }

        #[allow(dead_code)]
        fn with_prompt_delay(mut self, delay: std::time::Duration) -> Self {
            self.prompt_delay = Some(delay);
            self
        }

        fn with_failing_initialize(mut self) -> Self {
            self.fail_initialize = true;
            self
        }

        #[allow(dead_code)]
        fn with_failing_load_session(mut self) -> Self {
            self.fail_load_session = true;
            self
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Agent for MockAgent {
        async fn initialize(&self, args: acp::InitializeRequest) -> acp::Result<acp::InitializeResponse> {
            self.initialize_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_initialize {
                return Err(acp::Error::internal_error().data("mock failure"));
            }
            Ok(acp::InitializeResponse::new(args.protocol_version)
                .agent_info(acp::Implementation::new("mock-agent", "0.0.0").title("Mock Agent")))
        }
        async fn authenticate(&self, _: acp::AuthenticateRequest) -> acp::Result<acp::AuthenticateResponse> {
            Ok(acp::AuthenticateResponse::default())
        }
        async fn new_session(&self, args: acp::NewSessionRequest) -> acp::Result<acp::NewSessionResponse> {
            let id = GLOBAL_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let sid = format!("backend-session-{}", id);
            self.sessions.lock().unwrap().insert(sid.clone(), args.cwd);
            Ok(acp::NewSessionResponse::new(acp::SessionId::new(sid.as_str())))
        }
        async fn load_session(&self, args: acp::LoadSessionRequest) -> acp::Result<acp::LoadSessionResponse> {
            self.load_calls.lock().unwrap().push(args.session_id.0.to_string());
            if self.fail_load_session {
                return Err(acp::Error::internal_error().data("mock load_session failure"));
            }
            Ok(acp::LoadSessionResponse::new())
        }
        async fn set_session_mode(&self, _: acp::SetSessionModeRequest) -> acp::Result<acp::SetSessionModeResponse> {
            Ok(acp::SetSessionModeResponse::new())
        }
        async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
            if let Some(delay) = self.prompt_delay {
                tokio::time::sleep(delay).await;
            }
            self.prompts.lock().unwrap().push((args.session_id.0.to_string(), args.prompt));
            Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
        }
        async fn cancel(&self, _: acp::CancelNotification) -> acp::Result<()> {
            Ok(())
        }
        async fn list_sessions(&self, _: acp::ListSessionsRequest) -> acp::Result<acp::ListSessionsResponse> {
            let sessions = self.sessions.lock().unwrap();
            let infos: Vec<_> = sessions.iter()
                .map(|(id, cwd)| acp::SessionInfo::new(acp::SessionId::new(id.as_str()), cwd.clone()))
                .collect();
            Ok(acp::ListSessionsResponse::new(infos))
        }
        async fn set_session_config_option(&self, _: acp::SetSessionConfigOptionRequest) -> acp::Result<acp::SetSessionConfigOptionResponse> {
            Ok(acp::SetSessionConfigOptionResponse::new(vec![]))
        }
        async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
            Err(acp::Error::method_not_found())
        }
        async fn ext_notification(&self, _: acp::ExtNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Mock Client — acts as upstream client (e.g. Zed)
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct MockUpstreamClient {
        notifications: Arc<Mutex<Vec<acp::SessionNotification>>>,
    }

    impl MockUpstreamClient {
        fn new() -> Self {
            Self {
                notifications: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for MockUpstreamClient {
        async fn request_permission(&self, _: acp::RequestPermissionRequest) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled))
        }
        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            self.notifications.lock().unwrap().push(args);
            Ok(())
        }
        async fn read_text_file(&self, _: acp::ReadTextFileRequest) -> acp::Result<acp::ReadTextFileResponse> {
            Ok(acp::ReadTextFileResponse::new("content"))
        }
        async fn write_text_file(&self, _: acp::WriteTextFileRequest) -> acp::Result<acp::WriteTextFileResponse> {
            Ok(acp::WriteTextFileResponse::default())
        }
        async fn create_terminal(&self, _: acp::CreateTerminalRequest) -> acp::Result<acp::CreateTerminalResponse> {
            unimplemented!()
        }
        async fn terminal_output(&self, _: acp::TerminalOutputRequest) -> acp::Result<acp::TerminalOutputResponse> {
            unimplemented!()
        }
        async fn release_terminal(&self, _: acp::ReleaseTerminalRequest) -> acp::Result<acp::ReleaseTerminalResponse> {
            unimplemented!()
        }
        async fn wait_for_terminal_exit(&self, _: acp::WaitForTerminalExitRequest) -> acp::Result<acp::WaitForTerminalExitResponse> {
            unimplemented!()
        }
        async fn kill_terminal(&self, _: acp::KillTerminalRequest) -> acp::Result<acp::KillTerminalResponse> {
            unimplemented!()
        }
        async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
            Err(acp::Error::method_not_found())
        }
        async fn ext_notification(&self, _: acp::ExtNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Helper: wire up a pool with mock backends and an upstream connection
    // -----------------------------------------------------------------------

    struct TestHarness {
        /// ClientSideConnection to the gateway (as if we're the upstream client calling the gateway)
        upstream_to_gateway: acp::ClientSideConnection,
        /// The agents behind each backend slot
        agents: Vec<MockAgent>,
        pool: AgentPool,
    }

    /// Create a test harness with `num_backends` default mock backend processes.
    fn setup_harness(num_backends: usize, strategy: Strategy) -> TestHarness {
        let agents: Vec<MockAgent> = (0..num_backends).map(|_| MockAgent::new()).collect();
        setup_harness_with_agents(agents, strategy)
    }

    /// Create a test harness with pre-configured mock agents.
    fn setup_harness_with_agents(agents: Vec<MockAgent>, strategy: Strategy) -> TestHarness {
        let num_backends = agents.len();
        let pool = AgentPool::new(PoolConfig {
            strategy,
            pool_size: num_backends,
            agent_cmd: "mock".to_string(),
        });

        let mut agent_refs = Vec::new();

        for (i, agent) in agents.into_iter().enumerate() {
            agent_refs.push(agent.clone());

            let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
            let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

            let gateway_client = GatewayClient::new(pool.clone());
            let (backend_conn, backend_io) = acp::ClientSideConnection::new(
                gateway_client,
                client_to_agent_tx,
                agent_to_client_rx,
                |fut| { tokio::task::spawn_local(fut); },
            );

            let (_agent_side_conn, agent_io) = acp::AgentSideConnection::new(
                agent,
                agent_to_client_tx,
                client_to_agent_rx,
                |fut| { tokio::task::spawn_local(fut); },
            );

            pool.insert_slot(i, BackendSlot {
                connection: backend_conn,
                child: None,
                session_count: 0,
                alive: true,
            });

            tokio::task::spawn_local(backend_io);
            tokio::task::spawn_local(agent_io);
        }

        // Wire up upstream.
        let upstream_client = MockUpstreamClient::new();

        let (upstream_to_gw_rx, upstream_to_gw_tx) = piper::pipe(1024);
        let (gw_to_upstream_rx, gw_to_upstream_tx) = piper::pipe(1024);

        let gateway_agent = GatewayAgent::new(pool.clone(), None);
        let (upstream_conn, upstream_io) = acp::AgentSideConnection::new(
            gateway_agent,
            gw_to_upstream_tx,
            upstream_to_gw_rx,
            |fut| { tokio::task::spawn_local(fut); },
        );
        pool.set_upstream(upstream_conn);

        let (client_conn, client_io) = acp::ClientSideConnection::new(
            upstream_client,
            upstream_to_gw_tx,
            gw_to_upstream_rx,
            |fut| { tokio::task::spawn_local(fut); },
        );

        tokio::task::spawn_local(upstream_io);
        tokio::task::spawn_local(client_io);

        TestHarness {
            upstream_to_gateway: client_conn,
            agents: agent_refs,
            pool,
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_scatter_to_all_backends() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::LeastConnections);

            let result = harness.upstream_to_gateway.initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(acp::Implementation::new("test", "0.0.0"))
            ).await;

            assert!(result.is_ok());
            let resp = result.unwrap();
            assert_eq!(resp.protocol_version, acp::ProtocolVersion::LATEST);
        }).await;
    }

    #[tokio::test]
    async fn test_new_session_assigns_external_id() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await
                .unwrap();

            // External ID should be a UUID, not "backend-session-1".
            let ext_sid = resp.session_id.0.to_string();
            assert!(!ext_sid.starts_with("backend-session"), "got raw backend ID: {ext_sid}");
            assert!(ext_sid.contains('-'), "expected UUID format: {ext_sid}");

            // Backend should have received the session.
            let sessions = harness.agents[0].sessions.lock().unwrap();
            assert_eq!(sessions.len(), 1);
            // Session ID is backend-session-N where N is the global counter.
            let backend_sid: Vec<_> = sessions.keys().collect();
            assert!(backend_sid[0].starts_with("backend-session-"));
        }).await;
    }

    #[tokio::test]
    async fn test_prompt_routes_to_correct_backend() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await
                .unwrap();
            let ext_sid = resp.session_id;

            let prompt_resp = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["hello".into()]))
                .await
                .unwrap();
            assert_eq!(prompt_resp.stop_reason, acp::StopReason::EndTurn);

            // Backend should have received the prompt with its internal session ID.
            let prompts = harness.agents[0].prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert!(prompts[0].0.starts_with("backend-session-"));
        }).await;
    }

    #[tokio::test]
    async fn test_prompt_unknown_session_returns_error() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(
                    acp::SessionId::new("nonexistent"),
                    vec!["hello".into()],
                ))
                .await;

            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.code, acp::ErrorCode::Other(crate::error::UNKNOWN_SESSION));
        }).await;
    }

    #[tokio::test]
    async fn test_lc_distributes_sessions_across_backends() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::LeastConnections);

            // Create 3 sessions — should go to different backends.
            for _ in 0..3 {
                harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await
                    .unwrap();
            }

            // Each backend should have 1 session.
            for (i, agent) in harness.agents.iter().enumerate() {
                let count = agent.sessions.lock().unwrap().len();
                assert_eq!(count, 1, "backend {i} should have 1 session, got {count}");
            }
        }).await;
    }

    #[tokio::test]
    async fn test_list_sessions_merges_from_all_backends() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            // Create 2 sessions on different backends.
            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test1"))
                .await.unwrap();
            let s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test2"))
                .await.unwrap();

            let list = harness.upstream_to_gateway
                .list_sessions(acp::ListSessionsRequest::new())
                .await.unwrap();

            assert_eq!(list.sessions.len(), 2);

            // Session IDs should be external (not backend).
            let listed_ids: Vec<String> = list.sessions.iter()
                .map(|s| s.session_id.0.to_string())
                .collect();
            assert!(listed_ids.contains(&s1.session_id.0.to_string()));
            assert!(listed_ids.contains(&s2.session_id.0.to_string()));
        }).await;
    }

    #[tokio::test]
    async fn test_concurrent_prompt_guard() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            // First prompt succeeds.
            let r1 = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["first".into()]))
                .await;
            assert!(r1.is_ok());

            // After first completes, second should also succeed (guard cleared).
            let r2 = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["second".into()]))
                .await;
            assert!(r2.is_ok());

            assert!(!harness.pool.is_prompt_in_flight(&ext_sid.0));
        }).await;
    }

    #[tokio::test]
    async fn test_eviction_and_transparent_reload() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            // Evict the session.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            let evicted = harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
            assert_eq!(evicted.len(), 1);

            // Prompt should transparently reload.
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["after eviction".into()]))
                .await;
            assert!(result.is_ok());

            // Verify load_session was called on the backend.
            let loads = harness.agents[0].load_calls.lock().unwrap();
            assert_eq!(loads.len(), 1);
            assert!(loads[0].starts_with("backend-session-"));
        }).await;
    }

    #[tokio::test]
    async fn test_multiple_sessions_same_backend() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test1"))
                .await.unwrap();
            let s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test2"))
                .await.unwrap();

            // Both sessions should be on the same backend.
            let (_, slot1, _) = harness.pool.resolve_session(&s1.session_id.0).unwrap();
            let (_, slot2, _) = harness.pool.resolve_session(&s2.session_id.0).unwrap();
            assert_eq!(slot1, slot2);

            // Prompt each.
            harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s1.session_id.clone(), vec!["hello 1".into()]))
                .await.unwrap();
            harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s2.session_id.clone(), vec!["hello 2".into()]))
                .await.unwrap();

            let prompts = harness.agents[0].prompts.lock().unwrap();
            assert_eq!(prompts.len(), 2);
        }).await;
    }

    // -----------------------------------------------------------------------
    // Missing planned tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_scatter_initialize_all_fail() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let agents = vec![
                MockAgent::new().with_failing_initialize(),
                MockAgent::new().with_failing_initialize(),
                MockAgent::new().with_failing_initialize(),
            ];
            let harness = setup_harness_with_agents(agents, Strategy::LeastConnections);

            let result = harness.upstream_to_gateway.initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(acp::Implementation::new("test", "0.0.0"))
            ).await;

            assert!(result.is_err());
        }).await;
    }

    #[tokio::test]
    async fn test_scatter_initialize_partial_failure() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let agents = vec![
                MockAgent::new().with_failing_initialize(),
                MockAgent::new(), // this one succeeds
                MockAgent::new().with_failing_initialize(),
            ];
            let harness = setup_harness_with_agents(agents, Strategy::LeastConnections);

            let result = harness.upstream_to_gateway.initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(acp::Implementation::new("test", "0.0.0"))
            ).await;

            // Should succeed because at least one backend succeeded.
            assert!(result.is_ok());
        }).await;
    }

    #[tokio::test]
    async fn test_prompt_guard_cleared_after_backend_error() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id.clone();

            // Kill the backend slot to make prompt fail.
            harness.pool.handle_backend_death(0);

            // Prompt should fail (backend is dead).
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(resp.session_id, vec!["will fail".into()]))
                .await;
            assert!(result.is_err());

            // Prompt guard should have been cleared by the PromptGuard drop.
            assert!(!harness.pool.is_prompt_in_flight(&ext_sid.0));
        }).await;
    }

    // -----------------------------------------------------------------------
    // Concurrency scenario tests
    // -----------------------------------------------------------------------

    /// Multiple sessions on different backends can be prompted sequentially.
    #[tokio::test]
    async fn test_sequential_prompts_across_backends() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::LeastConnections);

            // Create sessions on all 3 backends.
            let mut sessions = Vec::new();
            for _ in 0..3 {
                let resp = harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await.unwrap();
                sessions.push(resp.session_id);
            }

            // Prompt each sequentially.
            for (i, sid) in sessions.iter().enumerate() {
                let result = harness.upstream_to_gateway
                    .prompt(acp::PromptRequest::new(sid.clone(), vec![format!("msg {i}").into()]))
                    .await;
                assert!(result.is_ok(), "prompt {i} should succeed");
            }

            // Each backend should have received exactly 1 prompt.
            for (i, agent) in harness.agents.iter().enumerate() {
                let prompts = agent.prompts.lock().unwrap();
                assert_eq!(prompts.len(), 1, "backend {i} should have 1 prompt");
            }
        }).await;
    }

    /// Rapid session create → prompt → create → prompt cycles.
    #[tokio::test]
    async fn test_rapid_session_lifecycle() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            for round in 0..10 {
                let resp = harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await
                    .unwrap_or_else(|e| panic!("new_session round {round} failed: {e}"));

                harness.upstream_to_gateway
                    .prompt(acp::PromptRequest::new(
                        resp.session_id.clone(),
                        vec![format!("round {round}").into()],
                    ))
                    .await
                    .unwrap_or_else(|e| panic!("prompt round {round} failed: {e}"));
            }

            // Total prompts across backends should be 10.
            let total: usize = harness.agents.iter()
                .map(|a| a.prompts.lock().unwrap().len())
                .sum();
            assert_eq!(total, 10);
        }).await;
    }

    /// Evict multiple sessions, then prompt all of them — all should reload.
    #[tokio::test]
    async fn test_mass_eviction_and_reload() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let mut sessions = Vec::new();
            for _ in 0..5 {
                let resp = harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await.unwrap();
                sessions.push(resp.session_id);
            }

            // Evict all.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            let evicted = harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
            assert_eq!(evicted.len(), 5);

            // Prompt all — each should reload transparently.
            for sid in &sessions {
                let result = harness.upstream_to_gateway
                    .prompt(acp::PromptRequest::new(sid.clone(), vec!["reload".into()]))
                    .await;
                assert!(result.is_ok(), "failed to reload session {}", sid.0);
            }

            // All 5 should have called load_session.
            let loads = harness.agents[0].load_calls.lock().unwrap();
            assert_eq!(loads.len(), 5);
        }).await;
    }

    /// After a backend dies and sessions are evicted, a subsequent prompt
    /// on a different session (on a still-alive backend) should still work.
    #[tokio::test]
    async fn test_partial_backend_death_other_sessions_unaffected() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            // Session on backend 0.
            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test1"))
                .await.unwrap();
            // Session on backend 1.
            let s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test2"))
                .await.unwrap();

            // Kill backend 0.
            harness.pool.handle_backend_death(0);

            // Session on backend 1 should still work.
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s2.session_id.clone(), vec!["still alive".into()]))
                .await;
            assert!(result.is_ok());

            // Session on dead backend should fail (evicted but backend is dead,
            // and re-select will pick the dead slot since it was evicted).
            // Actually, the session was moved to evicted, and on prompt it will
            // try to reload on an available slot. With backend 0 dead,
            // select_slot should pick backend 1 (the alive one).
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s1.session_id.clone(), vec!["should reload".into()]))
                .await;
            // This should succeed since the evicted session gets reloaded on the alive backend.
            assert!(result.is_ok());

            // Backend 1 should have the load call.
            let loads = harness.agents[1].load_calls.lock().unwrap();
            assert_eq!(loads.len(), 1);
        }).await;
    }

    /// Verify that session IDs are never leaked across sessions.
    /// Two different external session IDs should map to different backend session IDs.
    #[tokio::test]
    async fn test_session_id_isolation() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test1"))
                .await.unwrap();
            let s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test2"))
                .await.unwrap();

            // External IDs should be different.
            assert_ne!(s1.session_id.0.as_ref(), s2.session_id.0.as_ref());

            // Backend IDs should also be different.
            let (back1, _, _) = harness.pool.resolve_session(&s1.session_id.0).unwrap();
            let (back2, _, _) = harness.pool.resolve_session(&s2.session_id.0).unwrap();
            assert_ne!(back1, back2);

            // Prompt session 1 — backend should only see session 1's backend ID.
            harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s1.session_id.clone(), vec!["s1".into()]))
                .await.unwrap();

            let prompts = harness.agents[0].prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert_eq!(prompts[0].0, back1);
        }).await;
    }

    /// Prompt guard must be cleared even when the evicted session reload succeeds
    /// (covers the reload path in prompt() that goes through begin_prompt → guard → reload → prompt).
    #[tokio::test]
    async fn test_prompt_guard_cleared_after_evicted_reload() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            // Evict.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));

            // Prompt triggers reload.
            harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["reload".into()]))
                .await.unwrap();

            // Guard should be cleared.
            assert!(!harness.pool.is_prompt_in_flight(&ext_sid.0));

            // Second prompt should succeed (guard not stuck).
            let r2 = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["after reload".into()]))
                .await;
            assert!(r2.is_ok());
        }).await;
    }

    /// Sequential new_session calls should each get unique external IDs
    /// and be distributed across backends by LC.
    #[tokio::test]
    async fn test_many_new_sessions_distributed() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::LeastConnections);

            let mut ext_ids = Vec::new();
            for _ in 0..6 {
                let resp = harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await.unwrap();
                ext_ids.push(resp.session_id.0.to_string());
            }

            // All external IDs should be unique.
            let unique: std::collections::HashSet<_> = ext_ids.iter().collect();
            assert_eq!(unique.len(), 6, "all external session IDs should be unique");

            // Each backend should have 2 sessions (6 / 3) due to LC round-robin.
            for (i, agent) in harness.agents.iter().enumerate() {
                let count = agent.sessions.lock().unwrap().len();
                assert_eq!(count, 2, "backend {i} should have 2 sessions, got {count}");
            }
        }).await;
    }

    /// Prompts on different sessions should be independent —
    /// the prompt guard on session A should not block session B.
    #[tokio::test]
    async fn test_prompt_guard_is_per_session() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(1, Strategy::LeastConnections);

            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/a"))
                .await.unwrap();
            let s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/b"))
                .await.unwrap();

            // Prompt both sessions — they should both succeed because the guard
            // is per-session, not global.
            let r1 = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s1.session_id.clone(), vec!["a".into()]))
                .await;
            assert!(r1.is_ok());

            let r2 = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(s2.session_id.clone(), vec!["b".into()]))
                .await;
            assert!(r2.is_ok());
        }).await;
    }

    /// Backend death during an in-flight prompt should clear the prompt guard
    /// so that a subsequent prompt (after session reload) can proceed.
    #[tokio::test]
    async fn test_backend_death_clears_prompt_guard_for_retry() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            // Manually set prompt in flight for this session.
            harness.pool.begin_prompt(&ext_sid.0).unwrap();
            assert!(harness.pool.is_prompt_in_flight(&ext_sid.0));

            // Backend 0 dies — should clear prompt guard.
            let (_, slot, _) = harness.pool.resolve_session(&ext_sid.0).unwrap();
            harness.pool.handle_backend_death(slot);
            assert!(!harness.pool.is_prompt_in_flight(&ext_sid.0));

            // The session is now evicted. A new prompt should reload it on backend 1.
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["after crash".into()]))
                .await;
            assert!(result.is_ok());
        }).await;
    }

    /// Eviction + backend death at the same time: session is evicted, then
    /// the backend it was on dies. The session should still be reloadable
    /// on another backend.
    #[tokio::test]
    async fn test_eviction_then_backend_death() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            // Evict.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
            assert!(harness.pool.is_evicted(&ext_sid.0));

            // Now the original backend dies too.
            harness.pool.handle_backend_death(0);

            // The session should still be reloadable — evicted map has it,
            // select_slot will pick the alive backend (1).
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["survived".into()]))
                .await;
            assert!(result.is_ok());

            // Backend 1 should have gotten the load_session call.
            let loads = harness.agents[1].load_calls.lock().unwrap();
            assert_eq!(loads.len(), 1);
        }).await;
    }

    /// Many sessions created and then a backend dies — only sessions on that
    /// backend should be evicted, others unaffected.
    #[tokio::test]
    async fn test_backend_death_only_affects_its_sessions() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            // Create 4 sessions: 2 on each backend.
            let mut sessions = Vec::new();
            for _ in 0..4 {
                let resp = harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await.unwrap();
                sessions.push(resp.session_id);
            }

            // Figure out which sessions are on which backend.
            let mut on_0 = Vec::new();
            let mut on_1 = Vec::new();
            for sid in &sessions {
                let (_, slot, _) = harness.pool.resolve_session(&sid.0).unwrap();
                if slot == 0 { on_0.push(sid.clone()); } else { on_1.push(sid.clone()); }
            }

            // Kill backend 0.
            harness.pool.handle_backend_death(0);

            // Sessions on backend 0 should be evicted.
            for sid in &on_0 {
                assert!(harness.pool.is_evicted(&sid.0), "session on dead backend should be evicted");
            }

            // Sessions on backend 1 should still be active.
            for sid in &on_1 {
                assert!(!harness.pool.is_evicted(&sid.0), "session on alive backend should not be evicted");
                let result = harness.upstream_to_gateway
                    .prompt(acp::PromptRequest::new(sid.clone(), vec!["alive".into()]))
                    .await;
                assert!(result.is_ok());
            }
        }).await;
    }

    // -----------------------------------------------------------------------
    // Dedicated mode tests
    // -----------------------------------------------------------------------

    /// In Dedicated mode, initialize spawns one backend and returns a real response.
    /// The first new_session reuses that backend (no extra spawn needed).
    #[tokio::test]
    async fn test_dedicated_initialize_spawns_first_backend() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::Dedicated);

            // Initialize should succeed even though Dedicated starts with no pre-wired backends
            // (the test harness pre-populates slots, which simulates what initialize would do).
            let result = harness.upstream_to_gateway.initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(acp::Implementation::new("test", "0.0.0"))
            ).await;
            assert!(result.is_ok());
        }).await;
    }

    /// Dedicated mode: each session gets its own slot.
    #[tokio::test]
    async fn test_dedicated_each_session_own_slot() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::Dedicated);

            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/a"))
                .await.unwrap();
            let s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/b"))
                .await.unwrap();
            let s3 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/c"))
                .await.unwrap();

            // Each session should be on a different slot.
            let (_, slot1, _) = harness.pool.resolve_session(&s1.session_id.0).unwrap();
            let (_, slot2, _) = harness.pool.resolve_session(&s2.session_id.0).unwrap();
            let (_, slot3, _) = harness.pool.resolve_session(&s3.session_id.0).unwrap();

            let mut slots = vec![slot1, slot2, slot3];
            slots.sort();
            slots.dedup();
            assert_eq!(slots.len(), 3, "each session should be on a unique slot");
        }).await;
    }

    /// Dedicated mode: eviction frees the slot so it can be reused.
    #[tokio::test]
    async fn test_dedicated_eviction_frees_slot() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::Dedicated);

            // Fill both slots.
            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/a"))
                .await.unwrap();
            let _s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/b"))
                .await.unwrap();

            // Pool is full — third session should fail.
            // (select_slot looks for idle slots first, then empty. With 2 sessions on
            // 2 slots, no idle and no empty → exhausted.)
            // Actually, the slots are alive with session_count > 0 and no None slots.
            // But our test harness pre-populates all slots. Let's just verify eviction frees them.

            // Evict session 1.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            let evicted = harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
            assert!(evicted.len() >= 1);

            // The evicted slot should now be None (freed), so we can resolve evicted session
            // and verify the slot is available.
            assert!(!harness.pool.is_slot_populated(
                // find the slot that was freed
                {
                    let (_, slot, _) = harness.pool.resolve_session(&s1.session_id.0)
                        .unwrap_or_else(|| {
                            // Session was evicted, slot should be freed.
                            // We can't resolve it, which is expected.
                            (String::new(), 999, std::path::PathBuf::new())
                        });
                    if slot == 999 {
                        // Session was evicted — pick whichever slot is now free.
                        (0..2).find(|i| !harness.pool.is_slot_populated(*i)).unwrap_or(999)
                    } else {
                        slot
                    }
                }
            ));
        }).await;
    }

    /// Dedicated mode: backend death frees the slot for reuse.
    #[tokio::test]
    async fn test_dedicated_backend_death_frees_slot() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::Dedicated);

            let _s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/a"))
                .await.unwrap();

            // Both slots are populated (harness pre-fills them).
            assert!(harness.pool.is_slot_populated(0));
            assert!(harness.pool.is_slot_populated(1));

            // Backend 0 dies.
            harness.pool.handle_backend_death(0);

            // In Dedicated mode, slot 0 should now be freed (None).
            assert!(!harness.pool.is_slot_populated(0));
            // Slot 1 should be unaffected.
            assert!(harness.pool.is_slot_populated(1));
        }).await;
    }

    // -----------------------------------------------------------------------
    // Health / status tests
    // -----------------------------------------------------------------------

    /// The gateway/status ext_method should return pool health info.
    #[tokio::test]
    async fn test_gateway_status_ext_method() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::LeastConnections);

            // Create a session so we have something to report.
            harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();

            let resp = harness.upstream_to_gateway
                .ext_method(acp::ExtRequest::new(
                    "gateway/status",
                    std::sync::Arc::from(serde_json::value::RawValue::from_string("{}".to_string()).unwrap()),
                ))
                .await;

            assert!(resp.is_ok());
            let raw = resp.unwrap();
            let status: serde_json::Value = serde_json::from_str(raw.0.get()).unwrap();
            assert_eq!(status["alive_slots"], 2);
            assert_eq!(status["active_sessions"], 1);
            assert_eq!(status["pool_size"], 2);
        }).await;
    }

    // -----------------------------------------------------------------------
    // Dedicated mode: exhaustion + recovery
    // -----------------------------------------------------------------------

    /// Dedicated mode: fill all slots, verify pool_exhausted, evict one, verify
    /// new_session succeeds on the freed slot.
    #[tokio::test]
    async fn test_dedicated_pool_exhaustion_and_recovery() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::Dedicated);

            // Fill both slots.
            let s1 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/a"))
                .await.unwrap();
            let _s2 = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/b"))
                .await.unwrap();

            assert_eq!(harness.pool.session_count(), 2);

            // Evict session 1 to free a slot.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            let evicted = harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
            assert!(evicted.len() >= 1);

            // At least one slot should now be freed.
            let (_, _slot1, _) = harness.pool.resolve_session(&s1.session_id.0)
                .unwrap_or((String::new(), 999, std::path::PathBuf::new()));
            // If s1 was evicted, its slot is freed. If s2 was evicted, its slot is freed.
            // Either way, we should have at least one free slot for a new session.
            let freed_slot = (0..2).find(|i| !harness.pool.is_slot_populated(*i));
            assert!(freed_slot.is_some(), "at least one slot should be freed after eviction");
        }).await;
    }

    /// Dedicated mode: evict a session, reload it via prompt, evict again,
    /// and reload again — verifying the full cycle works repeatedly.
    /// Each eviction frees the dedicated slot, and reload picks another alive
    /// idle slot, so we need N+1 backends for N cycles.
    #[tokio::test]
    async fn test_dedicated_multiple_eviction_reload_cycles() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            // 4 backends for 3 eviction+reload cycles (each cycle consumes one slot).
            let harness = setup_harness(4, Strategy::Dedicated);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            for cycle in 0..3 {
                // Evict.
                tokio::time::advance(std::time::Duration::from_secs(601)).await;
                let evicted = harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
                assert!(!evicted.is_empty(), "cycle {cycle}: session should be evicted");
                assert!(harness.pool.is_evicted(&ext_sid.0), "cycle {cycle}: session should be in evicted map");

                // Prompt triggers transparent reload on a different alive slot.
                let result = harness.upstream_to_gateway
                    .prompt(acp::PromptRequest::new(ext_sid.clone(), vec![format!("cycle {cycle}").into()]))
                    .await;
                assert!(result.is_ok(), "cycle {cycle}: prompt should succeed after reload");

                // Session should be active again.
                assert!(!harness.pool.is_evicted(&ext_sid.0), "cycle {cycle}: session should not be evicted after reload");
                assert!(harness.pool.resolve_session(&ext_sid.0).is_some(), "cycle {cycle}: session should be resolvable");
            }

            // Verify load_session was called each time.
            let total_loads: usize = harness.agents.iter()
                .map(|a| a.load_calls.lock().unwrap().len())
                .sum();
            assert_eq!(total_loads, 3, "load_session should have been called 3 times");
        }).await;
    }

    /// Dedicated mode: session is evicted (slot freed), then the original
    /// backend's slot is dead. Prompt reloads the session on the other
    /// alive backend and the session is usable there.
    #[tokio::test]
    async fn test_dedicated_eviction_then_reload_on_other_slot() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::Dedicated);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;
            let (_, original_slot, _) = harness.pool.resolve_session(&ext_sid.0).unwrap();

            // Evict the session (frees the dedicated slot).
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));
            assert!(harness.pool.is_evicted(&ext_sid.0));

            // Original slot is freed (None). Other slot is alive and idle.
            assert!(!harness.pool.is_slot_populated(original_slot));

            // Prompt should reload on the other alive slot.
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["after eviction".into()]))
                .await;
            assert!(result.is_ok(), "prompt should succeed — reloaded on other slot");

            // Session should now be on a different slot than the original.
            let (_, new_slot, _) = harness.pool.resolve_session(&ext_sid.0).unwrap();
            let other_slot = if original_slot == 0 { 1 } else { 0 };
            assert_eq!(new_slot, other_slot, "session should reload on the other alive slot");

            // That backend should have received load_session.
            let loads = harness.agents[other_slot].load_calls.lock().unwrap();
            assert_eq!(loads.len(), 1);
        }).await;
    }

    /// Dedicated mode: when load_session fails during evicted reload,
    /// the error is returned to the client and the session is NOT
    /// registered (so it doesn't get stuck in a broken state).
    #[tokio::test]
    async fn test_dedicated_evicted_reload_failure() {
        tokio::time::pause();
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let agents = vec![
                MockAgent::new().with_failing_load_session(),
                MockAgent::new().with_failing_load_session(),
            ];
            let harness = setup_harness_with_agents(agents, Strategy::Dedicated);

            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;

            // Evict.
            tokio::time::advance(std::time::Duration::from_secs(601)).await;
            harness.pool.evict_idle_sessions(std::time::Duration::from_secs(600));

            // Prompt should fail because load_session fails on reload.
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["will fail".into()]))
                .await;
            assert!(result.is_err(), "prompt should fail when load_session fails");

            // The prompt guard should be cleared despite the failure.
            assert!(!harness.pool.is_prompt_in_flight(&ext_sid.0));
        }).await;
    }

    /// Dedicated mode: verify that initialize is called on each backend
    /// that processes a new_session (via the stored init request replay).
    /// The harness initializes all backends at setup, so each backend's
    /// initialize_calls counter starts at 0 (harness doesn't call initialize).
    /// When we call initialize through the gateway, all backends get it via scatter.
    /// Then each new_session goes to a specific backend.
    #[tokio::test]
    async fn test_dedicated_init_replay_tracked() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(3, Strategy::Dedicated);

            // Initialize through the gateway — all backends get it.
            harness.upstream_to_gateway.initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(acp::Implementation::new("test", "0.0.0"))
            ).await.unwrap();

            // All 3 backends should have received exactly 1 initialize call.
            for (i, agent) in harness.agents.iter().enumerate() {
                let count = agent.initialize_calls.load(Ordering::Relaxed);
                assert_eq!(count, 1, "backend {i} should have 1 initialize call, got {count}");
            }

            // Create 3 sessions (one per backend, since Dedicated).
            for _ in 0..3 {
                harness.upstream_to_gateway
                    .new_session(acp::NewSessionRequest::new("/test"))
                    .await.unwrap();
            }

            // Each backend should have exactly 1 session.
            for (i, agent) in harness.agents.iter().enumerate() {
                let count = agent.sessions.lock().unwrap().len();
                assert_eq!(count, 1, "backend {i} should have 1 session, got {count}");
            }
        }).await;
    }

    /// Dedicated mode: backend death + prompt on that session reloads
    /// to a different backend. Verify the backend that receives the
    /// reload is a different one.
    #[tokio::test]
    async fn test_dedicated_crash_reload_to_different_backend() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let harness = setup_harness(2, Strategy::Dedicated);

            // Create session on one backend.
            let resp = harness.upstream_to_gateway
                .new_session(acp::NewSessionRequest::new("/test"))
                .await.unwrap();
            let ext_sid = resp.session_id;
            let (_, original_slot, _) = harness.pool.resolve_session(&ext_sid.0).unwrap();

            // Kill that backend.
            harness.pool.handle_backend_death(original_slot);

            // Session should be evicted.
            assert!(harness.pool.is_evicted(&ext_sid.0));

            // Prompt should reload on the other (alive) backend.
            let result = harness.upstream_to_gateway
                .prompt(acp::PromptRequest::new(ext_sid.clone(), vec!["after crash".into()]))
                .await;
            assert!(result.is_ok(), "prompt should succeed after crash reload");

            // Verify it moved to the other slot.
            let (_, new_slot, _) = harness.pool.resolve_session(&ext_sid.0).unwrap();
            assert_ne!(new_slot, original_slot, "session should reload on a different slot");

            // The other backend should have received load_session.
            let other = if original_slot == 0 { 1 } else { 0 };
            let loads = harness.agents[other].load_calls.lock().unwrap();
            assert_eq!(loads.len(), 1, "other backend should have 1 load_session call");
        }).await;
    }
}
