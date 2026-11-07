//! A terminal: the person's own shell, in a pseudo console, its output read into a screen with a scrollback. Nothing here knows the window; the Terminal view of the panel on the right
//! (`ui::term_panel`) draws what `Terminal::with_screen` gives and sends what `Terminal::write` is given.
//!
//! - `shells`: which shells are on this PC, and how each is started.
//! - `keymap`: what a key sends to a shell.
//! - `pty`: the pseudo console (ConPTY) and the job that ends the shell, and all it started, with the terminal.
//! - `screen`: the grid, the scrollback, the selection, and the parsing of the escape sequences that a shell writes.
//! - `palette`: the colours of the cells, in the theme, and how far apart text is held from its ground.

pub mod keymap;
pub mod palette;
pub mod pty;
pub mod screen;
pub mod shells;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use self::screen::Screen;
use self::shells::Shell;

/// The most terminals the panel holds at once, running or exited.
pub const MOST_TERMINALS: usize = 8;

/// The lines a terminal keeps of what scrolled off its top.
pub const SCROLLBACK: usize = 10_000;

/// What a terminal tells the window, from the threads that read it. They are coalesced: a window is told once until it has looked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wake {
    /// There is more on the screen.
    Output,
    /// The shell has ended, and what it wrote is all on the screen.
    Ended,
}

/// What a terminal's threads share with the window.
struct Shared {
    screen: Mutex<Screen>,
    /// The shell's exit code, once it has ended.
    exit: Mutex<Option<u32>>,
    /// Everything it wrote has been read.
    finished: AtomicBool,
    /// The window has been told of output and has not looked yet.
    told: AtomicBool,
    /// When a synchronized update that was begun is to be shown whether it ended or not.
    sync_deadline: Mutex<Option<Instant>>,
    sync_changed: Condvar,
    /// The terminal is gone: its threads end.
    closed: AtomicBool,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Terminal {
    pub id: u64,
    pub shell: Shell,
    pub folder: PathBuf,
    /// The tab's words: the shell's name, until the person gives it another.
    pub name: String,
    pub renamed: bool,
    shared: Arc<Shared>,
    pty: pty::Pty,
}

impl Terminal {
    /// A shell in `folder`, `cols` by `rows` cells. `wake` is called from other threads, and does no more than tell a window.
    pub fn start(id: u64, shell: Shell, folder: &Path, cols: u16, rows: u16, wake: Arc<dyn Fn(u64, Wake) + Send + Sync>) -> std::io::Result<Terminal> {
        let shared = Arc::new(Shared {
            screen: Mutex::new(Screen::new(cols, rows, SCROLLBACK)),
            exit: Mutex::new(None),
            finished: AtomicBool::new(false),
            told: AtomicBool::new(false),
            sync_deadline: Mutex::new(None),
            sync_changed: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let tell = {
            let (shared, wake) = (shared.clone(), wake.clone());
            move |what: Wake| {
                if what == Wake::Ended || !shared.told.swap(true, Ordering::AcqRel) {
                    wake(id, what);
                }
            }
        };
        // (what it wrote last is still on its way through the pseudo console: the reader says when it has read it all)
        let ended = {
            let shared = shared.clone();
            move |code: u32| *locked(&shared.exit) = Some(code)
        };
        let (pty, mut output) = pty::Pty::spawn(&shell, folder, cols, rows, ended)?;
        let input = pty.input();
        // The reader: what the shell writes goes into the screen; what the screen answers (a device report, a colour asked for) goes back to the shell.
        {
            let (shared, tell, input) = (shared.clone(), tell.clone(), input.clone());
            std::thread::Builder::new().name("terminal-reader".into()).spawn(move || {
                let mut chunk = vec![0u8; 64 * 1024];
                loop {
                    match output.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            let (replies, deadline) = {
                                let mut screen = locked(&shared.screen);
                                let replies = screen.feed(&chunk[..count]);
                                (replies, screen.sync_deadline())
                            };
                            if !replies.is_empty() {
                                let _ = input.send(replies);
                            }
                            if let Some(deadline) = deadline {
                                *locked(&shared.sync_deadline) = Some(deadline);
                                shared.sync_changed.notify_one();
                            }
                            tell(Wake::Output);
                        }
                    }
                }
                shared.finished.store(true, Ordering::Release);
                tell(Wake::Ended);
            })?;
        }
        // A synchronized update (a program asks for its next screen to be shown whole) that is not ended in a moment is shown: the reader may have nothing more to read.
        {
            let (shared, tell) = (shared.clone(), tell);
            std::thread::Builder::new().name("terminal-sync".into()).spawn(move || {
                let mut deadline = locked(&shared.sync_deadline);
                while !shared.closed.load(Ordering::Acquire) {
                    let wanted = *deadline;
                    match wanted {
                        Some(at) if at <= Instant::now() => {
                            *deadline = None;
                            drop(deadline);
                            locked(&shared.screen).stop_sync();
                            tell(Wake::Output);
                            deadline = locked(&shared.sync_deadline);
                        }
                        _ => {
                            let wait = wanted.map_or(Duration::from_millis(500), |at| at - Instant::now());
                            deadline = shared.sync_changed.wait_timeout(deadline, wait).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
                        }
                    }
                }
            })?;
        }
        Ok(Terminal { id, name: shell.name.to_string(), shell, folder: folder.to_path_buf(), renamed: false, shared, pty })
    }

