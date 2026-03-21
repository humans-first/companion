use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use agent_client_protocol::{self as acp, Agent as _};
use tokio::process::Child;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::Strategy;
use crate::error;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

pub struct BackendSlot {
    pub connection: acp::ClientSideConnection,
    pub child: Option<Child>,
    pub session_count: usize,
    pub alive: bool,
}

pub struct SessionEntry {
    pub slot_index: usize,
    pub backend_sid: String,
    pub last_activity: Instant,
    pub cwd: std::path::PathBuf,
}

pub struct EvictedSession {
    pub backend_sid: String,
    pub cwd: std::path::PathBuf,
}

pub struct PoolConfig {
    pub strategy: Strategy,
    pub pool_size: usize,
    #[allow(dead_code)]
    pub agent_cmd: String,
}

struct PoolInner {
    slots: Vec<Option<BackendSlot>>,
    sessions: HashMap<String, SessionEntry>,
    reverse: HashMap<String, String>,
    evicted: HashMap<String, EvictedSession>,
    in_flight_prompts: HashSet<String>,
    upstream: Option<acp::AgentSideConnection>,
    config: PoolConfig,
    /// Stored InitializeRequest, replayed to each new Dedicated backend.
    init_request: Option<acp::InitializeRequest>,
}

#[derive(Clone)]
pub struct AgentPool {
    inner: Rc<RefCell<PoolInner>>,
}

