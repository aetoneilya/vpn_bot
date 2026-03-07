use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use teloxide::types::ChatId;
use tokio::sync::Mutex;

use crate::config::AppConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    approvals: Arc<ApprovalsState>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            approvals: Arc::new(ApprovalsState::new()),
        }
    }

    pub async fn create_request(&self, request: PendingCreateRequest) -> u64 {
        self.approvals.create_request(request).await
    }

    pub async fn take_request(&self, request_id: u64) -> Option<PendingCreateRequest> {
        self.approvals.take_request(request_id).await
    }
}

struct ApprovalsState {
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingCreateRequest>>,
}

impl ApprovalsState {
    fn new() -> Self {
        Self {
            next_request_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    async fn create_request(&self, request: PendingCreateRequest) -> u64 {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending.lock().await;
        pending.insert(request_id, request);
        request_id
    }

    async fn take_request(&self, request_id: u64) -> Option<PendingCreateRequest> {
        let mut pending = self.pending.lock().await;
        pending.remove(&request_id)
    }
}

#[derive(Clone, Debug)]
pub struct PendingCreateRequest {
    pub requester_chat_id: ChatId,
    pub requester_user_id: u64,
    pub custom_email: Option<String>,
}