    /// Bytes for the shell, as a key or a paste sends them.
    pub fn write(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.pty.input().send(bytes.to_vec());
        }
    }

    /// The screen, for as long as `look` takes: the thread that reads the shell waits for it.
    pub fn with_screen<R>(&self, look: impl FnOnce(&mut Screen) -> R) -> R {
        look(&mut locked(&self.shared.screen))
    }

    /// The window now has this many cells for it.
    pub fn resize(&self, cols: u16, rows: u16) {
        let changed = {
            let mut screen = locked(&self.shared.screen);
            let changed = screen.size() != (cols, rows);
            if changed {
                screen.resize(cols, rows);
            }
            changed
        };
        if changed {
            self.pty.resize(cols, rows);
        }
    }

    /// The window has looked: the next output tells it again.
    pub fn looked(&self) {
        self.shared.told.store(false, Ordering::Release);
    }

    /// The shell's exit code, once it has ended.
    pub fn exit_code(&self) -> Option<u32> {
        *locked(&self.shared.exit)
    }

    /// The shell has ended and all that it wrote is on the screen.
    pub fn is_over(&self) -> bool {
        self.exit_code().is_some() && self.shared.finished.load(Ordering::Acquire)
    }

    /// The shell's own process.
    #[cfg(any(test, debug_assertions))]
    pub fn pid(&self) -> u32 {
        self.pty.pid()
    }

    /// The processes that end with this terminal: the shell and all it started.
    pub fn processes(&self) -> Vec<u32> {
        self.pty.processes()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.sync_changed.notify_all();
        // (the pty goes on its own: its job ends the shell and what it started, and its console is closed on a thread of its own)
    }
}

