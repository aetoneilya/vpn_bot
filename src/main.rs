mod config;
mod handlers;
mod qr;
mod state;
mod storage;
mod xui;

use std::sync::Arc;

use crate::config::AppConfig;
use crate::state::AppState;
use anyhow::{Context, Result};
use teloxide::dispatching::UpdateFilterExt;
use teloxide::dptree;
use teloxide::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();
    log::info!("starting vpn_bot");

    let config = AppConfig::from_env().context("failed to load configuration")?;
    log::info!(
        "configuration loaded: inbound_id={}, sqlite_path={}",
        config.xui_inbound_id,
        config.sqlite_path
    );
    let state = Arc::new(AppState::new(config).context("failed to initialize app state")?);
    log::info!("app state initialized");
    let bot = Bot::from_env();
    log::info!("telegram bot initialized, entering dispatcher loop");

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    if let Some(text) = msg.text().map(str::to_owned) {
        log_handler_error(
            "handler",
            handlers::handle_text(bot, msg, &text, state).await,
        );
    } else {
        log_handler_error(
            "non-text handler",
            handlers::handle_non_text(bot, msg, state).await,
        );
    }
    Ok(())
}

async fn handle_callback(bot: Bot, q: CallbackQuery, state: Arc<AppState>) -> Result<()> {
    log_handler_error(
        "callback handler",
        handlers::handle_callback(bot, q, state).await,
    );
    Ok(())
}

fn log_handler_error(scope: &str, result: Result<()>) {
    if let Err(err) = result {
        log::error!("{scope} error: {err:#}");
    }
}
