use std::collections::HashSet;
use std::env;
use std::net::SocketAddr;

use anyhow::{Context, Result, anyhow, bail};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub panel: PanelConfig,
    pub sqlite_path: String,
    /// When set, only these Telegram users may request access.
    pub allow_user_ids: Option<HashSet<u64>>,
    pub approver_user_ids: HashSet<u64>,
    pub health: Option<HealthConfig>,
    pub web: Option<WebConfig>,
}

/// Telegram Mini App server; enabled when `WEB_PUBLIC_URL` is set.
#[derive(Clone, Debug)]
pub struct WebConfig {
    /// Local address the HTTP server binds to; TLS is terminated in front of it.
    pub listen: SocketAddr,
    /// Public HTTPS URL of the Mini App, as opened by Telegram.
    pub public_url: String,
}

#[derive(Clone, Debug)]
pub struct PanelConfig {
    /// Panel root including the secret web base path, e.g. `http://127.0.0.1:61563/<path>`.
    pub base_url: String,
    pub auth: PanelAuth,
    /// Every new client is attached to all of these inbounds.
    pub inbound_ids: Vec<i64>,
    /// VLESS flow for new clients: empty for XHTTP/gRPC, `xtls-rprx-vision` for raw TCP Reality.
    pub client_flow: String,
    /// Traffic limit for new clients in GB; 0 means unlimited.
    pub total_gb: u64,
    /// Public subscription base, e.g. `https://sub.example.com:2096/sub/`.
    pub subscription_base_url: String,
}

#[derive(Clone, Debug)]
pub enum PanelAuth {
    Token(String),
    Password { username: String, password: String },
}

/// End-to-end checks of the relay chain; disabled when `HEALTH_RELAY_ADDR` is unset.
#[derive(Clone, Debug)]
pub struct HealthConfig {
    pub relay_addr: SocketAddr,
    pub snis: Vec<String>,
    pub subscription_url: Option<String>,
    pub interval_secs: u64,
    pub fail_threshold: u32,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let approver_user_ids = parse_user_ids(&required_env("APPROVER_USER_IDS")?);
        if approver_user_ids.is_empty() {
            bail!("APPROVER_USER_IDS must contain at least one user id");
        }

        Ok(Self {
            panel: PanelConfig::from_env()?,
            sqlite_path: optional_env("SQLITE_PATH").unwrap_or_else(|| "vpn_bot.sqlite3".into()),
            allow_user_ids: optional_env("ALLOW_USER_IDS")
                .map(|v| parse_user_ids(&v))
                .filter(|ids| !ids.is_empty()),
            approver_user_ids,
            health: HealthConfig::from_env()?,
            web: WebConfig::from_env()?,
        })
    }

    pub fn is_approver(&self, user_id: u64) -> bool {
        self.approver_user_ids.contains(&user_id)
    }

    pub fn is_allowed(&self, user_id: u64) -> bool {
        self.allow_user_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(&user_id))
    }
}

impl PanelConfig {
    fn from_env() -> Result<Self> {
        let auth = match optional_env("XUI_API_TOKEN") {
            Some(token) => PanelAuth::Token(token),
            None => PanelAuth::Password {
                username: required_env("XUI_USERNAME")
                    .context("set XUI_API_TOKEN or XUI_USERNAME/XUI_PASSWORD")?,
                password: required_env("XUI_PASSWORD")
                    .context("set XUI_API_TOKEN or XUI_USERNAME/XUI_PASSWORD")?,
            },
        };

        let inbound_ids = parse_list::<i64>(&required_env("XUI_INBOUND_IDS")?)
            .context("XUI_INBOUND_IDS must be comma-separated integers")?;
        if inbound_ids.is_empty() {
            bail!("XUI_INBOUND_IDS must contain at least one inbound id");
        }

        let mut subscription_base_url = required_env("XUI_SUBSCRIPTION_BASE_URL")?;
        if !subscription_base_url.ends_with('/') {
            subscription_base_url.push('/');
        }

        Ok(Self {
            base_url: required_env("XUI_BASE_URL")?
                .trim_end_matches('/')
                .to_string(),
            auth,
            inbound_ids,
            client_flow: optional_env("XUI_CLIENT_FLOW").unwrap_or_default(),
            total_gb: parse_optional("XUI_TOTAL_GB")?.unwrap_or(0),
            subscription_base_url,
        })
    }
}

impl HealthConfig {
    fn from_env() -> Result<Option<Self>> {
        let Some(relay_addr) = optional_env("HEALTH_RELAY_ADDR") else {
            return Ok(None);
        };

        Ok(Some(Self {
            relay_addr: relay_addr
                .parse()
                .context("HEALTH_RELAY_ADDR must be ip:port")?,
            snis: parse_list::<String>(
                &optional_env("HEALTH_SNIS").unwrap_or_else(|| "ign.com".into()),
            )?,
            subscription_url: optional_env("HEALTH_SUBSCRIPTION_URL"),
            interval_secs: parse_optional("HEALTH_INTERVAL_SECS")?.unwrap_or(300),
            fail_threshold: parse_optional::<u32>("HEALTH_FAIL_THRESHOLD")?
                .unwrap_or(2)
                .max(1),
        }))
    }
}

impl WebConfig {
    fn from_env() -> Result<Option<Self>> {
        let Some(public_url) = optional_env("WEB_PUBLIC_URL") else {
            return Ok(None);
        };
        if !public_url.starts_with("https://") {
            bail!("WEB_PUBLIC_URL must be an https:// URL (Telegram requires it)");
        }
        Ok(Some(Self {
            listen: optional_env("WEB_LISTEN")
                .unwrap_or_else(|| "127.0.0.1:8080".into())
                .parse()
                .context("WEB_LISTEN must be ip:port")?,
            public_url,
        }))
    }
}

fn required_env(key: &str) -> Result<String> {
    optional_env(key).ok_or_else(|| anyhow!("{key} is not set"))
}

/// Reads a variable, trimming whitespace and surrounding quotes; empty values count as unset.
fn optional_env(key: &str) -> Option<String> {
    let raw = env::var(key).ok()?;
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim();
    (!unquoted.is_empty()).then(|| unquoted.to_string())
}

fn parse_optional<T: std::str::FromStr>(key: &str) -> Result<Option<T>> {
    optional_env(key)
        .map(|v| {
            v.parse::<T>()
                .map_err(|_| anyhow!("{key} has invalid value `{v}`"))
        })
        .transpose()
}

fn parse_list<T: std::str::FromStr>(raw: &str) -> Result<Vec<T>> {
    raw.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.parse::<T>()
                .map_err(|_| anyhow!("invalid list item `{v}`"))
        })
        .collect()
}

fn parse_user_ids(raw: &str) -> HashSet<u64> {
    raw.split(',')
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .collect()
}
