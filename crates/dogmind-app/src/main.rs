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
    let mut folder = None;
    let mut restart = ui::Restart::default();
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            // (from Add or remove programs: `dogmind-uninstall.exe` opens the page that does it, as Settings would)
            Some("--remove") => return ui::run(None, true, ui::Restart::default()),
            // (from a copy of this app that is giving way to a newer one: see `ui::Restart`)
            Some("--after") => restart.after = arguments.next().and_then(|pid| pid.to_str()?.parse().ok()),
            Some("--resume") => restart.resume = arguments.next().map(std::path::PathBuf::from),
            Some("--hidden") => restart.hidden = true,
            _ if folder.is_none() => folder = Some(std::path::PathBuf::from(argument)),
            _ => {}
        }
    }
    ui::run(folder, false, restart)
}
