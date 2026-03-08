use std::collections::HashSet;

use serde_json::{Map, Value};

use super::models::ExistingSubscription;

pub(crate) fn find_best_connection_url(
    value: &Value,
    email: &str,
    client_uuid: &str,
    sub_id: &str,
) -> Option<String> {
    let mut raw = Vec::new();
    collect_connection_urls(value, &mut raw);
    let candidates = dedup_preserve_order(raw);
    if candidates.is_empty() {
        return None;
    }

    let preferred = choose_url_with_priority(&candidates, email, client_uuid, sub_id);
    preferred.or_else(|| candidates.first().cloned())
}

fn collect_connection_urls(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if is_connection_url_candidate(s) {
                out.push(s.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_connection_urls(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["subUrl", "subscription", "subscriptionUrl", "url", "link"] {
                if let Some(value) = map.get(key) {
                    collect_connection_urls(value, out);
                }
            }
            for value in map.values() {
                collect_connection_urls(value, out);
            }
        }
        _ => {}
    }
}

fn is_connection_url_candidate(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    lower.starts_with("vless://")
        || lower.starts_with("vmess://")
        || lower.starts_with("trojan://")
        || lower.starts_with("ss://")
        || lower.starts_with("hysteria://")
        || lower.starts_with("tuic://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
}

fn is_subscription_url_candidate(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && (lower.contains("/sub") || lower.contains("subscription") || lower.contains("subscribe"))
}

fn is_config_url_candidate(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    lower.starts_with("vless://")
        || lower.starts_with("vmess://")
        || lower.starts_with("trojan://")
        || lower.starts_with("ss://")
        || lower.starts_with("hysteria://")
        || lower.starts_with("tuic://")
}

fn choose_url_with_priority(
    candidates: &[String],
    email: &str,
    client_uuid: &str,
    sub_id: &str,
) -> Option<String> {
    // 1) If any URL references this exact client identity, prefer those first.
    let identity_matched: Vec<&String> = candidates
        .iter()
        .filter(|url| candidate_matches_identity(url, email, client_uuid, sub_id))
        .collect();

    // 2) Inside the selected set, prefer subscription URL, then config URL.
    pick_by_kind(&identity_matched).or_else(|| {
        // 3) If no identity match found, still prefer subscription URL globally.
        let all: Vec<&String> = candidates.iter().collect();
        pick_by_kind(&all)
    })
}

fn pick_by_kind(urls: &[&String]) -> Option<String> {
    urls.iter()
        .find(|u| is_subscription_url_candidate(u))
        .map(|u| (*u).clone())
        .or_else(|| {
            urls.iter()
                .find(|u| is_config_url_candidate(u))
                .map(|u| (*u).clone())
        })
}

fn candidate_matches_identity(url: &str, email: &str, client_uuid: &str, sub_id: &str) -> bool {
    let has_any_identity = !email.is_empty() || !client_uuid.is_empty() || !sub_id.is_empty();
    if !has_any_identity {
        return false;
    }
    (!client_uuid.is_empty() && url.contains(client_uuid))
        || (!email.is_empty() && url.contains(email))
        || (!sub_id.is_empty() && url.contains(sub_id))
}

fn dedup_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::<String>::new();
    let mut out = Vec::new();
    for value in values {
        let key = value.trim().to_string();
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(value);
    }
    out
}

pub(crate) fn generate_connection_url_from_server_obj(
    obj: &Value,
    base_url: &str,
    inbound_id: i64,
    email: &str,
    client_uuid: &str,
) -> Option<String> {
    match obj {
        Value::Array(items) => items.iter().find_map(|v| {
            generate_connection_url_from_server_obj(v, base_url, inbound_id, email, client_uuid)
        }),
        Value::Object(map) => {
            if let Some(inner_obj) = map.get("obj") {
                return generate_connection_url_from_server_obj(
                    inner_obj,
                    base_url,
                    inbound_id,
                    email,
                    client_uuid,
                );
            }

            if map.get("id").and_then(Value::as_i64) == Some(inbound_id)
                || map.get("protocol").is_some()
            {
                return generate_vless_link_from_inbound(map, base_url, email, client_uuid);
            }

            None
        }
        _ => None,
    }
}

fn generate_vless_link_from_inbound(
    inbound: &Map<String, Value>,
    base_url: &str,
    email: &str,
    client_uuid: &str,
) -> Option<String> {
    if inbound.get("protocol")?.as_str()? != "vless" {
        return None;
    }

    let settings = parse_json_field(inbound.get("settings")?)?;
    let stream = parse_json_field(inbound.get("streamSettings")?)?;

    let clients = settings.get("clients")?.as_array()?;
    let client = clients.iter().find(|c| {
        (!client_uuid.is_empty() && c.get("id").and_then(Value::as_str) == Some(client_uuid))
            || (!email.is_empty() && c.get("email").and_then(Value::as_str) == Some(email))
    })?;

    let address = inbound_address(inbound, base_url)?;
    let port = inbound.get("port")?.as_i64()?;
    let security = stream
        .get("security")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let network = stream
        .get("network")
        .and_then(Value::as_str)
        .unwrap_or("tcp");

    let uuid = client.get("id")?.as_str()?;
    let mut url = reqwest::Url::parse(&format!("vless://{uuid}@{address}:{port}")).ok()?;
    url.query_pairs_mut().append_pair("type", network);

    match security {
        "reality" => {
            url.query_pairs_mut().append_pair("security", "reality");
            let reality = stream.get("realitySettings")?;
            let settings = reality.get("settings")?;
            append_if_non_empty(
                &mut url,
                "pbk",
                settings.get("publicKey").and_then(Value::as_str),
            );
            append_if_non_empty(
                &mut url,
                "fp",
                settings.get("fingerprint").and_then(Value::as_str),
            );

            if let Some(sni) = first_string(reality.get("serverNames")) {
                append_if_non_empty(&mut url, "sni", Some(&sni));
            }
            if let Some(sid) = first_string(reality.get("shortIds")) {
                append_if_non_empty(&mut url, "sid", Some(&sid));
            }
            append_if_non_empty(
                &mut url,
                "spx",
                settings.get("spiderX").and_then(Value::as_str),
            );

            if network == "tcp" {
                append_if_non_empty(&mut url, "flow", client.get("flow").and_then(Value::as_str));
            }
        }
        "tls" => {
            url.query_pairs_mut().append_pair("security", "tls");
            if let Some(tls_settings) = stream.get("tlsSettings") {
                append_if_non_empty(
                    &mut url,
                    "fp",
                    tls_settings
                        .get("settings")
                        .and_then(|s| s.get("fingerprint"))
                        .and_then(Value::as_str),
                );
                append_if_non_empty(
                    &mut url,
                    "sni",
                    tls_settings.get("serverName").and_then(Value::as_str),
                );
            }
            if network == "tcp" {
                append_if_non_empty(&mut url, "flow", client.get("flow").and_then(Value::as_str));
            }
        }
        _ => {
            url.query_pairs_mut().append_pair("security", "none");
        }
    }

    let remark = inbound
        .get("remark")
        .and_then(Value::as_str)
        .unwrap_or("vpn")
        .to_string();
    let client_email = client
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or(email)
        .to_string();
    url.set_fragment(Some(&format!("{remark}-{client_email}")));

    Some(url.to_string())
}

fn parse_json_field(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        Value::Object(_) | Value::Array(_) => Some(value.clone()),
        _ => None,
    }
}

