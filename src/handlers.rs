use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, User};

use crate::config::is_allowed;
use crate::qr::render_qr_png;
use crate::state::{AppState, PendingCreateRequest};
use crate::storage::InsertPendingResult;
use crate::xui::XuiClient;

const USER_COMMANDS_HINT: &str = "Доступные команды: /vpn и /qr";
const NO_USERNAME_HINT: &str =
    "У тебя не установлен Telegram username. Установи @username и попробуй снова.";

enum RequestAction {
    Approve,
    Deny,
}

pub async fn handle_text(bot: Bot, msg: Message, text: &str, state: Arc<AppState>) -> Result<()> {
    let command = normalize_command(text);
    let arg1 = first_arg(text);
    let actor_id = message_user_id(&msg)?;
    let is_admin = state.config.approver_user_ids.contains(&actor_id);

    if !is_admin
        && !matches!(
            command.as_deref(),
            Some("/vpn") | Some("/qr") | Some("/start") | Some("/help")
        )
    {
        bot.send_message(msg.chat.id, USER_COMMANDS_HINT).await?;
        return Ok(());
    }

    match command.as_deref() {
        Some("/start") | Some("/help") => send_help(&bot, msg.chat.id, is_admin).await?,
        Some("/vpn") => handle_vpn_access(bot, msg, state).await?,
        Some("/qr") => handle_qr(bot, msg, state).await?,
        Some("/subs") => handle_subs(bot, msg, state).await?,
        Some("/requests") => handle_requests(bot, msg, state).await?,
        Some("/delete") => handle_delete(bot, msg, arg1, state).await?,
        Some("/broadcast") => handle_broadcast(bot, msg, command_tail(text), state).await?,
        Some("/msg") => handle_direct_message(bot, msg, command_tail(text), state).await?,
        Some("/approve") => handle_approve_command(bot, msg, arg1, state).await?,
        Some("/deny") => handle_deny_command(bot, msg, arg1, state).await?,
        _ => {}
    }

    Ok(())
}

pub async fn handle_non_text(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let actor_id = message_user_id(&msg)?;
    if state.config.approver_user_ids.contains(&actor_id) {
        return Ok(());
    }

    if !is_allowed(actor_id, &state.config.allow_user_ids) {
        log::warn!("access denied for user_id={} on non-text message", actor_id);
        bot.send_message(msg.chat.id, "Access denied.").await?;
        return Ok(());
    }

    if !is_meme_message(&msg) {
        bot.send_message(msg.chat.id, USER_COMMANDS_HINT).await?;
        return Ok(());
    }

    let from = message_user(&msg)?;
    let login = from
        .username
        .as_deref()
        .map(format_login)
        .unwrap_or_else(|| "<no username>".to_string());
    let meta = format!(
        "Мем от {login} (id: {})\nchat_id: {}",
        from.id.0, msg.chat.id.0
    );

    for approver_id in &state.config.approver_user_ids {
        let admin_chat = ChatId(*approver_id as i64);
        bot.send_message(admin_chat, meta.clone()).await?;
        bot.copy_message(admin_chat, msg.chat.id, msg.id).await?;
    }

    bot.send_message(msg.chat.id, "Мем отправлен админу.").await?;
    Ok(())
}

pub async fn handle_callback(
    bot: Bot,
    callback: CallbackQuery,
    state: Arc<AppState>,
) -> Result<()> {
    let actor_id = callback.from.id.0;
    if !state.config.approver_user_ids.contains(&actor_id) {
        bot.answer_callback_query(callback.id)
            .text("Only approver can use this action")
            .await?;
        return Ok(());
    }

    let Some(data) = callback.data.as_deref() else {
        bot.answer_callback_query(callback.id)
            .text("Missing callback data")
            .await?;
        return Ok(());
    };

    let Some((action_raw, id_raw)) = data.split_once(':') else {
        bot.answer_callback_query(callback.id)
            .text("Invalid callback format")
            .await?;
        return Ok(());
    };

    let request_id = match id_raw.parse::<u64>() {
        Ok(v) => v,
        Err(_) => {
            bot.answer_callback_query(callback.id)
                .text("Invalid request id")
                .await?;
            return Ok(());
        }
    };

    let admin_chat_id = callback
        .message
        .as_ref()
        .map(|m| m.chat().id)
        .unwrap_or(ChatId(actor_id as i64));

    match parse_action(action_raw) {
        Some(RequestAction::Approve) => {
            approve_request(&bot, &state, request_id, admin_chat_id).await?;
            bot.answer_callback_query(callback.id)
                .text("Request approved")
                .await?;
        }
        Some(RequestAction::Deny) => {
            deny_request(&bot, &state, request_id, admin_chat_id).await?;
            bot.answer_callback_query(callback.id)
                .text("Request denied")
                .await?;
        }
        None => {
            bot.answer_callback_query(callback.id)
                .text("Unknown action")
                .await?;
        }
    }

    Ok(())
}

