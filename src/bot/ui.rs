//! Texts, keyboards, callback payloads and formatting shared by user and admin flows.

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, WebAppInfo};

use crate::panel::{Client, Panel};
use crate::qr::render_qr_png;
use crate::state::AppState;

pub const REQUEST_CREATED: &str =
    "Сейчас @aetoneilya решит, давать ли тебе доступ к VPN. Ответ придёт в течение 3 рабочих дней.";
pub const REQUEST_ALREADY_PENDING: &str =
    "У тебя уже есть заявка на доступ. Дождись решения администратора.";
pub const NO_USERNAME: &str =
    "У тебя не установлен Telegram username. Установи @username в настройках и попробуй снова.";
pub const ACCESS_DENIED: &str = "Доступ запрещён.";
pub const UNKNOWN_INPUT: &str = "Не понял. Доступные команды: /vpn, /guide, /meme.";
pub const MEME_PROMPT: &str = "Отправь мем следующим сообщением (стикер, фото, gif или видео). Возможно, это ускорит рассмотрение заявки — или я просто похихикаю.";
pub const MEME_NOT_ARMED: &str = "Чтобы отправить мем админу, сначала вызови /meme.";
pub const MEME_SENT: &str = "Мем будет обхихикан админом.😂👌";
pub const MEME_LIKE: &str = "ваш мем прикольный и смешной 👍(лайк)";
pub const MEME_DISLIKE: &str = "сожалеем, уровень прикола вашего мема неудовлетворительный📉🫤";
pub const USER_ERROR: &str = "Что-то пошло не так. Попробуй позже.";

pub const GUIDE: &str = "📖 Как подключиться

1. Установи приложение:
• iPhone, iPad, Mac — Happ (App Store)
• Android — Happ или v2rayNG
• Windows — Happ или Hiddify

2. Скопируй ссылку подписки из сообщения бота. Открывать её не нужно.

3. В приложении добавь подписку из буфера обмена: «+» → «Добавить из буфера». Happ обычно сам предлагает это сделать.

4. Выбери профиль «🇳🇱 основной» и подключись.

Профили в подписке:
• 🇳🇱 основной — используй по умолчанию.
• 🇷🇺 белые списки — когда на мобильном интернете открываются только VK и Яндекс, а основной профиль не подключается.

Если перестало работать:
1. Обнови подписку в приложении (в Happ — потяни список вниз или нажми ↻).
2. Попробуй другой профиль.
3. Не помогло — нажми «🔄 Получить ссылку заново» или напиши /vpn.";

/// Inline button payloads. Encoded as short `kind:arg` strings (Telegram limit is 64 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Guide,
    Resend,
    Approve(u64),
    Deny(u64),
    MemeLike(i64),
    MemeDislike(i64),
}

impl Action {
    pub fn encode(self) -> String {
        match self {
            Self::Guide => "guide".into(),
            Self::Resend => "resend".into(),
            Self::Approve(id) => format!("approve:{id}"),
            Self::Deny(id) => format!("deny:{id}"),
            Self::MemeLike(chat) => format!("meme_like:{chat}"),
            Self::MemeDislike(chat) => format!("meme_dislike:{chat}"),
        }
    }

    pub fn decode(data: &str) -> Option<Self> {
        let (kind, arg) = data.split_once(':').unwrap_or((data, ""));
        Some(match kind {
            "guide" => Self::Guide,
            "resend" => Self::Resend,
            "approve" => Self::Approve(arg.parse().ok()?),
            "deny" => Self::Deny(arg.parse().ok()?),
            "meme_like" => Self::MemeLike(arg.parse().ok()?),
            "meme_dislike" => Self::MemeDislike(arg.parse().ok()?),
            _ => return None,
        })
    }

    /// Actions that only approvers may trigger.
    pub fn is_admin_only(self) -> bool {
        !matches!(self, Self::Guide | Self::Resend)
    }

