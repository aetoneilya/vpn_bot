#!/usr/bin/env bash
set -euo pipefail

SERVICE_NAME="vpn-bot"
INSTALL_DIR="/opt/vpn-bot"
SERVICE_USER="vpn-bot"
SERVICE_GROUP="vpn-bot"
ENV_FILE=".env"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY_NAME="vpn_bot"
NO_BUILD="false"

usage() {
  cat <<USAGE
Usage: $0 [options]

Options:
  --service-name <name>   systemd service name (default: vpn-bot)
  --install-dir <path>    install directory (default: /opt/vpn-bot)
  --user <name>           service user (default: vpn-bot)
  --group <name>          service group (default: vpn-bot)
  --env-file <path>       path to .env file (default: .env in repo root)
  --repo-dir <path>       path to repo root (default: auto)
  --no-build              skip cargo build --release
  -h, --help              show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --service-name)
      SERVICE_NAME="$2"; shift 2 ;;
    --install-dir)
      INSTALL_DIR="$2"; shift 2 ;;
    --user)
      SERVICE_USER="$2"; shift 2 ;;
    --group)
      SERVICE_GROUP="$2"; shift 2 ;;
    --env-file)
      ENV_FILE="$2"; shift 2 ;;
    --repo-dir)
      REPO_DIR="$2"; shift 2 ;;
    --no-build)
      NO_BUILD="true"; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 1 ;;
  esac
done

if [[ "$ENV_FILE" != /* ]]; then
  ENV_FILE="$REPO_DIR/$ENV_FILE"
fi

if [[ ! -f "$ENV_FILE" ]]; then
  echo "Env file not found: $ENV_FILE" >&2
  exit 1
fi

if ! command -v sudo >/dev/null 2>&1; then
  echo "sudo is required" >&2
  exit 1
fi

if [[ "$NO_BUILD" != "true" ]]; then
  echo "[1/7] Building release binary"
  cargo build --release --manifest-path "$REPO_DIR/Cargo.toml"
else
  echo "[1/7] Skipping build (--no-build)"
fi

BIN_SRC="$REPO_DIR/target/release/$BINARY_NAME"
if [[ ! -x "$BIN_SRC" ]]; then
  echo "Binary not found: $BIN_SRC" >&2
  exit 1
fi

echo "[2/7] Ensuring service user/group"
if ! getent group "$SERVICE_GROUP" >/dev/null; then
  sudo groupadd --system "$SERVICE_GROUP"
fi
if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
  sudo useradd --system --no-create-home --gid "$SERVICE_GROUP" --shell /usr/sbin/nologin "$SERVICE_USER"
fi

echo "[3/7] Creating install directory"
sudo install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$INSTALL_DIR"

echo "[4/7] Installing binary and env"
sudo install -m 0755 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$BIN_SRC" "$INSTALL_DIR/$BINARY_NAME"
sudo install -m 0640 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$ENV_FILE" "$INSTALL_DIR/.env"

echo "[5/7] Preparing sqlite file permissions"
if grep -q '^SQLITE_PATH=' "$INSTALL_DIR/.env"; then
  SQLITE_PATH="$(grep '^SQLITE_PATH=' "$INSTALL_DIR/.env" | tail -n1 | cut -d'=' -f2-)"
  SQLITE_PATH="${SQLITE_PATH%\"}"
  SQLITE_PATH="${SQLITE_PATH#\"}"
  SQLITE_PATH="${SQLITE_PATH%\'}"
  SQLITE_PATH="${SQLITE_PATH#\'}"
  if [[ "$SQLITE_PATH" != /* ]]; then
    SQLITE_PATH="$INSTALL_DIR/$SQLITE_PATH"
  fi
  sudo install -m 0640 -o "$SERVICE_USER" -g "$SERVICE_GROUP" /dev/null "$SQLITE_PATH" || true
fi

echo "[6/7] Installing systemd service"
UNIT_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
TMP_UNIT="$(mktemp)"
cat > "$TMP_UNIT" <<UNIT
[Unit]
Description=VPN Telegram Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_GROUP}
WorkingDirectory=${INSTALL_DIR}
EnvironmentFile=${INSTALL_DIR}/.env
Environment=RUST_LOG=info
ExecStart=${INSTALL_DIR}/${BINARY_NAME}
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
sudo install -m 0644 "$TMP_UNIT" "$UNIT_PATH"
rm -f "$TMP_UNIT"

echo "[7/7] Reloading and restarting service"
sudo systemctl daemon-reload
sudo systemctl enable --now "$SERVICE_NAME"
sudo systemctl restart "$SERVICE_NAME"

sudo systemctl --no-pager --full status "$SERVICE_NAME" | sed -n '1,20p'

echo "Deploy completed. Follow logs with: journalctl -u ${SERVICE_NAME} -f"
