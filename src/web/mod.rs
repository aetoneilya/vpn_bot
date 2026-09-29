//! Telegram Mini App: a page inside the bot where users see their access status,
//! request access and copy their subscription.

mod auth;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;

use crate::access::{self, AccessStatus, RequestOutcome};
use crate::complaints::{self, Category, NewComplaint, SubmitOutcome};
use crate::geo::GeoInfo;
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
        .route("/api/context", post(get_context))
        .route("/api/complaint", post(post_complaint))
        .route("/api/ping", get(ping))
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
/// Location of the requester, from the real client IP the TLS proxy puts in `X-Real-IP`.
/// Trusted because the server only listens on localhost behind that proxy.
fn client_geo(app: &AppState, headers: &HeaderMap) -> GeoInfo {
    let ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    match (app.geo.as_ref(), ip) {
        (Some(db), Some(ip)) => db.lookup(ip),
        _ => GeoInfo::default(),
    }
}

/// Cheap endpoint the page times to measure the device → relay round trip.
async fn ping() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        StatusCode::NO_CONTENT,
    )
}

#[derive(Serialize)]
struct ContextResponse {
    geo: GeoInfo,
    summary: Option<String>,
}

/// What will be attached to a complaint, shown in the form before sending.
async fn get_context(
    State(web): State<WebState>,
    headers: HeaderMap,
    Json(req): Json<AuthRequest>,
) -> Response {
    if let Err(rejection) = authenticate(&web, &req.init_data) {
        return rejection.into_response();
    }
    let geo = client_geo(&web.app, &headers);
    let known = geo.asn.is_some() || geo.country.is_some();
    Json(ContextResponse {
        summary: known.then(|| geo.summary()),
        geo,
    })
    .into_response()
}

#[derive(Deserialize)]
struct ComplaintRequest {
    init_data: String,
    category: String,
    profile: Option<String>,
    network: Option<String>,
    site: Option<String>,
    comment: Option<String>,
    platform: Option<String>,
    client_rtt_ms: Option<f64>,
}

#[derive(Serialize)]
struct ComplaintResponse {
    id: u64,
}

async fn post_complaint(
    State(web): State<WebState>,
    headers: HeaderMap,
    Json(req): Json<ComplaintRequest>,
) -> Response {
    let user = match authenticate(&web, &req.init_data) {
        Ok(user) => user,
        Err(rejection) => return rejection.into_response(),
    };
    let short = |v: Option<String>| v.map(|s| s.chars().take(64).collect::<String>());
    let complaint = NewComplaint {
        user_id: user.id,
        // Mini Apps are opened from the bot chat, so the private chat id equals the user id.
        chat_id: ChatId(user.id as i64),
        login: user.username.as_deref().map(crate::panel::normalize_login),
        category: Category::from_code(&req.category),
        profile: short(req.profile),
        network: short(req.network),
        site: req.site,
        comment: req.comment,
        platform: short(req.platform),
        client_rtt_ms: req.client_rtt_ms.filter(|v| v.is_finite() && *v >= 0.0),
        geo: client_geo(&web.app, &headers),
    };
    match complaints::submit(&web.bot, &web.app, complaint).await {
        Ok(SubmitOutcome::Submitted(id)) => Json(ComplaintResponse { id }).into_response(),
        Ok(SubmitOutcome::RateLimited) => error(
            StatusCode::TOO_MANY_REQUESTS,
            "Слишком много жалоб за час — попробуй позже.",
        ),
        Err(err) => {
            log::error!("complaint submit failed: {err:#}");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Не получилось отправить. Попробуй позже или напиши /problem в боте.",
            )
        }
    }
}

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