/// The folder a shell starts in: the session's, when it has one that is there, and otherwise the person's own.
pub fn start_folder(workspace: &Path, home: Option<&Path>) -> PathBuf {
    let has_one = !workspace.as_os_str().is_empty() && workspace != Path::new(".") && workspace.is_dir();
    if has_one {
        return workspace.to_path_buf();
    }
    home.filter(|home| home.is_dir()).map(Path::to_path_buf).or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// The person's own folder.
pub fn home_folder() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from).filter(|home| !home.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_starts_in_the_sessions_folder_and_with_none_in_the_persons_own() {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        assert_eq!(start_folder(work.path(), Some(home.path())), work.path());
        // (no folder chosen yet is how the window says it: ".")
        assert_eq!(start_folder(Path::new("."), Some(home.path())), home.path());
        assert_eq!(start_folder(Path::new(""), Some(home.path())), home.path());
        assert_eq!(start_folder(&work.path().join("gone"), Some(home.path())), home.path(), "a folder that has gone is as none");
        assert!(start_folder(Path::new("."), None).is_dir(), "and with no home either, somewhere that is there");
    }

    fn command_prompt() -> Shell {
        Shell { id: "cmd", name: "Command Prompt", program: PathBuf::from(std::env::var("COMSPEC").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into())), arguments: &[], environment: &[] }
    }

    fn started(cols: u16, rows: u16) -> (Terminal, Arc<std::sync::atomic::AtomicUsize>) {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = wakes.clone();
        let terminal = Terminal::start(1, command_prompt(), &std::env::temp_dir(), cols, rows, Arc::new(move |_, _| {
            counted.fetch_add(1, Ordering::Relaxed);
        }))
        .unwrap();
        (terminal, wakes)
    }

    /// Waits until the screen satisfies `done`, as a window would see it: and says what it had, if it never does.
    fn until(terminal: &Terminal, what: &str, done: impl Fn(&mut Screen) -> bool) {
        let started = Instant::now();
        while !terminal.with_screen(|screen| done(screen)) {
            assert!(started.elapsed() < Duration::from_secs(20), "{what}: {:?}", terminal.with_screen(|screen| screen.text()));
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn a_terminal_runs_a_shell_and_its_screen_has_what_it_wrote_with_japanese_two_cells_each() {
        let (terminal, wakes) = started(80, 24);
        terminal.write("echo plain and あいう.\r".as_bytes());
        until(&terminal, "the line did not come", |screen| screen.visible_lines().iter().any(|line| line == "plain and あいう."));
        // (the dot after three wide characters is where ten narrow and six wide cells put it)
        let cells = terminal.with_screen(|screen| screen.cells_in_view());
        let row = terminal.with_screen(|screen| screen.visible_lines()).iter().position(|line| line == "plain and あいう.").unwrap();
        assert!(cells.iter().any(|cell| cell.0 == row && cell.2 == '.' && cell.1 == 16), "{cells:?}");
        assert!(cells.iter().any(|cell| cell.0 == row && cell.2 == 'あ' && cell.1 == 10 && cell.3 == 2));
        assert!(wakes.load(Ordering::Relaxed) > 0, "the window was told");
        assert!(terminal.exit_code().is_none() && !terminal.is_over());
    }

    #[test]
    fn a_shell_that_ends_leaves_its_screen_and_its_exit_code() {
        let (terminal, _) = started(80, 24);
        terminal.write(b"echo last words\r");
        terminal.write(b"exit 5\r");
        let started = Instant::now();
        while !terminal.is_over() {
            assert!(started.elapsed() < Duration::from_secs(20), "it did not end");
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(terminal.exit_code(), Some(5));
        assert!(terminal.with_screen(|screen| screen.text()).contains("last words"));
    }

    #[test]
    fn a_resize_reaches_the_console_and_the_screen() {
        let (terminal, _) = started(80, 24);
        terminal.resize(50, 12);
        assert_eq!(terminal.with_screen(|screen| screen.size()), (50, 12));
        terminal.write(b"mode con\r");
        until(&terminal, "the console did not say its size", |screen| {
            let text = screen.text();
            // (its lines and its columns, in the words of this PC's language)
            text.lines().any(|line| line.trim_end().ends_with("50")) && text.lines().any(|line| line.trim_end().ends_with("12"))
        });
    }

    #[test]
    fn closing_a_terminal_ends_its_shell_and_what_it_started() {
        let (terminal, _) = started(80, 24);
        terminal.write(b"ping -n 60 127.0.0.1 >nul\r");
        let started = Instant::now();
        while terminal.processes().len() < 2 {
            assert!(started.elapsed() < Duration::from_secs(20), "ping did not start: {:?}", terminal.processes());
            std::thread::sleep(Duration::from_millis(25));
        }
        let all = terminal.processes();
        drop(terminal);
        let started = Instant::now();
        while all.iter().any(|pid| pty::is_running(*pid)) {
            assert!(started.elapsed() < Duration::from_secs(20), "still running: {all:?}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
