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
use crate::service::GatewayService;

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
    pub last_sender: String,
}

pub struct EvictedSession {
    pub backend_sid: String,
    pub cwd: std::path::PathBuf,
    pub evicted_at: Instant,
    pub last_sender: String,
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
    prompt_senders: HashMap<String, String>,
    config: PoolConfig,
    /// Stored InitializeRequest, replayed to each new Dedicated backend.
    init_request: Option<acp::InitializeRequest>,
}

#[derive(Clone)]
pub struct AgentPool {
    inner: Rc<RefCell<PoolInner>>,
}

#[cfg(test)]
const DEFAULT_FRONTEND_ID: &str = "<default>";

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
                prompt_senders: HashMap::new(),
                config,
                init_request: None,
            })),
        }
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

    // -----------------------------------------------------------------------
    // Session management
    // -----------------------------------------------------------------------

    /// Register a new session mapping with a default sender.
    #[cfg(test)]
    pub fn register_session(
        &self,
        external_sid: &str,
        backend_sid: &str,
        slot_index: usize,
        cwd: std::path::PathBuf,
    ) {
        self.register_session_for_sender(
            external_sid,
            backend_sid,
            slot_index,
            cwd,
            DEFAULT_FRONTEND_ID,
        );
    }

    /// Register a new session mapping for a specific sender frontend.
    pub fn register_session_for_sender(
        &self,
        external_sid: &str,
        backend_sid: &str,
        slot_index: usize,
        cwd: std::path::PathBuf,
        frontend_id: &str,
    ) {
        let mut inner = self.inner.borrow_mut();
        info!(external_sid, backend_sid, slot_index, "registered session mapping");
        inner.sessions.insert(
            external_sid.to_string(),
            SessionEntry {
                slot_index,
                backend_sid: backend_sid.to_string(),
                last_activity: Instant::now(),
                cwd,
                last_sender: frontend_id.to_string(),
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

    /// Record which frontend most recently acted on a session.
    pub fn record_sender(&self, external_sid: &str, frontend_id: &str) {
        let mut inner = self.inner.borrow_mut();
        if let Some(entry) = inner.sessions.get_mut(external_sid) {
            entry.last_sender = frontend_id.to_string();
        } else if let Some(entry) = inner.evicted.get_mut(external_sid) {
            entry.last_sender = frontend_id.to_string();
        }
    }

    /// Resolve the frontend that should receive callbacks for a session.
    pub fn route_frontend_for_session(&self, external_sid: &str) -> Option<String> {
        let inner = self.inner.borrow();
        inner
            .prompt_senders
            .get(external_sid)
            .cloned()
            .or_else(|| inner.sessions.get(external_sid).map(|e| e.last_sender.clone()))
            .or_else(|| inner.evicted.get(external_sid).map(|e| e.last_sender.clone()))
    }

    /// Resolve the external session ID for a backend session ID.
    pub fn external_session_for_backend(&self, backend_sid: &str) -> Option<String> {
        self.inner.borrow().reverse.get(backend_sid).cloned()
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

    /// Map backend → external SessionId, or pass through with a warning if unknown.
    pub fn to_external(&self, sid: &acp::SessionId) -> acp::SessionId {
        let inner = self.inner.borrow();
        match inner.reverse.get(sid.0.as_ref()) {
            Some(external) => acp::SessionId::new(external.as_str()),
            None => {
                warn!(backend_sid = %sid.0, "unknown backend session ID in to_external, passing through");
                sid.clone()
            }
        }
    }

    /// Remove a session from the pool (on explicit close).
    /// For Dedicated mode, frees the slot when its last session is removed.
    /// Used by `close_session` (unstable_session_close feature).
    #[allow(dead_code)]
    pub fn remove_session(&self, external_sid: &str) {
        let mut child_to_kill = None;
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(entry) = inner.sessions.remove(external_sid) {
                inner.reverse.remove(&entry.backend_sid);
                inner.in_flight_prompts.remove(external_sid);
                inner.prompt_senders.remove(external_sid);
                let slot_index = entry.slot_index;
                if let Some(slot) = inner.slots.get_mut(slot_index).and_then(|s| s.as_mut()) {
                    slot.session_count = slot.session_count.saturating_sub(1);
                }
                // In Dedicated mode, free the slot when its session is closed.
                if inner.config.strategy == Strategy::Dedicated {
                    let should_free = inner.slots.get(slot_index)
                        .and_then(|s| s.as_ref())
                        .is_some_and(|s| s.session_count == 0);
                    if should_free {
                        info!(slot_index, "freeing dedicated slot after session close");
                        // Take the child out for graceful termination before dropping the slot.
                        if let Some(slot) = inner.slots[slot_index].as_mut() {
                            child_to_kill = slot.child.take();
                        }
                        inner.slots[slot_index] = None;
                    }
                }
            }
        }
        // Gracefully terminate the child process outside the borrow.
        if let Some(child) = child_to_kill {
            graceful_kill(child);
        }
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
    #[cfg(test)]
    pub fn begin_prompt(&self, external_sid: &str) -> Result<(), acp::Error> {
        self.begin_prompt_for_sender(external_sid, DEFAULT_FRONTEND_ID)
    }

    /// Mark a session as having an in-flight prompt for a specific frontend.
    pub fn begin_prompt_for_sender(
        &self,
        external_sid: &str,
        frontend_id: &str,
    ) -> Result<(), acp::Error> {
        let mut inner = self.inner.borrow_mut();
        if !inner.in_flight_prompts.insert(external_sid.to_string()) {
            return Err(error::prompt_in_flight(external_sid));
        }
        inner
            .prompt_senders
            .insert(external_sid.to_string(), frontend_id.to_string());
        if let Some(entry) = inner.sessions.get_mut(external_sid) {
            entry.last_sender = frontend_id.to_string();
        } else if let Some(entry) = inner.evicted.get_mut(external_sid) {
            entry.last_sender = frontend_id.to_string();
        }
        Ok(())
    }

    /// Clear the in-flight prompt flag for a session.
    pub fn end_prompt(&self, external_sid: &str) {
        let mut inner = self.inner.borrow_mut();
        inner.in_flight_prompts.remove(external_sid);
        inner.prompt_senders.remove(external_sid);
    }

    // -----------------------------------------------------------------------
    // Eviction
    // -----------------------------------------------------------------------

    /// Evict sessions idle for longer than `timeout`.
    /// Returns the list of evicted external session IDs.
    pub fn evict_idle_sessions(&self, timeout: std::time::Duration) -> Vec<String> {
        let mut children_to_kill = Vec::new();
        let mut evicted_sids = Vec::new();
        {
            let mut inner = self.inner.borrow_mut();
            let now = Instant::now();

            let idle: Vec<(String, String, std::path::PathBuf, usize, String)> = inner
                .sessions
                .iter()
                .filter(|(_, entry)| now.duration_since(entry.last_activity) >= timeout)
                .map(|(ext, entry)| {
                    (
                        ext.clone(),
                        entry.backend_sid.clone(),
                        entry.cwd.clone(),
                        entry.slot_index,
                        entry.last_sender.clone(),
                    )
                })
                .collect();

            // Collect which dedicated slots to free so we handle each only once.
            let mut dedicated_slots_to_free: Vec<usize> = Vec::new();

            for (ext_sid, backend_sid, cwd, slot_index, last_sender) in idle {
                info!(external_sid = %ext_sid, "evicting idle session");
                inner.sessions.remove(&ext_sid);
                inner.reverse.remove(&backend_sid);
                inner.prompt_senders.remove(&ext_sid);
                inner.evicted.insert(
                    ext_sid.clone(),
                    EvictedSession {
                        backend_sid,
                        cwd,
                        evicted_at: now,
                        last_sender,
                    },
                );

                if inner.config.strategy == Strategy::Dedicated {
                    if !dedicated_slots_to_free.contains(&slot_index) {
                        dedicated_slots_to_free.push(slot_index);
                    }
                } else if let Some(slot) = inner.slots.get_mut(slot_index).and_then(|s| s.as_mut()) {
                    slot.session_count = slot.session_count.saturating_sub(1);
                }
                evicted_sids.push(ext_sid);
            }

            // Free dedicated slots, extracting children for graceful kill.
            for slot_index in dedicated_slots_to_free {
                info!(slot_index, "freeing dedicated slot on eviction");
                if let Some(slot) = inner.slots[slot_index].as_mut() {
                    if let Some(child) = slot.child.take() {
                        children_to_kill.push(child);
                    }
                }
                inner.slots[slot_index] = None;
            }
        }
        // Gracefully terminate child processes outside the borrow.
        for child in children_to_kill {
            graceful_kill(child);
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

    /// Remove the oldest evicted entries when the map exceeds `max_entries`.
    /// Called periodically to prevent unbounded growth.
    pub fn purge_stale_evicted(&self, max_entries: usize) {
        let mut inner = self.inner.borrow_mut();
        if inner.evicted.len() > max_entries {
            let excess = inner.evicted.len() - max_entries;
            // Sort by eviction time, remove oldest.
            let mut entries: Vec<(String, Instant)> = inner.evicted.iter()
                .map(|(k, v)| (k.clone(), v.evicted_at))
                .collect();
            entries.sort_by_key(|(_, t)| *t);
            for (key, _) in entries.into_iter().take(excess) {
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
        let affected: Vec<(String, String, std::path::PathBuf, String)> = inner
            .sessions
            .iter()
            .filter(|(_, entry)| entry.slot_index == slot_index)
            .map(|(ext, entry)| {
                (
                    ext.clone(),
                    entry.backend_sid.clone(),
                    entry.cwd.clone(),
                    entry.last_sender.clone(),
                )
            })
            .collect();

        for (ext_sid, backend_sid, cwd, last_sender) in &affected {
            inner.sessions.remove(ext_sid);
            inner.reverse.remove(backend_sid);
            inner.prompt_senders.remove(ext_sid);
            inner.evicted.insert(
                ext_sid.clone(),
                EvictedSession {
                    backend_sid: backend_sid.clone(),
                    cwd: cwd.clone(),
                    evicted_at: Instant::now(),
                    last_sender: last_sender.clone(),
                },
            );
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
            if pid > i32::MAX as u32 {
                warn!(slot_index = i, pid, "PID exceeds i32::MAX, skipping SIGTERM");
                continue;
            }
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
    pub async fn spawn_backend_process(
        &self,
        service: &GatewayService,
        slot_index: usize,
    ) -> Result<(), String> {
        let agent_cmd = self.inner.borrow().config.agent_cmd.clone();
        crate::spawn::spawn_backend(service, &agent_cmd, slot_index)
            .await
            .map_err(|e| e.to_string())
    }

    /// Replay the stored InitializeRequest to a backend at `slot_index`.
    /// No-op if no init request has been stored yet.
    pub async fn replay_init(&self, slot_index: usize) -> Result<(), String> {
        let init_req = self.inner.borrow().init_request.clone();
        if let Some(req) = init_req {
            let conn = self.backend_conn(slot_index)
                .map_err(|e| format!("backend_conn for init replay: {e}"))?;
            conn.initialize(req).await
                .map_err(|e| format!("initialize backend: {e}"))?;
        }
        Ok(())
    }

    /// Spawn a dedicated backend process into the given slot and initialize it.
    ///
    /// This is used by Dedicated mode to spawn a backend on demand for
    /// `new_session` and session reload. The stored InitializeRequest is
    /// replayed to the new backend before it receives any other calls.
    pub async fn spawn_dedicated_backend(
        &self,
        service: &GatewayService,
        slot_index: usize,
    ) -> Result<(), String> {
        self.spawn_backend_process(service, slot_index).await?;
        self.replay_init(slot_index).await
    }

    /// Check if a slot has a backend (as opposed to being empty/None).
    pub fn is_slot_populated(&self, index: usize) -> bool {
        let inner = self.inner.borrow();
        inner.slots.get(index).is_some_and(|s| s.is_some())
    }

    /// Return pool health/status as a JSON value.
    pub fn pool_status(&self) -> serde_json::Value {
        let inner = self.inner.borrow();
        let mut slot_statuses = Vec::new();
        let mut alive = 0usize;
        let mut dead = 0usize;
        let mut empty = 0usize;
        for (i, slot) in inner.slots.iter().enumerate() {
            match slot {
                Some(s) if s.alive => {
                    alive += 1;
                    slot_statuses.push(serde_json::json!({
                        "index": i,
                        "state": "alive",
                        "session_count": s.session_count,
                    }));
                }
                Some(s) => {
                    dead += 1;
                    slot_statuses.push(serde_json::json!({
                        "index": i,
                        "state": "dead",
                        "session_count": s.session_count,
                    }));
                }
                None => {
                    empty += 1;
                    slot_statuses.push(serde_json::json!({
                        "index": i,
                        "state": "empty",
                        "session_count": 0,
                    }));
                }
            }
        }
        serde_json::json!({
            "strategy": format!("{:?}", inner.config.strategy),
            "pool_size": inner.config.pool_size,
            "alive_slots": alive,
            "dead_slots": dead,
            "empty_slots": empty,
            "active_sessions": inner.sessions.len(),
            "evicted_sessions": inner.evicted.len(),
            "in_flight_prompts": inner.in_flight_prompts.len(),
            "slots": slot_statuses,
        })
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
// Graceful process termination
// ---------------------------------------------------------------------------

/// Gracefully terminate a child process: SIGTERM → 500ms wait → SIGKILL.
///
/// Spawns a background task so callers don't need to be async.
fn graceful_kill(mut child: Child) {
    // Send SIGTERM to the process group.
    if let Some(pid) = child.id() {
        if pid <= i32::MAX as u32 {
            let pgid = -(pid as i32);
            debug!(pid, "sending SIGTERM to dedicated backend process group");
            unsafe {
                libc::kill(pgid as libc::pid_t, libc::SIGTERM);
            }
        } else {
            warn!(pid, "PID exceeds i32::MAX, falling back to kill_on_drop");
            return; // Drop child, triggering kill_on_drop.
        }
    } else {
        // Process already exited.
        return;
    }

    tokio::task::spawn_local(async move {
        // Wait up to 500ms for graceful exit.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match child.try_wait() {
            Ok(Some(_)) => {
                debug!("dedicated backend exited gracefully after SIGTERM");
            }
            _ => {
                debug!("dedicated backend did not exit, sending SIGKILL");
                let _ = child.kill().await;
            }
        }
    });
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

    // -- Session removal --

    #[test]
    fn test_remove_session_lc() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());
        pool.register_session("ext-2", "back-2", 0, "/test".into());
        assert_eq!(pool.session_count(), 2);

        pool.remove_session("ext-1");
        assert_eq!(pool.session_count(), 1);
        assert!(pool.resolve_session("ext-1").is_none());
        assert!(pool.resolve_session("ext-2").is_some());
        // Slot should still be populated (LC doesn't free slots).
        assert!(pool.is_slot_populated(0));
    }

    #[test]
    fn test_remove_session_dedicated_frees_slot() {
        let pool = make_pool(Strategy::Dedicated, 2);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());

        pool.remove_session("ext-1");
        assert_eq!(pool.session_count(), 0);
        // Dedicated mode should free the slot.
        assert!(!pool.is_slot_populated(0));
    }

    #[test]
    fn test_remove_session_clears_prompt_guard() {
        let pool = make_pool(Strategy::LeastConnections, 1);
        pool.insert_slot(0, make_mock_slot(true));
        pool.register_session("ext-1", "back-1", 0, "/test".into());
        pool.begin_prompt("ext-1").unwrap();

        pool.remove_session("ext-1");
        assert!(!pool.is_prompt_in_flight("ext-1"));
    }

    // -- Pool status --

    #[test]
    fn test_pool_status() {
        let pool = make_pool(Strategy::LeastConnections, 3);
        pool.insert_slot(0, make_mock_slot(true));
        pool.insert_slot(1, make_mock_slot(false)); // dead
        // slot 2 is empty (None)
        pool.register_session("ext-1", "back-1", 0, "/test".into());

        let status = pool.pool_status();
        assert_eq!(status["alive_slots"], 1);
        assert_eq!(status["dead_slots"], 1);
        assert_eq!(status["empty_slots"], 1);
        assert_eq!(status["active_sessions"], 1);
        assert_eq!(status["pool_size"], 3);
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
