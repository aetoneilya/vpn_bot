use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::config::AppConfig;

#[derive(Clone)]
pub struct XuiClient {
    http: Client,
    config: AppConfig,
}

#[derive(Debug, Deserialize)]
struct XuiApiResponse {
    success: Option<bool>,
    msg: Option<String>,
    obj: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct CreatedClientResult {
    pub summary: String,
    pub connection_url: Option<String>,
}

impl XuiClient {
    pub fn new(config: AppConfig) -> Result<Self> {
        let http = Client::builder()
            .no_proxy()
            .cookie_store(true)
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self { http, config })
    }

    pub async fn login(&self) -> Result<()> {
        let url = join_url(&self.config.xui_base_url, &self.config.xui_login_path);

        let form_resp = self
            .http
            .post(&url)
            .form(&[
                ("username", self.config.xui_username.as_str()),
                ("password", self.config.xui_password.as_str()),
            ])
            .send()
            .await
            .context("x-ui login request failed")?;

        let status = form_resp.status();
        let body = form_resp.text().await.context("x-ui login body failed")?;

        if status.is_success() && !looks_like_api_error(&body)? {
            return Ok(());
        }

        let json_resp = self
            .http
            .post(&url)
            .json(&json!({
                "username": self.config.xui_username,
                "password": self.config.xui_password,
            }))
            .send()
            .await
            .context("x-ui login request (json) failed")?;

        let status = json_resp.status();
        let body = json_resp
            .text()
            .await
            .context("x-ui login body (json) failed")?;

        if !status.is_success() {
            bail!("x-ui login failed with status {}: {}", status, body);
        }

        if looks_like_api_error(&body)? {
            bail!("x-ui login failed: {}", body);
        }

        Ok(())
    }

    pub async fn add_client(
        &self,
        telegram_user_id: u64,
        custom_email: Option<&str>,
    ) -> Result<CreatedClientResult> {
        let url = join_url(&self.config.xui_base_url, &self.config.xui_add_client_path);
        let client_uuid = Uuid::new_v4().to_string();
        let sub_id = Uuid::new_v4().to_string();
        let email = custom_email
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("tg_{telegram_user_id}"));

        let expiry_time_ms = if self.config.xui_expiry_days <= 0 {
            0
        } else {
            (Utc::now() + Duration::days(self.config.xui_expiry_days)).timestamp_millis()
        };

        let settings = json!({
            "clients": [
                {
                    "id": client_uuid,
                    "flow": "",
                    "email": email,
                    "limitIp": 0,
                    "totalGB": self.config.xui_total_gb * 1024 * 1024 * 1024,
                    "expiryTime": expiry_time_ms,
                    "enable": true,
                    "tgId": telegram_user_id.to_string(),
                    "subId": sub_id,
                    "reset": 0
                }
            ]
        });

        let payload = json!({
            "id": self.config.xui_inbound_id,
            "settings": settings.to_string(),
        });

        let response = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("x-ui addClient request failed")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("x-ui addClient body failed")?;

        if !status.is_success() {
            bail!("x-ui addClient failed with status {}: {}", status, body);
        }

        let parsed: XuiApiResponse = serde_json::from_str(&body).unwrap_or(XuiApiResponse {
            success: None,
            msg: None,
            obj: None,
        });

        if let Some(false) = parsed.success {
            let msg = parsed
                .msg
                .unwrap_or_else(|| "unknown x-ui error".to_string());
            bail!("x-ui addClient unsuccessful: {msg}");
        }

        let mut connection_url = parsed
            .obj
            .as_ref()
            .and_then(|obj| find_best_connection_url(obj, &email, &client_uuid, &sub_id));

        if connection_url.is_none() {
            connection_url = self
                .fetch_connection_url_from_server(&email, &client_uuid, &sub_id)
                .await;
        }

        let summary = if let Some(obj) = parsed.obj {
            format!(
                "Created client `{email}` (`{client_uuid}`)\\nPanel response: `{}`",
                compact_json(&obj)
            )
        } else {
            format!("Created client `{email}` (`{client_uuid}`).")
        };

