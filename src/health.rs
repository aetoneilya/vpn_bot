//! End-to-end checks of the VPN chain and a background monitor that alerts approvers.
//!
//! Each Reality profile is probed by resolving its SNI to the relay address and making a
//! plain HTTPS request: the relay forwards it to the exit node, Reality passes the
//! unauthenticated handshake through to the real site, so a valid response proves that
//! client → relay → exit is intact for that profile.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use teloxide::prelude::*;

use crate::config::HealthConfig;
use crate::state::AppState;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Latest result of all checks, shown to users in the Mini App.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub checked_at: i64,
    pub results: Vec<CheckResult>,
}

impl HealthSnapshot {
    pub fn all_ok(&self) -> bool {
        self.results.iter().all(|r| r.ok)
    }
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub async fn run_checks(config: &HealthConfig) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(config.snis.len() + 1);

    for sni in &config.snis {
        let name = format!("relay → exit ({sni})");
        let result = match probe_via_relay(config, sni).await {
            Ok(status) => CheckResult {
                name,
                ok: true,
                detail: format!("HTTP {status}"),
            },
            Err(err) => CheckResult {
                name,
                ok: false,
                detail: format!("{err:#}"),
            },
        };
        results.push(result);
    }

    if let Some(url) = config.subscription_url.as_deref() {
        let name = "подписки".to_string();
        let result = match probe_url(url).await {
            Ok(status) => CheckResult {
                name,
                ok: true,
                detail: format!("HTTP {status}"),
            },
            Err(err) => CheckResult {
                name,
                ok: false,
                detail: format!("{err:#}"),
            },
        };
        results.push(result);
    }

    results
}

async fn probe_via_relay(config: &HealthConfig, sni: &str) -> Result<u16> {
    let client = Client::builder()
        .no_proxy()
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(sni, config.relay_addr)
        .build()
        .context("failed to build probe client")?;
    let url = format!("https://{sni}:{}/", config.relay_addr.port());
    let response = client.get(&url).send().await.context("handshake failed")?;
    Ok(response.status().as_u16())
}

async fn probe_url(url: &str) -> Result<u16> {
    let client = Client::builder()
        .no_proxy()
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build probe client")?;
    let response = client.get(url).send().await.context("request failed")?;
    Ok(response.status().as_u16())
}

pub fn format_results(results: &[CheckResult]) -> String {
    results
        .iter()
        .map(|r| {
            let mark = if r.ok { "✅" } else { "❌" };
            format!("{mark} {} — {}", r.name, r.detail)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs checks forever and notifies approvers when a check goes down (after
/// `fail_threshold` consecutive failures) and when it recovers.
pub async fn monitor(bot: Bot, state: Arc<AppState>, config: HealthConfig) {
    log::info!(
        "health monitor started relay={} snis={:?} interval={}s",
        config.relay_addr,
        config.snis,
        config.interval_secs
    );
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut alerted: HashMap<String, bool> = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(config.interval_secs));

    loop {
        interval.tick().await;
        let results = run_checks(&config).await;
        state.set_health(HealthSnapshot {
            checked_at: chrono::Utc::now().timestamp(),
            results: results.clone(),
        });
        for result in results {
            let count = failures.entry(result.name.clone()).or_insert(0);
            let is_alerted = alerted.entry(result.name.clone()).or_insert(false);

            let message = if result.ok {
                *count = 0;
                if *is_alerted {
                    *is_alerted = false;
                    Some(format!("✅ Снова работает: {}", result.name))
                } else {
                    None
                }
            } else {
                *count += 1;
                log::warn!(
                    "health check failed name={} consecutive={} detail={}",
                    result.name,
                    count,
                    result.detail
                );
                if *count >= config.fail_threshold && !*is_alerted {
                    *is_alerted = true;
                    Some(format!(
                        "⚠️ Не работает: {}\n{}\n\nПроверка {} раз(а) подряд. Если выходной сервер заблокирован — на relay: exit-switch <новый IP>",
                        result.name, result.detail, count
                    ))
                } else {
                    None
                }
            };

            if let Some(text) = message {
                crate::bot::notify_approvers(&bot, &state, &text).await;
            }
        }
    }
}
