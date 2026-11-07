//! A shell in a pseudo console (ConPTY), so that PowerShell, cmd and what they start (git, python, vim, a progress bar) behave as they do in Windows Terminal, and in a job that ends it, and
//! everything it started, when the terminal goes: closed with its tab, or with the app, however the app ends.
//!
//! The shell is made suspended and put in the job before it runs a line, so that nothing it starts has been out of the job. Its input is written from a thread of its own, so that a shell
//! that is not reading never holds up the window.

use std::ffi::c_void;
use std::io::{self, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, SetConsoleCtrlHandler, COORD, HPCON};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;

use super::shells::{command_line, Shell};

/// A handle that closes with this.
struct Handle(HANDLE);

// SAFETY: a process handle is the system's, and is used from whichever thread holds it.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's, and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A job whose processes end when its last handle is closed, which is this value's drop.
pub struct Job(Handle);

impl Job {
    pub fn new() -> Option<Job> {
        // SAFETY: the structure is as long as the call is told, and the handle is live.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let job = Job(Handle(job));
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject((job.0).0, JobObjectExtendedLimitInformation, std::ptr::addr_of!(limits).cast(), std::mem::size_of_val(&limits) as u32);
            (set != 0).then_some(job)
        }
    }

    /// Whether the process is in the job now.
    pub fn assign(&self, process: HANDLE) -> bool {
        // SAFETY: two live handles.
        unsafe { AssignProcessToJobObject((self.0).0, process) != 0 }
    }

    /// The processes in the job: the shell and what it has started, and has not ended.
    #[cfg(any(test, debug_assertions))]
    pub fn processes(&self) -> Vec<u32> {
        // The list's two counts, and room for the ids: as many as the job will give (its own count is said, and a longer list is cut).
        let mut buffer = vec![0usize; 1 + 512];
        // SAFETY: the buffer is as long as the call is told, and aligned for the structure; the call fills the ids it names.
        unsafe {
            if QueryInformationJobObject((self.0).0, JobObjectBasicProcessIdList, buffer.as_mut_ptr().cast(), (buffer.len() * std::mem::size_of::<usize>()) as u32, std::ptr::null_mut()) == 0 {
                return Vec::new();
            }
            let list = &*(buffer.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST);
            let count = (list.NumberOfProcessIdsInList as usize).min(512);
            std::slice::from_raw_parts(std::ptr::addr_of!(list.ProcessIdList).cast::<usize>(), count).iter().map(|id| *id as u32).collect()
        }
    }
}

/// The pseudo console: its handle, until it is closed.
struct Console(Mutex<Option<HPCON>>);

impl Console {
    fn resize(&self, cols: u16, rows: u16) {
        if let Some(console) = *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            // SAFETY: a live pseudo console, closed only under this lock.
            unsafe { ResizePseudoConsole(console, COORD { X: cols.clamp(1, i16::MAX as u16) as i16, Y: rows.clamp(1, i16::MAX as u16) as i16 }) };
        }
    }

    /// Closed once. It waits for what the console still has to write to be read where Windows is older, so it is not called from a thread that has to go on.
    fn close(&self) {
        let console = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        if let Some(console) = console {
            // SAFETY: a pseudo console that is closed here, once.
            unsafe { ClosePseudoConsole(console) };
        }
    }
}

pub struct Pty {
    console: Arc<Console>,
    process: Arc<Handle>,
    pid: u32,
    /// The job that holds the shell, when the shell is in it.
    job: Option<Job>,
    input: Sender<Vec<u8>>,
}

