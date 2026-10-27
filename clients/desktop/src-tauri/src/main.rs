//! rd Desktop's desktop entry point. The app itself (and the Android entry point Tauri looks
//! for) is `rd_desktop::run` in lib.rs, because mobile builds are libraries.

// Release builds are GUI apps on Windows (no console window); tests keep the console.
#![cfg_attr(all(not(debug_assertions), not(test)), windows_subsystem = "windows")]

fn main() {
    rd_desktop::run();
}
