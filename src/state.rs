use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;

use crate::config::AppConfig;
use crate::geo::GeoDb;
use crate::health::HealthSnapshot;
use crate::panel::Panel;
use crate::storage::Store;

pub struct AppState {
    pub config: AppConfig,
    pub panel: Panel,
    pub store: Store,
    /// Offline IP geolocation; `None` when GEOIP_* is not configured.
    pub geo: Option<GeoDb>,
    /// Users who ran /meme and whose next media message goes to the admins.
    meme_armed: Mutex<HashSet<u64>>,
    /// Approver id -> complaint id they are currently writing a reply to.
    pending_replies: Mutex<HashMap<u64, u64>>,
    /// Latest health check results, `None` until the first run.
    health: Mutex<Option<HealthSnapshot>>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Result<Self> {
        Ok(Self {
            panel: Panel::new(config.panel.clone())?,
            store: Store::open(&config.sqlite_path)?,
            geo: config.geo.as_ref().map(GeoDb::open).transpose()?,
            config,
            meme_armed: Mutex::new(HashSet::new()),
            pending_replies: Mutex::new(HashMap::new()),
            health: Mutex::new(None),
        })
    }

    pub fn arm_meme(&self, user_id: u64) {
        self.meme_armed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(user_id);
    }

    /// Returns true (once) if the user armed meme mode.
    pub fn take_meme(&self, user_id: u64) -> bool {
        self.meme_armed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&user_id)
    }

    pub fn set_health(&self, snapshot: HealthSnapshot) {
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot);
    }

    pub fn health(&self) -> Option<HealthSnapshot> {
        self.health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn start_reply(&self, approver: u64, complaint: u64) {
        self.pending_replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(approver, complaint);
    }

    /// Returns (and forgets) the complaint the approver is replying to, if any.
    pub fn take_reply(&self, approver: u64) -> Option<u64> {
        self.pending_replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&approver)
    }
}
