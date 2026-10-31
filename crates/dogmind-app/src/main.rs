//! Dogmind as a Windows application: a native window and Dogmind's agent. The engine is `straydog.exe` beside it.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod engine;
mod icon;
mod paint;
mod rtf;
mod settings;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    ui::run(std::env::args_os().nth(1).map(std::path::PathBuf::from))
}
