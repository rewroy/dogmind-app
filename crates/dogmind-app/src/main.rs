//! Dogmind as a Windows application: a native window and Dogmind's agent. The engine is `straydog.exe` beside it.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod attach;
mod crash;
mod design;
mod diffview;
mod engine;
mod frametrace;
mod icon;
mod keys;
mod lang;
mod machine;
mod markdown;
mod highlight;
mod paint;
mod preview;
mod rtf;
mod settings;
mod toolrun;
mod ui;
mod vision;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Before anything starts a child, which has this from its parent: no box on the person's screen from anything the app runs.
    dogmind_agent::sandbox::no_error_boxes();
    // Before any font is made.
    let _ = paint::japanese_face();
    let first = std::env::args_os().nth(1);
    // (from Add or remove programs: `dogmind-uninstall.exe` opens the page that does it, as Settings would)
    if first.as_deref() == Some(std::ffi::OsStr::new("--remove")) {
        return ui::run(None, true);
    }
    ui::run(first.map(std::path::PathBuf::from), false)
}
