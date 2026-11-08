#!/usr/bin/env bash
# Installs rustyclaude-daemon as a systemd user service for the current user.
#
# Usage: ./deploy/install-daemon.sh [bind-ip] [port]
#   bind-ip  default: asks, offering this machine's addresses (LAN, Tailscale, ...)
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

# Bind address: argument, else ask (default: the first LAN address).
choose_ip() {
  local ips=() labels=() ip name i choice
  while read -r name ip; do
    ip="${ip%/*}"
    case "$ip" in 127.*|169.254.*) continue ;; esac
    ips+=("$ip")
    case "$name" in
      tailscale*) labels+=("Tailscale, reachable from your tailnet only") ;;
      docker*|br-*|veth*|virbr*) labels+=("$name, containers/VMs only") ;;
      *) labels+=("$name, local network") ;;
    esac
  done < <(ip -o -4 addr show up | awk '{print $2, $4}')
  ips+=("0.0.0.0"); labels+=("all interfaces, including public ones")
  echo "Which address should the daemon listen on?" >&2
  for i in "${!ips[@]}"; do printf '  %d) %-15s  %s\n' "$((i + 1))" "${ips[$i]}" "${labels[$i]}" >&2; done
  read -rp "Choice [1]: " choice </dev/tty
  choice="${choice:-1}"
  if [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#ips[@]} )); then
    echo "${ips[$((choice - 1))]}"
  else
    echo "$choice" # typed an address directly
  fi
}
if [ -n "${1:-}" ]; then
  BIND_IP="$1"
elif [ -t 0 ] || [ -r /dev/tty ]; then
  BIND_IP="$(choose_ip)"
else
  BIND_IP="$(hostname -I | awk '{print $1}')"
fi
# The address clients connect to (0.0.0.0 isn't one).
URL_IP="$BIND_IP"; [ "$BIND_IP" = 0.0.0.0 ] && URL_IP="$(hostname -I | awk '{print $1}')"
[[ "$BIND_IP" =~ ^[0-9]+(\.[0-9]+){3}$ ]] || { echo "Not an IPv4 address: $BIND_IP"; exit 1; }

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

# Linger keeps this user's services running without a login, and starts the user's
# systemd instance now. Do it first: shells from `su`/`sudo -u` have no user bus otherwise.
loginctl enable-linger "$USER" 2>/dev/null || sudo loginctl enable-linger "$USER"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] || ! systemctl --user show-environment >/dev/null 2>&1; then
  export XDG_RUNTIME_DIR="/run/user/$(id -u)"
  export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
  for _ in $(seq 20); do [ -S "$XDG_RUNTIME_DIR/bus" ] && break; sleep 0.5; done
fi
if ! systemctl --user show-environment >/dev/null 2>&1; then
  echo "Can't reach $USER's systemd user instance. Log in as $USER directly (SSH or desktop),"
  echo "not with su, and rerun this script. Or use:  sudo machinectl shell $USER@ $PWD/deploy/install-daemon.sh"
  exit 1
fi

systemctl --user daemon-reload
systemctl --user enable rustyclaude >/dev/null
systemctl --user restart rustyclaude

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

Daemon for $USER running on ws://$URL_IP:$PORT (starts at boot).

Paste the block between the lines into RustyClaude's hosts.toml (as its own entry,
keep the others), then restart RustyClaude:
  macOS:  ~/Library/Application Support/rustyclaude/hosts.toml
  Linux:  ~/.config/rustyclaude/hosts.toml
------------------------------------------------------------------------
[[host]]
name = "$(hostname)-$USER"
url = "ws://$URL_IP:$PORT"
token = "$TOKEN"
------------------------------------------------------------------------

Logs:  journalctl --user -u rustyclaude -f
DONE
