//! Desktop smoke-test entry point.
//!
//! On iOS/Android the platform shell enters through
//! `allmystuff_mobile_lib::run` (the `tauri::mobile_entry_point`); this `main`
//! exists so `pnpm tauri dev` can bring the same shell up in a desktop window
//! to iterate on the UI without a device or emulator.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    allmystuff_mobile_lib::run();
}
