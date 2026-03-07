use std::sync::Arc;

use anyhow::Result;
use teloxide::types::ChatId;

use crate::config::AppConfig;
use crate::storage::{InsertPendingResult, SqliteStore, StoredPendingRequest};

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    store: Arc<SqliteStore>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Result<Self> {
        let store = SqliteStore::new(&config.sqlite_path)?;
        Ok(Self {
            config,
            store: Arc::new(store),
        })
    }

    pub async fn create_request(
        &self,
        request: PendingCreateRequest,
    ) -> Result<InsertPendingResult> {
        self.store.insert_request(&request)
    }

    pub async fn take_request(&self, request_id: u64) -> Result<Option<PendingCreateRequest>> {
        self.store.take_request(request_id)
    }

    pub async fn list_requests(&self) -> Result<Vec<StoredPendingRequest>> {
        self.store.list_requests()
    }
}

#[derive(Clone, Debug)]
pub struct PendingCreateRequest {
    pub requester_chat_id: ChatId,
    pub requester_user_id: u64,
    pub custom_email: Option<String>,
}
