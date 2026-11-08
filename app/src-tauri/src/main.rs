//! RustyClaude: one native window, many Claude accounts.
//!
//! Layout: a narrow local "sidebar" webview on the left (account switcher), and one
//! claude.ai webview per account filling the rest. Each account webview has its own
//! WebKit data store, so every account stays signed in independently. All account
//! webviews stay loaded (hidden when not selected) so their notifications keep arriving.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod conn;
#[cfg(target_os = "linux")]
mod gtk_layout;
#[cfg(target_os = "linux")]
mod webauthn;
#[cfg(target_os = "linux")]
mod browser_signin;
mod hosts;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Mutex,
};

use config::{Account, Config};
use rustyclaude_protocol::ClientMsg;
use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    webview::{DownloadEvent, NewWindowResponse, WebviewBuilder},
    window::WindowBuilder,
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Url, WebviewUrl, WindowEvent, Wry,
};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_window_state::{StateFlags, WindowExt};

const SIDEBAR_WIDTH: f64 = 72.0;
/// Draggable strip above the account view (the native title bar is hidden).
#[cfg(not(target_os = "linux"))]
const TITLEBAR_HEIGHT: f64 = 32.0;
/// Linux keeps the native title bar, so there's no strip (and no extra webview) above the page.
#[cfg(target_os = "linux")]
const TITLEBAR_HEIGHT: f64 = 0.0;
const TITLEBAR: &str = "titlebar";
const MAIN: &str = "main";
pub const SIDEBAR: &str = "sidebar";
const TOGGLE_HOTKEY: &str = "ctrl+alt+c";
/// Safari's UA: some sign-in providers refuse "embedded browser" user agents.
#[cfg(not(target_os = "linux"))]
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
/// On Linux, Safari-on-Linux (like GNOME Web): claiming a Mac while the engine reports
/// Linux (navigator.platform etc.) fails bot checks such as claude.ai's email sign-in.
#[cfg(target_os = "linux")]
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
/// Popups to these hosts stay in-app (sign-in flows); everything else opens in the default browser.
const IN_APP_HOSTS: &[&str] = &[
    "claude.ai",
    "anthropic.com",
    "claude.com",
    "accounts.google.com",
    "appleid.apple.com",
    "login.microsoftonline.com",
];

/// An account switch waiting for the user to approve in an account tab.
struct PendingOauth {
    host: String,
    session: String,
    /// redirect_uri from the authorize URL; the code arrives as a navigation to it.
    redirect: Option<String>,
}

#[derive(Default)]
struct AppState {
    cfg: Config,
    active: Option<String>,
    /// Which full-width panel is open over the accounts: "accounts" | "sessions".
    panel: Option<String>,
    attention: HashSet<String>,
    /// Keyed by account id.
    pending_oauth: HashMap<String, PendingOauth>,
    /// The (id, name) pairs the current menu was built from.
    menu_accounts: Option<Vec<(String, String)>>,
    /// The account shown before the active one; kept rendered for a quick switch back.
    previous: Option<String>,
    /// Accounts whose page isn't loaded (staggered at startup, or parked); loaded on select.
    unloaded: HashSet<String>,
    /// When each account was last moved to the background.
    left_at: HashMap<String, std::time::Instant>,
}

impl AppState {
    fn wide(&self) -> bool {
        self.panel.is_some() || self.cfg.accounts.is_empty()
    }
}

struct Hosts(Vec<(String, conn::HostHandle)>);

/// Why hosts.toml couldn't be read, shown in the Host Sessions panel.
struct HostsError(Option<String>);

type Shared = Mutex<AppState>;

#[derive(Serialize, Clone)]
struct AccountView {
    id: String,
    name: String,
    color: String,
    attention: bool,
}

#[derive(Serialize, Clone)]
struct StateView {
    accounts: Vec<AccountView>,
    active: Option<String>,
    panel: Option<String>,
    start_url: String,
    hotkey: &'static str,
    config_path: String,
}

fn view(st: &AppState) -> StateView {
    StateView {
        accounts: st
            .cfg
            .accounts
            .iter()
            .map(|a| AccountView {
                id: a.id.clone(),
                name: a.name.clone(),
                color: a.color.clone(),
                attention: st.attention.contains(&a.id),
            })
            .collect(),
        active: st.active.clone(),
        panel: if st.cfg.accounts.is_empty() { Some("accounts".into()) } else { st.panel.clone() },
        start_url: st.cfg.start_url.clone(),
        hotkey: TOGGLE_HOTKEY,
        config_path: config::path().map(|p| p.display().to_string()).unwrap_or_default(),
    }
}

