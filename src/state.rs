use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::Result;

use crate::config::AppConfig;
use crate::health::HealthSnapshot;
use crate::panel::Panel;
use crate::storage::Store;

pub struct AppState {
    pub config: AppConfig,
    pub panel: Panel,
    pub store: Store,
    /// Users who ran /meme and whose next media message goes to the admins.
    meme_armed: Mutex<HashSet<u64>>,
    /// Latest health check results, `None` until the first run.
    health: Mutex<Option<HealthSnapshot>>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Result<Self> {
        Ok(Self {
            panel: Panel::new(config.panel.clone())?,
            store: Store::open(&config.sqlite_path)?,
            config,
            meme_armed: Mutex::new(HashSet::new()),
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
}
