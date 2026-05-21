// main.rs — Tauri application entry point
// On Windows this is a windows subsystem binary (no console window)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    gameshare_lib::run();
}
