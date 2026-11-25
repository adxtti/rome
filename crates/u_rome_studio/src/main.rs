//! ROME Studio entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    u_rome_studio::run()
}