async fn send_help(bot: &Bot, chat_id: ChatId, is_admin: bool) -> Result<()> {
    let text = if is_admin {
        "Commands:\n/vpn - Получить доступ к VPN\n/qr - Получить QR для существующего доступа\n/subs - Показать все подписки\n/requests - Показать все pending-запросы\n/delete <login> - Удалить подписку по логину\n/broadcast <text> - Рассылка всем пользователям\n/msg <@login|tg_id> <text> - Сообщение конкретному пользователю\n/approve <id> - Approve pending request\n/deny <id> - Deny pending request"
    } else {
        "Commands:\n/vpn - Получить доступ к VPN\n/qr - Получить QR для существующего доступа"
    };
    bot.send_message(chat_id, text).await?;
    Ok(())
}

async fn handle_vpn_access(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let Some((tg_user_id, email)) = resolve_user_login(&bot, &msg, &state, "/vpn").await? else {
        return Ok(());
    };

    log::info!(
        "vpn access command from user_id={} chat_id={} email={}",
        tg_user_id,
        msg.chat.id.0,
        email
    );

    let client = logged_in_xui(&state).await?;

    if let Some(url) = client.find_client_connection_url_by_email(&email).await? {
        log::info!(
            "existing vpn config found user_id={} email={}, sending URL+QR",
            tg_user_id,
            email
        );
        send_url_and_qr(
            &bot,
            msg.chat.id,
            &url,
            "Твоя VPN-конфигурация уже существует.",
        )
        .await?;
        return Ok(());
    }

    let request = PendingCreateRequest {
        requester_chat_id: msg.chat.id,
        requester_user_id: tg_user_id,
        custom_email: Some(email.clone()),
    };

    match state.create_request(request.clone()).await? {
        InsertPendingResult::Created(request_id) => {
            log::info!(
                "pending request created id={} requester_user_id={} email={}",
                request_id,
                tg_user_id,
                email
            );

            bot.send_message(
                msg.chat.id,
                "Сейчас @aetoneilya решит давать ли вам доступ к впн. Ответ придет в течении 3 рабочих дней",
            )
            .await?;

            let approver_text = format!(
                "New VPN request #{request_id}\nFrom user: `{}`\nLogin: `{}`",
                request.requester_user_id,
                format_login(request.custom_email.as_deref().unwrap_or("<none>"))
            );

            for approver_id in &state.config.approver_user_ids {
                log::debug!(
                    "notifying approver user_id={} about request_id={}",
                    approver_id,
                    request_id
                );
                bot.send_message(ChatId(*approver_id as i64), approver_text.clone())
                    .reply_markup(approval_keyboard(request_id))
                    .await?;
            }
        }
        InsertPendingResult::Existing(existing_id) => {
            log::info!(
                "duplicate pending request ignored id={} requester_user_id={}",
                existing_id,
                tg_user_id
            );
            bot.send_message(
                msg.chat.id,
                "У вас уже есть активный запрос на доступ к VPN. Пожалуйста, дождитесь решения администратора.",
            )
            .await?;
        }
    }

    Ok(())
}

async fn handle_qr(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let Some((tg_user_id, username)) = resolve_user_login(&bot, &msg, &state, "/qr").await? else {
        return Ok(());
    };

    let client = logged_in_xui(&state).await?;

    if let Some(url) = client
        .find_client_connection_url_by_email(&username)
        .await?
    {
        log::info!(
            "qr requested for existing config user_id={} email={}",
            tg_user_id,
            username
        );
        send_url_and_qr(&bot, msg.chat.id, &url, "Твоя текущая VPN-конфигурация:").await?;
    } else {
        log::warn!(
            "qr requested but config not found user_id={} email={}",
            tg_user_id,
            username
        );
        bot.send_message(
            msg.chat.id,
            "Конфигурация не найдена. Отправь /vpn, чтобы запросить доступ.",
        )
        .await?;
    }

    Ok(())
}

