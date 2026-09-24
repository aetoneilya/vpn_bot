//! Telegram Mini App: a page inside the bot where users see their access status,
//! request access and copy their subscription.

mod auth;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;

use crate::access::{self, AccessStatus, RequestOutcome};
use crate::latency;
use crate::panel::Client;
use crate::qr::render_qr_png;
use crate::state::AppState;
use auth::{AuthError, WebAppUser, verify_init_data};

const PAGE: &str = include_str!("page.html");
const OPEN_IN_APP_PAGE: &str = include_str!("open_in_app.html");

#[derive(Clone)]
struct WebState {
    app: Arc<AppState>,
    bot: Bot,
}

pub async fn serve(app: Arc<AppState>, bot: Bot) -> Result<()> {
    let config = app.config.web.clone().context("web is not configured")?;
    let router = Router::new()
        .route("/", get(page))
        .route("/api/state", post(get_state))
        .route("/api/request", post(request_access))
        .route("/api/status", post(get_status))
        .route("/open-in-app", get(open_in_app))
        .with_state(WebState { app, bot });

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("failed to bind {}", config.listen))?;
    log::info!(
        "mini app listening on {} ({})",
        config.listen,
        config.public_url
    );
    axum::serve(listener, router)
        .await
        .context("web server stopped")
}

async fn page() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-cache")], Html(PAGE))
}

#[derive(Deserialize)]
struct OpenInAppQuery {
    url: String,
}

/// Bridge for phones: Telegram's in-app browser on iOS refuses custom URL schemes, so the
/// Mini App opens this page in the system browser, which hands the subscription to Happ.
/// Only our own subscription URLs are accepted, so this can't be used as an open redirect.
async fn open_in_app(State(web): State<WebState>, Query(query): Query<OpenInAppQuery>) -> Response {
    if !query
        .url
        .starts_with(&web.app.config.panel.subscription_base_url)
    {
        return (StatusCode::BAD_REQUEST, "unknown subscription").into_response();
    }
    let deeplink = format!("happ://add/{}", query.url);
    let page = OPEN_IN_APP_PAGE
        .replace(
            "{{DEEPLINK_JSON}}",
            // `</` would let the value close the surrounding <script>.
            &serde_json::to_string(&deeplink)
                .unwrap_or_default()
                .replace("</", "<\\/"),
        )
        .replace("{{DEEPLINK_HTML}}", &html_escape(&deeplink))
        .replace("{{URL_HTML}}", &html_escape(&query.url));
    ([(header::CACHE_CONTROL, "no-store")], Html(page)).into_response()
}

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Deserialize)]
struct AuthRequest {
    init_data: String,
}

/// What the page renders. `status` drives the screen; the rest is filled when relevant.
#[derive(Serialize, Default)]
struct StateResponse {
    status: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_png_base64: Option<String>,
}

async fn get_state(State(web): State<WebState>, Json(req): Json<AuthRequest>) -> Response {
    respond(web, req, |web, user, username| async move {
        Ok(match access::status(&web.app, user.id, &username).await? {
            AccessStatus::Active(client) => active(&web.app, &user, &client)?,
            AccessStatus::Pending(id) => pending(&user, id),
            AccessStatus::None => StateResponse {
                status: "none",
                name: user.first_name.clone(),
                ..Default::default()
            },
        })
    })
    .await
}

async fn request_access(State(web): State<WebState>, Json(req): Json<AuthRequest>) -> Response {
    respond(web, req, |web, user, username| async move {
        // Mini Apps are opened from the bot chat, so the private chat id equals the user id.
        let chat_id = ChatId(user.id as i64);
        Ok(
            match access::request(&web.bot, &web.app, chat_id, user.id, &username).await? {
                RequestOutcome::AlreadyActive(client) => active(&web.app, &user, &client)?,
                RequestOutcome::Created(id) | RequestOutcome::AlreadyPending(id) => {
                    pending(&user, id)
                }
            },
        )
    })
    .await
}

