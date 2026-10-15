//! `captastic.exe`, the program that ships. See the library's `desktop` module.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    captastic_app::desktop_main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("captastic is currently available only on Windows; captastic-dev runs elsewhere");
    std::process::exit(1);
}
