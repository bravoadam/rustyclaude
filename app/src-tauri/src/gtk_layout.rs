//! Linux only: Tauri packs child webviews into the window's vertical gtk::Box, where
//! set_position/set_size have no effect and they stack on top of each other. So each
//! webview is moved into a gtk::Layout, which allows absolute placement and, unlike
//! gtk::Fixed, doesn't stop the window shrinking below its children's size.

use std::{cell::RefCell, collections::HashMap, sync::Mutex};

use gtk::{
    gio,
    glib::{ToVariant, Variant},
    prelude::*,
};
use tauri::{
    webview::{NewWindowFeatures, NewWindowResponse},
    AppHandle, WebviewUrl, WebviewWindowBuilder, Window, Wry,
};

thread_local! {
    /// GTK objects live on the main thread, which is where with_webview closures run.
    static LAYOUT: RefCell<Option<gtk::Layout>> = const { RefCell::new(None) };
}

/// Space below the menu bar, in logical pixels. Window::inner_size includes the menu bar.
static AREA: Mutex<(i32, i32)> = Mutex::new((0, 0));

/// Must run on the main thread, before any webview is placed.
pub fn install(app: &AppHandle, win: &Window) -> tauri::Result<()> {
    let vbox = win.default_vbox()?;
    let layout = gtk::Layout::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    vbox.pack_start(&layout, true, true, 0);
    layout.show();

    let app = app.clone();
    layout.connect_size_allocate(move |_, rect| {
        let new = (rect.width(), rect.height());
        let mut area = AREA.lock().unwrap();
        if *area != new {
            *area = new;
            // Resizing children during allocation is ignored by GTK; do it right after.
            let app = app.clone();
            gtk::glib::idle_add_local_once(move || crate::layout(&app));
        }
    });
    LAYOUT.with(|l| *l.borrow_mut() = Some(layout));
    Ok(())
}

pub fn area() -> Option<(f64, f64)> {
    let (w, h) = *AREA.lock().unwrap();
    (w > 1 && h > 1).then_some((w as f64, h as f64))
}

pub fn place(wv: &tauri::Webview, x: f64, y: f64, w: f64, h: f64) {
    let _ = wv.with_webview(move |platform| {
        let widget = platform.inner().upcast::<gtk::Widget>();
        LAYOUT.with(|l| {
            let l = l.borrow();
            let Some(layout) = l.as_ref() else { return };
            let (x, y) = (x as i32, y as i32);
            if widget.parent().as_ref() == Some(layout.upcast_ref()) {
                layout.move_(&widget, x, y);
            } else {
                if let Some(parent) = widget.parent().and_then(|p| p.downcast::<gtk::Container>().ok()) {
                    parent.remove(&widget);
                }
                layout.put(&widget, x, y);
            }
            widget.set_size_request(w as i32, h as i32);
        });
    });
}

/// Sign-in popup. wry's default is a tiny bare window without our user agent, so build a
/// real one. window_features ties it to the opener, so it shares that account's session.
pub fn popup(app: &AppHandle, features: NewWindowFeatures, user_agent: &str) -> NewWindowResponse<Wry> {
    let label = format!("popup-{}", uuid::Uuid::new_v4().simple());
    let built = WebviewWindowBuilder::new(app, label, WebviewUrl::External("about:blank".parse().unwrap()))
        .title("Sign in")
        .inner_size(520.0, 720.0)
        .user_agent(user_agent)
        .window_features(features)
        .build();
    match built {
        Ok(window) => {
            let _ = window.remove_menu();
            // window.close() from the page only destroys the webview; close its window too.
            let w = window.clone();
            let _ = window.with_webview(move |platform| {
                platform.inner().connect_destroy(move |_| {
                    let _ = w.destroy();
                });
            });
            NewWindowResponse::Create { window }
        }
        Err(e) => {
            eprintln!("could not open sign-in window: {e}");
            NewWindowResponse::Deny
        }
    }
}

/// Dock badge, via the Unity launcher signal that Plasma's task manager listens for.
/// Tauri's set_badge_count needs libunity, which most systems don't have.
pub fn set_badge(count: i64) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    let props: HashMap<String, Variant> =
        HashMap::from([("count".into(), count.to_variant()), ("count-visible".into(), (count > 0).to_variant())]);
    let body = ("application://RustyClaude.desktop", props).to_variant();
    let _ = bus.emit_signal(None, "/dev/rustyclaude/app", "com.canonical.Unity.LauncherEntry", "Update", Some(&body));
}
