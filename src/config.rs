use std::collections::HashSet;
use std::env;

use anyhow::{Context, Result, anyhow, bail};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub xui_base_url: String,
    pub xui_username: String,
    pub xui_password: String,
    pub xui_inbound_id: i64,
    pub xui_total_gb: u64,
    pub xui_login_path: String,
    pub xui_add_client_path: String,
    pub xui_delete_client_path: String,
    pub xui_get_inbound_path: String,
    pub xui_list_inbounds_path: String,
    pub sqlite_path: String,
    pub allow_user_ids: Option<HashSet<u64>>,
    pub approver_user_ids: HashSet<u64>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let xui_base_url = required_env("XUI_BASE_URL")?;
        let xui_username = required_env("XUI_USERNAME")?;
        let xui_password = required_env("XUI_PASSWORD")?;
        let xui_inbound_id = required_env("XUI_INBOUND_ID")?
            .parse::<i64>()
            .context("XUI_INBOUND_ID must be an integer")?;

        let xui_total_gb = env::var("XUI_TOTAL_GB")
            .unwrap_or_else(|_| "0".to_string())
            .parse::<u64>()
            .context("XUI_TOTAL_GB must be an integer")?;

        let xui_login_path = env::var("XUI_LOGIN_PATH").unwrap_or_else(|_| "/login".to_string());
        let xui_add_client_path = env::var("XUI_ADD_CLIENT_PATH")
            .unwrap_or_else(|_| "/panel/api/inbounds/addClient".to_string());
        let xui_delete_client_path = env::var("XUI_DELETE_CLIENT_PATH")
            .unwrap_or_else(|_| "/panel/api/inbounds/{id}/delClient/{clientId}".to_string());
        let xui_get_inbound_path = env::var("XUI_GET_INBOUND_PATH")
            .unwrap_or_else(|_| "/panel/api/inbounds/get/{id}".to_string());
        let xui_list_inbounds_path = env::var("XUI_LIST_INBOUNDS_PATH")
            .unwrap_or_else(|_| "/panel/api/inbounds/list".to_string());
        let sqlite_path = env::var("SQLITE_PATH").unwrap_or_else(|_| "vpn_bot.sqlite3".to_string());

        let allow_user_ids = parse_user_ids(optional_env("ALLOW_USER_IDS"));
        let approver_user_ids = parse_user_ids(Some(required_env("APPROVER_USER_IDS")?))
            .ok_or_else(|| anyhow!("APPROVER_USER_IDS must contain at least one user id"))?;

        Ok(Self {
            xui_base_url,
            xui_username,
            xui_password,
            xui_inbound_id,
            xui_total_gb,
            xui_login_path,
            xui_add_client_path,
            xui_delete_client_path,
            xui_get_inbound_path,
            xui_list_inbounds_path,
            sqlite_path,
            allow_user_ids,
            approver_user_ids,
        })
    }
}

pub fn required_env(key: &str) -> Result<String> {
    let raw = env::var(key).with_context(|| format!("{key} is not set"))?;
    normalize_env_value(raw)
}

pub fn optional_env(key: &str) -> Option<String> {
    env::var(key).ok().and_then(|v| normalize_env_value(v).ok())
}

fn normalize_env_value(value: String) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("environment value is empty");
    }

    let is_quoted = trimmed.len() >= 2
        && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"')));

    let unquoted = if is_quoted {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    Ok(unquoted.trim().to_string())
}

fn parse_user_ids(value: Option<String>) -> Option<HashSet<u64>> {
    let raw = value?;
    let ids: HashSet<u64> = raw
        .split(',')
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .collect();
    if ids.is_empty() { None } else { Some(ids) }
}

pub fn is_allowed(user_id: u64, allowlist: &Option<HashSet<u64>>) -> bool {
    match allowlist {
        Some(ids) => ids.contains(&user_id),
        None => true,
    }
}
