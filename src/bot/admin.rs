//! Approver-only commands and inline actions.

use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail};
use teloxide::prelude::*;
use teloxide::types::InlineKeyboardMarkup;

use super::AdminCommand;
use super::ui::{self, Action, format_bytes, format_duration, format_login, format_unix};
use crate::health;
use crate::panel::normalize_login;
use crate::state::AppState;

pub async fn handle(bot: &Bot, msg: &Message, cmd: AdminCommand, state: &AppState) -> Result<()> {
    let chat = msg.chat.id;
    match cmd {
        AdminCommand::Status => status(bot, chat, state).await,
        AdminCommand::Subs => subscriptions(bot, chat, state).await,
        AdminCommand::Requests => requests(bot, chat, state).await,
        AdminCommand::Approve(id) => approve(bot, chat, parse_request_id(&id)?, state).await,
        AdminCommand::Deny(id) => deny(bot, chat, parse_request_id(&id)?, state).await,
        AdminCommand::Delete(login) => delete(bot, chat, &login, state).await,
        AdminCommand::Broadcast(text) => broadcast(bot, chat, &text, state).await,
        AdminCommand::Msg(payload) => direct_message(bot, chat, &payload, state).await,
    }
}

pub async fn handle_action(
    bot: &Bot,
    query: &CallbackQuery,
    chat: ChatId,
    action: Action,
    state: &AppState,
) -> Result<()> {
    match action {
        Action::Approve(id) => approve(bot, chat, id, state).await?,
        Action::Deny(id) => deny(bot, chat, id, state).await?,
        Action::MemeLike(user_chat) => {
            bot.send_message(ChatId(user_chat), ui::MEME_LIKE).await?;
        }
        Action::MemeDislike(user_chat) => {
            bot.send_message(ChatId(user_chat), ui::MEME_DISLIKE)
                .await?;
        }
        Action::Guide | Action::Resend => unreachable!("user actions are routed elsewhere"),
    }
    clear_buttons(bot, query).await
}

async fn approve(bot: &Bot, chat: ChatId, id: u64, state: &AppState) -> Result<()> {
    let Some(request) = state.store.get(id)? else {
        bot.send_message(chat, format!("Заявка #{id} не найдена (уже обработана?)."))
            .await?;
        return Ok(());
    };

    let client = state
        .panel
        .create_client(&request.login, request.user_id)
        .await
        .with_context(|| format!("не удалось создать клиента для заявки #{id}"))?;
    state.store.delete(id)?;
    log::info!("access request #{id} approved, client {}", client.login);

    bot.send_message(
        chat,
        format!(
            "Заявка #{id} одобрена: {}\n{}",
            format_login(&client.login),
            state.panel.subscription_url(&client)
        ),
    )
    .await?;
    ui::send_subscription(
        bot,
        request.chat_id,
        &state.panel,
        &client,
        "Твоя заявка одобрена 🎉",
    )
    .await
}

async fn deny(bot: &Bot, chat: ChatId, id: u64, state: &AppState) -> Result<()> {
    let Some(request) = state.store.get(id)? else {
        bot.send_message(chat, format!("Заявка #{id} не найдена (уже обработана?)."))
            .await?;
        return Ok(());
    };
    state.store.delete(id)?;
    bot.send_message(chat, format!("Заявка #{id} отклонена."))
        .await?;
    bot.send_message(request.chat_id, "Твоя заявка отклонена.")
        .await?;
    Ok(())
}

async fn requests(bot: &Bot, chat: ChatId, state: &AppState) -> Result<()> {
    let pending = state.store.list()?;
    if pending.is_empty() {
        bot.send_message(chat, "Заявок нет.").await?;
        return Ok(());
    }
    for request in pending {
        bot.send_message(
            chat,
            format!(
                "#{} — {} (id {})\nсоздана {}",
                request.id,
                format_login(&request.login),
                request.user_id,
                format_unix(request.created_at_unix)
            ),
        )
        .reply_markup(ui::approval_keyboard(request.id))
        .await?;
    }
    Ok(())
}

async fn subscriptions(bot: &Bot, chat: ChatId, state: &AppState) -> Result<()> {
    let mut clients = state.panel.clients().await?;
    clients.sort_by_key(|c| normalize_login(&c.login));
    let inbound_names: std::collections::HashMap<i64, String> = state
        .panel
        .inbounds()
        .await?
        .into_iter()
        .map(|i| (i.id, i.remark))
        .collect();

    let mut lines = vec![format!("Подписок: {}", clients.len())];
    for c in &clients {
        let expiry = if c.expiry_time > 0 {
            format_unix(c.expiry_time / 1000)
        } else {
            "бессрочно".into()
        };
        let inbounds = c
            .inbound_ids
            .iter()
            .map(|id| {
                inbound_names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "{} {} | tg {} | {} | {}",
            if c.enabled { "🟢" } else { "⚪️" },
            format_login(&c.login),
            c.telegram_chat_id()
                .map_or_else(|| "-".to_string(), |id| id.to_string()),
            expiry,
            inbounds
        ));
    }
    ui::send_long(bot, chat, &lines.join("\n")).await
}