/// The environment the shell has: the person's own, with what the shell asks for added, and a colour terminal said to be one.
fn environment_block(extra: &[(&str, &str)]) -> Vec<u16> {
    let mut entries: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let mut add = |name: &str, value: &str, replace: bool| {
        let known = entries.iter().position(|(known, _)| known.to_string_lossy().eq_ignore_ascii_case(name));
        match known {
            Some(at) if replace => entries[at].1 = value.into(),
            Some(_) => {}
            None => entries.push((name.into(), value.into())),
        }
    };
    add("COLORTERM", "truecolor", false);
    add("TERM_PROGRAM", "Dogmind", true);
    for (name, value) in extra {
        add(name, value, true);
    }
    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(std::os::windows::ffi::OsStrExt::encode_wide(name.as_os_str()));
        block.push(u16::from(b'='));
        block.extend(std::os::windows::ffi::OsStrExt::encode_wide(value.as_os_str()));
        block.push(0);
    }
    block.push(0);
    block
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Pty {
    /// The shell, started in `folder` with a console of `cols` by `rows` cells; and what it writes, to be read from the pipe that comes with it. `ended` is called, from a thread, with
    /// its exit code when it has ended.
    pub fn spawn(shell: &Shell, folder: &Path, cols: u16, rows: u16, ended: impl FnOnce(u32) + Send + 'static) -> io::Result<(Pty, io::PipeReader)> {
        Pty::spawn_line(&command_line(shell), shell.environment, folder, cols, rows, ended)
    }

    /// The same for a command line of its own.
    pub fn spawn_line(line: &str, environment: &[(&str, &str)], folder: &Path, cols: u16, rows: u16, ended: impl FnOnce(u32) + Send + 'static) -> io::Result<(Pty, io::PipeReader)> {
        let (input_read, input_write) = io::pipe()?;
        let (output_read, output_write) = io::pipe()?;
        let size = COORD { X: cols.clamp(1, i16::MAX as u16) as i16, Y: rows.clamp(1, i16::MAX as u16) as i16 };
        // SAFETY: handles of this function's own, closed where they are done with; the attribute list lives in a buffer that outlives the call that reads it; the structures are as the calls ask.
        unsafe {
            let mut console: HPCON = 0;
            let made = CreatePseudoConsole(size, input_read.as_raw_handle(), output_write.as_raw_handle(), 0, &mut console);
            if made < 0 {
                return Err(io::Error::from_raw_os_error(made & 0xFFFF));
            }
            // (the console has its own copies of these two ends)
            drop((input_read, output_write));
            let console = Arc::new(Console(Mutex::new(Some(console))));
            let held = console.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).unwrap_or(0);

            let mut list_size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut list_size);
            let mut list_buffer = vec![0u64; list_size.div_ceil(8)];
            let list = list_buffer.as_mut_ptr().cast::<c_void>();
            if InitializeProcThreadAttributeList(list, 1, 0, &mut list_size) == 0 || UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize, held as *const c_void, std::mem::size_of::<HPCON>(), std::ptr::null_mut(), std::ptr::null()) == 0 {
                let error = io::Error::last_os_error();
                console.close();
                return Err(error);
            }
            // (a process that was started with Ctrl+C turned off, from a shell that had it so, passes that on to what it starts: a shell that gets no Ctrl+C is no use. It is turned on again here.)
            SetConsoleCtrlHandler(None, 0);
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // (none of this process's own standard handles go to the shell: it has the console's, and a process that was started with its output in a pipe would otherwise give the shell that pipe)
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            startup.lpAttributeList = list;
            let mut line = wide(line);
            let folder = wide(&folder.display().to_string());
            let environment = environment_block(environment);
            let mut process: PROCESS_INFORMATION = std::mem::zeroed();
            let started = CreateProcessW(std::ptr::null(), line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED, environment.as_ptr().cast(), folder.as_ptr(), &startup.StartupInfo, &mut process);
            let error = io::Error::last_os_error();
            DeleteProcThreadAttributeList(list);
            if started == 0 {
                console.close();
                return Err(error);
            }
            let process_handle = Arc::new(Handle(process.hProcess));
            // In the job before it runs; one that is not in it (a job that could not be made, or that this process may not join) is ended by its tree where it is closed.
            let job = Job::new().filter(|job| job.assign(process.hProcess));
            ResumeThread(process.hThread);
            CloseHandle(process.hThread);

            let (input, queue) = channel::<Vec<u8>>();
            let mut writer = input_write;
            std::thread::Builder::new().name("terminal-writer".into()).spawn(move || {
                for chunk in queue {
                    if writer.write_all(&chunk).and_then(|()| writer.flush()).is_err() {
                        break;
                    }
                }
            })?;
            {
                let (process, console) = (process_handle.clone(), console.clone());
                std::thread::Builder::new().name("terminal-exit".into()).spawn(move || {
                    // (the handle is held by the Arc this thread has)
                    WaitForSingleObject(process.0, INFINITE);
                    let mut code = 0u32;
                    GetExitCodeProcess(process.0, &mut code);
                    ended(code);
                    // (the last of what it wrote is on its way through the console: it is closed after a moment, which ends the reading)
                    std::thread::sleep(Duration::from_millis(150));
                    console.close();
                })?;
            }
            Ok((Pty { console, process: process_handle, pid: process.dwProcessId, job, input }, output_read))
        }
    }

    pub fn input(&self) -> Sender<Vec<u8>> {
        self.input.clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        self.console.resize(cols, rows);
    }

    #[cfg(any(test, debug_assertions))]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// What ends with the terminal: the shell and all it started and has not ended. Where the shell is in no job, the shell.
    #[cfg(any(test, debug_assertions))]
    pub fn processes(&self) -> Vec<u32> {
        match &self.job {
            Some(job) => job.processes(),
            None => vec![self.pid],
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.job.is_none() {
            // (no job holds it: the shell and what it started are ended by taskkill, which follows the tree)
            let mut kill = std::process::Command::new("taskkill");
            kill.args(["/T", "/F", "/PID", &self.pid.to_string()]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).creation_flags(0x0800_0000);
            let _ = kill.status();
        }
        // The job's last handle closes with it, which ends what is in it.
        drop(self.job.take());
        let console = self.console.clone();
        let process = self.process.clone();
        let _ = std::thread::Builder::new().name("terminal-close".into()).spawn(move || {
            // (the shell is gone or going: the console is closed once what is left in it is read)
            // SAFETY: a handle the Arc holds.
            unsafe { WaitForSingleObject(process.0, 5000) };
            console.close();
        });
    }
}

