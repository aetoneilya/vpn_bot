//! Minimal client for the 3x-ui 3.x panel API.
//!
//! One instance is shared by the whole bot. With password auth it keeps a cookie session,
//! logs in lazily and re-logs in once when a request comes back unauthorized.

mod models;

use anyhow::{Context, Result, bail};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::{PanelAuth, PanelConfig};
use models::ApiResponse;
pub use models::{Client, Inbound, ServerStatus};

pub struct Panel {
    http: reqwest::Client,
    config: PanelConfig,
    /// Serializes logins and remembers whether the cookie session is believed to be valid.
    session: Mutex<bool>,
}

impl Panel {
    pub fn new(config: PanelConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("failed to build panel HTTP client")?;
        Ok(Self {
            http,
            config,
            session: Mutex::new(false),
        })
    }

    pub fn subscription_url(&self, client: &Client) -> String {
        format!(
            "{}{}",
            self.config.subscription_base_url,
            urlencoding::encode(&client.sub_id)
        )
    }

    pub async fn clients(&self) -> Result<Vec<Client>> {
        let obj = self
            .call(Method::GET, "/panel/api/clients/list", None)
            .await?;
        serde_json::from_value(obj).context("unexpected clients/list payload")
    }

    pub async fn find_client(&self, login: &str) -> Result<Option<Client>> {
        let wanted = normalize_login(login);
        Ok(self
            .clients()
            .await?
            .into_iter()
            .find(|c| normalize_login(&c.login) == wanted))
    }

    /// Creates a client attached to all configured inbounds, or returns the existing one.
    pub async fn create_client(&self, login: &str, telegram_user_id: u64) -> Result<Client> {
        if let Some(existing) = self.find_client(login).await? {
            return Ok(existing);
        }

        let login = normalize_login(login);
        let payload = json!({
            "client": {
                "email": login,
                "id": Uuid::new_v4().to_string(),
                "subId": Uuid::new_v4().simple().to_string()[..16].to_string(),
                "flow": self.config.client_flow,
                "totalGB": self.config.total_gb * 1024 * 1024 * 1024,
                "expiryTime": 0,
                "limitIp": 0,
                "tgId": telegram_user_id,
                "comment": "",
                "enable": true,
                "reset": 0,
            },
            "inboundIds": self.config.inbound_ids,
        });
        self.call(Method::POST, "/panel/api/clients/add", Some(payload))
            .await
            .context("clients/add failed")?;

        self.find_client(&login)
            .await?
            .with_context(|| format!("client `{login}` not found right after creation"))
    }

    /// Deletes a client from every inbound. Returns false if no such client exists.
    pub async fn delete_client(&self, login: &str) -> Result<bool> {
        let Some(client) = self.find_client(login).await? else {
            return Ok(false);
        };
        let path = format!(
            "/panel/api/clients/del/{}",
            urlencoding::encode(&client.login)
        );
        self.call(Method::POST, &path, None).await?;
        Ok(true)
    }

    pub async fn online_logins(&self) -> Result<Vec<String>> {
        let obj = self
            .call(Method::POST, "/panel/api/clients/onlines", None)
            .await?;
        Ok(serde_json::from_value::<Option<Vec<String>>>(obj)
            .context("unexpected clients/onlines payload")?
            .unwrap_or_default())
    }

    pub async fn inbounds(&self) -> Result<Vec<Inbound>> {
        let obj = self
            .call(Method::GET, "/panel/api/inbounds/list/slim", None)
            .await?;
        serde_json::from_value(obj).context("unexpected inbounds/list/slim payload")
    }

    pub async fn server_status(&self) -> Result<ServerStatus> {
        let obj = self
            .call(Method::GET, "/panel/api/server/status", None)
            .await?;
        Ok(ServerStatus::from_value(&obj))
    }

    /// Performs an API call and returns `obj` of a successful response.
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let label = format!("{method} {path}");
        let mut relogged = false;
        loop {
            self.ensure_session().await?;
            let response = self.send(method.clone(), path, body.as_ref()).await?;
            let status = response.status();

            if is_unauthorized(status) && !relogged && self.uses_session() {
                log::info!("panel session rejected ({status}) on {label}, logging in again");
                *self.session.lock().await = false;
                relogged = true;
                continue;
            }

            let text = response
                .text()
                .await
                .with_context(|| format!("{label}: failed to read body"))?;
            if !status.is_success() {
                bail!("{label}: HTTP {status}: {}", truncate(&text, 300));
            }
            let parsed: ApiResponse = serde_json::from_str(&text)
                .with_context(|| format!("{label}: invalid JSON: {}", truncate(&text, 300)))?;
            if !parsed.success {
                bail!("{label}: {}", parsed.msg);
            }
            return Ok(parsed.obj);
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .request(method.clone(), self.url(path))
            .headers(self.base_headers()?);
        if method == Method::POST && self.uses_session() {
            request = request.header("X-CSRF-Token", self.csrf_token().await?);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        request
            .send()
            .await
            .with_context(|| format!("{method} {path}: request failed"))
    }

    async fn ensure_session(&self) -> Result<()> {
        let PanelAuth::Password { username, password } = &self.config.auth else {
            return Ok(());
        };
        let mut logged_in = self.session.lock().await;
        if *logged_in {
            return Ok(());
        }

        let response = self
            .http
            .post(self.url("/login"))
            .headers(self.base_headers()?)
            .header("X-CSRF-Token", self.csrf_token().await?)
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .context("panel login request failed")?;
        let status = response.status();
        let parsed: ApiResponse = response
            .json()
            .await
            .with_context(|| format!("panel login: unexpected response (HTTP {status})"))?;
        if !parsed.success {
            bail!("panel login failed: {}", parsed.msg);
        }

        log::info!("panel login successful");
        *logged_in = true;
        Ok(())
    }

    async fn csrf_token(&self) -> Result<String> {
        let parsed: ApiResponse = self
            .http
            .get(self.url("/csrf-token"))
            .headers(self.base_headers()?)
            .send()
            .await
            .context("csrf-token request failed")?
            .json()
            .await
            .context("csrf-token: unexpected response")?;
        parsed
            .obj
            .as_str()
            .map(ToString::to_string)
            .context("csrf-token: missing token")
    }

    fn base_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Requested-With",
            HeaderValue::from_static("XMLHttpRequest"),
        );
        if let PanelAuth::Token(token) = &self.config.auth {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .context("XUI_API_TOKEN contains invalid characters")?,
            );
        }
        Ok(headers)
    }

    fn uses_session(&self) -> bool {
        matches!(self.config.auth, PanelAuth::Password { .. })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url, path)
    }
}

/// Panel logins are Telegram usernames; compare them case-insensitively and without `@`.
pub fn normalize_login(login: &str) -> String {
    login.trim().trim_start_matches('@').to_lowercase()
}

/// 3x-ui hides API routes behind 404 for unauthenticated sessions.
fn is_unauthorized(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    )
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}
