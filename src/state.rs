use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use teloxide::types::ChatId;

use crate::config::AppConfig;
use crate::storage::{InsertPendingResult, SqliteStore, StoredPendingRequest};

pub struct AppState {
    pub config: AppConfig,
    store: Arc<SqliteStore>,
    meme_mode_users: Mutex<HashSet<u64>>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Result<Self> {
        let store = SqliteStore::new(&config.sqlite_path)?;
        Ok(Self {
            config,
            store: Arc::new(store),
            meme_mode_users: Mutex::new(HashSet::new()),
        })
    }

    pub fn create_request(&self, request: PendingCreateRequest) -> Result<InsertPendingResult> {
        self.store.insert_request(&request)
    }

    pub fn take_request(&self, request_id: u64) -> Result<Option<PendingCreateRequest>> {
        self.store.take_request(request_id)
    }

    pub fn list_requests(&self) -> Result<Vec<StoredPendingRequest>> {
        self.store.list_requests()
    }

    pub fn arm_meme_mode(&self, user_id: u64) -> Result<()> {
        let mut users = self
            .meme_mode_users
            .lock()
            .map_err(|_| anyhow::anyhow!("meme mode mutex poisoned"))?;
        users.insert(user_id);
        Ok(())
    }

    pub fn consume_meme_mode(&self, user_id: u64) -> Result<bool> {
        let mut users = self
            .meme_mode_users
            .lock()
            .map_err(|_| anyhow::anyhow!("meme mode mutex poisoned"))?;
        Ok(users.remove(&user_id))
    }
}

#[derive(Clone, Debug)]
pub struct PendingCreateRequest {
    pub requester_chat_id: ChatId,
    pub requester_user_id: u64,
    pub custom_email: Option<String>,
}
