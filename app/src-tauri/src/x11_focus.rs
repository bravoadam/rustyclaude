//! Keyboard focus for CEF's browsers on X11.
//!
//! The CEF runtime gives each browser its own X11 window, under an X11 host that sits in the
//! GTK toplevel. When the window manager activates the window, GTK answers WM_TAKE_FOCUS by
//! putting X input focus on its own 1x1 "focus window", a sibling of that host. Key presses
//! then go to GTK, which has no widget to give them to, and never reach Chromium: pages take
//! clicks but not typing.
//!
//! So this watches for X focus landing on the toplevel, GTK's focus window or the host, none of
//! which can use keys, and hands it to the browser that should be typing.

use std::{
    cell::{Cell, RefCell},
    os::raw::c_ulong,
    sync::LazyLock,
};

use cef::{ImplBrowser, ImplBrowserHost};
use tauri::{AppHandle, Manager, Webview};
use x11_dl::xlib;

static XLIB: LazyLock<Option<xlib::Xlib>> = LazyLock::new(|| xlib::Xlib::open().ok());

/// Picks the webview that should get the keyboard.
pub type TargetFn = fn(&AppHandle) -> Option<Webview>;

struct Watch {
    app: AppHandle,
    target: TargetFn,
    /// Windows that can hold X focus without any browser getting keys.
    traps: Vec<c_ulong>,
}

thread_local! {
    // Our own connection, only ever used on the main thread.
    static DISPLAY: Cell<*mut xlib::Display> = const { Cell::new(std::ptr::null_mut()) };
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

fn with_display<R>(f: impl FnOnce(&xlib::Xlib, *mut xlib::Display) -> R) -> Option<R> {
    let xlib = XLIB.as_ref()?;
    let d = DISPLAY.with(|cell| {
        if cell.get().is_null() {
            cell.set(unsafe { (xlib.XOpenDisplay)(std::ptr::null()) });
        }
        cell.get()
    });
    (!d.is_null()).then(|| f(xlib, d))
}

/// Gives `wv` the keyboard: X input focus on its browser window, and CEF focus inside it.
pub fn focus(wv: &Webview) {
    let _ = wv.with_webview(|pw| {
        let Some(host) = pw.downcast_ref::<tauri_runtime_cef::Webview>().and_then(|w| w.browser().host()) else {
            return;
        };
        let xid = host.window_handle() as c_ulong;
        if xid != 0 {
            with_display(|x, d| unsafe {
                // Focusing a window that isn't viewable is a BadMatch, and the process-wide
                // Xlib error handler isn't ours to replace, so check first. Hidden accounts are
                // unmapped by the runtime on this same thread, so this can't race.
                let mut attrs = std::mem::MaybeUninit::<xlib::XWindowAttributes>::uninit();
                if (x.XGetWindowAttributes)(d, xid, attrs.as_mut_ptr()) != 0
                    && attrs.assume_init().map_state == xlib::IsViewable
                {
                    (x.XSetInputFocus)(d, xid, xlib::RevertToParent, xlib::CurrentTime);
                    (x.XFlush)(d);
                }
            });
        }
        host.set_focus(1);
    });
}

/// Starts watching X focus. `any` is any webview in the main window, used to find the
/// toplevel, GTK's focus window and the CEF host.
pub fn install(app: &AppHandle, any: &Webview, target: TargetFn) {
    let app = app.clone();
    let _ = any.with_webview(move |pw| {
        let Some(host) = pw.downcast_ref::<tauri_runtime_cef::Webview>().and_then(|w| w.browser().host()) else {
            return;
        };
        let browser_xid = host.window_handle() as c_ulong;
        let Some(traps) = with_display(|x, d| unsafe { traps(x, d, browser_xid) }).flatten() else {
            eprintln!("keyboard focus: couldn't find the X11 windows to watch");
            return;
        };
        WATCH.with(|w| *w.borrow_mut() = Some(Watch { app: app.clone(), target, traps: traps.clone() }));
        // Not refocusing from here: this callback runs with the runtime's window registry
        // locked, so focus() -> with_webview would deadlock. The watch thread checks once
        // it's listening, through the event loop.
        std::thread::spawn(move || watch(app, traps));
    });
}

/// Blocks on focus events for the trap windows, on a connection of its own, and has the
/// main thread check where focus went after each FocusIn.
fn watch(app: AppHandle, traps: Vec<c_ulong>) {
    let Some(x) = XLIB.as_ref() else { return };
    unsafe {
        let d = (x.XOpenDisplay)(std::ptr::null());
        if d.is_null() {
            return;
        }
        for &w in &traps {
            (x.XSelectInput)(d, w, xlib::FocusChangeMask);
        }
        (x.XSync)(d, 0);
        // Any focus change from here on sends an event; this catches one that came before.
        let _ = app.run_on_main_thread(refocus_if_trapped);
        loop {
            let mut ev = std::mem::MaybeUninit::<xlib::XEvent>::uninit();
            (x.XNextEvent)(d, ev.as_mut_ptr());
            if ev.assume_init().get_type() == xlib::FocusIn && app.run_on_main_thread(refocus_if_trapped).is_err() {
                return;
            }
        }
    }
}

/// The windows that can hold X focus without any browser getting keys: the toplevel, the
/// CEF host and GTK's focus window (the toplevel's InputOnly child).
unsafe fn traps(x: &xlib::Xlib, d: *mut xlib::Display, browser: c_ulong) -> Option<Vec<c_ulong>> {
    if browser == 0 {
        return None;
    }
    let host = parent(x, d, browser)?;
    let top = parent(x, d, host)?;
    let mut traps = vec![top, host];
    for child in children(x, d, top) {
        let mut attrs = std::mem::MaybeUninit::<xlib::XWindowAttributes>::uninit();
        if (x.XGetWindowAttributes)(d, child, attrs.as_mut_ptr()) != 0
            && attrs.assume_init().class == xlib::InputOnly
        {
            traps.push(child);
        }
    }
    Some(traps)
}

unsafe fn parent(x: &xlib::Xlib, d: *mut xlib::Display, w: c_ulong) -> Option<c_ulong> {
    let (mut root, mut parent, mut kids, mut n) = (0, 0, std::ptr::null_mut(), 0);
    if (x.XQueryTree)(d, w, &mut root, &mut parent, &mut kids, &mut n) == 0 {
        return None;
    }
    if !kids.is_null() {
        (x.XFree)(kids.cast());
    }
    (parent != 0 && parent != root).then_some(parent)
}

unsafe fn children(x: &xlib::Xlib, d: *mut xlib::Display, w: c_ulong) -> Vec<c_ulong> {
    let (mut root, mut parent, mut kids, mut n) = (0, 0, std::ptr::null_mut::<c_ulong>(), 0);
    if (x.XQueryTree)(d, w, &mut root, &mut parent, &mut kids, &mut n) == 0 || kids.is_null() {
        return vec![];
    }
    let out = std::slice::from_raw_parts(kids, n as usize).to_vec();
    (x.XFree)(kids.cast());
    out
}

fn refocus_if_trapped() {
    let trapped = WATCH.with(|w| {
        let w = w.borrow();
        let w = w.as_ref()?;
        let focused = with_display(|x, d| unsafe {
            let (mut f, mut revert) = (0, 0);
            (x.XGetInputFocus)(d, &mut f, &mut revert);
            f
        })?;
        w.traps.contains(&focused).then(|| (w.app.clone(), w.target))
    });
    if let Some((app, target)) = trapped {
        if let Some(wv) = target(&app) {
            focus(&wv);
        }
    }
}