impl AgentPool {
    pub fn new(config: PoolConfig) -> Self {
        let capacity = config.pool_size;
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(None);
        }
        Self {
            inner: Rc::new(RefCell::new(PoolInner {
                slots,
                sessions: HashMap::new(),
                reverse: HashMap::new(),
                evicted: HashMap::new(),
                in_flight_prompts: HashSet::new(),
                upstream: None,
                config,
                init_request: None,
            })),
        }
    }

    pub fn set_upstream(&self, conn: acp::AgentSideConnection) {
        self.inner.borrow_mut().upstream = Some(conn);
    }

    // -----------------------------------------------------------------------
    // Slot management
    // -----------------------------------------------------------------------

    /// Insert a backend slot at the given index. Used by spawn_backend and tests.
    pub fn insert_slot(&self, index: usize, slot: BackendSlot) {
        let mut inner = self.inner.borrow_mut();
        if index >= inner.slots.len() {
            inner.slots.resize_with(index + 1, || None);
        }
        inner.slots[index] = Some(slot);
    }

    /// Select the best slot for a new session.
    /// LC: alive slot with fewest sessions.
    /// Dedicated: prefer an alive idle slot (e.g. pre-initialized), then first empty slot.
    pub fn select_slot(&self) -> Result<usize, acp::Error> {
        let inner = self.inner.borrow();
        match inner.config.strategy {
            Strategy::LeastConnections => {
                inner
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| {
                        slot.as_ref()
                            .filter(|s| s.alive)
                            .map(|s| (i, s.session_count))
                    })
                    .min_by_key(|(_, count)| *count)
                    .map(|(i, _)| i)
                    .ok_or_else(|| error::pool_exhausted(inner.config.pool_size))
            }
            Strategy::Dedicated => {
                // Prefer an alive slot with 0 sessions (pre-initialized backend).
                if let Some(idx) = inner.slots.iter().enumerate().find_map(|(i, s)| {
                    s.as_ref().filter(|s| s.alive && s.session_count == 0).map(|_| i)
                }) {
                    return Ok(idx);
                }
                // Otherwise find first empty slot (caller must spawn into it).
                inner
                    .slots
                    .iter()
                    .position(|s| s.is_none())
                    .ok_or_else(|| error::pool_exhausted(inner.config.pool_size))
            }
        }
    }

    /// Get a reference to the backend connection at `slot_index`.
    ///
    /// SAFETY: single-threaded LocalSet, connection outlives all requests.
    pub fn backend_conn(&self, slot_index: usize) -> Result<&acp::ClientSideConnection, acp::Error> {
        let inner = self.inner.borrow();
        let slot = inner.slots.get(slot_index)
            .and_then(|s| s.as_ref())
            .ok_or_else(acp::Error::internal_error)?;
        if !slot.alive {
            return Err(acp::Error::internal_error().data("backend slot is dead"));
        }
        let conn = &slot.connection;
        Ok(unsafe { &*(conn as *const acp::ClientSideConnection) })
    }

    /// Get a reference to the upstream connection.
    ///
    /// SAFETY: same as backend_conn.
    pub fn upstream_conn(&self) -> Result<&acp::AgentSideConnection, acp::Error> {
        let inner = self.inner.borrow();
        let conn = inner.upstream.as_ref().ok_or_else(acp::Error::internal_error)?;
        Ok(unsafe { &*(conn as *const acp::AgentSideConnection) })
    }

    // -----------------------------------------------------------------------
    // Session management
    // -----------------------------------------------------------------------

    /// Register a new session mapping.
    pub fn register_session(&self, external_sid: &str, backend_sid: &str, slot_index: usize, cwd: std::path::PathBuf) {
        let mut inner = self.inner.borrow_mut();
        info!(external_sid, backend_sid, slot_index, "registered session mapping");
        inner.sessions.insert(
            external_sid.to_string(),
            SessionEntry {
                slot_index,
                backend_sid: backend_sid.to_string(),
                last_activity: Instant::now(),
                cwd,
            },
        );
        inner.reverse.insert(backend_sid.to_string(), external_sid.to_string());
        if let Some(slot) = inner.slots.get_mut(slot_index).and_then(|s| s.as_mut()) {
            slot.session_count += 1;
        }
    }

    /// Look up the backend session ID, slot, and cwd for an external session.
    pub fn resolve_session(&self, external_sid: &str) -> Option<(String, usize, std::path::PathBuf)> {
        let inner = self.inner.borrow();
        inner.sessions.get(external_sid).map(|e| (e.backend_sid.clone(), e.slot_index, e.cwd.clone()))
    }

    /// Map external → backend SessionId, or pass through if unknown.
    #[allow(dead_code)]
    pub fn to_backend(&self, sid: &acp::SessionId) -> acp::SessionId {
        let inner = self.inner.borrow();
        inner
            .sessions
            .get(sid.0.as_ref())
            .map(|e| acp::SessionId::new(e.backend_sid.as_str()))
            .unwrap_or_else(|| sid.clone())
    }

    /// Map backend → external SessionId, or pass through if unknown.
    pub fn to_external(&self, sid: &acp::SessionId) -> acp::SessionId {
        let inner = self.inner.borrow();
        inner
            .reverse
            .get(sid.0.as_ref())
            .map(|e| acp::SessionId::new(e.as_str()))
            .unwrap_or_else(|| sid.clone())
    }

    /// Update last_activity timestamp for a session.
    pub fn touch_session(&self, external_sid: &str) {
        let mut inner = self.inner.borrow_mut();
        if let Some(entry) = inner.sessions.get_mut(external_sid) {
            entry.last_activity = Instant::now();
        }
    }

    /// Generate a new external session ID (UUID v7).
    pub fn new_external_sid() -> String {
        Uuid::now_v7().to_string()
    }

    /// Get the backend connection for an external session ID.
    pub fn backend_for_session(&self, external_sid: &str) -> Result<(&acp::ClientSideConnection, acp::SessionId), acp::Error> {
        let (backend_sid, slot_index, _cwd) = self.resolve_session(external_sid)
            .ok_or_else(|| error::unknown_session(external_sid))?;
        let conn = self.backend_conn(slot_index)?;
        Ok((conn, acp::SessionId::new(backend_sid.as_str())))
    }

    /// Get the slot index for a backend session ID (by looking up reverse map).
    #[allow(dead_code)]
    pub fn slot_for_backend_sid(&self, backend_sid: &str) -> Option<usize> {
        let inner = self.inner.borrow();
        let external_sid = inner.reverse.get(backend_sid)?;
        inner.sessions.get(external_sid).map(|e| e.slot_index)
    }

    // -----------------------------------------------------------------------
    // Prompt guard
    // -----------------------------------------------------------------------

    /// Mark a session as having an in-flight prompt. Returns error if already in flight.
    pub fn begin_prompt(&self, external_sid: &str) -> Result<(), acp::Error> {
        let mut inner = self.inner.borrow_mut();
        if !inner.in_flight_prompts.insert(external_sid.to_string()) {
            return Err(error::prompt_in_flight(external_sid));
        }
        Ok(())
    }

    /// Clear the in-flight prompt flag for a session.
    pub fn end_prompt(&self, external_sid: &str) {
        let mut inner = self.inner.borrow_mut();
        inner.in_flight_prompts.remove(external_sid);
    }

    // -----------------------------------------------------------------------
    // Eviction
    // -----------------------------------------------------------------------

    /// Evict sessions idle for longer than `timeout`.
    /// Returns the list of evicted external session IDs.
    pub fn evict_idle_sessions(&self, timeout: std::time::Duration) -> Vec<String> {
        let mut inner = self.inner.borrow_mut();
        let now = Instant::now();
        let mut evicted_sids = Vec::new();

        let idle: Vec<(String, String, std::path::PathBuf, usize)> = inner
            .sessions
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.last_activity) >= timeout)
            .map(|(ext, entry)| (ext.clone(), entry.backend_sid.clone(), entry.cwd.clone(), entry.slot_index))
            .collect();

        for (ext_sid, backend_sid, cwd, slot_index) in idle {
            info!(external_sid = %ext_sid, "evicting idle session");
            inner.sessions.remove(&ext_sid);
            inner.reverse.remove(&backend_sid);
            inner.evicted.insert(ext_sid.clone(), EvictedSession { backend_sid, cwd });

            if inner.config.strategy == Strategy::Dedicated {
                // Kill process and free the slot so it can be reused.
                info!(slot_index, "freeing dedicated slot on eviction");
                inner.slots[slot_index] = None;
            } else {
                if let Some(slot) = inner.slots.get_mut(slot_index).and_then(|s| s.as_mut()) {
                    slot.session_count = slot.session_count.saturating_sub(1);
                }
            }
            evicted_sids.push(ext_sid);
        }

        evicted_sids
    }

    /// Check if a session has been evicted (and take it for reload).
    pub fn take_evicted(&self, external_sid: &str) -> Option<EvictedSession> {
        self.inner.borrow_mut().evicted.remove(external_sid)
    }

    #[allow(dead_code)]
    pub fn is_evicted(&self, external_sid: &str) -> bool {
        self.inner.borrow().evicted.contains_key(external_sid)
    }

    /// Remove stale evicted entries older than `max_age`.
    /// Called periodically alongside idle eviction to prevent unbounded growth.
    pub fn purge_stale_evicted(&self, max_entries: usize) {
        let mut inner = self.inner.borrow_mut();
        if inner.evicted.len() > max_entries {
            let excess = inner.evicted.len() - max_entries;
            let keys_to_remove: Vec<String> = inner.evicted.keys().take(excess).cloned().collect();
            for key in keys_to_remove {
                debug!(external_sid = %key, "purging stale evicted session");
                inner.evicted.remove(&key);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Scatter-gather
    // -----------------------------------------------------------------------

    /// Get indices of all alive slots.
    pub fn alive_slots(&self) -> Vec<usize> {
        let inner = self.inner.borrow();
        inner
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                slot.as_ref().filter(|s| s.alive).map(|_| i)
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Crash recovery
    // -----------------------------------------------------------------------

    /// Handle a backend slot dying: mark dead, move sessions to evicted, clear prompts.
    pub fn handle_backend_death(&self, slot_index: usize) {
        let mut inner = self.inner.borrow_mut();
        warn!(slot_index, "backend process died");

        if inner.config.strategy == Strategy::Dedicated {
            // Free the slot entirely so it can be reused for new sessions.
            inner.slots[slot_index] = None;
        } else {
            if let Some(slot) = inner.slots.get_mut(slot_index).and_then(|s| s.as_mut()) {
                slot.alive = false;
                slot.session_count = 0;
            }
        }

        // Move all sessions on this slot to evicted.
        let affected: Vec<(String, String, std::path::PathBuf)> = inner
            .sessions
            .iter()
            .filter(|(_, entry)| entry.slot_index == slot_index)
            .map(|(ext, entry)| (ext.clone(), entry.backend_sid.clone(), entry.cwd.clone()))
            .collect();

        for (ext_sid, backend_sid, cwd) in &affected {
            inner.sessions.remove(ext_sid);
            inner.reverse.remove(backend_sid);
            inner.evicted.insert(ext_sid.clone(), EvictedSession { backend_sid: backend_sid.clone(), cwd: cwd.clone() });
            inner.in_flight_prompts.remove(ext_sid);
        }

        info!(
            slot_index,
            sessions_evicted = affected.len(),
            "moved sessions to evicted after backend death"
        );
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    /// Gracefully shut down all backend processes.
    ///
    /// Sends SIGTERM to process groups first, waits up to 500ms, then SIGKILL any remaining.
    pub async fn shutdown(&self) {
        info!("shutting down all backend processes");

        // Collect PIDs and mark all slots dead.
        let pids: Vec<(usize, u32)> = {
            let mut inner = self.inner.borrow_mut();
            let mut pids = Vec::new();
            for (i, slot) in inner.slots.iter_mut().enumerate() {
                if let Some(s) = slot.as_mut() {
                    s.alive = false;
                    if let Some(ref child) = s.child {
                        if let Some(pid) = child.id() {
                            pids.push((i, pid));
                        }
                    }
                }
            }
            pids
        };
        // RefCell borrow is dropped here.

        // Send SIGTERM to process groups (negative PID = entire group).
        for &(i, pid) in &pids {
            let pgid = -(pid as i32);
            debug!(slot_index = i, pid, "sending SIGTERM to backend process group");
            unsafe {
                libc::kill(pgid as libc::pid_t, libc::SIGTERM);
            }
        }

        // Wait up to 500ms for graceful exit.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // SIGKILL any still running. We take children out of slots to avoid
        // holding borrow_mut across the kill().await calls.
        let children: Vec<(usize, Child)> = {
            let mut inner = self.inner.borrow_mut();
            let mut children = Vec::new();
            for (i, slot) in inner.slots.iter_mut().enumerate() {
                if let Some(s) = slot.as_mut() {
                    if let Some(child) = s.child.take() {
                        children.push((i, child));
                    }
                }
            }
            children
        };
        // RefCell borrow is dropped here.

        for (i, mut child) in children.into_iter() {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    debug!(slot_index = i, "backend exited gracefully");
                }
                _ => {
                    debug!(slot_index = i, "backend did not exit, sending SIGKILL");
                    let _ = child.kill().await;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Strategy accessor
    // -----------------------------------------------------------------------

    /// Store the InitializeRequest so it can be replayed to new Dedicated backends.
    pub fn store_init_request(&self, req: acp::InitializeRequest) {
        self.inner.borrow_mut().init_request = Some(req);
    }

    /// Spawn a backend process into the given slot without initializing it.
    /// Used by `initialize` to spawn the first Dedicated backend for real init.
    pub async fn spawn_backend_process(&self, slot_index: usize) -> Result<(), String> {
        let agent_cmd = self.inner.borrow().config.agent_cmd.clone();
        crate::spawn_backend(self, &agent_cmd, slot_index)
            .await
            .map_err(|e| e.to_string())
    }

    /// Spawn a dedicated backend process into the given slot and initialize it.
    ///
    /// This is used by Dedicated mode to spawn a backend on demand for
    /// `new_session` and session reload. The stored InitializeRequest is
    /// replayed to the new backend before it receives any other calls.
    pub async fn spawn_dedicated_backend(&self, slot_index: usize) -> Result<(), String> {
        self.spawn_backend_process(slot_index).await?;

        // Replay stored init request to the new backend.
        let init_req = self.inner.borrow().init_request.clone();
        if let Some(req) = init_req {
            let conn = self.backend_conn(slot_index)
                .map_err(|e| format!("backend_conn after spawn: {e}"))?;
            conn.initialize(req).await
                .map_err(|e| format!("initialize new backend: {e}"))?;
        }

        Ok(())
    }

    /// Check if a slot has a backend (as opposed to being empty/None).
    pub fn is_slot_populated(&self, index: usize) -> bool {
        let inner = self.inner.borrow();
        inner.slots.get(index).is_some_and(|s| s.is_some())
    }

    pub fn strategy(&self) -> Strategy {
        self.inner.borrow().config.strategy.clone()
    }

    #[allow(dead_code)]
    pub fn pool_size(&self) -> usize {
        self.inner.borrow().config.pool_size
    }

    #[allow(dead_code)]
    pub fn agent_cmd(&self) -> String {
        self.inner.borrow().config.agent_cmd.clone()
    }

    /// Number of active sessions.
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.inner.borrow().sessions.len()
    }

    /// Number of evicted sessions.
    #[cfg(test)]
    pub fn evicted_count(&self) -> usize {
        self.inner.borrow().evicted.len()
    }

    /// Check if a prompt is in flight for a session.
    #[cfg(test)]
    pub fn is_prompt_in_flight(&self, external_sid: &str) -> bool {
        self.inner.borrow().in_flight_prompts.contains(external_sid)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pool(strategy: Strategy, pool_size: usize) -> AgentPool {
        AgentPool::new(PoolConfig {
            strategy,
            pool_size,
            agent_cmd: "echo".to_string(),
        })
    }

    fn make_mock_slot(alive: bool) -> BackendSlot {
        // Create a mock connection using piper pipes.
        // We need to run inside a LocalSet for this, but for unit tests
        // that only test pool data structures, we use a minimal setup.
        // For connection-dependent tests, we use the full LocalSet pattern.
        //
        // For pure data-structure tests, we create a dummy connection.
        let (rx1, tx1) = piper::pipe(1024);
        let (rx2, tx2) = piper::pipe(1024);

        struct NoopClient;
        #[async_trait::async_trait(?Send)]
        impl acp::Client for NoopClient {
            async fn request_permission(&self, _: acp::RequestPermissionRequest) -> acp::Result<acp::RequestPermissionResponse> { unimplemented!() }
            async fn session_notification(&self, _: acp::SessionNotification) -> acp::Result<()> { Ok(()) }
            async fn read_text_file(&self, _: acp::ReadTextFileRequest) -> acp::Result<acp::ReadTextFileResponse> { unimplemented!() }
            async fn write_text_file(&self, _: acp::WriteTextFileRequest) -> acp::Result<acp::WriteTextFileResponse> { unimplemented!() }
            async fn create_terminal(&self, _: acp::CreateTerminalRequest) -> acp::Result<acp::CreateTerminalResponse> { unimplemented!() }
            async fn terminal_output(&self, _: acp::TerminalOutputRequest) -> acp::Result<acp::TerminalOutputResponse> { unimplemented!() }
            async fn release_terminal(&self, _: acp::ReleaseTerminalRequest) -> acp::Result<acp::ReleaseTerminalResponse> { unimplemented!() }
            async fn wait_for_terminal_exit(&self, _: acp::WaitForTerminalExitRequest) -> acp::Result<acp::WaitForTerminalExitResponse> { unimplemented!() }
            async fn kill_terminal(&self, _: acp::KillTerminalRequest) -> acp::Result<acp::KillTerminalResponse> { unimplemented!() }
            async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> { unimplemented!() }
            async fn ext_notification(&self, _: acp::ExtNotification) -> acp::Result<()> { Ok(()) }
        }

        let (conn, _io) = acp::ClientSideConnection::new(
            NoopClient,
            tx1,
            rx2,
            |_fut| { /* don't spawn, we won't use the connection */ },
        );

        // Leak the pipes so they stay alive (test-only).
        std::mem::forget(rx1);
        std::mem::forget(tx2);

        BackendSlot {
            connection: conn,
            child: None,
            session_count: 0,
            alive,
        }
    }

    // -- Session registration and lookup --

    #[test]
    fn test_session_registration_and_lookup() {
        let pool = make_pool(Strategy::LeastConnections, 2);
        pool.insert_slot(0, make_mock_slot(true));

        pool.register_session("ext-1", "back-1", 0, "/test".into());

        // Forward lookup
        let (backend_sid, slot, cwd) = pool.resolve_session("ext-1").unwrap();
        assert_eq!(backend_sid, "back-1");
        assert_eq!(slot, 0);
        assert_eq!(cwd, std::path::PathBuf::from("/test"));

        // Reverse lookup via to_external
        let ext = pool.to_external(&acp::SessionId::new("back-1"));
        assert_eq!(ext.0.as_ref(), "ext-1");

        // Forward lookup via to_backend
        let back = pool.to_backend(&acp::SessionId::new("ext-1"));
        assert_eq!(back.0.as_ref(), "back-1");

        // Unknown session passes through
        let unknown = pool.to_backend(&acp::SessionId::new("unknown"));
        assert_eq!(unknown.0.as_ref(), "unknown");
    }

    // -- Least-connections slot selection --

    #[test]
    fn test_lc_selects_least_loaded() {
        let pool = make_pool(Strategy::LeastConnections, 3);
        let mut slot0 = make_mock_slot(true);
        slot0.session_count = 5;
        let mut slot1 = make_mock_slot(true);
        slot1.session_count = 2;
        let mut slot2 = make_mock_slot(true);
        slot2.session_count = 8;

        pool.insert_slot(0, slot0);
        pool.insert_slot(1, slot1);
        pool.insert_slot(2, slot2);

        let selected = pool.select_slot().unwrap();
        assert_eq!(selected, 1); // slot1 has fewest sessions
    }

    #[test]
    fn test_lc_skips_dead_slots() {
        let pool = make_pool(Strategy::LeastConnections, 2);
        let mut slot0 = make_mock_slot(false); // dead
        slot0.session_count = 0;
        let mut slot1 = make_mock_slot(true);
        slot1.session_count = 10;

        pool.insert_slot(0, slot0);
        pool.insert_slot(1, slot1);

        let selected = pool.select_slot().unwrap();
        assert_eq!(selected, 1); // only alive slot
    }

    // -- Dedicated slot selection --

    #[test]
    fn test_dedicated_prefers_idle_slot_over_empty() {
        let pool = make_pool(Strategy::Dedicated, 3);
        let mut slot0 = make_mock_slot(true);
        slot0.session_count = 1; // has a session
        pool.insert_slot(0, slot0);
        // slot 1 is None (empty)
        pool.insert_slot(2, make_mock_slot(true)); // alive, 0 sessions (idle)

        let selected = pool.select_slot().unwrap();
        assert_eq!(selected, 2); // prefers idle alive slot
    }

    #[test]
    fn test_dedicated_selects_empty_slot_when_no_idle() {
        let pool = make_pool(Strategy::Dedicated, 3);
        let mut slot0 = make_mock_slot(true);
        slot0.session_count = 1;
        pool.insert_slot(0, slot0);
        // slot 1 is None (empty)
        let mut slot2 = make_mock_slot(true);
        slot2.session_count = 1;
        pool.insert_slot(2, slot2);

        let selected = pool.select_slot().unwrap();
        assert_eq!(selected, 1); // first empty slot
    }

    #[test]
    fn test_dedicated_pool_exhausted() {
        let pool = make_pool(Strategy::Dedicated, 2);
        let mut slot0 = make_mock_slot(true);
        slot0.session_count = 1;
        pool.insert_slot(0, slot0);
        let mut slot1 = make_mock_slot(true);
        slot1.session_count = 1;
        pool.insert_slot(1, slot1);

        let result = pool.select_slot();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, acp::ErrorCode::Other(error::POOL_EXHAUSTED));
    }

    // -- Prompt guard --

    #[test]
    fn test_begin_prompt_succeeds_first_time() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        assert!(pool.begin_prompt("session-1").is_ok());
        assert!(pool.is_prompt_in_flight("session-1"));
    }

    #[test]
    fn test_concurrent_prompt_rejected() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.begin_prompt("session-1").unwrap();

        let result = pool.begin_prompt("session-1");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, acp::ErrorCode::Other(error::PROMPT_IN_FLIGHT));
    }

    #[test]
    fn test_end_prompt_allows_new_prompt() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.begin_prompt("session-1").unwrap();
        pool.end_prompt("session-1");
        assert!(!pool.is_prompt_in_flight("session-1"));

        // Should succeed again.
        assert!(pool.begin_prompt("session-1").is_ok());
    }

    // -- Idle eviction --

    #[tokio::test]
    async fn test_idle_eviction_moves_to_evicted() {
        tokio::time::pause();
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());

        // Advance past timeout.
        tokio::time::advance(std::time::Duration::from_secs(601)).await;

        let evicted = pool.evict_idle_sessions(std::time::Duration::from_secs(600));
        assert_eq!(evicted, vec!["ext-1"]);
        assert!(pool.is_evicted("ext-1"));
        assert_eq!(pool.session_count(), 0);
        assert_eq!(pool.evicted_count(), 1);
    }

    #[tokio::test]
    async fn test_touch_prevents_eviction() {
        tokio::time::pause();
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());

        // Advance but touch session.
        tokio::time::advance(std::time::Duration::from_secs(300)).await;
        pool.touch_session("ext-1");
        tokio::time::advance(std::time::Duration::from_secs(400)).await;

        let evicted = pool.evict_idle_sessions(std::time::Duration::from_secs(600));
        assert!(evicted.is_empty());
        assert_eq!(pool.session_count(), 1);
    }

    #[test]
    fn test_take_evicted() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());

        // Manually evict by calling evict with zero timeout.
        pool.evict_idle_sessions(std::time::Duration::ZERO);

        let evicted = pool.take_evicted("ext-1");
        assert!(evicted.is_some());
        let evicted = evicted.unwrap();
        assert_eq!(evicted.backend_sid, "back-1");
        assert_eq!(evicted.cwd, std::path::PathBuf::from("/test"));
        assert!(!pool.is_evicted("ext-1"));
    }

    // -- Crash recovery --

    #[test]
    fn test_crash_moves_sessions_to_evicted() {
        let pool = make_pool(Strategy::LeastConnections, 2);
        pool.insert_slot(0, make_mock_slot(true));
        pool.insert_slot(1, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());
        pool.register_session("ext-2", "back-2", 0, "/test".into());
        pool.register_session("ext-3", "back-3", 1, "/test".into());

        pool.handle_backend_death(0);

        // Sessions on slot 0 should be evicted.
        assert!(pool.is_evicted("ext-1"));
        assert!(pool.is_evicted("ext-2"));
        // Session on slot 1 should be unaffected.
        assert!(!pool.is_evicted("ext-3"));
        assert!(pool.resolve_session("ext-3").is_some());
    }

    #[test]
    fn test_crash_clears_in_flight_prompts() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());
        pool.begin_prompt("ext-1").unwrap();

        pool.handle_backend_death(0);

        assert!(!pool.is_prompt_in_flight("ext-1"));
    }

    // -- Session count tracking --

    #[test]
    fn test_register_increments_session_count() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));

        pool.register_session("ext-1", "back-1", 0, "/test".into());
        pool.register_session("ext-2", "back-2", 0, "/test".into());

        // After 2 registrations, select_slot should see session_count = 2.
        // We can verify indirectly via the LC algorithm.
        assert_eq!(pool.session_count(), 2);
    }
}