async fn delete(bot: &Bot, chat: ChatId, login: &str, state: &AppState) -> Result<()> {
    let login = normalize_login(login);
    if login.is_empty() {
        bail!("укажи логин: /delete <login>");
    }
    let text = if state.panel.delete_client(&login).await? {
        format!("Подписка {} удалена.", format_login(&login))
    } else {
        format!("Подписка {} не найдена.", format_login(&login))
    };
    bot.send_message(chat, text).await?;
    Ok(())
}

async fn broadcast(bot: &Bot, chat: ChatId, text: &str, state: &AppState) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        bail!("укажи текст: /broadcast <текст>");
    }

    let recipients: BTreeSet<i64> = state
        .panel
        .clients()
        .await?
        .iter()
        .filter_map(|c| c.telegram_chat_id())
        .collect();
    let (mut sent, mut failed) = (0, 0);
    for recipient in &recipients {
        match bot.send_message(ChatId(*recipient), text).await {
            Ok(_) => sent += 1,
            Err(err) => {
                failed += 1;
                log::warn!("broadcast to {recipient} failed: {err}");
            }
        }
    }
    bot.send_message(
        chat,
        format!("Рассылка завершена: доставлено {sent}, ошибок {failed}."),
    )
    .await?;
    Ok(())
}

async fn direct_message(bot: &Bot, chat: ChatId, payload: &str, state: &AppState) -> Result<()> {
    let (target, text) = payload
        .trim()
        .split_once(char::is_whitespace)
        .map(|(t, m)| (t, m.trim()))
        .filter(|(_, m)| !m.is_empty())
        .ok_or_else(|| anyhow!("формат: /msg <@login|tg_id> <текст>"))?;

    let recipient = match target.parse::<i64>() {
        Ok(id) if id > 0 => id,
        _ => state
            .panel
            .find_client(target)
            .await?
            .and_then(|c| c.telegram_chat_id())
            .ok_or_else(|| anyhow!("у {} нет Telegram id в подписке", format_login(target)))?,
    };

    bot.send_message(ChatId(recipient), text).await?;
    bot.send_message(chat, format!("Отправлено пользователю {recipient}."))
        .await?;
    Ok(())
}

async fn status(bot: &Bot, chat: ChatId, state: &AppState) -> Result<()> {
    let mut sections = Vec::new();

    sections.push(match state.config.health.as_ref() {
        Some(config) => format!(
            "Цепочка:\n{}",
            health::format_results(&health::run_checks(config).await)
        ),
        None => "Цепочка: проверки выключены (HEALTH_RELAY_ADDR не задан)".into(),
    });

    let panel = &state.panel;
    let (server, online, inbounds) = tokio::join!(
        panel.server_status(),
        panel.online_logins(),
        panel.inbounds()
    );

    sections.push(match server {
        Ok(s) => format!(
            "Выходной сервер:\nxray: {}\nCPU: {:.0}%\nRAM: {} / {}\nДиск: {} / {}\nАптайм: {}",
            s.xray_state.as_deref().unwrap_or("?"),
            s.cpu_percent,
            format_bytes(s.mem_used),
            format_bytes(s.mem_total),
            format_bytes(s.disk_used),
            format_bytes(s.disk_total),
            format_duration(s.uptime_secs)
        ),
        Err(err) => format!("Выходной сервер: ошибка {err:#}"),
    });

    sections.push(match online {
        Ok(mut logins) => {
            logins.sort();
            let list = if logins.is_empty() {
                "никого".to_string()
            } else {
                logins
                    .iter()
                    .map(|l| format_login(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("Онлайн ({}): {list}", logins.len())
        }
        Err(err) => format!("Онлайн: ошибка {err:#}"),
    });

    sections.push(match inbounds {
        Ok(inbounds) => {
            let lines = inbounds
                .iter()
                .map(|i| {
                    format!(
                        "• {}{}: ↑{} ↓{}",
                        i.remark,
                        if i.enabled { "" } else { " (выкл)" },
                        format_bytes(i.up),
                        format_bytes(i.down)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("Трафик за всё время:\n{lines}")
        }
        Err(err) => format!("Трафик: ошибка {err:#}"),
    });

    ui::send_long(bot, chat, &sections.join("\n\n")).await
}

fn parse_request_id(raw: &str) -> Result<u64> {
    raw.trim()
        .trim_start_matches('#')
        .parse()
        .map_err(|_| anyhow!("укажи номер заявки: /approve <id>"))
}

async fn clear_buttons(bot: &Bot, query: &CallbackQuery) -> Result<()> {
    if let Some(message) = query.message.as_ref() {
        bot.edit_message_reply_markup(message.chat().id, message.id())
            .reply_markup(InlineKeyboardMarkup::default())
            .await?;
    }
    Ok(())
}