    fn button(self, label: &str) -> InlineKeyboardButton {
        InlineKeyboardButton::callback(label, self.encode())
    }
}

pub fn subscription_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        Action::Guide.button("📖 Инструкция"),
        Action::Resend.button("🔄 Получить ссылку заново"),
    ]])
}

pub fn approval_keyboard(request_id: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        Action::Approve(request_id).button("✅ Одобрить"),
        Action::Deny(request_id).button("❌ Отклонить"),
    ]])
}

pub fn meme_keyboard(user_chat: ChatId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        Action::MemeLike(user_chat.0).button("лайк"),
        Action::MemeDislike(user_chat.0).button("дизлайк"),
    ]])
}

/// The Mini App link, if `WEB_PUBLIC_URL` is configured and valid.
pub fn web_app_info(state: &AppState) -> Option<WebAppInfo> {
    let web = state.config.web.as_ref()?;
    match web.public_url.parse() {
        Ok(url) => Some(WebAppInfo { url }),
        Err(err) => {
            log::warn!("invalid WEB_PUBLIC_URL {}: {err}", web.public_url);
            None
        }
    }
}

pub fn web_app_keyboard(state: &AppState) -> Option<InlineKeyboardMarkup> {
    web_app_info(state).map(|info| {
        InlineKeyboardMarkup::new([[InlineKeyboardButton::web_app("🔐 Открыть VPN", info)]])
    })
}

/// Sends the subscription link with explanation, buttons and a QR code.
pub async fn send_subscription(
    bot: &Bot,
    chat_id: ChatId,
    panel: &Panel,
    client: &Client,
    title: &str,
) -> Result<()> {
    let url = panel.subscription_url(client);
    let text = format!(
        "{title}\n\n👉 Это ссылка на подписку. Скопируй её и добавь в VPN-приложение как подписку — открывать ссылку не нужно.\n\nВ подписке 2 профиля:\n• 🇳🇱 основной — по умолчанию\n• 🇷🇺 белые списки — если на мобильном интернете работают только VK и Яндекс\n\nПодробнее — кнопка «📖 Инструкция» или /guide.\n\n{url}"
    );
    bot.send_message(chat_id, text)
        .reply_markup(subscription_keyboard())
        .await?;

    let qr = render_qr_png(&url)?;
    bot.send_photo(chat_id, InputFile::memory(qr).file_name("subscription.png"))
        .caption("QR-код подписки")
        .await?;
    Ok(())
}

/// Sends text split on line boundaries to stay under Telegram's message limit.
pub async fn send_long(bot: &Bot, chat_id: ChatId, text: &str) -> Result<()> {
    const LIMIT: usize = 3500;
    let mut chunk = String::new();
    for line in text.lines() {
        if !chunk.is_empty() && chunk.len() + line.len() + 1 > LIMIT {
            bot.send_message(chat_id, std::mem::take(&mut chunk))
                .await?;
        }
        if !chunk.is_empty() {
            chunk.push('\n');
        }
        chunk.push_str(line);
    }
    if !chunk.is_empty() {
        bot.send_message(chat_id, chunk).await?;
    }
    Ok(())
}

pub fn format_login(login: &str) -> String {
    format!("@{}", login.trim().trim_start_matches('@'))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if days > 0 {
        format!("{days} д {hours} ч")
    } else {
        format!("{hours} ч {minutes} мин")
    }
}

pub fn format_unix(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_roundtrip() {
        for action in [
            Action::Guide,
            Action::Resend,
            Action::Approve(42),
            Action::Deny(7),
            Action::MemeLike(-100123),
            Action::MemeDislike(5),
        ] {
            assert_eq!(Action::decode(&action.encode()), Some(action));
        }
        assert_eq!(Action::decode("approve:x"), None);
        assert_eq!(Action::decode("nope"), None);
    }

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(format_bytes(512), "512 Б");
        assert_eq!(format_bytes(1536), "1.5 КБ");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 ГБ");
    }
}