/// Push state to the sidebar, fix up layout, dock badge and menu.
fn refresh(app: &AppHandle) {
    let (v, rebuild_menu) = {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let v = view(&st);
        // Only the account list appears in the menu; rebuilding it needlessly upsets GTK.
        let key: Vec<_> = v.accounts.iter().map(|a| (a.id.clone(), a.name.clone())).collect();
        let changed = st.menu_accounts.as_ref() != Some(&key);
        st.menu_accounts = Some(key);
        (v, changed)
    };
    layout(app);
    let n = v.accounts.iter().filter(|a| a.attention).count() as i64;
    #[cfg(target_os = "linux")]
    gtk_layout::set_badge(n);
    #[cfg(not(target_os = "linux"))]
    if let Some(win) = app.get_window(MAIN) {
        let _ = win.set_badge_count(if n > 0 { Some(n) } else { None });
    }
    if rebuild_menu {
        if let Ok(menu) = build_menu(app, &v) {
            let _ = app.set_menu(menu);
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(win) = app.get_window(MAIN) {
        let name = v.accounts.iter().find(|a| Some(&a.id) == v.active.as_ref()).map(|a| a.name.as_str());
        let title = match (name, v.panel.is_some()) {
            (Some(n), false) => format!("{n} · RustyClaude"),
            _ => "RustyClaude".to_string(),
        };
        if win.title().ok().as_deref() != Some(title.as_str()) {
            let _ = win.set_title(&title);
        }
    }
    let _ = app.emit_to(TITLEBAR, "state", v.clone());
    let _ = app.emit_to(SIDEBAR, "state", v);
}

#[cfg(target_os = "linux")]
fn place(wv: &tauri::Webview, x: f64, y: f64, w: f64, h: f64) {
    gtk_layout::place(wv, x, y, w, h);
}

#[cfg(not(target_os = "linux"))]
fn place(wv: &tauri::Webview, x: f64, y: f64, w: f64, h: f64) {
    let _ = wv.set_position(LogicalPosition::new(x, y));
    let _ = wv.set_size(LogicalSize::new(w, h));
}

fn layout(app: &AppHandle) {
    let Some(win) = app.get_window(MAIN) else { return };
    let (Ok(size), Ok(scale)) = (win.inner_size(), win.scale_factor()) else { return };
    #[allow(unused_mut)]
    let mut size = size.to_logical::<f64>(scale);
    #[cfg(target_os = "linux")]
    if let Some((w, h)) = gtk_layout::area() {
        size = LogicalSize::new(w, h);
    }
    let st = app.state::<Shared>();
    let st = st.lock().unwrap();

    if let Some(sb) = app.get_webview(SIDEBAR) {
        let w = if st.wide() { size.width } else { SIDEBAR_WIDTH };
        place(&sb, 0.0, 0.0, w, size.height);
    }
    let content_w = (size.width - SIDEBAR_WIDTH).max(1.0);
    let content_h = (size.height - TITLEBAR_HEIGHT).max(1.0);
    if let Some(tb) = app.get_webview(TITLEBAR) {
        place(&tb, SIDEBAR_WIDTH, 0.0, content_w, TITLEBAR_HEIGHT);
        set_visible(&tb, !st.wide());
    }
    for a in &st.cfg.accounts {
        if let Some(wv) = app.get_webview(&a.label()) {
            let active = !st.wide() && st.active.as_deref() == Some(a.id.as_str());
            // Hidden pages are throttled and may drop what they drew, so showing them again
            // means a full repaint. On Linux the previous account stays rendered just past
            // the window's right edge instead, which makes switching back instant.
            let keep = cfg!(target_os = "linux") && !active && st.previous.as_deref() == Some(a.id.as_str());
            let x = if keep { size.width + 64.0 } else { SIDEBAR_WIDTH };
            place(&wv, x, TITLEBAR_HEIGHT, content_w, content_h);
            set_visible(&wv, active || keep);
        }
    }
}

/// Shows or hides a webview only when that actually changes; each call costs a trip
/// through the toolkit even when it's a no-op.
fn set_visible(wv: &tauri::Webview, visible: bool) {
    static SHOWN: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    let mut shown = SHOWN.lock().unwrap();
    let shown = shown.get_or_insert_with(HashMap::new);
    if shown.get(wv.label()) == Some(&visible) {
        return;
    }
    shown.insert(wv.label().to_string(), visible);
    let _ = if visible { wv.show() } else { wv.hide() };
}

fn host_allowed_in_app(url: &Url) -> bool {
    url.host_str().is_some_and(|h| IN_APP_HOSTS.iter().any(|a| h == *a || h.ends_with(&format!(".{a}"))))
}

fn downloads_dir() -> PathBuf {
    dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads"))).unwrap_or_else(|| ".".into())
}

fn create_account_webview(app: &AppHandle, start_url: &str, acct: &Account) -> tauri::Result<()> {
    let win = app.get_window(MAIN).expect("main window");
    let url: Url = start_url.parse().unwrap_or_else(|_| config::DEFAULT_START_URL.parse().unwrap());
    let app_for_popups = app.clone();
    let app_for_dl = app.clone();
    let app_for_nav = app.clone();
    let acct_id = acct.id.clone();
    let name = acct.name.clone();

    let builder = WebviewBuilder::new(acct.label(), WebviewUrl::External(url));
    // Per-account storage is what keeps sign-ins separate. data_store_identifier is
    // ignored outside macOS/iOS; elsewhere each account gets its own data directory.
    #[cfg(target_os = "macos")]
    let builder = builder.data_store_identifier(acct.store_id());
    #[cfg(not(target_os = "macos"))]
    let builder = builder.data_directory(app.path().app_data_dir()?.join("accounts").join(&acct.id));
    #[cfg(target_os = "linux")]
    let builder = builder
        .initialization_script(include_str!("webauthn.js"))
        .initialization_script(include_str!("signin.js"));
    let builder = builder
        .user_agent(USER_AGENT)
        .initialization_script(include_str!("inject.js"))
        .on_navigation(move |url| {
            oauth_capture(&app_for_nav, &acct_id, url);
            true
        })
        .on_new_window(move |url, _features| {
            if host_allowed_in_app(&url) {
                #[cfg(target_os = "linux")]
                return gtk_layout::popup(&app_for_popups, _features, USER_AGENT);
                #[cfg(not(target_os = "linux"))]
                NewWindowResponse::Allow
            } else {
                let _ = app_for_popups.opener().open_url(url.as_str(), None::<&str>);
                NewWindowResponse::Deny
            }
        })
        .on_download(move |_wv, ev| {
            match ev {
                DownloadEvent::Requested { url, destination } => {
                    let file = destination
                        .file_name()
                        .map(|f| f.to_os_string())
                        .or_else(|| url.path_segments()?.next_back().filter(|s| !s.is_empty()).map(Into::into))
                        .unwrap_or_else(|| "download".into());
                    *destination = downloads_dir().join(file);
                }
                DownloadEvent::Finished { success, .. } => {
                    let _ = app_for_dl
                        .notification()
                        .builder()
                        .title(format!("{name} · Download {}", if success { "finished" } else { "failed" }))
                        .body(if success { "Saved to Downloads" } else { "The download did not complete" })
                        .show();
                }
                _ => {}
            }
            true
        });

    let size = win.inner_size()?.to_logical::<f64>(win.scale_factor()?);
    let wv = win.add_child(
        builder,
        LogicalPosition::new(SIDEBAR_WIDTH, TITLEBAR_HEIGHT),
        LogicalSize::new((size.width - SIDEBAR_WIDTH).max(1.0), (size.height - TITLEBAR_HEIGHT).max(1.0)),
    )?;
    wv.hide()?;
    Ok(())
}

fn build_menu(app: &AppHandle, v: &StateView) -> tauri::Result<Menu<Wry>> {
    // Default menu supplies App / Edit (copy, paste!) / Window menus.
    let menu = Menu::default(app)?;

    let view_menu = Submenu::with_items(
        app,
        "Navigate",
        true,
        &[
            &MenuItem::with_id(app, "reload", "Reload", true, Some("CmdOrCtrl+R"))?,
            &MenuItem::with_id(app, "back", "Back", true, Some("CmdOrCtrl+["))?,
            &MenuItem::with_id(app, "forward", "Forward", true, Some("CmdOrCtrl+]"))?,
            &MenuItem::with_id(app, "home", "Go to Start Page", true, Some("CmdOrCtrl+Shift+H"))?,
        ],
    )?;

    let accounts = Submenu::new(app, "Accounts", true)?;
    for (i, a) in v.accounts.iter().enumerate() {
        let accel = (i < 9).then(|| format!("CmdOrCtrl+{}", i + 1));
        accounts.append(&MenuItem::with_id(app, format!("acct:{}", a.id), &a.name, true, accel.as_deref())?)?;
    }
    if !v.accounts.is_empty() {
        accounts.append(&PredefinedMenuItem::separator(app)?)?;
    }
    accounts.append(&MenuItem::with_id(app, "next", "Next Account", true, Some("CmdOrCtrl+Shift+]"))?)?;
    accounts.append(&MenuItem::with_id(app, "prev", "Previous Account", true, Some("CmdOrCtrl+Shift+["))?)?;
    accounts.append(&MenuItem::with_id(app, "settings", "Manage Accounts…", true, Some("CmdOrCtrl+,"))?)?;
    accounts.append(&MenuItem::with_id(app, "sessions", "Host Sessions…", true, Some("CmdOrCtrl+Shift+S"))?)?;

    menu.append(&view_menu)?;
    menu.append(&accounts)?;
    Ok(menu)
}

/// There's no Dock outside macOS, so the tray is how a closed window comes back
/// (the global hotkey doesn't work on Wayland) and how the app is quit.
#[cfg(not(target_os = "macos"))]
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "tray-show", "Show RustyClaude", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?,
        ],
    )?;
    let mut tray = tauri::tray::TrayIconBuilder::with_id("main").menu(&menu).tooltip("RustyClaude");
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn active_webview(app: &AppHandle) -> Option<tauri::Webview> {
    let id = app.state::<Shared>().lock().unwrap().active.clone()?;
    app.get_webview(&format!("acct-{id}"))
}

