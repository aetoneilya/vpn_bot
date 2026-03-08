# vpn_bot

Rust Telegram bot that creates VPN users in 3X-UI.

## Features

- Non-admin commands:
  - `/vpn` requests VPN access (or returns existing access immediately)
  - `/meme` arms meme sending flow; next meme is forwarded to admin (may speed up review)
- Existing config check by Telegram username before creating a pending request
- `/approve <id>` and `/deny <id>` commands for manual approval
- Admin command `/subs` shows all existing subscriptions
- Admin command `/requests` shows all pending access requests
- Admin command `/delete <login>` deletes a subscription by login
- Admin command `/broadcast <text>` sends a message to all users with non-empty `tgId`
- Admin command `/msg <@login|tg_id> <text>` sends a message to one user
- Approval messages for admins include inline `Approve` / `Deny` buttons
- After approval, bot sends connection URL and QR code to requester
- Optional Telegram user allowlist with `ALLOW_USER_IDS`
- Required approver allowlist with `APPROVER_USER_IDS`
- Configurable limits (`XUI_TOTAL_GB`)
- Pending requests are stored in SQLite and survive restarts

## Setup

1. Copy `.env.example` to `.env` and fill values.
2. Run the bot:

```bash
cargo run
```

## Required environment variables

- `TELOXIDE_TOKEN`
- `XUI_BASE_URL`
- `XUI_USERNAME`
- `XUI_PASSWORD`
- `XUI_INBOUND_ID`
- `APPROVER_USER_IDS`
- `SQLITE_PATH` (optional, default: `vpn_bot.sqlite3`)

## Notes

- 3X-UI API endpoints differ by version/build. If `/vpn` fails, inspect your panel's Network tab and adjust:
  - `XUI_LOGIN_PATH`
  - `XUI_ADD_CLIENT_PATH`
  - `XUI_DELETE_CLIENT_PATH`
  - `XUI_GET_INBOUND_PATH`
  - `XUI_LIST_INBOUNDS_PATH`
- For 3X-UI `2.8.x` builds, delete endpoint is usually: `/panel/api/inbounds/{id}/delClient/{clientId}`.
- If panel URL contains a secret prefix (example: `https://host:2053/<secret>/panel/inbounds`), set `XUI_BASE_URL` to `https://host:2053/<secret>` (without `/panel`).
- Keep your panel behind firewall/VPN and do not expose admin UI publicly.
- Approval flow:
  - User sends `/vpn`
  - If config for Telegram username already exists on host: bot sends URL + QR immediately
  - If config does not exist: bot sends request ID to approver IDs
  - Approver runs `/approve <id>` or `/deny <id>`
  - On approve, requester receives URL and QR code

## Deploy Script

For systemd deployment on a Linux server, use:

```bash
./scripts/deploy.sh
```

Useful options:

```bash
./scripts/deploy.sh --service-name vpn-bot --install-dir /opt/vpn-bot --env-file .env
./scripts/deploy.sh --no-build
```

After deploy:

```bash
systemctl status vpn-bot
journalctl -u vpn-bot -f
```
