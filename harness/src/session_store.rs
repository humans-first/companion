use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::Mutex;

use crate::error::HarnessError;
use crate::session::SessionData;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStoreCapabilities {
    pub list: bool,
    pub load: bool,
    pub resume: bool,
    pub fork: bool,
    pub close: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListing {
    pub id: String,
    pub cwd: PathBuf,
}

#[async_trait::async_trait(?Send)]
pub trait SessionStore {
    fn capabilities(&self) -> SessionStoreCapabilities;

    async fn create(&self, id: &str, data: SessionData) -> Result<(), HarnessError>;

    async fn load(&self, id: &str) -> Result<SessionData, HarnessError>;

    async fn save(&self, id: &str, data: SessionData) -> Result<(), HarnessError>;

    async fn delete(&self, id: &str) -> Result<(), HarnessError>;

    async fn list(&self) -> Result<Vec<SessionListing>, HarnessError>;
}

pub struct InMemorySessionStore {
    sessions: Mutex<HashMap<String, SessionData>>,
    capabilities: SessionStoreCapabilities,
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            capabilities: SessionStoreCapabilities {
                list: true,
                load: true,
                resume: true,
                fork: true,
                close: true,
            },
        }
    }

    pub fn with_capabilities(capabilities: SessionStoreCapabilities) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            capabilities,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SessionStore for InMemorySessionStore {
    fn capabilities(&self) -> SessionStoreCapabilities {
        self.capabilities
    }

    async fn create(&self, id: &str, data: SessionData) -> Result<(), HarnessError> {
        self.sessions.lock().await.insert(id.to_string(), data);
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<SessionData, HarnessError> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownSession(id.to_string()))
    }

    async fn save(&self, id: &str, data: SessionData) -> Result<(), HarnessError> {
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(id) {
            return Err(HarnessError::UnknownSession(id.to_string()));
        }
        sessions.insert(id.to_string(), data);
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), HarnessError> {
        let removed = self.sessions.lock().await.remove(id);
        if removed.is_some() {
            Ok(())
        } else {
            Err(HarnessError::UnknownSession(id.to_string()))
        }
    }

    async fn list(&self) -> Result<Vec<SessionListing>, HarnessError> {
        let sessions = self.sessions.lock().await;
        let mut listings = sessions
            .iter()
            .map(|(id, data)| SessionListing {
                id: id.clone(),
                cwd: data.cwd.clone(),
            })
            .collect::<Vec<_>>();
        listings.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(listings)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::session::{ConversationMessage, SessionData};

    fn session_data() -> SessionData {
        SessionData::new(PathBuf::from("/tmp"), "code", "test-model")
    }

    #[tokio::test]
    async fn create_load_save_and_delete_round_trip() {
        let store = InMemorySessionStore::new();
        store.create("s1", session_data()).await.unwrap();

        let mut loaded = store.load("s1").await.unwrap();
        loaded
            .history
            .push(ConversationMessage::user("hello".to_string()));
        store.save("s1", loaded.clone()).await.unwrap();

        let loaded = store.load("s1").await.unwrap();
        assert_eq!(loaded.history.len(), 1);

        store.delete("s1").await.unwrap();
        assert!(matches!(
            store.load("s1").await,
            Err(HarnessError::UnknownSession(_))
        ));
    }

    #[tokio::test]
    async fn list_returns_sorted_sessions() {
        let store = InMemorySessionStore::new();
        store.create("b", session_data()).await.unwrap();
        store.create("a", session_data()).await.unwrap();

        let listings = store.list().await.unwrap();
        assert_eq!(listings.len(), 2);
        assert_eq!(listings[0].id, "a");
        assert_eq!(listings[1].id, "b");
    }

    #[test]
    fn exposes_configured_capabilities() {
        let capabilities = SessionStoreCapabilities {
            list: true,
            load: false,
            resume: false,
            fork: true,
            close: false,
        };
        let store = InMemorySessionStore::with_capabilities(capabilities);
        assert_eq!(store.capabilities(), capabilities);
    }
}
