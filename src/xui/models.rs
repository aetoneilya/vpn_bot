use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub(crate) struct XuiApiResponse {
    pub(crate) success: Option<bool>,
    pub(crate) msg: Option<String>,
    pub(crate) obj: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct CreatedClientResult {
    pub summary: String,
    pub connection_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExistingSubscription {
    pub client_id: String,
    pub email: String,
    pub sub_id: Option<String>,
    pub tg_id: Option<String>,
    pub enabled: bool,
    pub expiry_time: i64,
    pub inbound_remark: String,
    pub inbound_id: i64,
}
