# vpn_bot

Rust Telegram bot that hands out VPN subscriptions from a 3X-UI 3.x panel.

## Features

Users:
- `/vpn` — returns the existing subscription, or files an access request for approvers
- `/guide` — how to connect, which profile to use, what to do when it stops working
- `/meme` — the next sticker/photo/gif/video is forwarded to approvers
- Buttons under the subscription message: «📖 Инструкция» and «🔄 Получить ссылку заново»

Approvers (`APPROVER_USER_IDS`):
- New requests arrive with «Одобрить» / «Отклонить» buttons; `/approve <id>`, `/deny <id>`, `/requests`
- `/subs` — all clients with their inbounds; `/delete <login>`
- `/broadcast <text>`, `/msg <@login|tg_id> <text>`
- `/status` — relay chain checks, exit server load, online clients and traffic
- Handler errors are reported to approvers in chat; users get a generic message

Telegram Mini App (optional, `WEB_PUBLIC_URL`): a «VPN» menu button opens a page inside the bot where users request access, see the request status, copy their subscription (or open it in Happ), scan a QR and read platform-specific instructions. Requests are authenticated with Telegram's signed `initData`, so no logins are needed.

Background health monitor (optional): probes every Reality profile through the relay and alerts approvers when a check goes down and when it recovers.

Clients are named after the Telegram username (the panel's `email` field) and attached to all `XUI_INBOUND_IDS` at once, so a single subscription carries every profile. Pending requests live in SQLite and survive restarts.

## Configuration

| Variable | Required | Meaning |
|---|---|---|
| `TELOXIDE_TOKEN` | yes | bot token from BotFather |
| `APPROVER_USER_IDS` | yes | comma-separated Telegram user ids of approvers |
| `ALLOW_USER_IDS` | no | if set, only these users may request access |
| `XUI_BASE_URL` | yes | panel root including the secret base path, e.g. `http://127.0.0.1:61563/<path>` |
| `XUI_API_TOKEN` | one of | panel API token (sent as `Authorization: Bearer`) |
| `XUI_USERNAME`, `XUI_PASSWORD` | one of | panel login; used when no API token is set |
| `XUI_INBOUND_IDS` | yes | comma-separated inbound ids every new client is attached to |
| `XUI_SUBSCRIPTION_BASE_URL` | yes | public subscription base, e.g. `https://sub.example.com:2096/sub/` |
| `XUI_CLIENT_FLOW` | no | VLESS flow for new clients; empty (default) for XHTTP/gRPC, `xtls-rprx-vision` for raw TCP Reality |
| `XUI_TOTAL_GB` | no | traffic limit per new client, 0 = unlimited |
| `SQLITE_PATH` | no | default `vpn_bot.sqlite3` |
| `WEB_PUBLIC_URL` | no | public HTTPS URL of the Mini App; unset disables it |
| `WEB_LISTEN` | no | local bind address of the Mini App server, default `127.0.0.1:8080` (put a TLS proxy in front) |
| `HEALTH_RELAY_ADDR` | no | relay `ip:port` clients connect to; unset disables health checks |
| `HEALTH_SNIS` | no | Reality SNIs to probe through the relay, default `ign.com` |
| `HEALTH_SUBSCRIPTION_URL` | no | subscription server URL to probe |
| `HEALTH_INTERVAL_SECS` | no | default `300` |
| `HEALTH_FAIL_THRESHOLD` | no | consecutive failures before an alert, default `2` |

A profile is probed by resolving its SNI to the relay and making an HTTPS request: Reality passes an unauthenticated handshake through to the real site, so a valid response proves client → relay → exit works for that profile.

## Run

```bash
cp .env.example .env   # fill in values
cargo run
```

## Deploy

On a Linux server with Rust installed:

```bash
./scripts/deploy.sh --env-file /path/to/.env
systemctl status vpn-bot
journalctl -u vpn-bot -f
```

The script builds a release binary and installs it as the `vpn-bot` systemd service under `/opt/vpn-bot`. The SQLite database is created once and kept across deploys.