async fn handle_subs(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;

    let client = logged_in_xui(&state).await?;
    let mut subs = client.list_existing_subscriptions().await?;
    subs.sort_by(|a, b| a.email.cmp(&b.email));

    if subs.is_empty() {
        bot.send_message(msg.chat.id, "Подписки не найдены.")
            .await?;
        return Ok(());
    }

    let mut lines = Vec::with_capacity(subs.len() + 1);
    lines.push(format!("Найдено подписок: {}", subs.len()));
    for s in subs {
        let expiry = if s.expiry_time <= 0 {
            "never".to_string()
        } else {
            Utc.timestamp_millis_opt(s.expiry_time)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| s.expiry_time.to_string())
        };
        lines.push(format!(
            "• {} | tg:{} | enabled:{} | exp:{} | inbound:{} ({})",
            format_login(&s.email),
            s.tg_id.unwrap_or_else(|| "-".to_string()),
            s.enabled,
            expiry,
            s.inbound_id,
            s.inbound_remark
        ));
    }

    send_text_chunks(&bot, msg.chat.id, &lines.join("\n"), 3500).await
}

async fn handle_requests(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;

    let requests = state.list_requests().await?;
    if requests.is_empty() {
        bot.send_message(msg.chat.id, "Pending-запросов нет.")
            .await?;
        return Ok(());
    }

    bot.send_message(msg.chat.id, format!("Pending-запросов: {}", requests.len()))
        .await?;

    for item in requests {
        let created = Utc
            .timestamp_opt(item.created_at_unix, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| item.created_at_unix.to_string());

        let text = format!(
            "#{}\nuser_id: {}\nchat_id: {}\nlogin: {}\ncreated: {}",
            item.id,
            item.request.requester_user_id,
            item.request.requester_chat_id.0,
            format_login(&item.request.custom_email.unwrap_or_else(|| "-".to_string())),
            created
        );

        bot.send_message(msg.chat.id, text)
            .reply_markup(approval_keyboard(item.id))
            .await?;
    }

    Ok(())
}

async fn handle_delete(
    bot: Bot,
    msg: Message,
    arg1: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;
    let login = arg1.ok_or_else(|| anyhow!("missing login for /delete"))?;
    let login = login.trim().trim_start_matches('@');
    if login.is_empty() {
        return Err(anyhow!("empty login for /delete"));
    }

    let client = logged_in_xui(&state).await?;
    let deleted = client.delete_subscription_by_email(login).await?;
    if deleted {
        bot.send_message(msg.chat.id, format!("Подписка `{login}` удалена."))
            .await?;
    } else {
        bot.send_message(
            msg.chat.id,
            format!("Подписка `{login}` не найдена или не удалена."),
        )
        .await?;
    }
    Ok(())
}

async fn handle_broadcast(
    bot: Bot,
    msg: Message,
    text: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;
    let text = text
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("missing text for /broadcast"))?;

    let client = logged_in_xui(&state).await?;
    let subs = client.list_existing_subscriptions().await?;
    let mut recipients = std::collections::BTreeSet::<i64>::new();
    for s in subs {
        if let Some(tg_id) = s.tg_id
            && let Ok(parsed) = tg_id.parse::<i64>()
            && parsed > 0
        {
            recipients.insert(parsed);
        }
    }

    if recipients.is_empty() {
        bot.send_message(
            msg.chat.id,
            "Не найдено получателей для рассылки (tgId пустой).",
        )
        .await?;
        return Ok(());
    }

    let mut sent = 0usize;
    let mut failed = 0usize;
    for chat_id in recipients {
        match bot.send_message(ChatId(chat_id), text.to_string()).await {
            Ok(_) => sent += 1,
            Err(err) => {
                failed += 1;
                log::warn!("broadcast failed chat_id={} error={}", chat_id, err);
            }
        }
    }

    bot.send_message(
        msg.chat.id,
        format!("Рассылка завершена. Успешно: {sent}, ошибок: {failed}."),
    )
    .await?;
    Ok(())
}

