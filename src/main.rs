mod config;
mod handlers;
mod qr;
mod state;
mod xui;

use std::sync::Arc;

use crate::config::AppConfig;
use crate::state::AppState;
use anyhow::{Context, Result};
use teloxide::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();

    let config = AppConfig::from_env().context("failed to load configuration")?;
    let state = Arc::new(AppState::new(config));
    let bot = Bot::from_env();

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let state = Arc::clone(&state);
        async move {
            if let Some(text) = msg.text().map(str::to_owned)
                && let Err(err) = handlers::handle_text(bot, msg, &text, state).await
            {
                log::error!("handler error: {err:#}");
            }
            respond(())
        }
    })
    .await;

    Ok(())
}
