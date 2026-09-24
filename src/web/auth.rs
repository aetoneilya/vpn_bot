//! Validation of Telegram Mini App `initData`
//! (https://core.telegram.org/bots/webapps#validating-data-received-via-the-mini-app).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How long a signed `initData` stays valid.
const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Deserialize)]
pub struct WebAppUser {
    pub id: u64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Malformed,
    BadSignature,
    Expired,
}

/// Verifies the signature and freshness of `init_data` and returns the Telegram user.
pub fn verify_init_data(init_data: &str, bot_token: &str) -> Result<WebAppUser, AuthError> {
    verify_at(init_data, bot_token, SystemTime::now())
}

fn verify_at(init_data: &str, bot_token: &str, now: SystemTime) -> Result<WebAppUser, AuthError> {
    let mut hash = None;
    let mut auth_date = None;
    let mut user = None;
    let mut pairs = Vec::new();

    for (key, value) in url::form_urlencoded::parse(init_data.as_bytes()) {
        match key.as_ref() {
            "hash" => hash = Some(value.into_owned()),
            _ => {
                if key == "auth_date" {
                    auth_date = value.parse::<u64>().ok();
                }
                if key == "user" {
                    user = Some(value.to_string());
                }
                pairs.push(format!("{key}={value}"));
            }
        }
    }

    let hash = hex::decode(hash.ok_or(AuthError::Malformed)?).map_err(|_| AuthError::Malformed)?;
    pairs.sort();
    let data_check_string = pairs.join("\n");

    let secret = sign(b"WebAppData", bot_token.as_bytes());
    let mut mac = HmacSha256::new_from_slice(&secret).expect("HMAC accepts any key length");
    mac.update(data_check_string.as_bytes());
    mac.verify_slice(&hash)
        .map_err(|_| AuthError::BadSignature)?;

    let signed_at = UNIX_EPOCH + Duration::from_secs(auth_date.ok_or(AuthError::Malformed)?);
    if now.duration_since(signed_at).unwrap_or_default() > MAX_AGE {
        return Err(AuthError::Expired);
    }

    serde_json::from_str(&user.ok_or(AuthError::Malformed)?).map_err(|_| AuthError::Malformed)
}

fn sign(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456:TEST-token";

    /// Builds `initData` the way Telegram does, signed with `TOKEN`.
    fn signed(fields: &[(&str, &str)]) -> String {
        let mut pairs: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.sort();
        let secret = sign(b"WebAppData", TOKEN.as_bytes());
        let hash = hex::encode(sign(&secret, pairs.join("\n").as_bytes()));

        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in fields {
            query.append_pair(k, v);
        }
        query.append_pair("hash", &hash);
        query.finish()
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    const USER: &str = r#"{"id":42,"first_name":"Ilya","username":"aetoneilya"}"#;

    #[test]
    fn accepts_valid_data() {
        let data = signed(&[("auth_date", "1000"), ("query_id", "q"), ("user", USER)]);
        let user = verify_at(&data, TOKEN, at(1100)).unwrap();
        assert_eq!(user.id, 42);
        assert_eq!(user.username.as_deref(), Some("aetoneilya"));
    }

    #[test]
    fn rejects_tampered_user() {
        let data = signed(&[("auth_date", "1000"), ("user", USER)]).replace("42", "43");
        assert_eq!(
            verify_at(&data, TOKEN, at(1100)).unwrap_err(),
            AuthError::BadSignature
        );
    }

    #[test]
    fn rejects_other_bot_token() {
        let data = signed(&[("auth_date", "1000"), ("user", USER)]);
        assert_eq!(
            verify_at(&data, "999:other", at(1100)).unwrap_err(),
            AuthError::BadSignature
        );
    }

    #[test]
    fn rejects_stale_data() {
        let data = signed(&[("auth_date", "1000"), ("user", USER)]);
        assert_eq!(
            verify_at(&data, TOKEN, at(1000 + 25 * 3600)).unwrap_err(),
            AuthError::Expired
        );
    }

    #[test]
    fn rejects_missing_hash() {
        assert_eq!(
            verify_at("auth_date=1&user=%7B%7D", TOKEN, at(2)).unwrap_err(),
            AuthError::Malformed
        );
    }
}
