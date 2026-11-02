#!/usr/bin/env bash
# Installs rustyclaude-daemon as a systemd user service for the current user.
#
# Usage: ./deploy/install-daemon.sh [bind-ip] [port]
#   bind-ip  default: this machine's LAN IP
#   port     default: the port this user used before, else the first free one from 7878
#
# Several users on one machine each get their own daemon, port and token.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v tmux >/dev/null || { echo "Install tmux first:  sudo apt install -y tmux"; exit 1; }
CLAUDE_BIN="$(command -v claude || true)"
[ -n "$CLAUDE_BIN" ] || CLAUDE_BIN="$HOME/.local/bin/claude"
[ -x "$CLAUDE_BIN" ] || { echo "Claude Code not found. Install it:  curl -fsSL https://claude.ai/install.sh | bash"; exit 1; }
command -v cargo >/dev/null || { [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"; }
command -v cargo >/dev/null || { echo "Install Rust:  curl https://sh.rustup.rs -sSf | sh"; exit 1; }

CONF_DIR="$HOME/.config/rustyclaude"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT="$UNIT_DIR/rustyclaude.service"
mkdir -p "$CONF_DIR" "$UNIT_DIR"

BIND_IP="${1:-$(hostname -I | awk '{print $1}')}"

port_in_use() { [ -n "$(ss -ltnH "sport = :$1" 2>/dev/null)" ]; }

# Port: argument > saved > this user's existing service > first free from 7878.
if [ -n "${2:-}" ]; then
  PORT="$2"
elif [ -f "$CONF_DIR/port" ]; then
  PORT="$(cat "$CONF_DIR/port")"
elif [ -f "$UNIT" ] && grep -qoE -- '--bind [^ ]+:[0-9]+' "$UNIT"; then
  PORT="$(grep -oE -- '--bind [^ ]+:[0-9]+' "$UNIT" | sed 's/.*://')"
else
  PORT=7878
  while port_in_use "$PORT"; do PORT=$((PORT + 1)); done
fi
echo "$PORT" > "$CONF_DIR/port"

echo "Building daemon…"
cargo build --release -p rustyclaude-daemon

if [ ! -f "$CONF_DIR/token" ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$CONF_DIR/token"
  chmod 600 "$CONF_DIR/token"
fi
TOKEN="$(cat "$CONF_DIR/token")"

cat > "$UNIT" <<UNITFILE
[Unit]
Description=RustyClaude daemon for $USER (keeps Claude Code tmux sessions running)
After=network-online.target

[Service]
Environment=RUSTYCLAUDE_TOKEN=$TOKEN
Environment=PATH=$(dirname "$CLAUDE_BIN"):/usr/local/bin:/usr/bin:/bin
ExecStart=$PWD/target/release/rustyclaude-daemon --bind $BIND_IP:$PORT --claude $CLAUDE_BIN
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
UNITFILE

systemctl --user daemon-reload
systemctl --user enable rustyclaude >/dev/null
systemctl --user restart rustyclaude
loginctl enable-linger "$USER" 2>/dev/null || sudo loginctl enable-linger "$USER"

# Make sure it actually came up (e.g. the port isn't held by someone else's daemon).
sleep 2
if ! systemctl --user is-active --quiet rustyclaude || ! port_in_use "$PORT"; then
  echo
  echo "The daemon did not start. Last log lines:"
  journalctl --user -u rustyclaude -n 15 --no-pager || true
  echo
  echo "If the port is taken, rerun with another one:  ./deploy/install-daemon.sh $BIND_IP $((PORT + 1))"
  exit 1
fi

cat <<DONE

Daemon for $USER running on ws://$BIND_IP:$PORT (starts at boot).

Paste the block between the lines into ~/Library/Application Support/rustyclaude/hosts.toml
on your Mac (as its own entry, keep the others), then restart RustyClaude.
------------------------------------------------------------------------
[[host]]
name = "$(hostname)-$USER"
url = "ws://$BIND_IP:$PORT"
token = "$TOKEN"
------------------------------------------------------------------------

Logs:  journalctl --user -u rustyclaude -f
DONE
