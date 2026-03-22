use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::{self as acp};
use tracing::warn;
use uuid::Uuid;

use crate::policy::PolicyEngine;
use crate::pool::AgentPool;

#[derive(Clone)]
pub struct GatewayService {
    inner: Rc<GatewayServiceInner>,
}

struct GatewayServiceInner {
    pool: AgentPool,
    policy: Option<Arc<PolicyEngine>>,
    frontends: RefCell<HashMap<String, acp::AgentSideConnection>>,
}

impl GatewayService {
    pub fn new(pool: AgentPool, policy: Option<Arc<PolicyEngine>>) -> Self {
        Self {
            inner: Rc::new(GatewayServiceInner {
                pool,
                policy,
                frontends: RefCell::new(HashMap::new()),
            }),
        }
    }

    pub fn pool(&self) -> AgentPool {
        self.inner.pool.clone()
    }

    pub fn policy(&self) -> Option<Arc<PolicyEngine>> {
        self.inner.policy.clone()
    }

    pub fn next_frontend_id(&self) -> String {
        Uuid::now_v7().to_string()
    }

    pub fn register_frontend(&self, frontend_id: impl Into<String>, conn: acp::AgentSideConnection) {
        self.inner.frontends.borrow_mut().insert(frontend_id.into(), conn);
    }

    pub fn unregister_frontend(&self, frontend_id: &str) {
        self.inner.frontends.borrow_mut().remove(frontend_id);
    }

    pub fn frontend_conn(
        &self,
        frontend_id: &str,
    ) -> Result<&acp::AgentSideConnection, acp::Error> {
        let frontends = self.inner.frontends.borrow();
        let conn = frontends
            .get(frontend_id)
            .ok_or_else(|| acp::Error::internal_error().data(format!("frontend disconnected: {frontend_id}")))?;
        Ok(unsafe { &*(conn as *const acp::AgentSideConnection) })
    }

    pub fn frontend_for_session(
        &self,
        external_sid: &str,
    ) -> Result<&acp::AgentSideConnection, acp::Error> {
        let frontend_id = self
            .inner
            .pool
            .route_frontend_for_session(external_sid)
            .ok_or_else(|| acp::Error::internal_error().data(format!("no frontend route for session: {external_sid}")))?;
        self.frontend_conn(&frontend_id)
    }

    pub fn frontend_for_backend_session(
        &self,
        backend_sid: &str,
    ) -> Result<(&acp::AgentSideConnection, acp::SessionId), acp::Error> {
        let external_sid = self
            .inner
            .pool
            .external_session_for_backend(backend_sid)
            .ok_or_else(|| acp::Error::internal_error().data(format!("unknown backend session route: {backend_sid}")))?;
        let conn = self.frontend_for_session(&external_sid)?;
        Ok((conn, acp::SessionId::new(external_sid)))
    }

    pub fn frontend_for_unscoped_callback(&self) -> Result<&acp::AgentSideConnection, acp::Error> {
        let frontends = self.inner.frontends.borrow();
        match frontends.len() {
            1 => {
                let conn = frontends.values().next().expect("single frontend exists");
                Ok(unsafe { &*(conn as *const acp::AgentSideConnection) })
            }
            0 => Err(acp::Error::internal_error().data("no frontend connections are registered")),
            _ => {
                warn!(frontend_count = frontends.len(), "unscoped backend callback is ambiguous across multiple frontends");
                Err(acp::Error::internal_error().data("unscoped backend callback is ambiguous across multiple frontends"))
            }
        }
    }
}