fn select(app: &AppHandle, id: &str) {
    let started = std::time::Instant::now();
    {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        if !st.cfg.accounts.iter().any(|a| a.id == id) {
            return;
        }
        if let Some(old) = st.active.clone().filter(|old| old != id) {
            st.left_at.insert(old.clone(), std::time::Instant::now());
            st.previous = Some(old);
        }
        st.active = Some(id.to_string());
        st.panel = None;
        if st.unloaded.remove(id) {
            let start = st.cfg.start_url.clone();
            if let (Some(wv), Ok(u)) = (app.get_webview(&format!("acct-{id}")), start.parse::<Url>()) {
                let _ = wv.navigate(u);
            }
        }
        st.attention.remove(id);
        st.cfg.last_active = Some(id.to_string());
    }
    save_soon(app);
    refresh(app);
    if let Some(wv) = active_webview(app) {
        let _ = wv.set_focus();
    }
    if std::env::var_os("RUSTYCLAUDE_TIMING").is_some() {
        eprintln!("switch: {:.2} ms", started.elapsed().as_secs_f64() * 1000.0);
    }
}

fn load_if_unloaded(app: &AppHandle, id: &str) {
    let start = {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        if !st.unloaded.remove(id) {
            return;
        }
        st.cfg.start_url.clone()
    };
    if let (Some(wv), Ok(u)) = (app.get_webview(&format!("acct-{id}")), start.parse::<Url>()) {
        let _ = wv.navigate(u);
    }
}