fn inbound_address(inbound: &Map<String, Value>, base_url: &str) -> Option<String> {
    let listen = inbound
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !listen.is_empty() && listen != "0.0.0.0" {
        return Some(listen.to_string());
    }
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

fn first_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .find_map(|v| v.as_str())
            .map(ToString::to_string),
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(ToString::to_string),
        _ => None,
    }
}

fn append_if_non_empty(url: &mut reqwest::Url, key: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
        url.query_pairs_mut().append_pair(key, v);
    }
}

pub(crate) fn collect_inbound_subscriptions(value: &Value, out: &mut Vec<ExistingSubscription>) {
    let Value::Object(inbound) = value else {
        return;
    };

    let inbound_id = inbound
        .get("id")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let inbound_remark = inbound
        .get("remark")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let settings = inbound
        .get("settings")
        .and_then(parse_json_field)
        .unwrap_or(Value::Null);
    let clients = settings.get("clients").and_then(Value::as_array);
    let Some(clients) = clients else {
        return;
    };

    for client in clients {
        let Value::Object(client) = client else {
            continue;
        };
        let client_id = client
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let email = client
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if email.is_empty() || client_id.is_empty() {
            continue;
        }
        let tg_id = client.get("tgId").and_then(json_value_as_non_empty_string);
        let sub_id = client.get("subId").and_then(json_value_as_non_empty_string);
        let enabled = client
            .get("enable")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let expiry_time = client
            .get("expiryTime")
            .and_then(Value::as_i64)
            .unwrap_or_default();

        out.push(ExistingSubscription {
            client_id,
            email,
            sub_id,
            tg_id,
            enabled,
            expiry_time,
            inbound_remark: inbound_remark.clone(),
            inbound_id,
        });
    }
}

fn json_value_as_non_empty_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
