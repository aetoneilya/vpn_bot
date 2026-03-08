use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, Message, User,
};

use crate::config::is_allowed;
use crate::qr::render_qr_png;
use crate::state::{AppState, PendingCreateRequest};
use crate::storage::InsertPendingResult;
use crate::xui::XuiClient;

const USER_COMMANDS_HINT: &str = "Доступные команды: /vpn и /meme";
const NO_USERNAME_HINT: &str =
    "У тебя не установлен Telegram username. Установи @username и попробуй снова.";
const ACCESS_DENIED: &str = "Access denied.";
const REQUEST_CREATED_TEXT: &str =
    "Сейчас @aetoneilya решит давать ли вам доступ к впн. Ответ придет в течении 3 рабочих дней";
const PENDING_ALREADY_EXISTS_TEXT: &str =
    "У вас уже есть активный запрос на доступ к VPN. Пожалуйста, дождитесь решения администратора.";
const MEME_PROMPT_TEXT: &str = "Отправь мем следующим сообщением (стикер/фото/gif/видео). Возможно это ускорит рассмотрение заявки или я просто похихикаю";
const ADMIN_HELP: &str = "Commands:\n/vpn - Получить доступ к VPN\n/subs - Показать все подписки\n/requests - Показать все pending-запросы\n/delete <login> - Удалить подписку по логину\n/broadcast <text> - Рассылка всем пользователям\n/msg <@login|tg_id> <text> - Сообщение конкретному пользователю\n/approve <id> - Approve pending request\n/deny <id> - Deny pending request";
const USER_HELP: &str = "Commands:\n/vpn - Получить доступ к VPN\n/meme - Отправить мем админу (может ускорить рассмотрение заявки)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestAction {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextCommand {
    Start,
    Help,
    Vpn,
    Meme,
    Subs,
    Requests,
    Delete,
    Broadcast,
    Msg,
    Approve,
    Deny,
    Unknown,
}

impl TextCommand {
    fn parse(input: &str) -> Self {
        match normalize_command(input).as_deref() {
            Some("/start") => Self::Start,
            Some("/help") => Self::Help,
            Some("/vpn") => Self::Vpn,
            Some("/meme") => Self::Meme,
            Some("/subs") => Self::Subs,
            Some("/requests") => Self::Requests,
            Some("/delete") => Self::Delete,
            Some("/broadcast") => Self::Broadcast,
            Some("/msg") => Self::Msg,
            Some("/approve") => Self::Approve,
            Some("/deny") => Self::Deny,
            _ => Self::Unknown,
        }
    }

    fn is_allowed_for_user(self) -> bool {
        matches!(self, Self::Start | Self::Help | Self::Vpn | Self::Meme)
    }
}

pub async fn handle_text(bot: Bot, msg: Message, text: &str, state: Arc<AppState>) -> Result<()> {
    let actor_id = message_user_id(&msg)?;
    let is_admin = state.config.approver_user_ids.contains(&actor_id);
    let command = TextCommand::parse(text);

    if !is_admin && !command.is_allowed_for_user() {
        bot.send_message(msg.chat.id, USER_COMMANDS_HINT).await?;
        return Ok(());
    }

    let arg1 = first_arg(text);
    match command {
        TextCommand::Start | TextCommand::Help => send_help(&bot, msg.chat.id, is_admin).await?,
        TextCommand::Vpn => handle_vpn_access(bot, msg, state).await?,
        TextCommand::Meme => handle_meme_command(bot, msg, state).await?,
        TextCommand::Subs => handle_subs(bot, msg, state).await?,
        TextCommand::Requests => handle_requests(bot, msg, state).await?,
        TextCommand::Delete => handle_delete(bot, msg, arg1, state).await?,
        TextCommand::Broadcast => handle_broadcast(bot, msg, command_tail(text), state).await?,
        TextCommand::Msg => handle_direct_message(bot, msg, command_tail(text), state).await?,
        TextCommand::Approve => handle_approve_command(bot, msg, arg1, state).await?,
        TextCommand::Deny => handle_deny_command(bot, msg, arg1, state).await?,
        TextCommand::Unknown => {}
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
        bot.send_message(msg.chat.id, ACCESS_DENIED).await?;
        return Ok(());
    }

    if !is_meme_message(&msg) {
        bot.send_message(msg.chat.id, USER_COMMANDS_HINT).await?;
        return Ok(());
    }

    if !state.consume_meme_mode(actor_id)? {
        bot.send_message(
            msg.chat.id,
            "Чтобы отправить мем админу, сначала вызови /meme.",
        )
        .await?;
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
        bot.send_message(admin_chat, meta.clone())
            .reply_markup(meme_feedback_keyboard(msg.chat.id))
            .await?;
        bot.copy_message(admin_chat, msg.chat.id, msg.id).await?;
    }

    bot.send_message(msg.chat.id, "Мем будет обхихикан админом.😂👌")
        .await?;
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

    if let Some(chat_id_raw) = data.strip_prefix("meme_like:") {
        let chat_id = parse_chat_id(chat_id_raw)?;
        bot.send_message(ChatId(chat_id), "ваш мем прикольный и смешной 👍(лайк)")
            .await?;
        clear_callback_buttons(&bot, &callback).await?;
        bot.answer_callback_query(callback.id)
            .text("Оценка отправлена")
            .await?;
        return Ok(());
    }

    if let Some(chat_id_raw) = data.strip_prefix("meme_dislike:") {
        let chat_id = parse_chat_id(chat_id_raw)?;
        bot.send_message(
            ChatId(chat_id),
            "сожалеем, уровень прикола вашего мема неудовлетворительный📉🫤",
        )
        .await?;
        clear_callback_buttons(&bot, &callback).await?;
        bot.answer_callback_query(callback.id)
            .text("Оценка отправлена")
            .await?;
        return Ok(());
    }

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
    let text = if is_admin { ADMIN_HELP } else { USER_HELP };
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

    match state.create_request(request.clone())? {
        InsertPendingResult::Created(request_id) => {
            log::info!(
                "pending request created id={} requester_user_id={} email={}",
                request_id,
                tg_user_id,
                email
            );

            bot.send_message(msg.chat.id, REQUEST_CREATED_TEXT).await?;

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
            bot.send_message(msg.chat.id, PENDING_ALREADY_EXISTS_TEXT)
                .await?;
        }
    }

    Ok(())
}