/// claude.ai's sign-in page keeps a CPU core busy. An account left on it in the background
/// for a while is unloaded until it's selected again (no notifications to miss there).
fn park_idle_logins(app: &AppHandle) {
    const IDLE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
    let candidates: Vec<String> = {
        let st = app.state::<Shared>();
        let st = st.lock().unwrap();
        st.cfg
            .accounts
            .iter()
            .map(|a| a.id.clone())
            .filter(|id| {
                st.active.as_ref() != Some(id)
                    && !st.unloaded.contains(id)
                    && !st.pending_oauth.contains_key(id)
                    && st.left_at.get(id).is_none_or(|t| t.elapsed() > IDLE)
            })
            .collect()
    };
    for id in candidates {
        let Some(wv) = app.get_webview(&format!("acct-{id}")) else { continue };
        let on_login = wv.url().is_ok_and(|u| u.host_str() == Some("claude.ai") && u.path().starts_with("/login"));
        if on_login && wv.navigate("about:blank".parse().unwrap()).is_ok() {
            app.state::<Shared>().lock().unwrap().unloaded.insert(id);
        }
    }
}

/// Writes accounts.toml in the background, so switching never waits on the disk. Saves
/// are serialized and always write the latest state, so a quick burst ends correctly.
fn save_soon(app: &AppHandle) {
    static SAVING: Mutex<()> = Mutex::new(());
    let app = app.clone();
    std::thread::spawn(move || {
        let _guard = SAVING.lock().unwrap();
        let cfg = app.state::<Shared>().lock().unwrap().cfg.clone();
        if let Err(e) = config::save(&cfg) {
            eprintln!("could not save accounts: {e:#}");
        }
    });
}

fn cycle(app: &AppHandle, step: isize) {
    let next = {
        let st = app.state::<Shared>();
        let st = st.lock().unwrap();
        let n = st.cfg.accounts.len() as isize;
        if n == 0 {
            return;
        }
        let cur = st.active.as_ref().and_then(|id| st.cfg.accounts.iter().position(|a| &a.id == id)).unwrap_or(0) as isize;
        st.cfg.accounts[((cur + step).rem_euclid(n)) as usize].id.clone()
    };
    select(app, &next);
}

/// Puts the window away while the app keeps running. On Linux it's minimized, not hidden:
/// with no window the dock thinks the app is closed, so a click starts a new process that
/// hands over and exits, and the dock spins until its startup timeout.
fn stash_main(win: &tauri::Window) {
    #[cfg(target_os = "linux")]
    let _ = win.minimize();
    #[cfg(not(target_os = "linux"))]
    let _ = win.hide();
}

fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_window(MAIN) {
        // Wayland has no "unminimize" request, but remapping the window restores it.
        #[cfg(target_os = "linux")]
        if !win.is_focused().unwrap_or(false) {
            let _ = win.hide();
        }
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    clear_active_attention(app);
}