        Ok(CreatedClientResult {
            summary,
            connection_url,
        })
    }

    async fn fetch_connection_url_from_server(
        &self,
        email: &str,
        client_uuid: &str,
        sub_id: &str,
    ) -> Option<String> {
        let get_inbound_url = self
            .config
            .xui_get_inbound_path
            .replace("{id}", &self.config.xui_inbound_id.to_string());
        let get_inbound_url = join_url(&self.config.xui_base_url, &get_inbound_url);
        if let Some(url) = self
            .extract_url_from_endpoint(&get_inbound_url, email, client_uuid, sub_id)
            .await
        {
            return Some(url);
        }

        let list_url = join_url(
            &self.config.xui_base_url,
            &self.config.xui_list_inbounds_path,
        );
        self.extract_url_from_endpoint(&list_url, email, client_uuid, sub_id)
            .await
    }

    async fn extract_url_from_endpoint(
        &self,
        endpoint_url: &str,
        email: &str,
        client_uuid: &str,
        sub_id: &str,
    ) -> Option<String> {
        let response = self.http.get(endpoint_url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.text().await.ok()?;
        let parsed = serde_json::from_str::<Value>(&body).ok()?;

        // Search full response first.
        if let Some(url) = find_best_connection_url(&parsed, email, client_uuid, sub_id) {
            return Some(url);
        }

        // Also support standard { success, obj, msg } wrappers.
        let wrapped = serde_json::from_str::<XuiApiResponse>(&body).ok()?;
        wrapped.obj.as_ref().and_then(|obj| {
            find_best_connection_url(obj, email, client_uuid, sub_id).or_else(|| {
                generate_connection_url_from_server_obj(
                    obj,
                    &self.config.xui_base_url,
                    self.config.xui_inbound_id,
                    email,
                    client_uuid,
                )
            })
        })
    }
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn looks_like_api_error(body: &str) -> Result<bool> {
    let parsed: Result<XuiApiResponse, _> = serde_json::from_str(body);
    match parsed {
        Ok(resp) => Ok(matches!(resp.success, Some(false))),
        Err(_) => Ok(false),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn find_best_connection_url(
    value: &Value,
    email: &str,
    client_uuid: &str,
    sub_id: &str,
) -> Option<String> {
    let mut matches = Vec::new();
    collect_connection_urls(value, &mut matches);
    if matches.is_empty() {
        return None;
    }

    let mut scored: Vec<(i32, String)> = matches
        .into_iter()
        .map(|candidate| {
            let mut score = 0;
            if candidate.contains(client_uuid) {
                score += 4;
            }
            if candidate.contains(email) {
                score += 2;
            }
            if candidate.contains(sub_id) {
                score += 1;
            }
            (score, candidate)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().next().map(|(_, url)| url)
}

fn collect_connection_urls(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if is_connection_url_candidate(s) {
                out.push(s.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_connection_urls(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["subUrl", "subscription", "subscriptionUrl", "url", "link"] {
                if let Some(value) = map.get(key) {
                    collect_connection_urls(value, out);
                }
            }
            for value in map.values() {
                collect_connection_urls(value, out);
            }
        }
        _ => {}
    }
}

fn is_connection_url_candidate(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    lower.starts_with("vless://")
        || lower.starts_with("vmess://")
        || lower.starts_with("trojan://")
        || lower.starts_with("ss://")
        || lower.starts_with("hysteria://")
        || lower.starts_with("tuic://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
}

fn generate_connection_url_from_server_obj(
    obj: &Value,
    base_url: &str,
    inbound_id: i64,
    email: &str,
    client_uuid: &str,
) -> Option<String> {
    match obj {
        Value::Array(items) => items.iter().find_map(|v| {
            generate_connection_url_from_server_obj(v, base_url, inbound_id, email, client_uuid)
        }),
        Value::Object(map) => {
            if let Some(inner_obj) = map.get("obj") {
                return generate_connection_url_from_server_obj(
                    inner_obj,
                    base_url,
                    inbound_id,
                    email,
                    client_uuid,
                );
            }

            if map.get("id").and_then(Value::as_i64) == Some(inbound_id)
                || map.get("protocol").is_some()
            {
                return generate_vless_link_from_inbound(map, base_url, email, client_uuid);
            }

            None
        }
        _ => None,
    }
}

fn generate_vless_link_from_inbound(
    inbound: &Map<String, Value>,
    base_url: &str,
    email: &str,
    client_uuid: &str,
) -> Option<String> {
    if inbound.get("protocol")?.as_str()? != "vless" {
        return None;
    }

    let settings = parse_json_field(inbound.get("settings")?)?;
    let stream = parse_json_field(inbound.get("streamSettings")?)?;

    let clients = settings.get("clients")?.as_array()?;
    let client = clients.iter().find(|c| {
        c.get("id").and_then(Value::as_str) == Some(client_uuid)
            || c.get("email").and_then(Value::as_str) == Some(email)
    })?;

    let address = inbound_address(inbound, base_url)?;
    let port = inbound.get("port")?.as_i64()?;
    let security = stream
        .get("security")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let network = stream
        .get("network")
        .and_then(Value::as_str)
        .unwrap_or("tcp");

    let uuid = client.get("id")?.as_str()?;
    let mut url = reqwest::Url::parse(&format!("vless://{uuid}@{address}:{port}")).ok()?;
    url.query_pairs_mut().append_pair("type", network);

    match security {
        "reality" => {
            url.query_pairs_mut().append_pair("security", "reality");
            let reality = stream.get("realitySettings")?;
            let settings = reality.get("settings")?;
            append_if_non_empty(
                &mut url,
                "pbk",
                settings.get("publicKey").and_then(Value::as_str),
            );
            append_if_non_empty(
                &mut url,
                "fp",
                settings.get("fingerprint").and_then(Value::as_str),
            );

            if let Some(sni) = first_string(reality.get("serverNames")) {
                append_if_non_empty(&mut url, "sni", Some(&sni));
            }
            if let Some(sid) = first_string(reality.get("shortIds")) {
                append_if_non_empty(&mut url, "sid", Some(&sid));
            }
            append_if_non_empty(
                &mut url,
                "spx",
                settings.get("spiderX").and_then(Value::as_str),
            );

            if network == "tcp" {
                append_if_non_empty(&mut url, "flow", client.get("flow").and_then(Value::as_str));
            }
        }
        "tls" => {
            url.query_pairs_mut().append_pair("security", "tls");
            if let Some(tls_settings) = stream.get("tlsSettings") {
                append_if_non_empty(
                    &mut url,
                    "fp",
                    tls_settings
                        .get("settings")
                        .and_then(|s| s.get("fingerprint"))
                        .and_then(Value::as_str),
                );
                append_if_non_empty(
                    &mut url,
                    "sni",
                    tls_settings.get("serverName").and_then(Value::as_str),
                );
            }
            if network == "tcp" {
                append_if_non_empty(&mut url, "flow", client.get("flow").and_then(Value::as_str));
            }
        }
        _ => {
            url.query_pairs_mut().append_pair("security", "none");
        }
    }

    let remark = inbound
        .get("remark")
        .and_then(Value::as_str)
        .unwrap_or("vpn")
        .to_string();
    let client_email = client
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or(email)
        .to_string();
    url.set_fragment(Some(&format!("{remark}-{client_email}")));

    Some(url.to_string())
}

fn parse_json_field(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        Value::Object(_) | Value::Array(_) => Some(value.clone()),
        _ => None,
    }
}

fn inbound_address(inbound: &Map<String, Value>, base_url: &str) -> Option<String> {
    let listen = inbound
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !listen.is_empty() && listen != "0.0.0.0" {
        return Some(listen.to_string());
    }
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

fn first_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .find_map(|v| v.as_str())
            .map(ToString::to_string),
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(ToString::to_string),
        _ => None,
    }
}

fn append_if_non_empty(url: &mut reqwest::Url, key: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
        url.query_pairs_mut().append_pair(key, v);
    }
}