async fn handle_meme_command(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let actor_id = message_user_id(&msg)?;
    if !is_allowed(actor_id, &state.config.allow_user_ids) {
        bot.send_message(msg.chat.id, ACCESS_DENIED).await?;
        return Ok(());
    }

    state.arm_meme_mode(actor_id)?;
    bot.send_message(msg.chat.id, MEME_PROMPT_TEXT).await?;
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

    let requests = state.list_requests()?;
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
    let login = normalize_login(login);
    if login.is_empty() {
        return Err(anyhow!("empty login for /delete"));
    }

    let client = logged_in_xui(&state).await?;
    let deleted = client.delete_subscription_by_email(&login).await?;
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

    let recipients = collect_recipients_from_subscriptions(&state).await?;
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

    let chat_id = resolve_message_target_chat_id(target_raw, &state).await?;

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

    let Some(request) = state.take_request(request_id)? else {
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
            "Твой запрос подтвержден, но ссылка не найдена в ответе сервера. Попробуй /vpn через минуту.",
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

    let Some(request) = state.take_request(request_id)? else {
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

fn meme_feedback_keyboard(user_chat_id: ChatId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("лайк", format!("meme_like:{}", user_chat_id.0)),
        InlineKeyboardButton::callback("дизлайк", format!("meme_dislike:{}", user_chat_id.0)),
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
        bot.send_message(msg.chat.id, ACCESS_DENIED).await?;
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

async fn collect_recipients_from_subscriptions(state: &AppState) -> Result<BTreeSet<i64>> {
    let client = logged_in_xui(state).await?;
    let subs = client.list_existing_subscriptions().await?;
    let mut recipients = BTreeSet::<i64>::new();

    for s in subs {
        if let Some(tg_id) = s.tg_id
            && let Ok(parsed) = tg_id.trim().parse::<i64>()
            && parsed > 0
        {
            recipients.insert(parsed);
        }
    }

    Ok(recipients)
}

async fn resolve_message_target_chat_id(target_raw: &str, state: &AppState) -> Result<i64> {
    if let Ok(id) = target_raw.trim().parse::<i64>() {
        if id > 0 {
            return Ok(id);
        }
        return Err(anyhow!("tg_id must be positive"));
    }

    let target_login = normalize_login(target_raw);
    if target_login.is_empty() {
        return Err(anyhow!("empty login in /msg"));
    }

    let client = logged_in_xui(state).await?;
    let subs = client.list_existing_subscriptions().await?;
    let tg_id = subs
        .into_iter()
        .find(|s| normalize_login(&s.email) == target_login)
        .and_then(|s| s.tg_id)
        .ok_or_else(|| anyhow!("user not found or tgId is empty"))?;

    let chat_id = tg_id
        .trim()
        .parse::<i64>()
        .context("invalid tgId format in subscription")?;
    if chat_id <= 0 {
        return Err(anyhow!("invalid tgId value in subscription"));
    }
    Ok(chat_id)
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

fn parse_chat_id(raw: &str) -> Result<i64> {
    raw.parse::<i64>().context("invalid chat id")
}

async fn clear_callback_buttons(bot: &Bot, callback: &CallbackQuery) -> Result<()> {
    if let Some(message) = callback.message.as_ref() {
        bot.edit_message_reply_markup(message.chat().id, message.id())
            .reply_markup(InlineKeyboardMarkup::new(
                Vec::<Vec<InlineKeyboardButton>>::new(),
            ))
            .await?;
    }
    Ok(())
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

fn normalize_login(login: &str) -> String {
    login.trim().trim_start_matches('@').to_lowercase()
}

fn is_meme_message(msg: &Message) -> bool {
    msg.sticker().is_some()
        || msg.photo().is_some()
        || msg.animation().is_some()
        || msg.video().is_some()
}