/// Whether a process by this id is still running.
#[cfg(test)]
pub fn is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    // SAFETY: a handle opened here and closed here.
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let running = WaitForSingleObject(handle, 0) != WAIT_OBJECT_0;
        CloseHandle(handle);
        running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    fn command() -> String {
        format!("\"{}\"", std::env::var("COMSPEC").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()))
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// What the console wrote, read on a thread until it ends.
    fn read_all(mut output: io::PipeReader) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(count) = output.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                all.extend_from_slice(&chunk[..count]);
            }
            all
        })
    }

    #[test]
    fn the_job_ends_a_shell_and_a_program_it_started_when_it_is_dropped() {
        let folder = std::env::temp_dir();
        let (pty, output) = Pty::spawn_line(&format!("{} /c ping -n 60 127.0.0.1 >nul", command()), &[], &folder, 80, 24, |_| {}).unwrap();
        let reading = read_all(output);
        let shell = pty.pid();
        // The shell and ping: two, as soon as ping is started.
        until("ping did not start under the shell", || pty.processes().len() >= 2);
        let all = pty.processes();
        assert!(all.contains(&shell));
        assert!(all.iter().all(|pid| is_running(*pid)));
        drop(pty);
        until("what the shell started is still running", || all.iter().all(|pid| !is_running(*pid)));
        let _ = reading.join();
    }

    #[test]
    fn a_shell_that_ends_is_told_with_its_exit_code_and_what_it_wrote_is_all_read() {
        let (sender, receiver) = channel();
        let folder = std::env::temp_dir();
        let (pty, output) = Pty::spawn_line(&format!("{} /c echo seen&exit 7", command()), &[], &folder, 80, 24, move |code| {
            let _ = sender.send(code);
        })
        .unwrap();
        let reading = read_all(output);
        assert_eq!(receiver.recv_timeout(Duration::from_secs(20)).unwrap(), 7);
        let all = String::from_utf8_lossy(&reading.join().unwrap()).into_owned();
        assert!(all.contains("seen"), "{all:?}");
        drop(pty);
    }

    #[test]
    fn the_shell_starts_in_the_folder_it_is_given_and_with_the_environment_asked_for() {
        let folder = tempfile::tempdir().unwrap();
        let (sender, receiver) = channel();
        let (pty, output) = Pty::spawn_line(&format!("{} /c cd&echo [%DOGMIND_TERM_TEST%]&echo [%TERM_PROGRAM%]&echo [%COLORTERM%]", command()), &[("DOGMIND_TERM_TEST", "zebra42")], folder.path(), 120, 24, move |code| {
            let _ = sender.send(code);
        })
        .unwrap();
        let reading = read_all(output);
        receiver.recv_timeout(Duration::from_secs(20)).unwrap();
        let all = String::from_utf8_lossy(&reading.join().unwrap()).into_owned();
        let canonical = std::fs::canonicalize(folder.path()).unwrap();
        let shown = canonical.display().to_string();
        let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown);
        assert!(all.to_lowercase().contains(&shown.to_lowercase()), "{all:?}");
        assert!(all.contains("[zebra42]") && all.contains("[Dogmind]") && all.contains("[truecolor]"), "{all:?}");
        drop(pty);
    }

    #[test]
    fn what_is_written_to_the_shell_reaches_it() {
        let (sender, receiver) = channel();
        let folder = std::env::temp_dir();
        let (pty, output) = Pty::spawn_line(&format!("{} /q", command()), &[], &folder, 80, 24, move |code| {
            let _ = sender.send(code);
        })
        .unwrap();
        let reading = read_all(output);
        pty.input().send(b"echo typed here\r".to_vec()).unwrap();
        pty.input().send(b"exit 3\r".to_vec()).unwrap();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(20)).unwrap(), 3);
        let all = String::from_utf8_lossy(&reading.join().unwrap()).into_owned();
        assert!(all.contains("typed here"), "{all:?}");
        drop(pty);
    }

    #[test]
    fn a_shell_that_cannot_start_is_an_error_and_not_a_terminal() {
        let folder = std::env::temp_dir();
        assert!(Pty::spawn_line("\"C:\\no\\such\\program.exe\"", &[], &folder, 80, 24, |_| {}).is_err());
    }
}
