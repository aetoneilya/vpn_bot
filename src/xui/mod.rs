mod links;
mod models;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

use crate::config::AppConfig;
use links::{
    collect_inbound_subscriptions, find_best_connection_url,
    generate_connection_url_from_server_obj,
};
use models::XuiApiResponse;
pub use models::{CreatedClientResult, ExistingSubscription};

#[derive(Clone)]
pub struct XuiClient {
    http: Client,
    config: AppConfig,
}

impl XuiClient {
    pub fn new(config: AppConfig) -> Result<Self> {
        log::debug!(
            "creating xui client base_url={} inbound_id={}",
            config.xui_base_url,
            config.xui_inbound_id
        );
        let http = Client::builder()
            .no_proxy()
            .cookie_store(true)
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self { http, config })
    }

    pub async fn login(&self) -> Result<()> {
        let mut paths = vec![self.config.xui_login_path.clone()];
        for fallback in ["/login", "/panel/login", "/xui/login"] {
            if !paths.iter().any(|p| p == fallback) {
                paths.push(fallback.to_string());
            }
        }

        let mut tried = HashSet::new();
        let mut last_err: Option<anyhow::Error> = None;
        for path in paths {
            if !tried.insert(path.clone()) {
                continue;
            }
            match self.login_with_path(&path).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let msg = err.to_string();
                    // Only fallback on 404-style path errors; otherwise return immediately.
                    if msg.contains("404 Not Found") || msg.contains("status 404") {
                        log::warn!("x-ui login path {} returned 404, trying fallback", path);
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("x-ui login failed on all fallback paths")))
    }

    async fn login_with_path(&self, login_path: &str) -> Result<()> {
        let url = join_url(&self.config.xui_base_url, login_path);
        log::info!("x-ui login request to {}", url);

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

        if status.is_success()
            && let Ok(api) = serde_json::from_str::<XuiApiResponse>(&body)
        {
            if api.success == Some(true) {
                log::info!("x-ui login successful (form)");
                return Ok(());
            }
            if api.success == Some(false) {
                bail!(
                    "x-ui login failed: {}",
                    api.msg.unwrap_or_else(|| "unknown x-ui error".to_string())
                );
            }
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
        log::info!("x-ui login successful (json)");
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
        log::info!(
            "x-ui add client request inbound_id={} tg_user_id={} email={}",
            self.config.xui_inbound_id,
            telegram_user_id,
            email
        );

        // Access is always issued without expiration.
        let expiry_time_ms = 0;

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

        let mut connection_url = self.find_client_subscription_url_by_email(&email).await?;

        if connection_url.is_none() {
            connection_url = parsed
                .obj
                .as_ref()
                .and_then(|obj| find_best_connection_url(obj, &email, &client_uuid, &sub_id));
        }

        if connection_url.is_none() {
            log::debug!("x-ui addClient did not return URL, using server endpoint fallback");
            connection_url = self
                .fetch_connection_url_from_server(&email, &client_uuid, &sub_id)
                .await;
        }
        log::info!(
            "x-ui add client done email={} connection_url_found={}",
            email,
            connection_url.is_some()
        );

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

    pub async fn find_client_connection_url_by_email(&self, email: &str) -> Result<Option<String>> {
        if let Some(subscription_url) = self.find_client_subscription_url_by_email(email).await? {
            return Ok(Some(subscription_url));
        }

        let get_inbound_url = self.get_inbound_url();
        if let Some(url) = self
            .extract_specific_client_url(&get_inbound_url, email)
            .await
        {
            return Ok(Some(url));
        }

        let list_url = self.list_inbounds_url();
        Ok(self.extract_specific_client_url(&list_url, email).await)
    }

    async fn find_client_subscription_url_by_email(&self, email: &str) -> Result<Option<String>> {
        let sub_base = match self.fetch_subscription_base_url().await? {
            Some(v) => v,
            None => return Ok(None),
        };

        let email_norm = normalize_login(email);
        let subs = self.list_existing_subscriptions().await?;
        let sub_id = subs
            .into_iter()
            .find(|s| normalize_login(&s.email) == email_norm)
            .and_then(|s| s.sub_id)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        Ok(sub_id.map(|id| format!("{sub_base}{id}")))
    }

    pub async fn list_existing_subscriptions(&self) -> Result<Vec<ExistingSubscription>> {
        let list_url = self.list_inbounds_url();
        log::debug!("requesting subscriptions from {}", list_url);

        let response = self
            .http
            .get(&list_url)
            .send()
            .await
            .context("x-ui list inbounds request failed")?;

        if !response.status().is_success() {
            bail!(
                "x-ui list inbounds failed with status {}",
                response.status()
            );
        }

        let body = response
            .text()
            .await
            .context("x-ui list inbounds body failed")?;
        let wrapped: XuiApiResponse =
            serde_json::from_str(&body).context("x-ui list inbounds invalid json")?;
        let obj = wrapped.obj.unwrap_or(Value::Null);

        let mut out = Vec::new();
        if let Value::Array(inbounds) = obj {
            for inbound in inbounds {
                collect_inbound_subscriptions(&inbound, &mut out);
            }
        } else {
            collect_inbound_subscriptions(&obj, &mut out);
        }

        Ok(out)
    }

    pub async fn delete_subscription_by_email(&self, email: &str) -> Result<bool> {
        let needle = normalize_login(email);
        let subs = self.list_existing_subscriptions().await?;
        let mut matched: Vec<ExistingSubscription> = subs
            .into_iter()
            .filter(|s| normalize_login(&s.email) == needle)
            .collect();

        if matched.is_empty() {
            return Ok(false);
        }
        if matched.len() > 1 {
            bail!("multiple subscriptions found for login `{email}`");
        }
        let sub = matched.remove(0);

        let fallback_paths = [
            self.config.xui_delete_client_path.as_str(),
            "/panel/api/inbounds/{id}/delClient/{clientId}",
            "/panel/inbound/{id}/delClient/{clientId}",
            "/panel/api/inbounds/delClient",
        ];
        for path in fallback_paths {
            if self
                .try_delete_client(path, sub.inbound_id, &sub.email, &sub.client_id)
                .await?
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn fetch_connection_url_from_server(
        &self,
        email: &str,
        client_uuid: &str,
        sub_id: &str,
    ) -> Option<String> {
        let get_inbound_url = self.get_inbound_url();
        log::debug!("fetching connection URL from {}", get_inbound_url);
        if let Some(url) = self
            .extract_url_from_endpoint(&get_inbound_url, email, client_uuid, sub_id)
            .await
        {
            return Some(url);
        }

        let list_url = self.list_inbounds_url();
        log::debug!("fetching connection URL from {}", list_url);
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
        log::debug!("requesting endpoint {}", endpoint_url);
        let response = self.http.get(endpoint_url).send().await.ok()?;
        if !response.status().is_success() {
            log::debug!(
                "endpoint {} returned non-success status {}",
                endpoint_url,
                response.status()
            );
            return None;
        }
        let body = response.text().await.ok()?;
        let parsed = serde_json::from_str::<Value>(&body).ok()?;

        if let Some(url) = find_best_connection_url(&parsed, email, client_uuid, sub_id) {
            return Some(url);
        }

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

    async fn extract_specific_client_url(&self, endpoint_url: &str, email: &str) -> Option<String> {
        log::debug!("requesting endpoint {} for email={}", endpoint_url, email);
        let response = self.http.get(endpoint_url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.text().await.ok()?;
        let parsed = serde_json::from_str::<Value>(&body).ok()?;

        if let Some(found) = find_best_connection_url(&parsed, email, "", "") {
            return Some(found);
        }

        generate_connection_url_from_server_obj(
            &parsed,
            &self.config.xui_base_url,
            self.config.xui_inbound_id,
            email,
            "",
        )
        .or_else(|| {
            let wrapped = serde_json::from_str::<XuiApiResponse>(&body).ok()?;
            let obj = wrapped.obj.as_ref()?;
            if let Some(found) = find_best_connection_url(obj, email, "", "") {
                return Some(found);
            }
            generate_connection_url_from_server_obj(
                obj,
                &self.config.xui_base_url,
                self.config.xui_inbound_id,
                email,
                "",
            )
        })
    }

    async fn try_delete_client(
        &self,
        delete_path: &str,
        inbound_id: i64,
        email: &str,
        client_id: &str,
    ) -> Result<bool> {
        // Route style endpoint (used by your panel):
        // /panel/inbound/{id}/delClient/{clientId}
        if delete_path.contains("{id}") || delete_path.contains("{clientId}") {
            // Different 3X-UI builds may expect {clientId} as UUID or as email.
            let candidates = [client_id, email];
            for candidate in candidates {
                let encoded_client_id = urlencoding::encode(candidate);
                let route_path = delete_path
                    .replace("{id}", &inbound_id.to_string())
                    .replace("{clientId}", &encoded_client_id);
                let url = join_url(&self.config.xui_base_url, &route_path);
                log::info!(
                    "x-ui delete client route request path={} inbound_id={} email={} client_id_candidate={}",
                    route_path,
                    inbound_id,
                    email,
                    candidate
                );

                let response = self.http.post(&url).send().await.with_context(|| {
                    format!("x-ui delete client route request failed for {url}")
                })?;

                if response.status().is_success() {
                    let body = response
                        .text()
                        .await
                        .context("x-ui delete client route body failed")?;
                    let parsed: Result<XuiApiResponse, _> = serde_json::from_str(&body);
                    if !matches!(parsed, Ok(ref api) if api.success == Some(false)) {
                        return Ok(true);
                    }
                } else {
                    log::debug!(
                        "x-ui delete client route non-success status={} path={} method=POST",
                        response.status(),
                        route_path
                    );
                }

                // Some legacy builds accept GET on route endpoint.
                let response =
                    self.http.get(&url).send().await.with_context(|| {
                        format!("x-ui delete client route GET failed for {url}")
                    })?;
                if response.status().is_success() {
                    let body = response
                        .text()
                        .await
                        .context("x-ui delete client route body (GET) failed")?;
                    let parsed: Result<XuiApiResponse, _> = serde_json::from_str(&body);
                    if !matches!(parsed, Ok(ref api) if api.success == Some(false)) {
                        return Ok(true);
                    }
                } else {
                    log::debug!(
                        "x-ui delete client route non-success status={} path={} method=GET",
                        response.status(),
                        route_path
                    );
                }
            }
            return Ok(false);
        }

        let url = join_url(&self.config.xui_base_url, delete_path);
        log::info!(
            "x-ui delete client json request path={} inbound_id={} email={}",
            delete_path,
            inbound_id,
            email
        );

        // Some 3X-UI builds expect clientId=email, others clientId=uuid.
        let candidates = [email, client_id];
        for candidate in candidates {
            let payload = json!({
                "id": inbound_id,
                "clientId": candidate,
                "email": email
            });

            let response = self
                .http
                .post(&url)
                .json(&payload)
                .send()
                .await
                .with_context(|| format!("x-ui delete client request failed for {url}"))?;

            if !response.status().is_success() {
                log::debug!(
                    "x-ui delete client non-success status={} path={} clientId={}",
                    response.status(),
                    delete_path,
                    candidate
                );
                continue;
            }

            let body = response
                .text()
                .await
                .context("x-ui delete client response body failed")?;
            let parsed: Result<XuiApiResponse, _> = serde_json::from_str(&body);
            match parsed {
                Ok(api) if api.success == Some(false) => {
                    log::debug!(
                        "x-ui delete unsuccessful path={} clientId={} msg={}",
                        delete_path,
                        candidate,
                        api.msg.unwrap_or_else(|| "unknown".to_string())
                    );
                }
                _ => return Ok(true),
            }
        }

        Ok(false)
    }

    async fn fetch_subscription_base_url(&self) -> Result<Option<String>> {
        let url = join_url(&self.config.xui_base_url, "/panel/setting/defaultSettings");
        let response = self
            .http
            .post(&url)
            .send()
            .await
            .with_context(|| format!("x-ui defaultSettings request failed for {url}"))?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let body = response
            .text()
            .await
            .context("x-ui defaultSettings response body failed")?;
        let parsed: Value =
            serde_json::from_str(&body).context("x-ui defaultSettings invalid json")?;

        let sub_enable = parsed
            .get("obj")
            .and_then(|v| v.get("subEnable"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !sub_enable {
            return Ok(None);
        }

        let sub_uri = parsed
            .get("obj")
            .and_then(|v| v.get("subURI"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let Some(sub_uri) = sub_uri else {
            return Ok(None);
        };

        let absolute = if sub_uri.starts_with("http://") || sub_uri.starts_with("https://") {
            sub_uri.to_string()
        } else {
            join_url(&self.config.xui_base_url, sub_uri)
        };

        Ok(Some(ensure_trailing_slash(&absolute)))
    }

    fn list_inbounds_url(&self) -> String {
        join_url(
            &self.config.xui_base_url,
            &self.config.xui_list_inbounds_path,
        )
    }

    fn get_inbound_url(&self) -> String {
        let path = self
            .config
            .xui_get_inbound_path
            .replace("{id}", &self.config.xui_inbound_id.to_string());
        join_url(&self.config.xui_base_url, &path)
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

fn normalize_login(login: &str) -> String {
    login.trim().trim_start_matches('@').to_lowercase()
}

fn ensure_trailing_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_string()
    } else {
        format!("{value}/")
    }
}
