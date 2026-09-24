mod access;
mod bot;
mod config;
mod health;
mod latency;
mod panel;
mod qr;
mod state;
mod storage;
mod web;

use std::sync::Arc;

use anyhow::{Context, Result};
use teloxide::prelude::*;

use crate::config::AppConfig;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();

    let config = AppConfig::from_env().context("failed to load configuration")?;
    log::info!(
        "starting vpn_bot: panel={} inbounds={:?} health={} mini_app={}",
        config.panel.base_url,
        config.panel.inbound_ids,
        config.health.is_some(),
        config.web.as_ref().map_or("off", |w| w.public_url.as_str())
    );
    let state = Arc::new(AppState::new(config)?);
    let bot = Bot::from_env();

    if let Err(err) = bot::register_commands(&bot, &state).await {
        log::warn!("failed to register bot commands: {err:#}");
    }
    if let Some(health) = state.config.health.clone() {
        tokio::spawn(latency::sampler(state.clone(), health.relay_addr));
        tokio::spawn(health::monitor(bot.clone(), state.clone(), health));
    }
    if state.config.web.is_some() {
        let (state, bot) = (state.clone(), bot.clone());
        tokio::spawn(async move {
            if let Err(err) = web::serve(state, bot).await {
                log::error!("mini app server failed: {err:#}");
            }
        });
    }

    Dispatcher::builder(bot, bot::schema())
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    Ok(())
}
