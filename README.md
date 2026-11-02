# RustyClaude

A native macOS app for using several Claude accounts at once, with a way to move Claude Code
sessions between accounts when one hits its usage limit.

```
app/              Tauri desktop app
crates/daemon     runs on each machine that hosts Claude Code sessions
crates/protocol   WebSocket messages between app and daemon
archive/          earlier daemon-only client UI (terminal + chat views), kept for reference
```

## The app

- One sidebar icon per account; each opens claude.ai/code in its own isolated, persistent sign-in.
- ⌘1–⌘9 switch accounts, ⌘⇧[ / ⌘⇧] cycle, ⌘, manages accounts, ⌘⇧S opens Host Sessions,
  ⌃⌥C shows/hides the window from anywhere.
- Web notifications become macOS notifications tagged with the account; accounts needing
  attention get a red dot and a Dock badge. Closing the window keeps the app running.
- Sign-in popups stay in the app; other links open in your browser; downloads go to ~/Downloads.

```sh
cd app && npm install && npx tauri dev     # or: npx tauri build
```

Accounts are stored in `~/Library/Application Support/rustyclaude/accounts.toml`.

## Moving sessions between accounts

Sessions started from **Host Sessions** run on your machines as
`claude --remote-control <name>` under a daemon, so they also appear in the account's claude.ai tab.
When a session hits its usage limit it's flagged. Right-click it → **Move to ▸ account**:

1. the daemon types `/login` into the session,
2. the sign-in page opens in that account's tab (already signed in), you click **Authorize**,
3. the code is captured from the redirect and typed back into the session (or paste it manually).

The conversation keeps running in the same process, now on the new account.
Only sessions started from the app can be moved.

### Host setup (each machine)

```sh
# install Claude Code and log in once, then:
cargo build --release -p rustyclaude-daemon
RUSTYCLAUDE_TOKEN=<long random secret> \
  ./target/release/rustyclaude-daemon --bind <LAN or Tailscale IP>:7878
```

Then on the Mac, in `~/Library/Application Support/rustyclaude/hosts.toml`:

```toml
[[host]]
name = "linux-box"
url = "ws://192.168.1.20:7878"
token = "<same secret>"
```

Traffic is plain `ws://`: fine on a trusted LAN, use Tailscale for anything else.

## Status

The `/login` automation was tested against a stand-in for Claude Code, not a real login yet.
It matches Claude Code's login screen text ("login method", "Paste code", "Login successful"),
so wording changes in Claude Code may need small adjustments in `crates/daemon/src/main.rs`.
