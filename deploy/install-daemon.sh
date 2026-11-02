#!/usr/bin/env bash
# Installs rustyclaude-daemon as a systemd user service on this Linux machine.
# Usage: ./deploy/install-daemon.sh [bind-ip]      (default: this machine's LAN IP)
set -euo pipefail
cd "$(dirname "$0")/.."

command -v tmux >/dev/null || { echo "Install tmux first:  sudo apt install -y tmux"; exit 1; }
CLAUDE_BIN="$(command -v claude || true)"
[ -n "$CLAUDE_BIN" ] || CLAUDE_BIN="$HOME/.local/bin/claude"
[ -x "$CLAUDE_BIN" ] || { echo "Claude Code not found. Install it:  curl -fsSL https://claude.ai/install.sh | bash"; exit 1; }
command -v cargo >/dev/null || { [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"; }
command -v cargo >/dev/null || { echo "Install Rust:  curl https://sh.rustup.rs -sSf | sh"; exit 1; }

BIND_IP="${1:-$(hostname -I | awk '{print $1}')}"
echo "Building daemon…"
cargo build --release -p rustyclaude-daemon

CONF_DIR="$HOME/.config/rustyclaude"
mkdir -p "$CONF_DIR"
if [ ! -f "$CONF_DIR/token" ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$CONF_DIR/token"
  chmod 600 "$CONF_DIR/token"
fi
TOKEN="$(cat "$CONF_DIR/token")"

UNIT_DIR="$HOME/.config/systemd/user"
mkdir -p "$UNIT_DIR"
cat > "$UNIT_DIR/rustyclaude.service" <<UNIT
[Unit]
Description=RustyClaude daemon (keeps Claude Code tmux sessions running)
After=network-online.target

[Service]
Environment=RUSTYCLAUDE_TOKEN=$TOKEN
Environment=PATH=$(dirname "$CLAUDE_BIN"):/usr/local/bin:/usr/bin:/bin
ExecStart=$PWD/target/release/rustyclaude-daemon --bind $BIND_IP:7878 --claude $CLAUDE_BIN
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
UNIT

systemctl --user daemon-reload
systemctl --user enable --now rustyclaude
systemctl --user restart rustyclaude
loginctl enable-linger "$USER" 2>/dev/null || sudo loginctl enable-linger "$USER"

cat <<DONE

Daemon running on ws://$BIND_IP:7878 (starts at boot).
Add this to ~/Library/Application Support/rustyclaude/hosts.toml on your Mac, then restart RustyClaude:

[[host]]
name = "$(hostname)"
url = "ws://$BIND_IP:7878"
token = "$TOKEN"

Logs:  journalctl --user -u rustyclaude -f
DONE
