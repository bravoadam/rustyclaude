# RustyClaude

A native macOS and Linux app for using several Claude accounts at once, with a way to move Claude Code
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
  attention get a red dot and a dock badge. Closing the window keeps the app running.
- Sign-in popups stay in the app; other links open in your browser; downloads go to ~/Downloads.

```sh
cd app && npm install && npx tauri dev     # or: npx tauri build
```

Accounts are stored in `~/Library/Application Support/rustyclaude/accounts.toml` (macOS) or
`~/.config/rustyclaude/accounts.toml` (Linux).

### Linux

```sh
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev build-essential libssl-dev
```

Then `deploy/install-linux-app.sh` builds and installs the app, pins it to the KDE Plasma or
GNOME dock, and sets up Ctrl+Alt+C through the desktop (key grabs don't work on Wayland).

Shortcuts use Ctrl instead of ⌘. Closing the window (or Quit in the dock) exits the app; minimize it
instead to keep notifications coming. Security keys (YubiKey etc.) work on the Claude and Google sign-in pages through the app's own
WebAuthn bridge (`src/webauthn.rs`), since WebKitGTK has none. claude.ai's bot check rejects WebKitGTK for email and Apple
sign-in, so the login page offers **Sign in with browser**: it signs in through Brave or Chromium with a throwaway profile and copies
the claude.ai session into the account (`src/browser_signin.rs`). Google sign-in works directly. Each account's sign-in lives in `~/.local/share/dev.rustyclaude.app/accounts/<id>`.

## Host sessions (the daemon)

Run `deploy/install-daemon.sh` on each Linux machine (needs tmux, Claude Code and Rust).
It installs a systemd user service that starts at boot and prints the `[[host]]` block to
paste into `~/Library/Application Support/rustyclaude/hosts.toml` on the Mac.

The daemon keeps one tmux session per project folder (`21wild`, `blockari`, …):

- **Defined once, started automatically.** Add a folder from **Host Sessions → New session**.
  It's saved in `~/.config/rustyclaude/daemon.toml` on that machine, started at boot,
  restarted if it exits, and runs with `--remote-control <name>`. No `/rc` needed.
- **Still plain tmux.** `tmux attach -t 21wild` works as before (right-click → copy command).
  tmux sessions you started by hand are listed too; right-click → **Manage** to adopt them.
- **One login per session.** Each session has its own Claude config dir
  (`~/.claude-sessions/<name>`), so its login survives restarts and never affects other
  sessions. Shared settings (`settings.json`, `CLAUDE.md`, agents, commands, skills, plugins,
  hooks) are symlinked from `~/.claude`. User-level MCP servers (`~/.claude.json`) are not shared.
- **Logging in / moving accounts.** Right-click → **Log in as ▸** (new sessions) or
  **Move to ▸** (e.g. after "limit reached"). The daemon drives `/login`, the sign-in page
  opens in that account's tab (already signed in), you click **Authorize**, and the code is
  captured and typed back automatically (or paste it in the panel).

## Status

The daemon is tested end to end with real tmux and a stand-in for Claude Code (login, move,
limit detection, stop/start, adopt), not yet against a real Claude login. It matches Claude
Code's screen text ("login method", "Paste code", "Login successful", trust/theme prompts),
so wording changes in Claude Code may need small adjustments in `crates/daemon/src/main.rs`.