fn toggle_main(app: &AppHandle) {
    if let Some(win) = app.get_window(MAIN) {
        let up = win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false);
        if up {
            stash_main(&win);
        } else {
            show_main(app);
        }
    }
}

/// Global key grabs only work on X11. On Wayland the desktop owns the shortcut and runs
/// `rustyclaude --toggle` instead (see deploy/install-linux-app.sh).
fn can_grab_hotkey() -> bool {
    #[cfg(target_os = "linux")]
    return std::env::var_os("WAYLAND_DISPLAY").is_none();
    #[cfg(not(target_os = "linux"))]
    true
}

fn clear_active_attention(app: &AppHandle) {
    let changed = {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        match (st.active.clone(), st.wide()) {
            (Some(id), false) => st.attention.remove(&id),
            _ => false,
        }
    };
    if changed {
        refresh(app);
    }
}

// ---- commands: sidebar ------------------------------------------------------

#[tauri::command]
fn get_state(state: State<Shared>) -> StateView {
    view(&state.lock().unwrap())
}

#[tauri::command]
fn select_account(app: AppHandle, id: String) {
    select(&app, &id);
}

#[tauri::command]
fn add_account(app: AppHandle, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Name can't be empty".into());
    }
    let (acct, start_url) = {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let acct = st.cfg.new_account(name);
        st.cfg.accounts.push(acct.clone());
        config::save(&st.cfg).map_err(|e| e.to_string())?;
        (acct, st.cfg.start_url.clone())
    };
    create_account_webview(&app, &start_url, &acct).map_err(|e| e.to_string())?;
    select(&app, &acct.id);
    Ok(())
}

#[tauri::command]
fn rename_account(app: AppHandle, id: String, name: String) -> Result<(), String> {
    {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let a = st.cfg.accounts.iter_mut().find(|a| a.id == id).ok_or("no such account")?;
        if !name.trim().is_empty() {
            a.name = name.trim().to_string();
        }
        config::save(&st.cfg).map_err(|e| e.to_string())?;
    }
    refresh(&app);
    Ok(())
}

#[tauri::command]
fn set_account_color(app: AppHandle, id: String, color: String) -> Result<(), String> {
    {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let a = st.cfg.accounts.iter_mut().find(|a| a.id == id).ok_or("no such account")?;
        a.color = color;
        config::save(&st.cfg).map_err(|e| e.to_string())?;
    }
    refresh(&app);
    Ok(())
}

#[tauri::command]
fn move_account(app: AppHandle, id: String, delta: isize) -> Result<(), String> {
    {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let n = st.cfg.accounts.len() as isize;
        let i = st.cfg.accounts.iter().position(|a| a.id == id).ok_or("no such account")? as isize;
        let j = (i + delta).clamp(0, n - 1);
        st.cfg.accounts.swap(i as usize, j as usize);
        config::save(&st.cfg).map_err(|e| e.to_string())?;
    }
    refresh(&app);
    Ok(())
}

/// Removes the account from the app. Its WebKit data store (cookies) is left on disk.
#[tauri::command]
fn remove_account(app: AppHandle, id: String) -> Result<(), String> {
    {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        st.cfg.accounts.retain(|a| a.id != id);
        st.attention.remove(&id);
        st.unloaded.remove(&id);
        st.left_at.remove(&id);
        if st.previous.as_deref() == Some(id.as_str()) {
            st.previous = None;
        }
        if st.active.as_deref() == Some(id.as_str()) {
            st.active = st.cfg.accounts.first().map(|a| a.id.clone());
        }
        if st.cfg.last_active.as_deref() == Some(id.as_str()) {
            st.cfg.last_active = st.active.clone();
        }
        config::save(&st.cfg).map_err(|e| e.to_string())?;
    }
    if let Some(wv) = app.get_webview(&format!("acct-{id}")) {
        let _ = wv.close();
    }
    refresh(&app);
    Ok(())
}

#[tauri::command]
fn set_panel(app: AppHandle, panel: Option<String>) {
    app.state::<Shared>().lock().unwrap().panel = panel;
    refresh(&app);
}

// ---- hosts / sessions -----------------------------------------------------------

#[derive(Serialize)]
struct HostView {
    name: String,
    url: String,
    status: conn::HostStatus,
}

#[tauri::command]
fn list_hosts(hosts: State<Hosts>) -> Vec<HostView> {
    hosts
        .0
        .iter()
        .map(|(n, h)| HostView { name: n.clone(), url: h.url.clone(), status: h.status.lock().unwrap().clone() })
        .collect()
}

