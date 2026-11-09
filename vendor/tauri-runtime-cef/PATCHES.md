# Local patches to tauri-runtime-cef 3.0.0-alpha.7

This is the crates.io release, vendored through `[patch.crates-io]` in the workspace
`Cargo.toml`, with the changes below. Drop the patch once upstream has equivalents.

## 1. Chromium's GLib work source no longer spins the main loop (Linux)

`src/external_message_pump/linux.rs` (`tame_chromium_work_source`), `mod.rs`.

Chromium's UI-thread `MessagePumpGlib` attaches a work source to the default GLib
context. Its prepare returns a zero timeout whenever Chromium's RunLoop is not running
(`if (!state_) return 0;`), and its check won't dispatch in that state. With
`external_message_pump`, Chromium's RunLoop only runs inside `cef::do_message_loop_work`,
so the rest of the time every GLib iteration polls with timeout 0 and dispatches nothing:
the main thread sat at 100% CPU with the app idle, even with no pages loaded.

The pump now wraps that source's prepare: outside `do_message_loop_work`, a "not ready,
timeout 0" answer becomes "no timeout". CEF still reports all work through
`OnScheduleMessagePumpWork`, which the pump turns into `do_message_loop_work` calls.

Idle main-thread CPU: ~100% -> ~2%.

## 2. `OnScheduleMessagePumpWork` doesn't wake winit (Linux)

`src/cef_impl/client/process.rs`.

The Linux pump is a GLib source woken through its own pipe, and winit-gtk4 blocks in the
same GLib context, so CEF's work runs without waking winit. Also waking winit made it run
a full event-loop iteration for every task CEF posts (~500/s with claude.ai open, vs ~5/s
after).
