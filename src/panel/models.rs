use serde::Deserialize;
use serde_json::Value;

/// Envelope of every 3x-ui API response.
#[derive(Debug, Deserialize)]
pub(super) struct ApiResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub msg: String,
    #[serde(default)]
    pub obj: Value,
}

/// A client as returned by `/panel/api/clients/list`. The panel calls the login `email`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    #[serde(rename = "email")]
    pub login: String,
    #[serde(default)]
    pub sub_id: String,
    #[serde(default)]
    pub tg_id: i64,
    #[serde(default, rename = "enable")]
    pub enabled: bool,
    /// Unix millis; 0 or negative means no expiry.
    #[serde(default)]
    pub expiry_time: i64,
    #[serde(default)]
    pub inbound_ids: Vec<i64>,
}

impl Client {
    pub fn telegram_chat_id(&self) -> Option<i64> {
        (self.tg_id > 0).then_some(self.tg_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Inbound {
    pub id: i64,
    #[serde(default)]
    pub remark: String,
    #[serde(default, rename = "enable")]
    pub enabled: bool,
    #[serde(default)]
    pub up: u64,
    #[serde(default)]
    pub down: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ServerStatus {
    pub cpu_percent: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub uptime_secs: u64,
    pub xray_state: Option<String>,
}

impl ServerStatus {
    pub(super) fn from_value(obj: &Value) -> Self {
        Self {
            cpu_percent: obj["cpu"].as_f64().unwrap_or_default(),
            mem_used: obj["mem"]["current"].as_u64().unwrap_or_default(),
            mem_total: obj["mem"]["total"].as_u64().unwrap_or_default(),
            disk_used: obj["disk"]["current"].as_u64().unwrap_or_default(),
            disk_total: obj["disk"]["total"].as_u64().unwrap_or_default(),
            uptime_secs: obj["uptime"].as_u64().unwrap_or_default(),
            xray_state: obj["xray"]["state"].as_str().map(ToString::to_string),
        }
    }
}