fn host_send_inner(app: &AppHandle, host: &str, msg: ClientMsg) -> Result<(), String> {
    let hosts = app.state::<Hosts>();
    let (_, h) = hosts.0.iter().find(|(n, _)| n == host).ok_or_else(|| format!("unknown host '{host}'"))?;
    h.tx.send(msg).map_err(|_| "connection task stopped".to_string())
}

#[tauri::command]
fn host_send(app: AppHandle, host: String, msg: ClientMsg) -> Result<(), String> {
    host_send_inner(&app, &host, msg)
}

#[tauri::command]
fn hosts_path(err: State<HostsError>) -> serde_json::Value {
    serde_json::json!({
        "path": hosts::path().map(|p| p.display().to_string()).unwrap_or_default(),
        "error": err.0,
    })
}

#[tauri::command]
fn notify_native(app: AppHandle, title: String, body: String) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Opens the sign-in URL in the target account's tab (already signed in to claude.ai
/// as that account) and waits for the redirect that carries the code.
#[tauri::command]
fn begin_oauth(app: AppHandle, account: String, host: String, session: String, url: String) -> Result<(), String> {
    let parsed: Url = url.parse().map_err(|_| "bad sign-in URL".to_string())?;
    let redirect = parsed.query_pairs().find(|(k, _)| k == "redirect_uri").map(|(_, v)| v.into_owned());
    let wv = app.get_webview(&format!("acct-{account}")).ok_or("no such account tab")?;
    {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        st.pending_oauth.insert(account.clone(), PendingOauth { host, session, redirect });
        // We're about to load the sign-in page here; selecting must not replace it.
        st.unloaded.remove(&account);
    }
    wv.navigate(parsed).map_err(|e| e.to_string())?;
    select(&app, &account);
    Ok(())
}

#[tauri::command]
fn cancel_oauth(app: AppHandle, session: String) {
    app.state::<Shared>().lock().unwrap().pending_oauth.retain(|_, p| p.session != session);
}

/// Called for every navigation in an account tab.
fn oauth_capture(app: &AppHandle, account: &str, url: &Url) {
    let Some(code) = url.query_pairs().find(|(k, _)| k == "code").map(|(_, v)| v.into_owned()) else { return };
    let pending = {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let Some(p) = st.pending_oauth.get(account) else { return };
        let matches = match &p.redirect {
            Some(r) => url.as_str().starts_with(r.as_str()),
            None => url.path().contains("/oauth/code/callback"),
        };
        if !matches {
            return;
        }
        st.pending_oauth.remove(account).unwrap()
    };
    let state = url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned());
    let full = match state {
        Some(s) => format!("{code}#{s}"),
        None => code,
    };
    let _ = host_send_inner(app, &pending.host, ClientMsg::LoginCode { session: pending.session.clone(), code: full });
    let _ = app.emit_to(SIDEBAR, "oauth-captured", serde_json::json!({ "host": pending.host, "session": pending.session }));

    // Let the confirmation page show briefly, then return the tab to claude.ai.
    let (app, account) = (app.clone(), account.to_string());
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let start = app.state::<Shared>().lock().unwrap().cfg.start_url.clone();
        if let (Some(wv), Ok(u)) = (app.get_webview(&format!("acct-{account}")), start.parse::<Url>()) {
            let _ = wv.navigate(u);
        }
    });
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let al = app.autolaunch();
    if enabled { al.enable() } else { al.disable() }.map_err(|e| e.to_string())
}

// ---- commands: claude.ai webviews (see capabilities/claude-web.json) ----------

#[tauri::command]
fn web_notify(app: AppHandle, webview: tauri::Webview, title: String, body: String) {
    let Some(id) = webview.label().strip_prefix("acct-").map(str::to_string) else { return };
    let focused = app.get_window(MAIN).and_then(|w| w.is_focused().ok()).unwrap_or(false);
    let name = {
        let st = app.state::<Shared>();
        let mut st = st.lock().unwrap();
        let visible = focused && !st.wide() && st.active.as_deref() == Some(id.as_str());
        if !visible {
            st.attention.insert(id.clone());
        }
        st.cfg.accounts.iter().find(|a| a.id == id).map(|a| a.name.clone()).unwrap_or_default()
    };
    let title: String = title.chars().take(200).collect();
    let body: String = body.chars().take(500).collect();
    let _ = app.notification().builder().title(format!("{name} · {title}")).body(body).show();
    refresh(&app);
}

