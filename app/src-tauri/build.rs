fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
        "get_state",
        "select_account",
        "add_account",
        "rename_account",
        "set_account_color",
        "move_account",
        "remove_account",
        "set_panel",
        "list_hosts",
        "host_send",
        "hosts_path",
        "notify_native",
        "begin_oauth",
        "cancel_oauth",
        "get_autostart",
        "set_autostart",
        "web_notify",
        "webauthn",
        "webauthn_cancel",
    ])))
    .expect("failed to run tauri-build");
}
