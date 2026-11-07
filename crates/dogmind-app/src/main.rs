//! Dogmind as a Windows application: a native window and Dogmind's agent. The engine is `straydog.exe` beside it.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod attach;
mod cards;
mod trajectory;
mod crash;
mod design;
mod diffview;
mod drafter;
mod engine;
mod files;
mod frametrace;
mod icon;
mod keys;
mod lang;
mod machine;
mod markdown;
mod mask;
mod office;
mod highlight;
mod notice;
mod paint;
mod preview;
mod render;
mod rtf;
mod table;
mod settings;
mod textview;
mod toolrun;
mod ui;
mod updates;
mod vision;

use std::process::ExitCode;

/// `dogmind-app --trajectory <file> [--engine-log <json>]`: how a run went, read from its trajectory (see `dogmind_agent::trajectory`). The app has no console of its own, so it
/// takes its parent's, if there is one; where its output is redirected it goes there.
fn print_trajectory(arguments: &[std::ffi::OsString]) -> ExitCode {
    use std::io::Write;
    #[cfg(windows)]
    // SAFETY: a call with no pointers; it fails harmlessly when there is no console to take.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let arguments: Vec<String> = arguments.iter().map(|argument| argument.to_string_lossy().into_owned()).collect();
    match dogmind_agent::trajectory_report::run(&arguments) {
        Ok(text) => {
            let _ = std::io::stdout().write_all(text.as_bytes());
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "{error}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    // (a reader of the app's own record of a run, which prints and ends: no window)
    let given: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if given.first().is_some_and(|first| first == "--trajectory") {
        return print_trajectory(&given[1..]);
    }
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