async fn handle_direct_message(
    bot: Bot,
    msg: Message,
    payload: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;
    let payload = payload
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("usage: /msg <@login|tg_id> <text>"))?;

    let (target_raw, text) = split_target_and_text(payload)
        .ok_or_else(|| anyhow!("usage: /msg <@login|tg_id> <text>"))?;

    let chat_id = if let Ok(id) = target_raw.parse::<i64>() {
        if id <= 0 {
            return Err(anyhow!("tg_id must be positive"));
        }
        id
    } else {
        let target_login = target_raw.trim().trim_start_matches('@').to_lowercase();
        if target_login.is_empty() {
            return Err(anyhow!("empty login in /msg"));
        }

        let client = logged_in_xui(&state).await?;
        let subs = client.list_existing_subscriptions().await?;
        let tg_id = subs
            .into_iter()
            .find(|s| s.email.trim().eq_ignore_ascii_case(&target_login))
            .and_then(|s| s.tg_id)
            .ok_or_else(|| anyhow!("user not found or tgId is empty"))?;
        let parsed = tg_id
            .trim()
            .parse::<i64>()
            .context("invalid tgId format in subscription")?;
        if parsed <= 0 {
            return Err(anyhow!("invalid tgId value in subscription"));
        }
        parsed
    };

    bot.send_message(ChatId(chat_id), text.to_string()).await?;
    bot.send_message(
        msg.chat.id,
        format!("Сообщение отправлено пользователю `{}`.", chat_id),
    )
    .await?;
    Ok(())
}

async fn handle_approve_command(
    bot: Bot,
    msg: Message,
    arg1: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;
    let request_id = parse_request_id(arg1)?;
    approve_request(&bot, &state, request_id, msg.chat.id).await
}

async fn handle_deny_command(
    bot: Bot,
    msg: Message,
    arg1: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;
    let request_id = parse_request_id(arg1)?;
    deny_request(&bot, &state, request_id, msg.chat.id).await
}

async fn approve_request(
    bot: &Bot,
    state: &AppState,
    request_id: u64,
    admin_chat_id: ChatId,
) -> Result<()> {
    log::info!(
        "approve action request_id={} admin_chat_id={}",
        request_id,
        admin_chat_id.0
    );

    let Some(request) = state.take_request(request_id).await? else {
        log::warn!("approve failed: request_id={} not found", request_id);
        bot.send_message(admin_chat_id, format!("Request #{request_id} not found."))
            .await?;
        return Ok(());
    };

    bot.send_message(admin_chat_id, format!("Approving request #{request_id}..."))
        .await?;

    let client = logged_in_xui(state).await?;
    let created = client
        .add_client(request.requester_user_id, request.custom_email.as_deref())
        .await
        .with_context(|| format!("failed to create VPN user for request #{request_id}"))?;

    log::info!(
        "request approved id={} requester_user_id={} url_found={}",
        request_id,
        request.requester_user_id,
        created.connection_url.is_some()
    );

    let mut approver_message = format!("Approved #{request_id}.\n{}", created.summary);
    if let Some(url) = &created.connection_url {
        approver_message.push_str(&format!("\nConnection URL: {url}"));
    }
    bot.send_message(admin_chat_id, approver_message).await?;

    if let Some(url) = &created.connection_url {
        send_url_and_qr(
            bot,
            request.requester_chat_id,
            url,
            "Твой запрос подтвержден.",
        )
        .await?;
    } else {
        bot.send_message(
            request.requester_chat_id,
            "Твой запрос подтвержден, но ссылка не найдена в ответе сервера. Попробуй /qr через минуту.",
        )
        .await?;
    }

    Ok(())
}

async fn deny_request(
    bot: &Bot,
    state: &AppState,
    request_id: u64,
    admin_chat_id: ChatId,
) -> Result<()> {
    log::info!(
        "deny action request_id={} admin_chat_id={}",
        request_id,
        admin_chat_id.0
    );

    let Some(request) = state.take_request(request_id).await? else {
        log::warn!("deny failed: request_id={} not found", request_id);
        bot.send_message(admin_chat_id, format!("Request #{request_id} not found."))
            .await?;
        return Ok(());
    };

    bot.send_message(admin_chat_id, format!("Request #{request_id} denied."))
        .await?;
    bot.send_message(request.requester_chat_id, "Твой запрос отклонен.")
        .await?;

    Ok(())
}

async fn send_url_and_qr(bot: &Bot, chat_id: ChatId, url: &str, title: &str) -> Result<()> {
    bot.send_message(chat_id, format!("{title}\n\nURL:\n{url}"))
        .await?;

    match render_qr_png(url) {
        Ok(qr_png) => {
            let image = InputFile::memory(qr_png).file_name("vpn-connection.png");
            bot.send_photo(chat_id, image)
                .caption("VPN connection QR code")
                .await?;
        }
        Err(err) => {
            log::warn!("failed to render qr: {err:#}");
            bot.send_message(chat_id, format!("Не удалось сгенерировать QR: {err:#}"))
                .await?;
        }
    }

    Ok(())
}