/// Security-key requests from webauthn.js (Linux). The origin comes from the webview's URL.
#[tauri::command]
async fn webauthn(webview: tauri::Webview, req: serde_json::Value) -> Result<serde_json::Value, String> {
    #[cfg(target_os = "linux")]
    {
        let url = webview.url().map_err(|e| e.to_string())?;
        let what = format!("{} {} rp={} appid={} allow={}", webview.label(), url.host_str().unwrap_or("?"),
            req["options"]["rpId"].as_str().or(req["options"]["rp"]["id"].as_str()).unwrap_or("-"),
            req["options"]["extensions"]["appid"].as_str().unwrap_or("-"),
            req["options"]["allowCredentials"].as_array().map_or(0, |a| a.len()));
        let req: webauthn::Request = serde_json::from_value(req).map_err(|e| format!("TypeError: {e}"))?;
        let r = tauri::async_runtime::spawn_blocking(move || webauthn::handle(url, req))
            .await
            .map_err(|e| e.to_string())?;
        eprintln!("webauthn {what}: {}", r.as_ref().map(|_| "ok".to_string()).unwrap_or_else(|e| e.clone()));
        return r;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (webview, req);
        Err("NotSupportedError: not available on this platform".into())
    }
}

/// Email/Apple sign-in through Brave or Chromium (Linux): claude.ai's bot check rejects
/// WebKitGTK. Copies the resulting claude.ai cookies into this account and reloads it.
#[cfg(target_os = "linux")]
#[tauri::command]
async fn browser_signin(app: AppHandle, webview: tauri::Webview) -> Result<(), String> {
    let root = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let cookies = tauri::async_runtime::spawn_blocking(move || browser_signin::run(&root))
        .await
        .map_err(|e| e.to_string())??;
    let (tx, rx) = std::sync::mpsc::channel();
    let wv = webview.clone();
    app.run_on_main_thread(move || {
        use tauri::webview::{cookie::time::OffsetDateTime, Cookie};
        let mut r = Ok(());
        for c in cookies {
            let mut b = Cookie::build((c.name, c.value))
                .domain(c.domain)
                .path(c.path)
                .secure(c.secure)
                .http_only(c.http_only);
            if let Some(t) = c.expires.and_then(|e| OffsetDateTime::from_unix_timestamp(e).ok()) {
                b = b.expires(t);
            }
            if let Err(e) = wv.set_cookie(b.build()) {
                r = Err(e.to_string());
            }
        }
        let _ = tx.send(r);
    })
    .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())??;
    webview.navigate("https://claude.ai/new".parse().unwrap()).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn browser_signin() -> Result<(), String> {
    Err("only needed on Linux".into())
}


#[tauri::command]
fn webauthn_cancel() {
    #[cfg(target_os = "linux")]
    webauthn::cancel();
}

// ---- app ---------------------------------------------------------------------

