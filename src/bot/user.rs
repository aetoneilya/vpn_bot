//! Commands available to everyone: requesting access, the guide and memes.

use anyhow::{Context, Result};
use teloxide::prelude::*;
use teloxide::types::User;
use teloxide::utils::command::BotCommands;

use super::ui::{self, format_login};
use super::{AdminCommand, UserCommand};
use crate::access::{self, RequestOutcome};
use crate::state::AppState;

pub async fn handle(bot: &Bot, msg: &Message, cmd: UserCommand, state: &AppState) -> Result<()> {
    let user = msg.from.as_ref().context("message without sender")?;
    match cmd {
        UserCommand::Start | UserCommand::Help => send_help(bot, msg.chat.id, user, state).await,
        UserCommand::Vpn => request_access(bot, msg.chat.id, user, state).await,
        UserCommand::Guide => send_guide(bot, msg.chat.id).await,
        UserCommand::Meme => {
            if !state.config.is_allowed(user.id.0) {
                bot.send_message(msg.chat.id, ui::ACCESS_DENIED).await?;
                return Ok(());
            }
            state.arm_meme(user.id.0);
            bot.send_message(msg.chat.id, ui::MEME_PROMPT).await?;
            Ok(())
        }
    }
}

/// Anything that is not a known command: a meme after /meme, otherwise a hint.
pub async fn handle_other(bot: &Bot, msg: &Message, state: &AppState) -> Result<()> {
    let Some(user) = msg.from.as_ref() else {
        return Ok(());
    };
    if state.config.is_approver(user.id.0) {
        return Ok(());
    }

    let is_media = msg.sticker().is_some()
        || msg.photo().is_some()
        || msg.animation().is_some()
        || msg.video().is_some();
    if !is_media {
        bot.send_message(msg.chat.id, ui::UNKNOWN_INPUT).await?;
        return Ok(());
    }
    if !state.take_meme(user.id.0) {
        bot.send_message(msg.chat.id, ui::MEME_NOT_ARMED).await?;
        return Ok(());
    }

    let who = user
        .username
        .as_deref()
        .map(format_login)
        .unwrap_or_else(|| "<без username>".into());
    for approver in &state.config.approver_user_ids {
        let admin_chat = ChatId(*approver as i64);
        bot.send_message(admin_chat, format!("Мем от {who} (id {})", user.id.0))
            .reply_markup(ui::meme_keyboard(msg.chat.id))
            .await?;
        bot.copy_message(admin_chat, msg.chat.id, msg.id).await?;
    }
    bot.send_message(msg.chat.id, ui::MEME_SENT).await?;
    Ok(())
}

pub async fn send_guide(bot: &Bot, chat_id: ChatId) -> Result<()> {
    bot.send_message(chat_id, ui::GUIDE).await?;
    Ok(())
}

/// Resends the subscription of an existing client (the «get link again» button).
pub async fn send_existing(
    bot: &Bot,
    chat_id: ChatId,
    user: &User,
    state: &AppState,
) -> Result<()> {
    let Some(login) = checked_login(bot, chat_id, user, state).await? else {
        return Ok(());
    };
    match state.panel.find_client(&login).await? {
        Some(client) => {
            ui::send_subscription(
                bot,
                chat_id,
                &state.panel,
                &client,
                "Твоя ссылка на подписку.",
            )
            .await
        }
        None => {
            bot.send_message(chat_id, "Подписка не найдена. Запроси доступ через /vpn.")
                .await?;
            Ok(())
        }
    }
}

async fn request_access(bot: &Bot, chat_id: ChatId, user: &User, state: &AppState) -> Result<()> {
    let Some(login) = checked_login(bot, chat_id, user, state).await? else {
        return Ok(());
    };

    match access::request(bot, state, chat_id, user.id.0, &login).await? {
        RequestOutcome::AlreadyActive(client) => {
            ui::send_subscription(
                bot,
                chat_id,
                &state.panel,
                &client,
                "Твоя VPN-подписка уже есть.",
            )
            .await
        }
        RequestOutcome::Created(_) => {
            bot.send_message(chat_id, ui::REQUEST_CREATED).await?;
            Ok(())
        }
        RequestOutcome::AlreadyPending(_) => {
            bot.send_message(chat_id, ui::REQUEST_ALREADY_PENDING)
                .await?;
            Ok(())
        }
    }
}

async fn send_help(bot: &Bot, chat_id: ChatId, user: &User, state: &AppState) -> Result<()> {
    let mut text = String::new();
    if state.config.web.is_some() {
        text.push_str(
            "Нажми «🔐 Открыть VPN» — там можно запросить доступ, скопировать подписку и прочитать инструкцию.\n\n",
        );
    }
    text.push_str(&UserCommand::descriptions().to_string());
    if state.config.is_approver(user.id.0) {
        text.push_str("\n\nАдмин:\n");
        text.push_str(&AdminCommand::descriptions().to_string());
    }

    let mut message = bot.send_message(chat_id, text);
    if let Some(keyboard) = ui::web_app_keyboard(state) {
        message = message.reply_markup(keyboard);
    }
    message.await?;
    Ok(())
}

/// Returns the user's login if they may use the bot, otherwise explains why not.
async fn checked_login(
    bot: &Bot,
    chat_id: ChatId,
    user: &User,
    state: &AppState,
) -> Result<Option<String>> {
    if !state.config.is_allowed(user.id.0) {
        bot.send_message(chat_id, ui::ACCESS_DENIED).await?;
        return Ok(None);
    }
    match user.username.as_deref() {
        Some(username) => Ok(Some(crate::panel::normalize_login(username))),
        None => {
            bot.send_message(chat_id, ui::NO_USERNAME).await?;
            Ok(None)
        }
    }
}
