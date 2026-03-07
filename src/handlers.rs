use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use teloxide::prelude::*;
use teloxide::types::InputFile;

use crate::config::is_allowed;
use crate::qr::render_qr_png;
use crate::state::{AppState, PendingCreateRequest};
use crate::xui::XuiClient;

pub async fn handle_text(bot: Bot, msg: Message, text: &str, state: Arc<AppState>) -> Result<()> {
    let command = normalize_command(text);
    let mut parts = text.split_whitespace();
    let _ = parts.next();
    let arg1 = parts.next().map(str::trim).filter(|v| !v.is_empty());

    match command.as_deref() {
        Some("/start") | Some("/help") => send_help(&bot, msg.chat.id).await?,
        Some("/create") => handle_create(bot, msg, arg1, state).await?,
        Some("/approve") => handle_approve(bot, msg, arg1, state).await?,
        Some("/deny") => handle_deny(bot, msg, arg1, state).await?,
        _ => {}
    }

    Ok(())
}

async fn send_help(bot: &Bot, chat_id: ChatId) -> Result<()> {
    let help = "Commands:\n/start - Show help\n/create [email] - Request VPN user creation\n/approve <id> - Approve pending request (approver only)\n/deny <id> - Deny pending request (approver only)";
    bot.send_message(chat_id, help).await?;
    Ok(())
}

async fn handle_create(
    bot: Bot,
    msg: Message,
    arg1: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    let tg_user_id = message_user_id(&msg)?;

    if !is_allowed(tg_user_id, &state.config.allow_user_ids) {
        bot.send_message(msg.chat.id, "Access denied.").await?;
        return Ok(());
    }

    let request = PendingCreateRequest {
        requester_chat_id: msg.chat.id,
        requester_user_id: tg_user_id,
        custom_email: arg1.map(str::to_string),
    };

    let request_id = state.create_request(request.clone()).await;

    bot.send_message(
        msg.chat.id,
        format!("Request #{request_id} created and sent for approval."),
    )
    .await?;

    let approver_text = format!(
        "New VPN request #{request_id}\nFrom user: `{}`\nEmail: `{}`\nApprove: `/approve {request_id}`\nDeny: `/deny {request_id}`",
        request.requester_user_id,
        request
            .custom_email
            .as_deref()
            .unwrap_or("<auto: tg_user_id>")
    );

    for approver_id in &state.config.approver_user_ids {
        bot.send_message(ChatId(*approver_id as i64), approver_text.clone())
            .await?;
    }

    Ok(())
}

async fn handle_approve(
    bot: Bot,
    msg: Message,
    arg1: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;

    let request_id = parse_request_id(arg1)?;
    let Some(request) = state.take_request(request_id).await else {
        bot.send_message(msg.chat.id, format!("Request #{request_id} not found."))
            .await?;
        return Ok(());
    };

    bot.send_message(msg.chat.id, format!("Approving request #{request_id}..."))
        .await?;

    let client = XuiClient::new(state.config.clone())?;
    client.login().await?;
    let created = client
        .add_client(request.requester_user_id, request.custom_email.as_deref())
        .await
        .with_context(|| format!("failed to create VPN user for request #{request_id}"))?;

    let mut approver_message = format!("Approved #{request_id}.\n{}", created.summary);
    if let Some(url) = &created.connection_url {
        approver_message.push_str(&format!("\nConnection URL: {url}"));
    }
    bot.send_message(msg.chat.id, approver_message).await?;

    let mut requester_message = format!("Your request #{request_id} was approved.");
    if let Some(url) = &created.connection_url {
        requester_message.push_str(&format!("\nConnection URL:\n{url}"));
    } else {
        requester_message
            .push_str("\nClient was created, but connection URL was not found in panel response.");
    }

    bot.send_message(request.requester_chat_id, requester_message)
        .await?;

    if let Some(url) = &created.connection_url {
        match render_qr_png(url) {
            Ok(qr_png) => {
                let image =
                    InputFile::memory(qr_png).file_name(format!("vpn-request-{request_id}.png"));
                bot.send_photo(request.requester_chat_id, image)
                    .caption("VPN connection QR code")
                    .await?;
            }
            Err(err) => {
                bot.send_message(
                    request.requester_chat_id,
                    format!("Failed to generate QR code: {err:#}"),
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn handle_deny(
    bot: Bot,
    msg: Message,
    arg1: Option<&str>,
    state: Arc<AppState>,
) -> Result<()> {
    ensure_approver(&bot, &msg, &state).await?;

    let request_id = parse_request_id(arg1)?;
    let Some(request) = state.take_request(request_id).await else {
        bot.send_message(msg.chat.id, format!("Request #{request_id} not found."))
            .await?;
        return Ok(());
    };

    bot.send_message(msg.chat.id, format!("Request #{request_id} denied."))
        .await?;
    bot.send_message(
        request.requester_chat_id,
        format!("Your request #{request_id} was denied."),
    )
    .await?;

    Ok(())
}

async fn ensure_approver(bot: &Bot, msg: &Message, state: &AppState) -> Result<()> {
    let actor_id = message_user_id(msg)?;
    if state.config.approver_user_ids.contains(&actor_id) {
        return Ok(());
    }

    bot.send_message(msg.chat.id, "Only approver can use this command.")
        .await?;
    Err(anyhow!(
        "unauthorized approver command from user {actor_id}"
    ))
}

fn normalize_command(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    if !first.starts_with('/') {
        return None;
    }
    Some(first.split('@').next().unwrap_or(first).to_string())
}

fn message_user_id(msg: &Message) -> Result<u64> {
    msg.from
        .as_ref()
        .map(|u| u.id.0)
        .ok_or_else(|| anyhow!("missing telegram user in message"))
}

fn parse_request_id(request_id: Option<&str>) -> Result<u64> {
    let request_id = request_id.ok_or_else(|| anyhow!("missing request id"))?;
    request_id
        .parse::<u64>()
        .context("request id must be an integer")
}