/// Authenticates the request and runs `handler` for users allowed to use the bot.
async fn respond<F, Fut>(web: WebState, req: AuthRequest, handler: F) -> Response
where
    F: FnOnce(WebState, WebAppUser, String) -> Fut,
    Fut: std::future::Future<Output = Result<StateResponse>>,
{
    let user = match authenticate(&web, &req.init_data) {
        Ok(user) => user,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(username) = user.username.clone() else {
        return Json(StateResponse {
            status: "no_username",
            name: user.first_name,
            ..Default::default()
        })
        .into_response();
    };

    match handler(web, user, username).await {
        Ok(state) => Json(state).into_response(),
        Err(err) => {
            log::error!("mini app handler error: {err:#}");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Что-то пошло не так. Попробуй позже.",
            )
        }
    }
}

/// Why a Mini App request was rejected before reaching a handler.
enum Rejection {
    Unauthorized(&'static str),
    Forbidden,
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized(message) => error(StatusCode::UNAUTHORIZED, message),
            Self::Forbidden => error(StatusCode::FORBIDDEN, crate::bot::ui::ACCESS_DENIED),
        }
    }
}

/// Verifies Telegram's signature and the allowlist.
fn authenticate(web: &WebState, init_data: &str) -> std::result::Result<WebAppUser, Rejection> {
    let user = verify_init_data(init_data, web.bot.token()).map_err(|err| {
        log::warn!("mini app auth failed: {err:?}");
        Rejection::Unauthorized(match err {
            AuthError::Expired => "Сессия устарела — закрой и открой страницу заново.",
            AuthError::Malformed | AuthError::BadSignature => {
                "Открой эту страницу из бота в Telegram."
            }
        })
    })?;
    if !web.app.config.is_allowed(user.id) {
        return Err(Rejection::Forbidden);
    }
    Ok(user)
}

#[derive(Deserialize)]
struct StatusRequest {
    init_data: String,
    /// `1h`, `24h` (default) or `7d`.
    range: Option<String>,
}

#[derive(Serialize)]
struct StatusResponse {
    /// `ok`, `degraded`, or `unknown` before the first health check.
    vpn: &'static str,
    checked_at: Option<i64>,
    checks_ok: usize,
    checks_total: usize,
    latency: latency::Series,
}

/// VPN health and relay ↔ exit latency history; available to any allowed user.
async fn get_status(State(web): State<WebState>, Json(req): Json<StatusRequest>) -> Response {
    if let Err(rejection) = authenticate(&web, &req.init_data) {
        return rejection.into_response();
    }
    let series = match latency::series(&web.app, latency::Range::parse(req.range.as_deref())) {
        Ok(series) => series,
        Err(err) => {
            log::error!("latency query failed: {err:#}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Что-то пошло не так. Попробуй позже.",
            );
        }
    };
    let health = web.app.health();
    Json(StatusResponse {
        vpn: match &health {
            None => "unknown",
            Some(h) if h.all_ok() => "ok",
            Some(_) => "degraded",
        },
        checked_at: health.as_ref().map(|h| h.checked_at),
        checks_ok: health
            .as_ref()
            .map_or(0, |h| h.results.iter().filter(|r| r.ok).count()),
        checks_total: health.as_ref().map_or(0, |h| h.results.len()),
        latency: series,
    })
    .into_response()
}

fn active(app: &AppState, user: &WebAppUser, client: &Client) -> Result<StateResponse> {
    let url = app.panel.subscription_url(client);
    let qr = base64::engine::general_purpose::STANDARD.encode(render_qr_png(&url)?);
    Ok(StateResponse {
        status: "active",
        name: user.first_name.clone(),
        subscription_url: Some(url),
        qr_png_base64: Some(qr),
        ..Default::default()
    })
}

fn pending(user: &WebAppUser, id: u64) -> StateResponse {
    StateResponse {
        status: "pending",
        name: user.first_name.clone(),
        request_id: Some(id),
        ..Default::default()
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}
