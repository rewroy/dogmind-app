//! What this crate's tests do when they run inside an AppContainer themselves, as a session on the app's own source runs them, by a command of the app's:
//! a temp folder a container can work in, and the tests that need what no container can do left out, counted, and said so at the end of the run.
#![allow(dead_code)] // (each crate's tests give some of the reasons and not others)

use std::collections::BTreeMap;
use std::sync::{Mutex, Once};

/// Why a test cannot run in a container, as the end of the run says it ("31 that ...").
pub(crate) const FOLDERS: &str = "change who may read or write a folder, which only the person can";
pub(crate) const SANDBOX: &str = "make the sandbox's own desktops, guards and runs as the person, which no container can";
pub(crate) const PROGRAMS: &str = "start and end programs with jobs or terminals of their own, or reach the network";
pub(crate) const SYSTEM: &str = "look at a drive's root and facts, or draw on a desktop, which a container cannot";

/// The tests left out, each with why.
static LEFT_OUT: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// `tempfile::tempdir`, made where a container can work in it when the tests run in one. A container's temp folder is always its own, under C:\Users (the
/// system gives it whatever TEMP says), and git looks at every folder of a path from the drive down before it makes one, C:\Users too, which a container
/// cannot: no test could make a repository there ("cannot lock ref 'HEAD': unable to create directory"). Outside a container it is `tempfile::tempdir`.
pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
    if crate::sandbox::in_a_container() {
        in_reach();
    }
    tempfile::tempdir()
}

/// Commands on Windows can grant access to their workspace. Give each test its own folder under the checkout,
/// even when Cargo writes its build artifacts elsewhere. Keep `..` out of the path used in approval checks.
pub(crate) fn folder_for_commands() -> tempfile::TempDir {
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
    let parent = checkout.join("target");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

/// A temp folder beside the test program (in the workspace's target folder, which the container was given), and git kept from looking above it for a
/// repository: it lies in the workspace's own, which a test's folder in the temp folder never did.
fn in_reach() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(folder) = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("container-temp"))) else { return };
        if std::fs::create_dir_all(&folder).is_err() {
            return;
        }
        let _ = tempfile::env::override_temp_dir(&folder);
        std::env::set_var("GIT_CEILING_DIRECTORIES", &folder);
    });
}

/// Whether this test cannot run where it runs, inside a container: it needs what no container can do (`why`). It is then counted, said at the end of the
/// run, and the test returns at once.
pub(crate) fn cannot_run(why: &'static str) -> bool {
    if !crate::sandbox::in_a_container() {
        return false;
    }
    static SAY: Once = Once::new();
    // SAFETY: the C runtime calls it once, as the process ends; it only reads the list and writes a line.
    SAY.call_once(|| unsafe {
        atexit(say_left_out);
    });
    LEFT_OUT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(why);
    true
}

/// The last line of the run: after the test harness's own, so that what reads the run's end reads it.
extern "C" fn say_left_out() {
    let left = LEFT_OUT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut by_why: BTreeMap<&str, usize> = BTreeMap::new();
    for why in left.iter() {
        *by_why.entry(why).or_insert(0) += 1;
    }
    let whys: Vec<String> = by_why.iter().map(|(why, count)| format!("{count} that {why}")).collect();
    eprintln!("\nnote: {} of these tests did not run, as they ran inside a sandbox, where they cannot: {}. They pass or fail only outside it, as the person runs them; all of them run before a release.", left.len(), whys.join("; "));
}

extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}
