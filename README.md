# vpn_bot

Rust Telegram bot that creates VPN users in 3X-UI.

## Features

- `/start` and `/help` commands
- `/create [email]` command creates a pending request
- `/approve <id>` and `/deny <id>` commands for manual approval
- After approval, bot sends connection URL and QR code to requester
- Optional Telegram user allowlist with `ALLOW_USER_IDS`
- Required approver allowlist with `APPROVER_USER_IDS`
- Configurable limits (`XUI_TOTAL_GB`, `XUI_EXPIRY_DAYS`)

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

## Notes

- 3X-UI API endpoints differ by version/build. If `/create` fails, inspect your panel's Network tab and adjust:
  - `XUI_LOGIN_PATH`
  - `XUI_ADD_CLIENT_PATH`
  - `XUI_GET_INBOUND_PATH`
  - `XUI_LIST_INBOUNDS_PATH`
- Keep your panel behind firewall/VPN and do not expose admin UI publicly.
- Approval flow:
  - User sends `/create [email]`
  - Bot sends request ID to approver IDs
  - Approver runs `/approve <id>` or `/deny <id>`
  - On approve, requester receives URL and QR code
