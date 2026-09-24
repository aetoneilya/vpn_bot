//! Telegram layer: command parsing, routing and error reporting.

mod admin;
pub(crate) mod ui;
mod user;

use std::sync::Arc;

use anyhow::Result;
use teloxide::dispatching::{HandlerExt, UpdateFilterExt, UpdateHandler};
use teloxide::prelude::*;
use teloxide::types::{BotCommandScope, InlineKeyboardMarkup, MenuButton, Recipient};
use teloxide::utils::command::BotCommands;

use crate::state::AppState;
use ui::Action;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase")]
pub enum UserCommand {
    #[command(description = "начать")]
    Start,
    #[command(description = "список команд")]
    Help,
    #[command(description = "получить доступ к VPN или ссылку заново")]
    Vpn,
    #[command(description = "как подключиться")]
    Guide,
    #[command(description = "отправить мем админу")]
    Meme,
}

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase")]
pub enum AdminCommand {
    #[command(description = "состояние серверов, онлайн и трафик")]
    Status,
    #[command(description = "все подписки")]
    Subs,
    #[command(description = "заявки на доступ")]
    Requests,
    #[command(description = "одобрить заявку: /approve <id>")]
    Approve(String),
    #[command(description = "отклонить заявку: /deny <id>")]
    Deny(String),
    #[command(description = "удалить подписку: /delete <login>")]
    Delete(String),
    #[command(description = "рассылка всем: /broadcast <текст>")]
    Broadcast(String),
    #[command(description = "сообщение пользователю: /msg <@login|tg_id> <текст>")]
    Msg(String),
}

pub fn schema() -> UpdateHandler<anyhow::Error> {
    let messages = Update::filter_message()
        .branch(
            dptree::filter(|msg: Message, state: Arc<AppState>| {
                sender_id(&msg).is_some_and(|id| state.config.is_approver(id))
            })
            .filter_command::<AdminCommand>()
            .endpoint(on_admin_command),
        )
        .branch(
            dptree::entry()
                .filter_command::<UserCommand>()
                .endpoint(on_user_command),
        )
        .branch(dptree::endpoint(on_other_message));

    dptree::entry()
        .branch(messages)
        .branch(Update::filter_callback_query().endpoint(on_callback))
}

/// Registers the command menu (user commands for everyone, plus admin commands for
/// approvers) and, when the Mini App is configured, the «VPN» menu button that opens it.
pub async fn register_commands(bot: &Bot, state: &AppState) -> Result<()> {
    let user = UserCommand::bot_commands();
    bot.set_my_commands(user.clone()).await?;

    let mut all = user;
    all.extend(AdminCommand::bot_commands());
    for approver in &state.config.approver_user_ids {
        bot.set_my_commands(all.clone())
            .scope(BotCommandScope::Chat {
                chat_id: Recipient::Id(ChatId(*approver as i64)),
            })
            .await?;
    }

    let menu = match ui::web_app_info(state) {
        Some(web_app) => MenuButton::WebApp {
            text: "VPN".into(),
            web_app,
        },
        None => MenuButton::Commands,
    };
    bot.set_chat_menu_button().menu_button(menu).await?;
    Ok(())
}

pub async fn notify_approvers(bot: &Bot, state: &AppState, text: &str) {
    for approver in &state.config.approver_user_ids {
        if let Err(err) = bot.send_message(ChatId(*approver as i64), text).await {
            log::warn!("failed to notify approver {approver}: {err}");
        }
    }
}

/// The message approvers get for a new access request, with approve/deny buttons.
pub fn approval_request_message(
    id: u64,
    login: &str,
    user_id: u64,
) -> (String, InlineKeyboardMarkup) {
    (
        format!(
            "Новая заявка #{id}\nОт: {} (id {user_id})",
            ui::format_login(login)
        ),
        ui::approval_keyboard(id),
    )
}

async fn on_admin_command(
    bot: Bot,
    msg: Message,
    cmd: AdminCommand,
    state: Arc<AppState>,
) -> Result<()> {
    let result = admin::handle(&bot, &msg, cmd, &state).await;
    report(&bot, msg.chat.id, true, result).await
}

async fn on_user_command(
    bot: Bot,
    msg: Message,
    cmd: UserCommand,
    state: Arc<AppState>,
) -> Result<()> {
    let is_admin = sender_id(&msg).is_some_and(|id| state.config.is_approver(id));
    let result = user::handle(&bot, &msg, cmd, &state).await;
    report(&bot, msg.chat.id, is_admin, result).await
}

async fn on_other_message(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let result = user::handle_other(&bot, &msg, &state).await;
    report(&bot, msg.chat.id, false, result).await
}

async fn on_callback(bot: Bot, query: CallbackQuery, state: Arc<AppState>) -> Result<()> {
    let actor = query.from.id.0;
    let chat_id = query
        .message
        .as_ref()
        .map(|m| m.chat().id)
        .unwrap_or(ChatId(actor as i64));
    let is_admin = state.config.is_approver(actor);

    let Some(action) = query.data.as_deref().and_then(Action::decode) else {
        bot.answer_callback_query(query.id)
            .text("Неизвестное действие")
            .await?;
        return Ok(());
    };
    if action.is_admin_only() && !is_admin {
        bot.answer_callback_query(query.id)
            .text("Только для админа")
            .await?;
        return Ok(());
    }
    bot.answer_callback_query(query.id.clone()).await?;

    let result = match action {
        Action::Guide => user::send_guide(&bot, chat_id).await,
        Action::Resend => user::send_existing(&bot, chat_id, &query.from, &state).await,
        _ => admin::handle_action(&bot, &query, chat_id, action, &state).await,
    };
    report(&bot, chat_id, is_admin, result).await
}

/// Logs a handler error and tells the chat: full details for admins, a generic line for users.
async fn report(bot: &Bot, chat_id: ChatId, is_admin: bool, result: Result<()>) -> Result<()> {
    let Err(err) = result else {
        return Ok(());
    };
    log::error!("handler error in chat {}: {err:#}", chat_id.0);
    let text = if is_admin {
        format!("⚠️ Ошибка: {err:#}")
    } else {
        ui::USER_ERROR.to_string()
    };
    bot.send_message(chat_id, text).await?;
    Ok(())
}

fn sender_id(msg: &Message) -> Option<u64> {
    msg.from.as_ref().map(|u| u.id.0)
}
