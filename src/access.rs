//! Access flow shared by the bot commands and the Mini App: who has a subscription,
//! who is waiting for approval, and filing new requests.

use anyhow::Result;
use teloxide::prelude::*;

use crate::bot::approval_request_message;
use crate::panel::{Client, normalize_login};
use crate::state::AppState;
use crate::storage::InsertResult;

pub enum AccessStatus {
    Active(Client),
    Pending(u64),
    None,
}

pub enum RequestOutcome {
    AlreadyActive(Client),
    Created(u64),
    AlreadyPending(u64),
}

pub async fn status(state: &AppState, user_id: u64, username: &str) -> Result<AccessStatus> {
    if let Some(client) = state.panel.find_client(username).await? {
        return Ok(AccessStatus::Active(client));
    }
    Ok(match state.store.find_by_user(user_id)? {
        Some(request) => AccessStatus::Pending(request.id),
        None => AccessStatus::None,
    })
}

/// Files an access request (or returns the existing subscription) and notifies approvers.
/// `chat_id` is where the approval result will be delivered.
pub async fn request(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    user_id: u64,
    username: &str,
) -> Result<RequestOutcome> {
    let login = normalize_login(username);
    if let Some(client) = state.panel.find_client(&login).await? {
        return Ok(RequestOutcome::AlreadyActive(client));
    }

    match state.store.insert(chat_id, user_id, &login)? {
        InsertResult::AlreadyPending(id) => Ok(RequestOutcome::AlreadyPending(id)),
        InsertResult::Created(id) => {
            log::info!("access request #{id} from {login} ({user_id})");
            for approver in &state.config.approver_user_ids {
                let (text, keyboard) = approval_request_message(id, &login, user_id);
                bot.send_message(ChatId(*approver as i64), text)
                    .reply_markup(keyboard)
                    .await?;
            }
            Ok(RequestOutcome::Created(id))
        }
    }
}
