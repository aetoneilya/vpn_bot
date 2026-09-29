//! User-reported problems: filed from the Mini App (with network context) or via /problem,
//! delivered to approvers, answered and resolved through the bot.

use anyhow::Result;
use teloxide::prelude::*;

use crate::bot::ui;
use crate::geo::GeoInfo;
use crate::state::AppState;

/// Complaints a single user may file per hour.
const HOURLY_LIMIT: u64 = 5;
const MAX_TEXT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    NoConnection,
    Slow,
    Site,
    Other,
}

impl Category {
    pub fn code(self) -> &'static str {
        match self {
            Self::NoConnection => "no_connection",
            Self::Slow => "slow",
            Self::Site => "site",
            Self::Other => "other",
        }
    }

    pub fn from_code(code: &str) -> Self {
        match code {
            "no_connection" => Self::NoConnection,
            "slow" => Self::Slow,
            "site" => Self::Site,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NoConnection => "Не подключается",
            Self::Slow => "Медленно работает",
            Self::Site => "Не открывается сайт",
            Self::Other => "Другое",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewComplaint {
    pub user_id: u64,
    pub chat_id: ChatId,
    pub login: Option<String>,
    pub category: Category,
    pub profile: Option<String>,
    pub network: Option<String>,
    pub site: Option<String>,
    pub comment: Option<String>,
    pub platform: Option<String>,
    /// Round trip from the user's device to the relay, measured by the Mini App.
    pub client_rtt_ms: Option<f64>,
    pub geo: GeoInfo,
}

#[derive(Debug, Clone)]
pub struct Complaint {
    pub id: u64,
    pub created_at: i64,
    pub details: NewComplaint,
}

pub enum SubmitOutcome {
    Submitted(u64),
    RateLimited,
}

/// Stores the complaint and forwards it to approvers.
pub async fn submit(
    bot: &Bot,
    state: &AppState,
    mut complaint: NewComplaint,
) -> Result<SubmitOutcome> {
    let hour_ago = chrono::Utc::now().timestamp() - 3600;
    if state.store.complaints_since(complaint.user_id, hour_ago)? >= HOURLY_LIMIT {
        return Ok(SubmitOutcome::RateLimited);
    }
    for field in [&mut complaint.site, &mut complaint.comment] {
        if let Some(text) = field {
            *text = text.trim().chars().take(MAX_TEXT).collect();
        }
        if field.as_deref().is_some_and(str::is_empty) {
            *field = None;
        }
    }

    let id = state.store.insert_complaint(&complaint)?;
    log::info!(
        "complaint #{id} from {} ({}): {}",
        complaint.login.as_deref().unwrap_or("-"),
        complaint.user_id,
        complaint.category.code()
    );
    let card = Complaint {
        id,
        created_at: chrono::Utc::now().timestamp(),
        details: complaint,
    };
    for approver in &state.config.approver_user_ids {
        bot.send_message(ChatId(*approver as i64), admin_card(&card))
            .reply_markup(ui::complaint_keyboard(id))
            .await?;
    }
    Ok(SubmitOutcome::Submitted(id))
}

/// The message approvers see for a complaint.
pub fn admin_card(c: &Complaint) -> String {
    let d = &c.details;
    let who = d
        .login
        .as_deref()
        .map(ui::format_login)
        .unwrap_or_else(|| format!("id {}", d.user_id));
    let mut lines = vec![format!("🆘 Жалоба #{} от {who}", c.id), String::new()];

    let mut what = d.category.label().to_string();
    if let Some(site) = &d.site {
        what.push_str(&format!(": {site}"));
    }
    lines.push(what);

    let context: Vec<String> = [
        d.profile.as_ref().map(|p| format!("профиль: {p}")),
        d.network.as_ref().map(|n| format!("сеть: {n}")),
        d.platform.as_ref().map(|p| format!("устройство: {p}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !context.is_empty() {
        lines.push(context.join(" · "));
    }

    if d.geo.asn.is_some() || d.geo.country.is_some() {
        lines.push(format!("📍 {}", d.geo.summary()));
    }
    if let Some(rtt) = d.client_rtt_ms {
        lines.push(format!("📶 пинг до входа: {rtt:.0} мс"));
    }
    if let Some(comment) = &d.comment {
        lines.push(String::new());
        lines.push(format!("«{comment}»"));
    }
    lines.push(String::new());
    lines.push(format!("создана {}", ui::format_unix(c.created_at)));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_codes_roundtrip() {
        for c in [
            Category::NoConnection,
            Category::Slow,
            Category::Site,
            Category::Other,
        ] {
            assert_eq!(Category::from_code(c.code()), c);
        }
        assert_eq!(Category::from_code("garbage"), Category::Other);
    }

    #[test]
    fn admin_card_includes_context() {
        let card = admin_card(&Complaint {
            id: 7,
            created_at: 0,
            details: NewComplaint {
                user_id: 1,
                chat_id: ChatId(1),
                login: Some("vika".into()),
                category: Category::Site,
                profile: Some("основной".into()),
                network: Some("мобильный".into()),
                site: Some("youtube.com".into()),
                comment: Some("видео не грузится".into()),
                platform: Some("ios".into()),
                client_rtt_ms: Some(52.4),
                geo: GeoInfo {
                    operator: Some("MTS PJSC".into()),
                    asn: Some(8359),
                    city: Some("Москва".into()),
                    ..Default::default()
                },
            },
        });
        assert!(card.contains("Жалоба #7 от @vika"));
        assert!(card.contains("Не открывается сайт: youtube.com"));
        assert!(card.contains("MTS PJSC (AS8359) · Москва"));
        assert!(card.contains("пинг до входа: 52 мс"));
    }
}
