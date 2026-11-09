//! Linux desktop integration: the dock badge and the dock's Quit action.

use std::collections::HashMap;

use gtk::{
    gio,
    glib::{prelude::ToVariant, Variant},
    prelude::*,
};
use tauri::{AppHandle, Manager, Window};

/// Dock badge, via the Unity launcher signal that Plasma's task manager and Dash to Dock
/// listen for. Tauri's set_badge_count needs libunity, which most systems don't have.
pub fn set_badge(count: i64) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    let props: HashMap<String, Variant> =
        HashMap::from([("count".into(), count.to_variant()), ("count-visible".into(), (count > 0).to_variant())]);
    let body = ("application://dev.rustyclaude.app.desktop", props).to_variant();
    let _ = bus.emit_signal(None, "/dev/rustyclaude/app", "com.canonical.Unity.LauncherEntry", "Update", Some(&body));
}

/// The dock's "Quit" calls the app's exported `quit` action when there is one. Needs
/// enableGTKAppId (tauri.conf), which also makes the Wayland app id dev.rustyclaude.app.
pub fn add_quit_action(app: &AppHandle, win: &Window) {
    let Some(gtk_app) = win.gtk_window().ok().and_then(|w| w.application()) else { return };
    let quit = gio::SimpleAction::new("quit", None);
    let app = app.clone();
    quit.connect_activate(move |_, _| app.exit(0));
    gtk_app.add_action(&quit);
}

thread_local! {
    /// The CEF runtime's webview area: the box below the menu bar. Looked up once at
    /// setup, because asking the runtime for the GTK window from layout code (which
    /// also runs inside the runtime's window-event callbacks) deadlocks.
    static WEBVIEW_AREA: std::cell::RefCell<Option<gtk::Widget>> = const { std::cell::RefCell::new(None) };
}

/// Logical height available to webviews, below the menu bar. Window::inner_size
/// includes the menu bar.
pub fn content_height() -> Option<f64> {
    let h = WEBVIEW_AREA.with(|a| a.borrow().as_ref().map(|a| a.height()))?;
    (h > 0).then_some(h as f64)
}

/// Finds the webview area (the last child of the window's vertical box; the menu bar,
/// when there is one, comes before it) and calls `f` on the main thread whenever its
/// height changes, which happens without a window resize when the menu bar appears.
/// Call from setup.
pub fn watch_content_height(app: &AppHandle, win: &Window, f: fn(&AppHandle)) {
    let Ok(gw) = win.gtk_window() else { return };
    let Some(area) = gw.child().and_then(|b| b.last_child()) else { return };
    let Some(surface) = gw.surface() else { return };
    WEBVIEW_AREA.with(|a| *a.borrow_mut() = Some(area.clone()));
    let last = std::cell::Cell::new(-1);
    let app = app.clone();
    // GTK emits this after each layout pass of the window, as the CEF runtime uses it to
    // keep its X11 host over the same area.
    surface.connect_layout(move |_, _, _| {
        if last.replace(area.height()) != area.height() {
            // GTK callbacks can run while the CEF runtime holds its window registry,
            // and layout() calls into the runtime, so run it from the runtime's own
            // queue. Posting from the main thread would run it inline; post from another.
            let app = app.clone();
            std::thread::spawn(move || {
                let app2 = app.clone();
                let _ = app.run_on_main_thread(move || f(&app2));
            });
        }
    });
}