fn main() {
    // GTK uses this as the Wayland app id, so the window matches dev.rustyclaude.app.desktop
    // (one dock icon). The same name is registered as the GTK app id via tauri.conf.
    #[cfg(target_os = "linux")]
    gtk::glib::set_prgname(Some("dev.rustyclaude.app"));

    tauri::Builder::default()
        // Must be first. A dock click on a hidden window starts a second process, which
        // hands over to this one and exits; two would also fight over the same storage.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if args.iter().any(|a| a == "--toggle") {
                toggle_main(app);
            } else {
                show_main(app);
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
                .build(),
        )
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, ev| {
                    if ev.state() != ShortcutState::Pressed {
                        return;
                    }
                    toggle_main(app);
                })
                .build(),
        )
        .manage(Shared::default())
        .setup(|app| {
            let cfg = config::load().unwrap_or_else(|e| {
                eprintln!("config error, starting empty: {e:#}");
                Config::default()
            });
            {
                let st = app.state::<Shared>();
                let mut st = st.lock().unwrap();
                st.active = cfg
                    .last_active
                    .clone()
                    .filter(|id| cfg.accounts.iter().any(|a| &a.id == id))
                    .or_else(|| cfg.accounts.first().map(|a| a.id.clone()));
                st.cfg = cfg.clone();
            }

            let builder = WindowBuilder::new(app, MAIN)
                .title("RustyClaude")
                .inner_size(1320.0, 860.0)
                .min_inner_size(720.0, 480.0);
            #[cfg(target_os = "macos")]
            let builder = builder.title_bar_style(tauri::TitleBarStyle::Overlay).hidden_title(true);
            let win = builder.build()?;
            let _ = win.restore_state(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED);

            #[cfg(target_os = "linux")]
            gtk_layout::install(app.handle(), &win)?;

            let size = win.inner_size()?.to_logical::<f64>(win.scale_factor()?);
            win.add_child(
                WebviewBuilder::new(SIDEBAR, WebviewUrl::App("index.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(SIDEBAR_WIDTH, size.height),
            )?;
            #[cfg(not(target_os = "linux"))]
            win.add_child(
                WebviewBuilder::new(TITLEBAR, WebviewUrl::App("index.html#titlebar".into())),
                LogicalPosition::new(SIDEBAR_WIDTH, 0.0),
                LogicalSize::new((size.width - SIDEBAR_WIDTH).max(1.0), TITLEBAR_HEIGHT),
            )?;
            // The active account loads now; the others a few seconds apart, so the first
            // one isn't competing with all of them. Selecting one loads it at once.
            let first = app.state::<Shared>().lock().unwrap().active.clone();
            let mut later = vec![];
            for a in &cfg.accounts {
                if Some(&a.id) == first.as_ref() {
                    create_account_webview(app.handle(), &cfg.start_url, a)?;
                } else {
                    create_account_webview(app.handle(), "about:blank", a)?;
                    later.push(a.id.clone());
                }
            }
            app.state::<Shared>().lock().unwrap().unloaded.extend(later.iter().cloned());
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    for (i, id) in later.into_iter().enumerate() {
                        std::thread::sleep(std::time::Duration::from_secs(if i == 0 { 4 } else { 2 }));
                        let h2 = h.clone();
                        let _ = h.run_on_main_thread(move || load_if_unloaded(&h2, &id));
                    }
                });
            }
            {
                let h = app.handle().clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    let h2 = h.clone();
                    let _ = h.run_on_main_thread(move || park_idle_logins(&h2));
                });
            }

            let (host_cfgs, hosts_err) = match hosts::load() {
                Ok(h) => (h, None),
                Err(e) => (hosts::HostsFile::default(), Some(format!("{e:#}"))),
            };
            app.manage(HostsError(hosts_err));
            let handles =
                host_cfgs.hosts.into_iter().map(|h| (h.name.clone(), conn::spawn(app.handle().clone(), h))).collect();
            app.manage(Hosts(handles));

            let handle = app.handle().clone();
            win.on_window_event(move |ev| match ev {
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => layout(&handle),
                WindowEvent::Focused(true) => clear_active_attention(&handle),
                // On Linux, closing quits: docks like Dash-to-Dock implement "Quit" by
                // closing the window, so the two can't be told apart.
                #[cfg(target_os = "linux")]
                WindowEvent::CloseRequested { .. } => handle.exit(0),
                #[cfg(not(target_os = "linux"))]
                WindowEvent::CloseRequested { api, .. } => {
                    // Keep running (and notifying) in the background, like the official app.
                    api.prevent_close();
                    if let Some(w) = handle.get_window(MAIN) {
                        stash_main(&w);
                    }
                }
                _ => {}
            });

            #[cfg(not(target_os = "macos"))]
            build_tray(app.handle())?;
            #[cfg(target_os = "linux")]
            gtk_layout::add_quit_action(app.handle(), &win);

            if can_grab_hotkey() {
                if let Err(e) = app.global_shortcut().register(TOGGLE_HOTKEY) {
                    eprintln!("could not register {TOGGLE_HOTKEY}: {e}");
                }
            }

            refresh(app.handle());
            if let Some(wv) = active_webview(app.handle()) {
                let _ = wv.set_focus();
            }
            Ok(())
        })
        .on_menu_event(|app, ev| {
            let id = ev.id().as_ref();
            match id {
                "reload" => {
                    if let Some(wv) = active_webview(app) {
                        let _ = wv.reload();
                    }
                }
                "back" => {
                    if let Some(wv) = active_webview(app) {
                        let _ = wv.eval("history.back()");
                    }
                }
                "forward" => {
                    if let Some(wv) = active_webview(app) {
                        let _ = wv.eval("history.forward()");
                    }
                }
                "home" => {
                    let start = app.state::<Shared>().lock().unwrap().cfg.start_url.clone();
                    if let (Some(wv), Ok(url)) = (active_webview(app), start.parse::<Url>()) {
                        let _ = wv.navigate(url);
                    }
                }
                "tray-show" => show_main(app),
                "tray-quit" => app.exit(0),
                "next" => cycle(app, 1),
                "prev" => cycle(app, -1),
                "settings" | "sessions" => {
                    show_main(app);
                    {
                        let st = app.state::<Shared>();
                        let mut st = st.lock().unwrap();
                        let want = if id == "settings" { "accounts" } else { "sessions" };
                        st.panel = if st.panel.as_deref() == Some(want) { None } else { Some(want.to_string()) };
                    }
                    refresh(app);
                }
                _ => {
                    if let Some(acct) = id.strip_prefix("acct:") {
                        show_main(app);
                        select(app, acct);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            select_account,
            add_account,
            rename_account,
            set_account_color,
            move_account,
            remove_account,
            set_panel,
            list_hosts,
            host_send,
            hosts_path,
            notify_native,
            begin_oauth,
            cancel_oauth,
            get_autostart,
            set_autostart,
            web_notify,
            webauthn,
            webauthn_cancel,
            browser_signin,
        ])
        .build(tauri::generate_context!())
        .expect("error while building RustyClaude")
        .run(|_app, _ev| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _ev {
                show_main(_app); // Dock icon click brings the hidden window back
            }
        });
}