fn approval_keyboard(request_id: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Approve", format!("approve:{request_id}")),
        InlineKeyboardButton::callback("Deny", format!("deny:{request_id}")),
    ]])
}

fn parse_action(raw: &str) -> Option<RequestAction> {
    match raw {
        "approve" => Some(RequestAction::Approve),
        "deny" => Some(RequestAction::Deny),
        _ => None,
    }
}

async fn logged_in_xui(state: &AppState) -> Result<XuiClient> {
    let client = XuiClient::new(state.config.clone())?;
    client.login().await?;
    Ok(client)
}

async fn resolve_user_login(
    bot: &Bot,
    msg: &Message,
    state: &AppState,
    command: &str,
) -> Result<Option<(u64, String)>> {
    let user = message_user(msg)?;
    let tg_user_id = user.id.0;

    if !is_allowed(tg_user_id, &state.config.allow_user_ids) {
        log::warn!("access denied for user_id={} on {}", tg_user_id, command);
        bot.send_message(msg.chat.id, "Access denied.").await?;
        return Ok(None);
    }

    let Some(username) = user.username.clone() else {
        bot.send_message(msg.chat.id, NO_USERNAME_HINT).await?;
        return Ok(None);
    };

    Ok(Some((tg_user_id, username)))
}

async fn ensure_approver(bot: &Bot, msg: &Message, state: &AppState) -> Result<()> {
    let actor_id = message_user_id(msg)?;
    if state.config.approver_user_ids.contains(&actor_id) {
        return Ok(());
    }

    log::warn!("unauthorized approver command from user_id={actor_id}");
    bot.send_message(msg.chat.id, "Only approver can use this command.")
        .await?;
    Err(anyhow!(
        "unauthorized approver command from user {actor_id}"
    ))
}

async fn send_text_chunks(bot: &Bot, chat_id: ChatId, text: &str, chunk_size: usize) -> Result<()> {
    if text.len() <= chunk_size {
        bot.send_message(chat_id, text.to_string()).await?;
        return Ok(());
    }

    let mut current = String::new();
    for line in text.lines() {
        if current.len() + line.len() + 1 > chunk_size && !current.is_empty() {
            bot.send_message(chat_id, current.clone()).await?;
            current.clear();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        bot.send_message(chat_id, current).await?;
    }
    Ok(())
}

fn normalize_command(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    if !first.starts_with('/') {
        return None;
    }
    Some(first.split('@').next().unwrap_or(first).to_string())
}

fn first_arg(text: &str) -> Option<&str> {
    let mut parts = text.split_whitespace();
    let _ = parts.next();
    parts.next().map(str::trim).filter(|v| !v.is_empty())
}

fn command_tail(text: &str) -> Option<&str> {
    let first_space = text.find(char::is_whitespace)?;
    Some(text[first_space..].trim())
}

fn split_target_and_text(payload: &str) -> Option<(&str, &str)> {
    let mut parts = payload.splitn(2, char::is_whitespace);
    let target = parts.next()?.trim();
    let text = parts.next()?.trim();
    if target.is_empty() || text.is_empty() {
        return None;
    }
    Some((target, text))
}

fn message_user_id(msg: &Message) -> Result<u64> {
    Ok(message_user(msg)?.id.0)
}

fn message_user(msg: &Message) -> Result<&User> {
    msg.from
        .as_ref()
        .ok_or_else(|| anyhow!("missing telegram user in message"))
}

fn parse_request_id(request_id: Option<&str>) -> Result<u64> {
    let request_id = request_id.ok_or_else(|| anyhow!("missing request id"))?;
    request_id
        .parse::<u64>()
        .context("request id must be an integer")
}

fn format_login(login: &str) -> String {
    let trimmed = login.trim();
    if trimmed.is_empty() || trimmed == "-" || trimmed == "<none>" {
        return trimmed.to_string();
    }
    if trimmed.starts_with('@') {
        trimmed.to_string()
    } else {
        format!("@{trimmed}")
    }
}

fn is_meme_message(msg: &Message) -> bool {
    msg.sticker().is_some()
        || msg.photo().is_some()
        || msg.animation().is_some()
        || msg.video().is_some()
}
