//! What the window's frames cost, written down for a test that asks: a debug build with `DOGMIND_TEST_TRACE` set to a file. Nothing of it runs otherwise.
//! A slow message says how long it held the window and how much of that the thread really ran, which tells work from waiting: for a turn of the CPU,
//! for a lock, or for the GPU under GDI.
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// A message held the window this long, in microseconds, or it is not worth a line.
const SLOW_US: u64 = 3000;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThread() -> *mut core::ffi::c_void;
    fn QueryThreadCycleTime(thread: *mut core::ffi::c_void, cycles: *mut u64) -> i32;
}

struct Trace {
    began: Instant,
    /// Cycles of this thread in a microsecond.
    cycles_per_us: f64,
    lines: String,
    file: std::fs::File,
    flushed: Instant,
}

static TRACE: OnceLock<Option<Mutex<Trace>>> = OnceLock::new();
static ON: AtomicBool = AtomicBool::new(false);
/// When the frame thread last posted, and how long its wait for the compositor was, in microseconds from the start.
static POSTED: AtomicU64 = AtomicU64::new(0);
static WAITED: AtomicU64 = AtomicU64::new(0);

fn cycles() -> u64 {
    let mut count = 0;
    // SAFETY: the current thread's pseudo handle, and a place for the answer.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut count) };
    count
}

fn trace() -> Option<&'static Mutex<Trace>> {
    TRACE
        .get_or_init(|| {
            if !cfg!(debug_assertions) {
                return None;
            }
            let path = std::env::var_os("DOGMIND_TEST_TRACE")?;
            let file = std::fs::File::create(path).ok()?;
            let (clock, from) = (Instant::now(), cycles());
            while clock.elapsed().as_micros() < 4000 {
                std::hint::spin_loop();
            }
            let cycles_per_us = (cycles() - from) as f64 / clock.elapsed().as_micros() as f64;
            ON.store(true, Ordering::Relaxed);
            Some(Mutex::new(Trace { began: Instant::now(), cycles_per_us, lines: String::new(), file, flushed: Instant::now() }))
        })
        .as_ref()
}

/// Asks once, at the start, on the window's thread.
pub fn start() {
    let _ = trace();
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Off screen a control is not asked to paint: a test that wants what its paint costs has it print itself, all of it, into memory, each frame.
pub fn prints_too() -> bool {
    static PRINTS: OnceLock<bool> = OnceLock::new();
    on() && *PRINTS.get_or_init(|| std::env::var_os("DOGMIND_TEST_TRACE_PRINT").is_some())
}

fn now_us() -> u64 {
    trace().map_or(0, |trace| trace.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).began.elapsed().as_micros() as u64)
}

/// The frame thread is about to post a frame, having waited this long.
pub fn frame_posted(waited_us: u64) {
    if on() {
        POSTED.store(now_us(), Ordering::Relaxed);
        WAITED.store(waited_us, Ordering::Relaxed);
    }
}

/// A message about to be dispatched.
#[derive(Clone, Copy)]
pub struct Mark {
    at: Instant,
    cycles: u64,
}

pub fn begin() -> Option<Mark> {
    on().then(|| Mark { at: Instant::now(), cycles: cycles() })
}

/// Its end: `who` says whose window it was.
pub fn end(mark: Option<Mark>, message: u32, who: &str) {
    let Some(mark) = mark else { return };
    let spent = mark.at.elapsed().as_micros() as u64;
    if spent < SLOW_US {
        return;
    }
    let ran = cycles() - mark.cycles;
    if let Some(trace) = trace() {
        let mut trace = trace.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let ran = ran as f64 / trace.cycles_per_us;
        let at = trace.began.elapsed().as_micros() as u64 - spent;
        let _ = writeln!(trace.lines, "S {at} {spent} {ran:.0} {message:#x} {who}");
        flush(&mut trace);
    }
}

/// The parts of a frame, each from `begin` to the next.
pub struct Frame {
    at: Instant,
    cycles: u64,
    parts: [u64; 3],
    part_at: Instant,
    posted: u64,
    waited: u64,
    queued: u64,
}

pub fn frame() -> Option<Frame> {
    if !on() {
        return None;
    }
    let now = now_us();
    let posted = POSTED.load(Ordering::Relaxed);
    Some(Frame { at: Instant::now(), cycles: cycles(), parts: [0; 3], part_at: Instant::now(), posted, waited: WAITED.load(Ordering::Relaxed), queued: now.saturating_sub(posted) })
}

impl Frame {
    pub fn part(&mut self, which: usize) {
        self.parts[which] = self.part_at.elapsed().as_micros() as u64;
        self.part_at = Instant::now();
    }

    /// `step`: how far the view moved, in pixels. `chars`: what went in.
    pub fn end(self, step: i32, chars: usize, waiting: usize) {
        let spent = self.at.elapsed().as_micros() as u64;
        let ran = cycles() - self.cycles;
        if let Some(trace) = trace() {
            let mut trace = trace.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let ran = ran as f64 / trace.cycles_per_us;
            let at = trace.began.elapsed().as_micros() as u64 - spent;
            let _ = writeln!(trace.lines, "F {at} {spent} {ran:.0} posted={} waited={} queued={} insert={} scroll={} paint={} step={step} chars={chars} waiting={waiting}", self.posted, self.waited, self.queued, self.parts[0], self.parts[1], self.parts[2]);
            flush(&mut trace);
        }
    }
}

fn flush(trace: &mut Trace) {
    if trace.flushed.elapsed().as_millis() >= 1000 {
        let lines = std::mem::take(&mut trace.lines);
        let _ = trace.file.write_all(lines.as_bytes());
        trace.flushed = Instant::now();
    }
}
