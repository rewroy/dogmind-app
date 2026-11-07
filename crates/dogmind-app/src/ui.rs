//! The window. The app draws its own surface (`paint`) and keeps two system controls where they are worth having: RichEdit
//! for the transcript and the composer, so that IME input, selection and copy are the system's own. No GPU memory goes to
//! drawing. Everything slow happens on other threads, which post a wake message; the UI thread sleeps otherwise.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::os::windows::ffi::OsStrExt;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use dogmind_agent::agent::{self, Approval, Command, Decision, Event, Handle};
use dogmind_agent::anthropic::{self, Anthropic};
use dogmind_agent::background::Waiting;
use dogmind_agent::codex::{self, Codex};
use dogmind_agent::llm::{self, Client, Health, Provider};
use dogmind_agent::message::Message;
use dogmind_agent::prompt::typed_text;
use dogmind_agent::rules;
use dogmind_agent::read_folders;
use dogmind_agent::safety::{AlwaysScope, Mode};
use dogmind_agent::session;
use dogmind_agent::tools::{FileChange, GitInfo};
use dogmind_agent::conversation_key::{self, Cipher, KeyError, Keystore, Pending, Policy};
use dogmind_agent::conversion;
use dogmind_agent::web::{WebSettings, WARNING_MARK};

use crate::design as ds;
use crate::machine::readable_build;
use crate::diffview::{self, RowKind};
use crate::settings::{KeySource, RightView};
use crate::toolrun::{self, LineKind, RunEntry, Tone, ToolRun};
use crate::cards::{self, AnswerState, Card, PlanState};
use crate::notice;
use crate::updates;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::attach;
use crate::drafter;
use crate::files;
use crate::engine::{self, Catalog, CatalogEntry, Engine, Refusal, SetupLine};
use crate::preview::{self, Preview};
use crate::icon;
use crate::paint::*;
use crate::rtf;
use crate::lang::{self, t, tf};
use crate::markdown;
use crate::settings::{self, Kind, ProviderConfig, Settings, TEXT_SIZES};

mod files_panel;
mod term_panel;

const WM_WAKE: u32 = WM_APP + 1;
const WM_PREVIEW_MADE: u32 = WM_APP + 2;
const WM_TRAY: u32 = WM_APP + 3;
const WM_FRAME: u32 = WM_APP + 4;
/// A terminal has written something, or has ended: its id, and 0 or 1.
const WM_TERM: u32 = WM_APP + 6;
const TRAY_BALLOON_CLICK: u32 = WM_USER + 5; // NIN_BALLOONUSERCLICK
const M_TRAY_OPEN: usize = 9101;
const M_TRAY_QUIT: usize = 9102;
const TIMER_ENGINE: usize = 1;
const TIMER_DOTS: usize = 2;

// RichEdit's interface, which windows-sys does not carry (Richedit.h).
const EM_EXGETSEL: u32 = WM_USER + 52;
const EM_EXLIMITTEXT: u32 = WM_USER + 53;
const EM_EXSETSEL: u32 = WM_USER + 55;
const EM_SETBKGNDCOLOR: u32 = WM_USER + 67;
const EM_SETCHARFORMAT: u32 = WM_USER + 68;
const EM_SETPARAFORMAT: u32 = WM_USER + 71;
const EM_FINDTEXTEXW: u32 = WM_USER + 124;
const EM_SETTEXTEX: u32 = WM_USER + 97;
const EM_SETEDITSTYLE: u32 = WM_USER + 204;
const SES_HIDEGRIDLINES: usize = 0x0002_0000;
const ST_SELECTION: u32 = 2;
const WM_IME_STARTCOMPOSITION_: u32 = 0x010D;
const WM_IME_ENDCOMPOSITION_: u32 = 0x010E;
const WM_IME_COMPOSITION_: u32 = 0x010F;
const FR_DOWN: usize = 1;
const EM_SETTEXTMODE: u32 = WM_USER + 89;
const EM_GETSCROLLPOS: u32 = WM_USER + 221;
const EM_SETSCROLLPOS: u32 = WM_USER + 222;
const SCF_SELECTION: u32 = 1;
const SCF_ALL: u32 = 4;
const SCF_ASSOCIATEFONT: u32 = 0x10;
const CFM_CHARSET: u32 = 0x0800_0000;
const CFM_BOLD: u32 = 0x0000_0001;
const CFM_ITALIC: u32 = 0x0000_0002;
const CFM_UNDERLINE: u32 = 0x0000_0004;
const CFM_STRIKEOUT: u32 = 0x0000_0008;
const CFM_BACKCOLOR: u32 = 0x0400_0000;
const CFM_FACE: u32 = 0x2000_0000;
const CFM_COLOR: u32 = 0x4000_0000;
const CFM_SIZE: u32 = 0x8000_0000;
const CFE_BOLD: u32 = 1;
const CFE_ITALIC: u32 = 2;
const CFE_UNDERLINE: u32 = 4;
const CFE_STRIKEOUT: u32 = 8;
const CFE_AUTOBACKCOLOR: u32 = 0x0400_0000;
const PFM_STARTINDENT: u32 = 0x0000_0001;
const PFM_OFFSET: u32 = 0x0000_0004;
const PFM_TABSTOPS: u32 = 0x0000_0010;
const PFM_SPACEBEFORE: u32 = 0x0000_0040;
const PFM_SPACEAFTER: u32 = 0x0000_0080;
const PFM_LINESPACING: u32 = 0x0000_0100;
const TM_PLAINTEXT: u32 = 1;
const TM_MULTILEVELUNDO: u32 = 8;
const TM_MULTICODEPAGE: u32 = 32;
const ES_NOOLEDRAGDROP: u32 = 8;
const ES_DISABLENOSCROLL: u32 = 0x2000;
const SS_NOPREFIX: u32 = 0x80;
const EM_SETEVENTMASK: u32 = WM_USER + 69;
const EM_POSFROMCHAR_RICH: u32 = 0x00D6;
const EM_CHARFROMPOS_RICH: u32 = 0x00D7;
const EM_GETTEXTLENGTHEX: u32 = WM_USER + 95;
#[repr(C)]
struct TextLengthEx {
    flags: u32,
    codepage: u32,
}
/// Characters as the control counts them, for the character messages: GTL_NUMCHARS | GTL_PRECISE, in UTF-16.
static TEXT_LENGTH_CHARS: TextLengthEx = TextLengthEx { flags: 8 | 2, codepage: 1200 };
const ENM_CHANGE: u32 = 1;

#[repr(C, packed(4))]
struct CharFormat {
    size: u32,
    mask: u32,
    effects: u32,
    height: i32,
    offset: i32,
    color: u32,
    char_set: u8,
    pitch_and_family: u8,
    face: [u16; 32],
    weight: u16,
    spacing: i16,
    back_color: u32,
    lcid: u32,
    reserved: u32,
    style: i16,
    kerning: u16,
    underline_type: u8,
    animation: u8,
    rev_author: u8,
    underline_color: u8,
}

#[repr(C, packed(4))]
struct ParaFormat {
    size: u32,
    mask: u32,
    numbering: u16,
    effects: u16,
    start_indent: i32,
    right_indent: i32,
    offset: i32,
    alignment: u16,
    tab_count: i16,
    tabs: [i32; 32],
    space_before: i32,
    space_after: i32,
    line_spacing: i32,
    style: i16,
    line_spacing_rule: u8,
    outline_level: u8,
    shading_weight: u16,
    shading_style: u16,
    numbering_start: u16,
    numbering_style: u16,
    numbering_tab: u16,
    border_space: u16,
    border_width: u16,
    borders: u16,
}

#[repr(C)]
struct CharRange {
    first: i32,
    last: i32,
}

#[repr(C)]
struct SetTextEx {
    flags: u32,
    code_page: u32,
}

#[repr(C)]
struct FindText {
    range: CharRange,
    text: *const u16,
    found: CharRange,
}

// Control ids.
const ID_SEND: isize = 10;
const ID_APPROVE: isize = 11;
const ID_ALWAYS: isize = 12;
const ID_REJECT: isize = 13;
const ID_NEW_SESSION: isize = 14;
const ID_SETTINGS: isize = 15;
const ID_MODEL_CHIP: isize = 16;
const ID_MODE_CHIP: isize = 17;
const ID_EFFORT_CHIP: isize = 18;
const ID_KIND_CHIP: isize = 19;
const ID_BUTTONS: isize = 20; // ..=24
const ID_DRAFTER: isize = 27;
const ID_LIST: isize = 30;
const ID_SIDE: isize = 31;
const ID_HELP: isize = 35;
const ID_SUGGEST: isize = 38;
const ID_TOGGLE_THINKING: isize = 39;
const ID_TOGGLE_SAFE: isize = 40;
const ID_FIND_PREVIOUS: isize = 41;
const ID_FIND_NEXT: isize = 42;
const ID_FIND_CLOSE: isize = 43;
const ID_PANEL_TOGGLE: isize = 44;
const ID_TOGGLE_WEB: isize = 45;
const ID_TOGGLE_COMPACT: isize = 9056;
const ID_TOGGLE_LANGUAGE: isize = 46;
const ID_PREVIEW_RELOAD: isize = 9047;
const ID_PREVIEW_BROWSER: isize = 9048;
const ID_PREVIEW_CLOSE: isize = 9049;
const ID_PROVIDER_UP: isize = 9050;
const ID_FOLDER_CHIP: isize = 9052;
const ID_TOGGLE_DISPLAY: isize = 9054;
const ID_ATTACH: isize = 9055;
const ID_ATTACHED: isize = 9060; // ..+ attach::MOST_IMAGES
/// The list of the Files view of the panel on the right, and the little menu of a row of it.
const ID_FILES: isize = 9070;
const ID_FILES_MENU: isize = 9071;
/// The Terminal view's controls: a tab and its close for each terminal there can be, a new terminal and the shells beside it, the page's own two and the bar's one, and the field a tab is renamed in.
const ID_TERM_TAB: isize = 11000; // ..+ term::MOST_TERMINALS
const ID_TERM_CLOSE: isize = 11010; // ..+ term::MOST_TERMINALS
const ID_TERM_NEW: isize = 11020;
const ID_TERM_SHELLS: isize = 11021;
const ID_TERM_START: isize = 11022;
const ID_TERM_START_SHELLS: isize = 11023;
const ID_TERM_AGAIN: isize = 11024;
const ID_TERM_RENAME: isize = 11025;
/// How many pictures of a transcript are drawn at once: more than any view holds, and fewer than the small copies that are kept.
const PICTURES_IN_VIEW: usize = 48;
/// How many small copies are kept: a third of a megabyte at most for one.
const THUMBS_KEPT: usize = 64;
const M_DISPLAY: usize = 9310; // ..+ 3
const M_THEME: usize = 9320; // ..+ 3
const M_ANSWERS: usize = 9330; // ..+ 3
const M_WEB_CHOICE: usize = 9350; // ..+ 2
const M_COMPACT_CHOICE: usize = 9355; // ..+ 2
const DISPLAY_LANGUAGES: [(&str, &str); 3] = [("system", "As Windows is set"), ("en", "English"), ("ja", "日本語")];
const THEMES: [(&str, &str); 3] = [("system", "As Windows is set"), ("dark", "Dark"), ("light", "Light")];
const ANSWER_LANGUAGES: [(&str, &str); 3] = [("auto", "As you write: Japanese on this system"), ("ja", "Japanese, always"), ("en", "English, always")];
const M_FOLDER_CHOOSE: usize = 9199;
const M_FOLDER_RECENT: usize = 9200; // ..+ FOLDERS_SHOWN
const FOLDERS_SHOWN: usize = 12;
const M_EDIT_CUT: usize = 9301;
const M_EDIT_COPY: usize = 9302;
const M_EDIT_PASTE: usize = 9303;
const M_EDIT_ALL: usize = 9304;
const ID_PROVIDER_DOWN: isize = 9051;
/// Hidden controls of no size, one for each icon over the transcript that is in view: they are what Tab reaches and what a screen reader names, and an icon is pressed through them
/// with Enter or Space as it is with the pointer. The icons themselves are drawn in the transcript's own paint (`paint_overlays`).
const ID_PROXY_POOL: isize = 300; // ..300 + PROXY_POOL
const PROXY_POOL: usize = 40;
const ID_MODAL_CLOSE: isize = 9600;
const ID_NAV: isize = 9601; // ..+ NAV_POOL
const NAV_POOL: usize = 11;
const ID_THEME_SEGMENT: isize = 9640; // ..+ 3 (past ID_NAV's pool)
const ID_TEXT_SIZE: isize = 9630; // ..+ TEXT_SIZES
const ID_SKILL_TOGGLE: isize = 10000; // ..+ as many skills as there are
const ID_MEMORY_REMOVE: isize = 10400; // ..+ as many notes as there are
const ID_MODE_SELECT: isize = 9614;
const ID_KEY_REPLACE: isize = 9615;
const ID_KEY_REMOVE: isize = 9616;
const ID_ADVANCED: isize = 9617;
const ID_VARIABLE_TOGGLE: isize = 9618;
const ID_MCP_IMPORT: isize = 9619;
const ID_SETUP_AGAIN: isize = 9620;
const ID_REMOVE_ALL: isize = 9621;
const ID_MODEL_LICENCE: isize = 9622;
const ID_MCP_TRUST: isize = 9700; // ..+ as many servers as there are
const ID_VISION: isize = 9903;
const ID_VISION_DELETE: isize = 9904;
const ID_VISION_OFFER: isize = 9905;
const ID_UPDATE_OFFER: isize = 9906;
const ID_RELEASE_OFFER: isize = 9907;
const ID_TOGGLE_UPDATES: isize = 9908;
const ID_TOGGLE_REMEMBER: isize = 9912;
const ID_SPEED_FIRST: isize = 9909;
const ID_NOTICE_LATER: isize = 9910;
const ID_GOAL_CLEAR: isize = 9911;
const ID_DIFF_PREVIOUS: isize = 9900;
const ID_DIFF_NEXT: isize = 9901;
const ID_DIFF_CLOSE: isize = 9902;
const ID_READ_FOLDER_ADD: isize = 9650;
const ID_READ_FOLDER_STOP: isize = 9651;
const ID_READ_FOLDER_REMOVE: isize = 9800; // ..+ as many folders as there are
/// What the cards of the transcript put over it as windows of their own (a choice, a file, the button of a field): a pool that goes round, as the pictures' does.
const TIMER_COPIED: usize = 3;
/// The hidden controls are made to say what is in view once the view has stood still a moment.
const TIMER_PROXIES: usize = 21;
/// Posted by a hidden control that gains or loses the focus: the icon it stands for shows the ring, or does not.
pub(crate) const WM_PROXY_FOCUS: u32 = WM_APP + 9;
/// The id of the one tip that follows the icon the pointer is on.
const OVERLAY_TIP: usize = usize::MAX - 200;
const TIMER_SEARCH: usize = 4;
const TIMER_CHANGES: usize = 5;
const TIMER_REFLOW: usize = 6;
const TIMER_RECONNECT: usize = 7;
/// Whether a new build has come in over the app or the engine it runs.
const TIMER_UPDATE: usize = 8;
/// Whether it is time to ask GitHub for a newer release: the first time a little after the start, then once an hour (it asks only when six hours have gone by since the last
/// answer; and when the window comes back from the tray, an hour: Self::tray_open).
const TIMER_RELEASES: usize = 9;
const FIRST_RELEASE_LOOK_MS: u32 = 30_000;
const RELEASE_LOOK_MS: u32 = 60 * 60 * 1000;
/// The conversation's width below which the panel on the right gives way, at 96 dpi.
const RIGHT_NEEDS: i32 = 480;
/// The panel on the right is no wider than this while it shows a terminal.
const TERMINAL_PANEL_MOST: i32 = 720;
/// How many commands, and as many waits, the panel on the right lists of what waits in the background: a session has five waits at most, and few commands.
const WAITING_LISTED: usize = 6;
/// How often a server that cannot be reached is tried again, while something waits to be sent to it.
const RECONNECT_MS: u32 = 5000;
/// How often the ring turns while there is work.
const SPINNER_MS: u32 = 33;
/// How often text goes into the transcript as an answer comes. The view moves on every frame; to insert costs a millisecond or more, and
/// nobody reads the difference between thirty insertions a second and more of them.
const REVEAL_EVERY_MS: u128 = 30;
/// The least time between frames: a screen faster than this is served every other time it refreshes.
const FRAME_LEAST: std::time::Duration = std::time::Duration::from_micros(6500);
/// How long frames wait where there is no composition to wait for.
const FRAME_ALONE: std::time::Duration = std::time::Duration::from_millis(16);
/// Room a following view keeps for the last line, from that line's top.
const LAST_LINE: i32 = 26;
/// How long a turn of the wheel can take to move the view away from the end.
const SCROLL_UP_TAKES: std::time::Duration = std::time::Duration::from_millis(500);
/// How quickly a following view closes on the end: about the time it takes to get most of the way, in seconds.
const FOLLOW_TIME: f32 = 0.2;
/// The slowest a following view moves, in pixels a second at 96 dpi: it arrives, rather than creeping for ever.
const FOLLOW_LEAST: i32 = 30;

/// What the frames of an answer coming in are asked for with, and what keeps them from piling up on a busy window.
struct FrameClock {
    wanted: AtomicBool,
    posted: AtomicBool,
}

/// One frame each time the screen is composed, while they are wanted. A timer cannot give this: SetTimer's ticks fall on the system's
/// 15.6 ms and never on the screen's, and restarting one with each insertion made them 15, 30 or 60 ms apart, which is what the eye saw.
fn frame_loop(window: isize, clock: Arc<FrameClock>) {
    // It only waits and posts, and a frame late is what the eye sees. Not the engine's threads' to hold up, when they have every core.
    // SAFETY: the current thread's pseudo handle.
    unsafe { windows_sys::Win32::System::Threading::SetThreadPriority(windows_sys::Win32::System::Threading::GetCurrentThread(), windows_sys::Win32::System::Threading::THREAD_PRIORITY_HIGHEST) };
    let mut last = std::time::Instant::now();
    loop {
        while !clock.wanted.load(Ordering::Relaxed) {
            std::thread::park();
        }
        let began = std::time::Instant::now();
        // SAFETY: it takes nothing and only waits.
        let flushed = unsafe { DwmFlush() } == 0;
        let spent = began.elapsed();
        // (with nothing to compose, or no composition, it returns at once)
        if !flushed || spent < FRAME_LEAST / 2 {
            std::thread::sleep(FRAME_ALONE.saturating_sub(spent));
        }
        if last.elapsed() < FRAME_LEAST {
            continue;
        }
        last = std::time::Instant::now();
        if !clock.posted.swap(true, Ordering::Relaxed) {
            crate::frametrace::frame_posted(spent.as_micros() as u64);
            // SAFETY: posting to a window that has gone does nothing, and says so.
            if unsafe { PostMessageW(window as HWND, WM_FRAME, 0, 0) } == 0 {
                clock.posted.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// How long what has come takes to show, at most about: tokens come in bursts, and this much behind is not seen, while an even flow is.
const REVEAL_BEHIND: f32 = 0.4;
/// How quickly the flow takes up a change in what is coming, in seconds.
const REVEAL_EASE: f32 = 0.3;
/// The slowest it goes, in characters a second: the last of an answer does not creep.
const REVEAL_LEAST: f32 = 20.0;

/// One insertion, `seconds` after the last: how many of `waiting` characters show, the speed it goes on at, and the part of a character carried over.
/// The speed follows what is waiting, smoothly: a share of what waits at each frame ran out fast after each burst and then crawled, which read as a stutter of the words.
fn reveal_step(waiting: usize, rate: f32, carried: f32, seconds: f32) -> (usize, f32, f32) {
    if waiting == 0 {
        return (0, rate * (-seconds / REVEAL_EASE).exp(), 0.0);
    }
    let wanted = (waiting as f32 / REVEAL_BEHIND).max(REVEAL_LEAST);
    let rate = wanted + (rate - wanted) * (-seconds / REVEAL_EASE).exp();
    let reach = rate * seconds + carried;
    // (and no more than goes in within a frame: a burst of a hundred thousand held the window for a fifth of a second)
    let count = (reach as usize).min(waiting).min(400);
    (count, rate, if count == waiting { 0.0 } else { (reach - count as f32).min(4.0) })
}
/// A scrollbar thumb's height for `page` of `range`, in a track of `track`.
fn thumb_height(track: i32, page: i32, range: i32, least: i32) -> i32 {
    let share = (i64::from(track) * i64::from(page) / i64::from(range.max(1))) as i32;
    share.clamp(least.min(track), track)
}
/// How far down a thumb is, with `room` to move in, at `position` of `extent`.
fn thumb_offset(room: i32, position: i32, extent: i32) -> i32 {
    (i64::from(room) * i64::from(position.clamp(0, extent.max(1))) / i64::from(extent.max(1))) as i32
}
/// One frame of a view following the end: the whole pixels it moves of the `behind` it has to go, the speed it goes on at, and the part of a
/// pixel it carries to the next frame. A critically damped spring, right for a frame of any length: it gathers speed and loses it, so that
/// lines arriving one after another make one even glide and not a jerk and a stop for each of them.
fn follow_step(behind: i32, speed: f32, carried: f32, seconds: f32, least: f32) -> (i32, f32, f32) {
    if behind <= 0 {
        return (0, 0.0, 0.0);
    }
    let distance = behind as f32;
    let omega = 2.0 / FOLLOW_TIME;
    let x = omega * seconds;
    let decay = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x);
    let pull = (speed - omega * distance) * seconds;
    let moved = distance + (pull - distance) * decay;
    let speed = ((speed - omega * pull) * decay).max(0.0);
    let reach = moved.max(least * seconds) + carried;
    let step = (reach as i32).clamp(0, behind);
    (step, speed, if step == behind { 0.0 } else { reach - step as f32 })
}

// Menu ids.
const M_PROVIDERS: usize = 110;
const M_MODELS: usize = 111;
const M_RESTART: usize = 112;
const M_LOG: usize = 113;
const M_COMPACT: usize = 114;
const M_COPY: usize = 115;
const M_THINKING: usize = 116;
const M_SAFE: usize = 117;
const M_MODE: usize = 120; // ..=123
const M_EFFORT: usize = 130; // ..=135
const M_KIND: usize = 140; // ..=143
const M_OPEN_SESSION: usize = 150;
const M_RENAME_SESSION: usize = 151;
const M_DELETE_SESSION: usize = 152;
const M_NEW_THERE: usize = 153;
const M_PIN_SESSION: usize = 154;
const M_PROVIDER: usize = 200; // and on
const M_MODEL_CHOICE: usize = 9500; // and on

/// What `/` offers, in this order.
const COMMANDS: [(&str, &str); 47] = [
    ("/setup", "Go through the first setup again"),
    ("/report", "Write what can be sent about a problem"),
    ("/help", "How to use this app"),
    ("/status", "Version, model, folder and connection"),
    ("/about", "Which versions these are"),
    ("/new", "A new session in this folder"),
    ("/clear", "The same as /new"),
    ("/resume", "Open another session of this folder"),
    ("/find", "Find in this conversation: /find words"),
    ("/search", "Search all sessions: /search words"),
    ("/rename", "Name this session"),
    ("/delete", "Delete this session"),
    ("/pin", "Pin this session to the top of the sidebar, or unpin it"),
    ("/compact", "Sum up the conversation to make room"),
    ("/retry", "Ask the last request again"),
    ("/rewind", "Go back to an earlier turn: its files, the conversation, or both"),
    ("/copy", "Copy the last answer"),
    ("/attach", "Add a picture to the next message: /attach file, or choose one"),
    ("/diff", "What changed in the folder, file by file"),
    ("/usage", "Tokens and speed of this session"),
    ("/trajectory", "Where this session's time went: each turn's requests and tools"),
    ("/goal", "What this session works toward, kept across turns: /goal text, /goal clear"),
    ("/init", "Have the model write this project's DOGMIND.md"),
    ("/model", "Change how the model runs: /model Claude"),
    ("/effort", "How hard to think: none low medium high xhigh/max default"),
    ("/mode", "When to ask: ask edits auto plan"),
    ("/plan", "Look and plan first, and change nothing until you say"),
    ("/skills", "The skills of this folder, from Claude's and Codex's folders too"),
    ("/memory", "What is kept between sessions"),
    ("/mcp", "MCP servers: /mcp shows them, /mcp import takes Claude's and Codex's"),
    ("/permissions", "What may always run in this folder"),
    ("/think", "Show or hide the thinking"),
    ("/tools", "Open another program's folder on PATH to sandboxed commands, with one Windows prompt"),
    ("/instructions", "How the agent should talk and work, for every new session"),
    ("/dashboard", "Open Straydog's dashboard in the browser"),
    ("/panel", "Show or hide the panel on the right"),
    ("/preview", "Show a web page or picture of the folder beside the conversation"),
    ("/files", "Show the files of the folder in the panel on the right"),
    ("/terminal", "Show a terminal in the panel on the right, or open one"),
    ("/theme", "As Windows is set, dark, or light: the next of them"),
    ("/safe", "Allow safe commands without asking, or not"),
    ("/workspace", "Start a session in another folder"),
    ("/providers", "Settings: models and connections"),
    ("/models", "Settings: models on this PC"),
    ("/settings", "Settings: general"),
    ("/log", "Open the engine's log"),
    ("/quit", "Close the app"),
];
const HELP: &str = "# Getting started
Write what you want done. Enter sends it, and the model asks before it changes anything.
A session goes on working when you open another.

/\tcommands
@\ta file of this folder
Ctrl+V\ta picture or a file

# Keys
Enter\tsend; while it works, before its next step
Ctrl+Enter\tsend after the work under way
Shift+Enter\ta new line
Esc\tstop, reject, or close
Ctrl+K\tcommands, from anywhere
Ctrl+/\tthese keys
Ctrl+N\ta new session
Ctrl+Tab\tthe next session
Ctrl+1 to 9\tthat session of the sidebar
Ctrl+,\tsettings
Ctrl+Shift+C\tcopy the last answer
Ctrl+B\tthe panel on the right
Ctrl+`\ta new terminal in it
F6\tleave the terminal, for the message
Ctrl+F\tfind in this conversation
Ctrl+Shift+F\tsearch all sessions
F2\tname this session, or the terminal tab that has the keys
Y / A / N\tapprove, always, reject
N / P\tnext and previous change, in Changes

# Commands
{commands}

# Approvals
Plan first changes nothing. Ask first asks each time. Accept edits changes files without asking. Auto also runs commands, but never what is high risk, outside the folder or wants the network.
Always saves a rule for this folder, and no rule reaches those. Settings lists the rules.
A command can write only in the session's folder, and has no network unless you approve that.

# Going back
/rewind goes back to the beginning of an earlier turn: its files, the conversation, or both. Only the agent's own edits come back. What it overwrites is saved first.

# More
Keys you paste go to Windows Credential Manager, and nowhere else.
Pictures: the paperclip, /attach, a paste or a drop. PNG, JPEG, WebP and GIF.
Skills: .claude/skills, .agents/skills, .codex/skills and .dogmind/skills, in the project and in your home. /name starts one.
MCP servers: mcp.json in the data folder. They start with the app, and each call asks. Settings > Tools can trust what one returns.
Web: what comes back is marked as data, and once it is in a conversation every command asks.
Your instructions (Settings) go for every session; a DOGMIND.md for its project. /init has the model write one.
Terminal: the panel on the right has your own shell. Its commands run with your rights, apart from the agent's, and what it shows is not sent to the model. Every key goes to the shell: Ctrl+C copies when something is selected and otherwise stops what runs, and Ctrl+V pastes.
";
const INIT_PROMPT: &str = "Look through this project and write a DOGMIND.md at its root for agents who work here: what the project is, how to build and test it, its layout and its conventions. Keep it short, and only what you have checked.";

const EFFORTS: [(Option<&str>, &str); 6] = [(None, "Default"), (Some("none"), "None"), (Some("low"), "Low"), (Some("medium"), "Medium"), (Some("high"), "High"), (Some("xhigh"), "Extra high")];
/// What the highest effort is called for a DeepSeek model: its template names that level max. What is sent stays xhigh.
const DEEPSEEK_HIGHEST: &str = "Max";
const MODES: [(Mode, &str); 4] = [(Mode::Default, "Ask first"), (Mode::AcceptEdits, "Accept edits"), (Mode::Auto, "Auto"), (Mode::Plan, "Plan first")];

/// Work going on through a folder's tree.
struct ReadFolderWork {
    adding: bool,
    folder: PathBuf,
    count: u64,
    cancel: Arc<AtomicBool>,
    can_stop: bool,
    stopping: bool,
}

impl ReadFolderWork {
    fn words(&self) -> String {
        if self.stopping {
            return t("Stopping, and undoing what was done...").to_string();
        }
        tf(if self.adding { "Opening {0}: {1} so far" } else { "Taking back {0}: {1} so far" }, &[&read_folder_name(&self.folder), &self.count])
    }
}

/// Tells the window how far work has come, a few times a second however fast it goes.
struct ReadFolderTeller<'a> {
    poster: &'a Poster,
    told: std::time::Instant,
}

impl<'a> ReadFolderTeller<'a> {
    fn new(poster: &'a Poster) -> Self {
        ReadFolderTeller { poster, told: std::time::Instant::now() }
    }

    fn tell(&mut self, count: u64) {
        if self.told.elapsed() >= std::time::Duration::from_millis(200) {
            self.told = std::time::Instant::now();
            self.poster.send(Note::ReadFolderProgress(count));
        }
    }
}

fn read_folder_name(folder: &Path) -> String {
    folder.file_name().map_or_else(|| folder.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// A path that fits a line, its middle given up: the start says where, and the end which.
fn shown_path(folder: &Path) -> String {
    const MOST: usize = 76;
    let text = folder.display().to_string();
    let count = text.chars().count();
    if count <= MOST {
        return text;
    }
    let (head, tail): (String, String) = (text.chars().take(MOST / 3).collect(), text.chars().skip(count - MOST * 2 / 3).collect());
    format!("{head}...{tail}")
}

/// What the page says of a folder besides its name, if there is anything to say.
fn read_folder_detail(folder: &read_folders::Folder) -> Option<String> {
    let mut parts = Vec::new();
    if folder.state != read_folders::State::Open {
        parts.push(t("Not finished: this is put right when the app next starts.").to_string());
    }
    if folder.held_out() > 0 {
        parts.push(tf("{0} that looked like keys when it was added are left out.", &[&folder.held_out()]));
    }
    if folder.not_changed > 0 {
        parts.push(tf("{0} could not be changed, so commands cannot read those.", &[&folder.not_changed]));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// What to say of an adding, and whether that is a problem.
fn read_folder_added_words(folder: &Path, result: &Result<read_folders::Added, String>) -> (String, bool) {
    match result {
        Ok(read_folders::Added::Open { held_out, .. }) if *held_out > 0 => (tf("Commands can now read {0}. {1} that look like keys were left out, as they are now: one made later can be read.", &[&read_folder_name(folder), held_out]), false),
        Ok(read_folders::Added::Open { .. }) => (tf("Commands can now read {0}.", &[&read_folder_name(folder)]), false),
        Ok(read_folders::Added::Stopped) => (t("Stopped, and all of it undone.").to_string(), false),
        Err(error) => (lang::notice(error), true),
    }
}

/// What other threads have to tell the window, besides the agent's events.
/// The places "Always" would allow.
fn always_places(scope: &Option<AlwaysScope>) -> String {
    match scope {
        Some(AlwaysScope::File(file)) => file.display().to_string(),
        Some(AlwaysScope::Folders(folders)) => folders.iter().map(|folder| folder.display().to_string()).collect::<Vec<_>>().join(", "),
        Some(AlwaysScope::Network(reach)) => reach.clone(),
        None => String::new(),
    }
}

/// The files the settings name as holding keys: reading one asks, as for any place secrets are kept.
fn key_files(settings: &Settings) -> Vec<PathBuf> {
    settings.providers.iter().map(|provider| provider.api_key.trim()).chain([settings.search_key.trim()])
        .filter(|key| matches!(KeySource::of(key), KeySource::File(_))).map(PathBuf::from).collect()
}

/// Whether a model is one of DeepSeek's, told by its name.
fn is_deepseek(model: &str) -> bool {
    model.to_ascii_lowercase().contains("deepseek")
}

/// The efforts as a model's own words name them: for DeepSeek the highest is Max, for the rest Extra high.
fn efforts_for(model: &str) -> [(Option<&'static str>, &'static str); 6] {
    let mut efforts = EFFORTS;
    if is_deepseek(model) {
        for (known, name) in efforts.iter_mut() {
            if *known == Some("xhigh") {
                *name = DEEPSEEK_HIGHEST;
            }
        }
    }
    efforts
}

/// The model the efforts are named for: the active provider's setting, or, where that is empty, the one the connection settled on.
fn model_of_effort<'a>(settings: &'a Settings, connected: &'a str) -> &'a str {
    settings.active_provider().map(|provider| provider.model.as_str()).filter(|model| !model.is_empty()).unwrap_or(connected)
}

/// Which effort a word asks for: the one the engine takes, its English name, or Max / 最大, which are xhigh for any model.
fn effort_asked_for(word: &str) -> Option<usize> {
    let wanted = word.trim().to_lowercase();
    let wanted = if wanted == "max" || wanted == "最大" { "xhigh" } else { wanted.as_str() };
    EFFORTS.iter().position(|(known, name)| known.map_or(wanted == "default", |known| known == wanted) || name.eq_ignore_ascii_case(wanted))
}

/// The model chip's menu: the providers, then the models the active one offers, if it says, then the way to the settings.
fn model_chip_items(providers: &[ProviderConfig], active: &str, models: &[String], model: &str) -> Vec<(usize, String, bool)> {
    let mut items: Vec<(usize, String, bool)> = providers.iter().enumerate().map(|(at, provider)| (M_PROVIDER + at, provider.name.clone(), provider.name == active)).collect();
    if !models.is_empty() {
        items.push((0, String::new(), false));
        items.extend(models.iter().enumerate().map(|(at, known)| (M_MODEL_CHOICE + at, known.clone(), known == model)));
    }
    items.push((0, String::new(), false));
    items.push((M_PROVIDERS, t("Connections...").into(), false));
    items
}

/// What can be done about a picture that cannot be read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VisionOffer {
    /// The file is not there: this many bytes.
    Download(u64),
    /// It is there, set aside.
    TurnOn,
}

/// Where the update to a newer release is, on a copy the installer put where it is.
enum ReleaseUpdate {
    None,
    /// Its setup is on its way, fetched by a thread: how far, and what stops it.
    Fetching { version: String, done: u64, total: u64, stop: Arc<AtomicBool> },
    /// Its setup is here and checked, and waits for the person to restart into it.
    Ready(updates::Ready),
}

/// What About says when the setup of an update was not fetched, and whether that is a trouble.
fn fetch_words(failed: &updates::Failed) -> (String, bool) {
    use updates::Failed;
    match failed {
        Failed::Cancelled => (t("The update download was cancelled, and its partial file deleted. The next automatic look tries again.").to_string(), false),
        Failed::Metered => (t("The update was not downloaded: this connection is metered. Check for updates downloads it anyway.").to_string(), false),
        Failed::NotReached => (t("The update could not be downloaded: GitHub could not be reached. Its partial file is deleted, and the next automatic look tries again.").to_string(), true),
        Failed::Refused(status) => (tf("The update could not be downloaded: GitHub answered with an error ({0}). The next automatic look tries again.", &[status]), true),
        Failed::NotAllowed => (t("The update was not downloaded: its address is not one Dogmind accepts. Nothing was changed.").to_string(), true),
        Failed::DidNotMatch => (t("The download did not match the size and hash GitHub published, so it was deleted. Nothing was run or changed. The next automatic look tries again.").to_string(), true),
        Failed::Disk(why) => (tf("The update could not be saved: {0}. Its partial file is deleted, and the next automatic look tries again.", &[why]), true),
    }
}

enum Note {
    /// What fetching a vision file for this model would take; `None`: there is no such file for it.
    VisionSize(Option<u64>),
    /// A vision file set aside or brought back, or why not.
    VisionSwitched(Result<bool, String>),
    /// A key an older version left in the file, which Credential Manager would not keep: said once, at the start.
    KeyNotKept(String),
    /// What `straydog --version` said, if it said anything.
    EngineVersion(Option<String>, Option<u64>),
    /// A look at GitHub's latest release is over: whether the app made it on its own, and what it came to.
    ReleaseLooked { on_its_own: bool, outcome: updates::Outcome },
    /// How far the setup of a newer release has come, of how many bytes.
    ReleaseProgress { done: u64, total: u64 },
    /// The setup of a newer release is fetched and checked and waits to be run, or why it is not.
    ReleaseFetched(Result<updates::Ready, updates::Failed>),
    /// The setup that waits was read again through a handle no one else can write through, before it is run: that handle with it, to be held while it starts, or why it is not run.
    SetupChecked(updates::Ready, Result<std::fs::File, updates::NotRun>),
    Status(u64, String),
    /// The models the provider just connected to offers.
    Models(u64, Vec<String>),
    EngineStarted(u64, Engine),
    Connected { generation: u64, provider: Arc<dyn Provider>, model: String, window: u64, straydog: bool, vision: Option<bool> },
    ConnectFailed(u64, String),
    Catalog(Result<Catalog, String>),
    /// The report is written, or why not.
    Report(Result<PathBuf, String>),
    /// What was left on this PC is gone, but for what this says.
    Removed(String),
    Sessions(Vec<session::Summary>),
    Workspace { path: PathBuf, files: Vec<String>, branch: Option<String>, skills: Vec<dogmind_agent::skills::Skill> },
    /// For this workspace: there may be another by the time it comes.
    Changes(PathBuf, Result<Vec<FileChange>, String>),
    /// A folder of the Files view has been read (`path`: from the workspace, with `/`), or why not.
    FolderRead { root: PathBuf, path: String, result: Result<Vec<files::Entry>, String> },
    /// The disk says these folders of the workspace have changed.
    WorkspaceChanged { root: PathBuf, changed: files::Changed },
    Found(u64, Vec<session::Hit>),
    Git(PathBuf, Option<GitInfo>),
    Setup(SetupLine),
    SetupDone(Result<(), String>),
    /// A file has been looked over, to go with the next message.
    Attached { path: PathBuf, result: Result<dogmind_agent::images::Attachment, String> },
    /// A small copy of a picture, or why there is none.
    Thumb { path: PathBuf, room: (i32, i32), decoded: Result<attach::Decoded, attach::Undecoded> },
    /// How far the work on a folder commands can read has come.
    ReadFolderProgress(u64),
    /// It has ended, with what to say of it, and whether that is a problem.
    ReadFolderDone(String, bool),
    /// How many conversations and notes are changed, of how many.
    CryptProgress(usize, usize),
    CryptDone(CryptOutcome),
}

/// A picture waiting to go: the agent has looked it over and holds its bytes, and the file is what is shown of it.
#[derive(Clone, PartialEq, Debug)]
struct Attached {
    path: PathBuf,
    picture: dogmind_agent::images::Attachment,
}

/// A message on its way: what goes to the model, the pictures that go with it, and what its card says when the turn reaches it.
#[derive(Clone, Default, PartialEq, Debug)]
struct Outgoing {
    text: String,
    images: Vec<Attached>,
    card: String,
}

impl Outgoing {
    /// Words of the person's, whose card says just that.
    fn words(text: String) -> Outgoing {
        Outgoing { card: text.clone(), text, images: Vec::new() }
    }
}

/// Now, as a message's time reads beside it.
fn clock_now() -> String {
    let now = dogmind_agent::message::written_now();
    dogmind_agent::message::clock_words(&now, &now)
}

/// When a message of a saved session was written, as it reads beside it: its own time, or for one from before messages kept it, the time its
/// first line gives. Nothing when neither says.
fn written_words(message: &Message) -> String {
    let today = dogmind_agent::message::written_now();
    let at = message.at.as_deref().or_else(|| message.content.as_deref().and_then(dogmind_agent::prompt::said_at));
    at.map_or(String::new(), |at| dogmind_agent::message::clock_words(at, &today))
}

/// A message of the person's, as the rail of turns beside the transcript knows it: where it begins, and its first words.
struct TurnMark {
    start: i32,
    prompt: String,
}

/// The rail's measures, at 100%: from one mark to the next, the room above the first and under the last, its width, and a mark's length,
/// as one is and as the one being read is.
const RAIL_PITCH: i32 = 10;
const RAIL_INSET: i32 = 6;
const RAIL_WIDTH: i32 = 26;
const RAIL_MARK: i32 = 10;
const RAIL_MARK_READ: i32 = 18;

/// Words for a tip on the rail: on one line, at most `most` letters, the marks markdown writes left out.
fn rail_words(text: &str, most: usize) -> String {
    let line = text.replace(['#', '*', '`', '>', '|'], "").split_whitespace().collect::<Vec<_>>().join(" ");
    let mut words: String = line.chars().take(most).collect();
    if line.chars().count() > most {
        words.push('…');
    }
    words
}

/// Where the marks of `count` turns stand in a rail `tall` high: each one's centre from the rail's top, for those in it. A ladder longer
/// than the rail is moved so that the turn being read stands in its middle, and the ends of it are past the rail's.
fn rail_marks(count: usize, reading: usize, tall: i32, pitch: i32, inset: i32) -> Vec<(usize, i32)> {
    let whole = count as i32 * pitch + 2 * inset;
    let shift = if whole <= tall { 0 } else { (inset + reading as i32 * pitch + pitch / 2 - tall / 2).clamp(0, whole - tall) };
    (0..count).filter_map(|at| {
        let y = inset + at as i32 * pitch + pitch / 2 - shift;
        (y >= pitch / 2 && y <= tall - pitch / 2).then_some((at, y))
    }).collect()
}

/// What a button on a message waiting above the input does.
#[derive(Clone, Copy, Debug, PartialEq)]
enum QueueAct {
    /// Into the work under way, at its next step, rather than after it.
    Now,
    /// Back to the box, to be changed and sent again.
    Edit,
    /// Gone: it is not sent.
    Drop,
}

impl QueueAct {
    fn icon(self) -> &'static str {
        match self {
            QueueAct::Now => "arrow-up",
            QueueAct::Edit => "pencil",
            QueueAct::Drop => "x",
        }
    }

    fn tip(self) -> &'static str {
        match self {
            QueueAct::Now => "Send it into this work now",
            QueueAct::Edit => "Edit",
            QueueAct::Drop => "Discard",
        }
    }
}

/// The buttons a waiting message has, from the left: one that waits for the work under way to end, and is words only, can go into it now.
fn queue_acts(can_go_now: bool) -> &'static [QueueAct] {
    if can_go_now { &[QueueAct::Now, QueueAct::Edit, QueueAct::Drop] } else { &[QueueAct::Edit, QueueAct::Drop] }
}

/// How a message waiting above the input reads: its first line, and its pictures if any.
fn waiting_words(text: &str, pictures: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    match (line.is_empty(), pictures) {
        (_, 0) => line.to_string(),
        (true, count) => tf("{0} pictures", &[&count]),
        (false, count) => format!("{line}  ·  {}", tf("{0} pictures", &[&count])),
    }
}

/// A block of code in the transcript: where it begins, once that has been read past what the control keeps at a row's start, and what is in it.
struct CodeBlock {
    at: i32,
    found: bool,
    text: String,
}

/// The icon at a point of the transcript, among those drawn in its last paint; the first drawn where two overlap.
fn icon_under(hits: &[OverlayHit], x: i32, y: i32) -> Option<OverlayKey> {
    hits.iter().find(|hit| x >= hit.area.left && x < hit.area.right && y >= hit.area.top && y < hit.area.bottom).map(|hit| hit.key)
}

/// Where a message's picture stands in the transcript: the place of its room's first character is the top of the row, and the card keeps its padding there. `offset`: the picture's
/// place among those of its message (`attach::Tiles::at`); `size`: a tile's.
fn picture_area(room: POINT, offset: (i32, i32), size: (i32, i32), dpi: u32) -> RECT {
    rect(room.x + offset.0, room.y + offset.1 + scale(dpi, 7), size.0, size.1)
}

/// Whether any of a tile is in a view `height` tall.
fn tile_is_in_view(area: &RECT, height: i32) -> bool {
    area.bottom > 0 && area.top < height
}

/// A file's name as it is said: its last part, or the whole path where it has none.
fn file_name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// What a reader says of a control of a card: a choice's number and words, a file with its lines, an icon by its tip, the rest by what is written on it.
fn card_part_words(part: &CardPart) -> String {
    let mut pieces = part.text.split(PART);
    match part.kind {
        ButtonKind::Choice => {
            let (number, _suggested, name, meaning) = (pieces.next().unwrap_or(""), pieces.next().unwrap_or(""), pieces.next().unwrap_or(""), pieces.next().unwrap_or(""));
            if meaning.is_empty() { format!("{number}. {name}") } else { format!("{number}. {name}: {meaning}") }
        }
        ButtonKind::FileRow => {
            let (added, removed) = (pieces.next().unwrap_or("0"), pieces.next().unwrap_or("0"));
            let path = pieces.nth(2).unwrap_or("");
            format!("{path}: +{added} \u{2212}{removed}")
        }
        _ if !part.tip.is_empty() => part.tip.clone(),
        _ => part.text.split_once('|').map_or(part.text.clone(), |(_, label)| label.to_string()),
    }
}

/// The order Tab goes to the icons in, which the hidden controls are put in: top to bottom and left to right, as the page's controls are, and no more than the controls there are.
fn proxy_order(hits: &[OverlayHit]) -> Vec<OverlayKey> {
    let mut order: Vec<(i32, i32, OverlayKey)> = hits.iter().map(|hit| (hit.area.top, hit.area.left, hit.key)).collect();
    order.sort_by_key(|(top, left, _)| (*top, *left));
    order.into_iter().take(PROXY_POOL).map(|(_, _, key)| key).collect()
}

/// An icon or control drawn in the transcript's paint, over its text: which answer, block, run or card it is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OverlayKey {
    /// Copies the answer.
    Copy(usize),
    /// Branches the conversation from the answer.
    Fork(usize),
    /// Copies the block of code.
    Block(usize),
    /// Opens or closes the row of calls.
    Run(usize),
    /// A control of a card: the card's room (`card_slots`) and the control's place across its row.
    Card(usize, usize),
    /// A picture of a message or an answer (`picture_blocks`) and its place among that message's pictures: opened beside the conversation.
    Picture(usize, usize),
}

/// Where an icon was drawn the last time the transcript was painted, in the transcript's own pixels.
struct OverlayHit {
    area: RECT,
    key: OverlayKey,
}

/// The widths of the three chips in the foot of the composer, in pixels. The mode's and the effort's are their words' (`mode`, `effort`: what their words need), whatever room there is: the
/// mode's "Plan first" was cut to "Plan..." in a narrow window by a share of the room. The model's is its own words (`model`) up to `most`, and what the others leave, down to `least`
/// (or its own words, if they are fewer): its long name is what gives way first, then the effort's words (the mode's are never cut).
fn chip_widths(room: i32, mode: i32, effort: i32, model: i32, most: i32, least: i32) -> (i32, i32, i32) {
    let floor = least.min(model);
    // (the effort's words are cut before the mode's, and after the model's name has given what it can)
    let effort = effort.min((room - mode - floor).max(0));
    (mode, effort, model.min(most).min(room - mode - effort).max(floor))
}

/// What a control that a card puts over the transcript does when it is pressed.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CardAct {
    /// One of the model's choices: this label is the answer.
    Choose(String),
    /// What is written in the composer is the answer.
    SendOwn,
    /// The card's field, which is the composer: the focus goes there.
    FocusField,
    /// The plan is approved.
    Carry,
    /// The plan is to be changed: what is written next says how.
    Change,
    /// The changes of this file of an answer (the turn, and the file's place in its card), on the Changes page.
    OpenDiff(u64, usize),
    /// The file itself, in its own app, or shown in Explorer.
    OpenFile(u64, usize),
    /// All the files of an answer, on the Changes page.
    MoreFiles(u64),
}

/// One control of a card, on a row of its room.
#[derive(Clone)]
struct CardPart {
    text: String,
    kind: ButtonKind,
    act: CardAct,
    /// In pixels: nothing, the rest of the row's width.
    width: i32,
    tip: String,
}

/// The room a card keeps for controls, a row of an exact height: where it begins, and what is drawn over it, from the left, in the transcript's paint.
struct CardSlot {
    start: i32,
    parts: Vec<CardPart>,
    tall: i32,
}

/// What the trajectory page shows: the session's turns, which of them are open, and the rows of the table as that makes them.
struct TrajectoryPage {
    summary: dogmind_agent::trajectory_report::Summary,
    open: std::collections::BTreeSet<u64>,
    rows: Vec<crate::trajectory::Row>,
}

/// The model's question that waits for the person: where the answer goes, the question as it was asked, and the card that shows it.
struct AskedQuestion {
    answer: std::sync::mpsc::Sender<String>,
    question: String,
    options: Vec<(String, String)>,
    card: u64,
}

/// The plan that waits for the person's decision: where the answer goes, the plan as it was put, the card that shows it, and whether the person has pressed Change and what is written next
/// is the change asked for.
struct AskedPlan {
    answer: std::sync::mpsc::Sender<Result<(), String>>,
    plan: String,
    card: u64,
    changing: bool,
}

/// A message's pictures in the transcript: where their room in its card begins.
struct PictureBlock {
    start: i32,
    paths: Vec<PathBuf>,
    tiles: attach::Tiles,
}

/// A small copy, and when it was last asked for.
struct KeptThumb {
    thumb: Thumb,
    used: u64,
}

/// How a session stands in the sidebar, in a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideState {
    /// The model waits for the person: to approve something, to answer its question, to decide on its plan.
    Waiting,
    Working,
    /// No turn is at work, and a background command or a wait will wake it.
    StandingBy,
    Error,
}

/// What the sidebar says of a session. `parked`: it is out of sight with its agent, whether it waits for the person, and whether a turn is at work (else it stands by). Else it is the one on
/// screen (`current`), which `waits`, `running` and `standing_by` (something waits in the background) speak for.
fn side_state(parked: Option<(bool, bool)>, current: bool, waits: bool, running: bool, standing_by: bool, failed: bool) -> Option<SideState> {
    match parked {
        Some((true, _)) => Some(SideState::Waiting),
        Some((false, true)) => Some(SideState::Working),
        Some((false, false)) => Some(SideState::StandingBy),
        None if current && waits => Some(SideState::Waiting),
        None if current && running => Some(SideState::Working),
        None if current && standing_by => Some(SideState::StandingBy),
        None if failed => Some(SideState::Error),
        None => None,
    }
}

/// Whether the send button is a stop button: the model is at work. Not while it waits for words from the person (a question, a plan), when what is typed is its answer and the button sends it.
fn is_stoppable(running: bool, asking: bool, connecting: bool) -> bool {
    (running && !asking) || connecting
}

/// What Esc means now.
#[derive(Debug, PartialEq, Eq)]
enum EscapeDoes {
    ClosePanel,
    Reject,
    Stop,
    Nothing,
}

/// A panel over the conversation goes first: an answer coming behind Settings is not what Esc was pressed about.
fn escape_does(in_panel: bool, panel_can_close: bool, pending: bool, working: bool) -> EscapeDoes {
    if in_panel {
        return if panel_can_close { EscapeDoes::ClosePanel } else { EscapeDoes::Nothing };
    }
    if pending {
        EscapeDoes::Reject
    } else if working {
        EscapeDoes::Stop
    } else {
        EscapeDoes::Nothing
    }
}

/// Whether a card asking for approval may take the focus, where Y, A and N answer it. Not from someone writing: the next letters typed would approve a command.
fn approval_takes_focus(in_a_field: bool, draft_empty: bool, since_typed: Option<std::time::Duration>) -> bool {
    !in_a_field || (draft_empty && since_typed.is_none_or(|since| since > std::time::Duration::from_secs(2)))
}

/// How long a failed engine waits for the next keystroke to try again: each key of a sentence is no new request, and a load that fails takes the GPU with it each
/// time. Sending always tries.
const ENGINE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether what the person has just begun starts the engine: only one waiting for that, and not again right after it failed.
fn starts_the_engine(waits: bool, setting_up: bool, failed: Option<std::time::Duration>) -> bool {
    waits && !setting_up && failed.map_or(true, |ago| ago >= ENGINE_RETRY_AFTER)
}

/// Whether a setup under way keeps an engine from running. All of them do, for they work on the model's files and probe the card, but the fetch of the faster-writing data: it reads no
/// shard and takes nothing of the card, so the engine that runs goes on, and one that is asked for starts.
fn setup_holds_off_the_engine(setting_up: bool, fetching_drafter: bool) -> bool {
    setting_up && !fetching_drafter
}

/// What the engine does once the faster-writing data is in the settings.
#[derive(Debug, PartialEq)]
enum DrafterRestart {
    /// It has not begun, or is gone: its next start reads the settings as they are.
    Not,
    Now,
    /// A turn, a tool run or a session in the background is not cut short for it.
    WhenIdle,
}

fn drafter_restart(engine_began: bool, work_under_way: bool) -> DrafterRestart {
    match (engine_began, work_under_way) {
        (false, _) => DrafterRestart::Not,
        (true, false) => DrafterRestart::Now,
        (true, true) => DrafterRestart::WhenIdle,
    }
}

/// What the status line says while the engine waits for the work under way to end before it starts again with the faster-writing data.
fn drafter_restart_words() -> &'static str {
    t("The faster-writing data is ready. The engine restarts when the work under way ends.")
}

/// Whether to have the engine read the prompt and the tools now. With a message already waiting that message's own request reads them, once.
fn warms_on_connect(local: bool, straydog: bool, nothing_waits: bool) -> bool {
    local && straydog && nothing_waits
}

/// What the dog says on an empty chat, one to each new chat. In English whatever the display language: it is the dog's own voice, not a label.
const GREETINGS: [&str; 8] = [
    "What are we sniffing out today?",
    "Throw me something.",
    "Ready when you are.",
    "Point me at it.",
    "Got a scent to follow?",
    "What's in the yard today?",
    "Nose down. What are we after?",
    "Sit. Stay. Ship.",
];

/// What the dog says the first time it meets someone: in its own voice, as on an empty chat.
const WELCOME: &str = "Hi. I'm your new dog.";

/// The line for a new chat: never the one the last chat had, so that a new chat looks new.
fn next_greeting(last: usize, seed: u64) -> usize {
    (last + 1 + (seed % (GREETINGS.len() as u64 - 1)) as usize) % GREETINGS.len()
}

/// The line the engine or a server said of a failure, under the app's sentence in `text`, goes to the report (`/report`). Not while the app is taking itself off this PC: nothing is written then.
fn keep_server_line(folder: &Path, leaving: &Leaving, text: &str) {
    if let (Leaving::No, Some(line)) = (leaving, llm::line_under(text)) {
        crate::crash::note_engine_error(folder, line);
    }
}

/// Why a connection failed, shortly: the agent's words are for wherever they are shown, and these have a window around them.
fn connect_failure(error: &str) -> String {
    // (the line a server said under the sentence is shown apart from it)
    let error = llm::sentence_of(error);
    if error == llm::KEY_REFUSED {
        return t("The server refused the API key. Check it in Settings.").to_string();
    }
    if let Some(address) = error.strip_prefix(llm::NO_ANSWER) {
        return tf("No answer from {0}. Is it running?", &[&address]);
    }
    tf("Could not connect: {0}", &[&lang::notice(error)])
}

/// The line of About for the engine, from what its version option said.
fn engine_words(said: Option<&str>, made: Option<u64>) -> String {
    match said.and_then(|said| said.lines().next()).map(str::trim).filter(|line| !line.is_empty()) {
        Some(line) => {
            let version = line.strip_prefix("straydog").map_or(line, str::trim);
            match made {
                Some(made) => format!("Straydog  {version}  ({})", readable_build(&format!("x built at {made}")).trim_start_matches("x (").trim_end_matches(')')),
                None => format!("Straydog  {version}"),
            }
        }
        None => t("Straydog: no version given").to_string(),
    }
}

/// "Changes", with how much, and without a nought.
fn changes_heading(added: usize, removed: usize) -> String {
    match (added, removed) {
        (0, 0) => t("Changes").to_string(),
        (added, 0) => format!("{}  +{added}", t("Changes")),
        (0, removed) => format!("{}  −{removed}", t("Changes")),
        (added, removed) => format!("{}  +{added} −{removed}", t("Changes")),
    }
}

/// What an empty field says it is for, in place of a line under it.
unsafe fn field_cue(edit: HWND, words: &str) {
    let cue = wide(words);
    SendMessageW(edit, EM_SETCUEBANNER, 1, cue.as_ptr() as LPARAM);
}


/// When a session began, as short as it can be said: the time today, the day this year, or else the year. `today`: as `today_text` says it.
fn short_when(created: &str, today: &str) -> String {
    let day = created.get(0..10).unwrap_or("");
    if day.is_empty() {
        return String::new();
    }
    if day == today {
        return created.get(11..16).unwrap_or("").to_string();
    }
    if day.get(0..4) == today.get(0..4) {
        let part = |range: std::ops::Range<usize>| day.get(range).unwrap_or("").trim_start_matches('0').to_string();
        return format!("{}/{}", part(5..7), part(8..10));
    }
    day.get(0..4).unwrap_or("").to_string()
}

/// The day as a session's beginning says it.
unsafe fn today_text() -> String {
    let mut now: SYSTEMTIME = std::mem::zeroed();
    GetLocalTime(&mut now);
    format!("{:04}-{:02}-{:02}", now.wYear, now.wMonth, now.wDay)
}

/// Windows starts no program in a folder whose path is longer than this, whatever the file system can hold.
fn too_long_for_commands(folder: &Path) -> bool {
    folder.as_os_str().encode_wide().count() >= 248
}

/// What "as you write" comes to here: with a Japanese Windows, kanji alone count as Japanese.
fn auto_answer_words() -> &'static str {
    "As you write"
}

/// The way to run the model that the first screen offers first, from what it found: what is ready to use, then what can be set up here.
fn suggested_way(engine_found: bool, model_ready: bool, codex_found: bool, claude_key_found: bool) -> usize {
    if engine_found && model_ready {
        0
    } else if codex_found {
        2
    } else if claude_key_found {
        3
    } else if engine_found {
        0
    } else {
        1
    }
}

/// What a provider's setting for its key becomes, from what the person has done with its fields. `typed` wins, then the variable.
fn key_setting(current: &str, variable_on: bool, variable: &str, typed: &str, target: &str) -> String {
    if variable_on {
        return if variable.is_empty() { String::new() } else { format!("env:{variable}") };
    }
    if !typed.is_empty() {
        return format!("{}{target}", settings::CREDENTIAL);
    }
    match KeySource::of(current) {
        // (turned off: it does not read that variable any more)
        KeySource::Variable(_) => String::new(),
        _ => current.trim().to_string(),
    }
}

/// Whether a line starting with `/` means a command: `/usr/bin/env は何？` is a question about a path.
fn reads_as_command(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    first.strip_prefix('/').is_some_and(|name| !name.is_empty() && name.is_ascii() && !name.contains('/'))
}

/// The fields of a page with a key.
const KEY_FIELD: usize = 3;
const VARIABLE_FIELD: usize = 4;

/// The help, in sections: a `# ` line begins one.
fn help_sections() -> Vec<(String, String)> {
    let commands: Vec<String> = COMMANDS.iter().map(|(name, about)| format!("{name}\t{}", t(about))).collect();
    let text = (if lang::is_japanese() { lang::HELP_JA } else { HELP }).replace("{commands}", &commands.join("\n"));
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("# ") {
            sections.push((name.to_string(), String::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    sections
}

fn is_settings_place(panel: Panel) -> bool {
    SETTINGS_PLACES.iter().any(|(known, _, _)| *known == panel)
}

/// A row of the list of what waits, for the time that is drawn on it as it runs on: the nth command, or the nth wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitRow {
    Command(usize),
    Wait(usize),
}

/// A session that is not on screen, at work or with something waiting that will wake it: its agent goes on, and what it says waits here.
struct Parked {
    path: Option<PathBuf>,
    workspace: PathBuf,
    title: String,
    agent: Handle,
    events: Receiver<Event>,
    pending: Option<Approval>,
    /// The model's question, and its plan, that wait for the person.
    question: Option<agent::Question>,
    plan_review: Option<agent::PlanReview>,
    /// What has streamed since the last whole message, which the session file holds.
    partial: String,
    tools_running: HashMap<String, (String, String)>,
    queued: Vec<Outgoing>,
    interjected: Vec<String>,
    /// What the model has written so far of calls it has not finished.
    writing: Vec<(usize, String, String)>,
    used: u64,
    window_tokens: u64,
    rate: Option<f64>,
    totals: (u64, u64, u64, u32),
    /// What it said that no file keeps, for when it is opened.
    notices: Vec<(String, Style)>,
    /// Its turn ended in a failure.
    failed: bool,
    todos: Vec<serde_json::Value>,
    web_seen: bool,
    /// The session is being made shorter, and nothing has been asked of the model since.
    compacting: bool,
    /// A turn is at work. False: it stands by, with a background command or a wait that will wake it.
    running: bool,
    /// The turn at work is one nobody sent: a command's end, a line, a wait woke it.
    woken: bool,
    /// What it has going or waiting in the background, as its agent last said, and when that was said.
    waiting: Waiting,
    waiting_taken: std::time::Instant,
}

#[derive(Clone)]
struct Poster {
    notes: Sender<Note>,
    window: isize,
}

impl Poster {
    fn send(&self, note: Note) {
        let _ = self.notes.send(note);
        self.wake();
    }

    fn wake(&self) {
        // SAFETY: posting to a window that has gone fails and does nothing.
        unsafe { PostMessageW(self.window as HWND, WM_WAKE, 0, 0) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Chat,
    /// Where a new session starts.
    Start,
    Providers,
    Models,
    General,
    /// A new name for a session.
    Rename,
    /// Is this session to go?
    ConfirmDelete,
    ConfirmRemove,
    /// The sessions of this workspace, for `/resume`.
    Sessions,
    Help,
    About,
    /// Back to an earlier turn, for `/rewind`.
    Rewind,
    /// The saved rules of this workspace.
    Permissions,
    /// What changed, file by file.
    Diff,
    /// The person's own instructions for every session.
    Instructions,
    /// The skills of this workspace.
    Skills,
    /// The first run: the dog says hello.
    Welcome,
    /// The first run: how to reach a model.
    Setup,
    /// The first run: the details of a server, of Codex or of Claude.
    SetupDetails,
    /// How the window looks.
    Appearance,
    /// What the model can reach: the web, and other programs.
    Tools,
    /// What the model keeps between sessions, for `/memory`.
    Memory,
    /// Whether conversations are encrypted, and the passphrase and the recovery code that go with it.
    Privacy,
    /// The choice to encrypt, with the passphrase it asks for.
    CryptoOn,
    CryptoPassphrase,
    CryptoCode,
    CryptoOff,
    /// A change of every conversation, going on or done, with the recovery code that is shown once.
    CryptoWork,
    /// At the start of an app whose conversations are encrypted, with the key not yet in memory.
    Unlock,
}

/// The places of the settings, in their list.
const SETTINGS_PLACES: [(Panel, &str, &str); 11] = [
    (Panel::General, "settings", "General"),
    (Panel::Appearance, "sun", "Appearance"),
    (Panel::Providers, "plug", "Models and connections"),
    (Panel::Models, "cpu", "Models on this PC"),
    (Panel::Permissions, "shield", "Permissions and safety"),
    (Panel::Tools, "wrench", "Tools"),
    (Panel::Skills, "book-open", "Skills"),
    (Panel::Memory, "paw-print", "Memory"),
    (Panel::Instructions, "pencil", "Your instructions"),
    (Panel::Privacy, "lock", "Privacy"),
    (Panel::About, "info", "About"),
];

/// A page of the settings, which puts its controls in rows: a label and a value, the values in one line, and what needs saying under them.
struct Form {
    shapes: Vec<Shape>,
    /// What this page shows.
    used: Vec<HWND>,
    y: i32,
    width: i32,
    stacked: bool,
    label: i32,
    fonts: Fonts,
    dpi: u32,
    first_section: bool,
    /// What the row being laid out needs beyond its control's height, for a label of several lines: `end_row` adds it.
    extra: i32,
}

impl Form {
    fn new(width: i32, top: i32, fonts: Fonts, dpi: u32) -> Form {
        let stacked = width < scale(dpi, ds::size::STACK_BELOW);
        let label = scale(dpi, ds::size::LABEL_WIDTH).min(width * 42 / 100);
        Form { shapes: Vec::new(), used: Vec::new(), y: top, width, stacked, label, fonts, dpi, first_section: true, extra: 0 }
    }

    fn s(&self, value: i32) -> i32 {
        scale(self.dpi, value)
    }

    fn value_x(&self) -> i32 {
        if self.stacked { 0 } else { self.label + self.s(ds::space::M) }
    }

    fn value_width(&self) -> i32 {
        (self.width - self.value_x()).min(self.s(ds::size::VALUE_WIDTH))
    }

    /// The name of a group of rows.
    unsafe fn section(&mut self, words: &str) {
        if !self.first_section {
            self.y += self.s(ds::space::L);
        }
        self.first_section = false;
        self.shapes.push(Shape::Text { area: rect(0, self.y, self.width, self.s(ds::space::XL)), text: words.to_string(), font: FontKind::Strong, color: theme::text(), flags: ONE_LINE });
        self.y += self.s(ds::space::XL) + self.s(ds::space::S);
    }

    /// The label, on the row or over it, and where the row's value goes. A label that does not fit on a line goes on to as many as it needs, and the row is as tall as they are:
    /// its control stays level with the first line, where it was.
    unsafe fn label(&mut self, words: &str, height: i32) {
        let room = if self.stacked { self.width } else { self.label };
        let one = text_height(self.fonts.body, "A", 100_000);
        let tall = text_height(self.fonts.body, words, room.max(1));
        let wrapped = tall > one + 1;
        if self.stacked {
            if wrapped {
                self.shapes.push(Shape::Text { area: rect(0, self.y, self.width, tall), text: words.to_string(), font: FontKind::Body, color: theme::soft(), flags: DT_WORDBREAK });
                self.y += tall + self.s(ds::space::XL) - self.s(ds::size::TEXT_LINE);
            } else {
                self.shapes.push(Shape::Text { area: rect(0, self.y, self.width, self.s(ds::size::TEXT_LINE)), text: words.to_string(), font: FontKind::Body, color: theme::soft(), flags: ONE_LINE });
                self.y += self.s(ds::space::XL);
            }
        } else if wrapped {
            // (the first line is level with the middle of the control, as a label of one line is)
            let top = ((height - one) / 2).max(0);
            self.shapes.push(Shape::Text { area: rect(0, self.y + top, self.label, tall), text: words.to_string(), font: FontKind::Body, color: theme::soft(), flags: DT_WORDBREAK });
            self.extra = (top + tall - height).max(0);
        } else {
            self.shapes.push(Shape::Text { area: rect(0, self.y, self.label, height), text: words.to_string(), font: FontKind::Body, color: theme::soft(), flags: ONE_LINE });
        }
    }

    unsafe fn end_row(&mut self, height: i32) {
        self.y += height + std::mem::take(&mut self.extra) + self.s(ds::space::M);
    }

    /// A control this wide, in the value's place.
    unsafe fn control(&mut self, words: &str, window: HWND, width: i32) {
        let height = self.s(ds::size::CONTROL);
        self.label(words, height);
        place(window, self.value_x(), self.y, width.min(self.width - self.value_x()), height);
        self.used.push(window);
        self.end_row(height);
    }

    unsafe fn select(&mut self, words: &str, window: HWND) {
        let width = self.value_width();
        self.control(words, window, width);
    }

    unsafe fn toggle(&mut self, words: &str, window: HWND) {
        let height = self.s(ds::size::CONTROL);
        self.label(words, height);
        let (across, tall) = (self.s(ds::size::TOGGLE_WIDTH), self.s(ds::size::TOGGLE_HEIGHT));
        place(window, self.value_x(), self.y + (height - tall) / 2, across, tall);
        self.used.push(window);
        self.end_row(height);
    }

    /// A button in the middle of the page, as wide as its words need.
    unsafe fn button_centered(&mut self, window: HWND) {
        let height = self.s(ds::size::CONTROL);
        let across = (text_width(self.fonts.strong, &get_text(window)) + self.s(ds::space::XXL)).max(self.s(80)).min(self.width);
        place(window, (self.width - across) / 2, self.y, across, height);
        self.used.push(window);
        self.y += height + self.s(ds::space::S);
    }

    /// A link (a button that looks like words) in the middle of the page, as wide as its words need.
    unsafe fn link_centered(&mut self, window: HWND) {
        let across = text_width(self.fonts.small, &get_text(window)) + self.s(ds::space::L);
        place(window, (self.width - across) / 2, self.y, across, self.s(ds::size::LINK));
        self.used.push(window);
        self.y += self.s(ds::size::LINK) + self.s(ds::space::S);
    }

    /// A switch and its words after it, the two together in the middle of the page.
    unsafe fn toggle_centered(&mut self, words: &str, window: HWND) {
        let height = self.s(ds::size::CONTROL);
        let (across, tall, gap) = (self.s(ds::size::TOGGLE_WIDTH), self.s(ds::size::TOGGLE_HEIGHT), self.s(ds::space::S));
        let x = ((self.width - across - gap - text_width(self.fonts.body, words)) / 2).max(0);
        place(window, x, self.y + (height - tall) / 2, across, tall);
        self.used.push(window);
        self.shapes.push(Shape::Text { area: rect(x + across + gap, self.y, (self.width - x - across - gap).max(0), height), text: words.to_string(), font: FontKind::Body, color: theme::soft(), flags: ONE_LINE });
        self.end_row(height);
    }

    /// A switch with what it does beside it, for rows whose label names something else.
    unsafe fn toggle_beside(&mut self, words: &str, window: HWND, beside: &str) {
        let height = self.s(ds::size::CONTROL);
        let across = self.s(ds::size::TOGGLE_WIDTH) + self.s(ds::space::S);
        // (below the label when they are stacked: `label` moves down)
        self.label(words, height);
        let (x, y) = (self.value_x() + across, self.y);
        self.shapes.push(Shape::Text { area: rect(x, y, (self.width - x).max(0), height), text: beside.to_string(), font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
        let tall = self.s(ds::size::TOGGLE_HEIGHT);
        place(window, self.value_x(), y + (height - tall) / 2, self.s(ds::size::TOGGLE_WIDTH), tall);
        self.used.push(window);
        self.end_row(height);
    }

    /// A row of a list of things to switch: its name across the row, and the switch at the right end with what it is set to just before it.
    unsafe fn toggle_at_end(&mut self, words: &str, window: HWND, state: &str) {
        let height = self.s(ds::size::CONTROL);
        let (across, tall) = (self.s(ds::size::TOGGLE_WIDTH), self.s(ds::size::TOGGLE_HEIGHT));
        let state_width = text_width(self.fonts.small, state) + self.s(ds::space::S);
        let switch_x = self.width - across;
        let state_x = switch_x - self.s(ds::space::S) - state_width;
        self.shapes.push(Shape::Text { area: rect(0, self.y, (state_x - self.s(ds::space::M)).max(0), height), text: words.to_string(), font: FontKind::Body, color: theme::text(), flags: ONE_LINE });
        self.shapes.push(Shape::Text { area: rect(state_x, self.y, state_width, height), text: state.to_string(), font: FontKind::Small, color: theme::muted(), flags: ONE_LINE | DT_RIGHT });
        place(window, switch_x, self.y + (height - tall) / 2, across, tall);
        self.used.push(window);
        self.end_row(height);
    }

    /// A field: the system's edit on a frame of ours, which shows when it has the focus.
    unsafe fn field(&mut self, words: &str, edit: HWND) {
        let height = self.s(ds::size::CONTROL);
        self.label(words, height);
        let (x, width) = (self.value_x(), self.value_width());
        self.frame(x, width, edit);
        self.end_row(height);
    }

    /// The same, with what goes with it at its end: a way to choose what goes in it.
    unsafe fn field_with_button(&mut self, words: &str, edit: HWND, button: HWND) {
        let height = self.s(ds::size::CONTROL);
        self.label(words, height);
        let label = get_text(button);
        let across = (text_width(self.fonts.body, &label) + self.s(ds::size::ICON_BUTTON)).max(self.s(72));
        let (x, width) = (self.value_x(), self.value_width() - across - self.s(ds::space::S));
        self.frame(x, width, edit);
        place(button, x + width + self.s(ds::space::S), self.y, across, height);
        self.used.push(button);
        self.end_row(height);
    }

    unsafe fn frame(&mut self, x: i32, width: i32, edit: HWND) {
        let height = self.s(ds::size::CONTROL);
        let border = if GetFocus() == edit { theme::accent() } else { theme::line() };
        self.shapes.push(Shape::Round { area: rect(x, self.y, width, height), radius: self.s(ds::radius::CONTROL), fill: Some(theme::surface()), border: Some(border) });
        place(edit, x + self.s(ds::space::M), self.y + self.s(ds::size::FIELD_INSET), width - 2 * self.s(ds::space::M), self.s(ds::size::FIELD_TEXT));
        self.used.push(edit);
    }

    /// Words in the value's place. A path that does not fit is shortened in the middle: its start and its last name show.
    unsafe fn value(&mut self, words: &str, value: &str) {
        let height = self.s(ds::size::CONTROL);
        self.label(words, height);
        let room = self.width - self.value_x();
        self.shapes.push(Shape::Text { area: rect(self.value_x(), self.y, room, height), text: compact_path(self.fonts.small, value, room), font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
        self.end_row(height);
    }

    /// A label and one button, as wide as its words, and what it is about beside it.
    unsafe fn button_row(&mut self, words: &str, button: HWND, beside: &str) {
        let height = self.s(ds::size::CONTROL);
        self.label(words, height);
        let across = (text_width(self.fonts.body, &get_text(button)) + self.s(ds::space::XXL)).max(self.s(80));
        place(button, self.value_x(), self.y, across, height);
        self.used.push(button);
        let x = self.value_x() + across + self.s(ds::space::S);
        let room = (self.width - x).max(0);
        self.shapes.push(Shape::Text { area: rect(x, self.y, room, height), text: compact_path(self.fonts.small, beside, room), font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
        self.end_row(height);
    }

    /// Words across the page.
    unsafe fn note(&mut self, words: &str, color: u32) {
        if words.is_empty() {
            return;
        }
        let tall = text_height(self.fonts.small, words, self.width);
        self.shapes.push(Shape::Text { area: rect(0, self.y, self.width, tall + self.s(ds::space::HAIR)), text: words.to_string(), font: FontKind::Small, color, flags: DT_WORDBREAK });
        self.y += tall + self.s(ds::space::M);
    }

    /// Words across the page with a button at their right, level with their first line: a row of a list the person can take things out of.
    unsafe fn note_with_button(&mut self, words: &str, button: HWND) {
        let height = self.s(ds::size::CONTROL);
        let across = (text_width(self.fonts.body, &get_text(button)) + self.s(ds::space::XXL)).max(self.s(80));
        let room = (self.width - across - self.s(ds::space::M)).max(self.s(80));
        let tall = text_height(self.fonts.body, words, room).max(height);
        self.shapes.push(Shape::Text { area: rect(0, self.y + self.s(ds::space::XS), room, tall), text: words.to_string(), font: FontKind::Body, color: theme::text(), flags: DT_WORDBREAK });
        place(button, self.width - across, self.y, across, height);
        self.used.push(button);
        self.y += tall + self.s(ds::space::M);
    }

    /// Buttons side by side from this place, as wide as their words, wrapping.
    unsafe fn buttons(&mut self, x: i32, row: &[HWND]) {
        let height = self.s(ds::size::CONTROL);
        let widths: Vec<i32> = row.iter().map(|button| {
            let label = get_text(*button);
            let (glyph, words) = label.split_once('|').map_or((0, label.as_str()), |(_, words)| (self.s(ds::space::XL), words));
            (text_width(self.fonts.strong, words) + glyph + self.s(ds::space::XXL)).max(self.s(80))
        }).collect();
        let places = wrap_row(&widths, self.s(ds::space::S), self.width - x);
        for (at, (line, left)) in places.iter().enumerate() {
            place(row[at], x + left, self.y + line * (height + self.s(ds::space::S)), widths[at], height);
            self.used.push(row[at]);
        }
        let lines = places.last().map_or(1, |(line, _)| line + 1);
        self.y += lines * height + (lines - 1) * self.s(ds::space::S) + self.s(ds::space::M);
    }

    /// A list on its frame.
    unsafe fn list(&mut self, window: HWND, height: i32) {
        self.shapes.push(Shape::Round { area: rect(0, self.y, self.width, height), radius: self.s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
        let inset = self.s(ds::space::XS);
        place(window, inset, self.y + inset, self.width - 2 * inset, height - 2 * inset);
        self.used.push(window);
        self.y += height + self.s(ds::space::M);
    }
}

/// What the transcript holds, in order: a block's width is fixed when it goes in, so a new width means writing it all again.
enum Shown {
    Text(String, Style, Para),
    /// Its words, and where they lead.
    Link(String, String, Para),
    /// `width`: none for the column's.
    Row { lines: Vec<String>, face: String, points: i32, color: u32, back: u32, width: Option<i32>, right: bool, padding: (i32, i32) },
    /// An answer's text, for its copy button, and when it was given, as its row says it (or nothing).
    Answer(String, String),
    /// A line of code, coloured when it is drawn: `open` is whether a block comment runs into it.
    Code { line: String, language: String, open: bool },
    /// A Markdown table: its first row is the heading. Drawn afresh each time, as its colours and widths depend on the moment.
    Table(Vec<Vec<String>>),
    /// A block of code begins here: all of it, for its copy button.
    Block(String),
    /// The pictures of a message and their sizes, at the top of its card. `alone`: no text under them.
    /// `right`: what the person sent; otherwise what a tool opened for the model.
    Pictures { pictures: Vec<(PathBuf, (i32, i32))>, width: i32, alone: bool, right: bool },
    /// Tool calls as one row: which of `runs`.
    Run(usize),
    /// A card that asks something of the person or shows something of the work, in the state it is in now.
    Card(Card),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Body,
    Strong,
    Emphasis,
    Struck,
    Quote,
    Link,
    /// `#`
    Title,
    /// The space between two blocks.
    Gap,
    /// `##`
    Heading,
    /// `###` and on.
    Subheading,
    InlineCode,
    Thinking,
    Tool,
    CopyRow,
    /// When the person said what is in the card above it, at the card's right.
    SaidAt,
    /// When an answer was given, in its copy row, past where the button stands.
    AnsweredAt,
    Output,
    Warn,
    Bad,
    /// The line the engine or a server said, under the app's own sentence about a failure: smaller, in the muted colour.
    Said,
}

/// Where an answer's time begins in its row, in twips: past its copy and branch buttons, each an icon button wide at 100%, and a little room.
const ANSWERED_AT_TAB: i32 = (2 * ds::size::ICON_BUTTON + 12) * 15;
const PFM_ALIGNMENT: u32 = 0x0000_0008;
const PFM_RIGHTINDENT: u32 = 0x0000_0002;
const PFA_LEFT: u16 = 1;
const PFA_RIGHT: u16 = 2;

/// What the parent window paints itself, recomputed by `layout`.
enum Shape {
    Round { area: RECT, radius: i32, fill: Option<u32>, border: Option<u32> },
    Text { area: RECT, text: String, font: FontKind, color: u32, flags: u32 },
    Logo { x: i32, y: i32, size: i32 },
    /// The app's name in its own lettering.
    TitleMark { x: i32, y: i32, width: i32 },
    /// A speech bubble whose tail comes out of its underside to `tip`.
    Bubble { area: RECT, radius: i32, tip: (i32, i32), fill: u32, line: u32, thickness: f32 },
    Icon { name: &'static str, area: RECT, size: i32, color: u32 },
    /// A plain rectangle, as a line or a part of a panel.
    Fill { area: RECT, color: u32 },
}

impl Shape {
    /// All it can touch, for a paint of a part of the window to know whether to draw it.
    fn bounds(&self) -> RECT {
        let (outer, margin) = match self {
            Shape::Round { area, .. } | Shape::Text { area, .. } | Shape::Fill { area, .. } => (*area, 2),
            Shape::Logo { x, y, size } => (rect(*x, *y, *size, *size), 2),
            Shape::TitleMark { x, y, width } => (rect(*x, *y, *width, title_height(*width)), 2),
            Shape::Bubble { area, tip, thickness, .. } => {
                let around = RECT { left: area.left.min(tip.0), top: area.top.min(tip.1), right: area.right.max(tip.0), bottom: area.bottom.max(tip.1) };
                (around, 2 + thickness.ceil() as i32)
            }
            // (centred in its area, whichever of the two is the larger)
            Shape::Icon { area, size, .. } => (*area, 2 + *size / 2),
        };
        RECT { left: outer.left - margin, top: outer.top - margin, right: outer.right + margin, bottom: outer.bottom + margin }
    }
}

#[derive(Clone, Copy)]
enum FontKind {
    Body,
    Small,
    Strong,
    Title,
    Heading,
    Greeting,
}

#[derive(Clone)]
enum SideRow {
    Workspace { path: PathBuf, name: String, collapsed: bool },
    Session { path: PathBuf, title: String, when: String },
    Hint(String),
    /// The name of the pinned ones.
    Pinned,
}

#[derive(Clone)]
struct ListRow {
    title: String,
    detail: String,
    active: bool,
}

#[derive(Clone)]
struct Suggestion {
    title: String,
    detail: String,
    /// What it puts in place of the word being typed.
    insert: String,
}

struct App {
    window: HWND,
    folder: PathBuf,
    settings: Settings,
    agent: Handle,
    events: Receiver<Event>,
    notes: Receiver<Note>,
    poster: Poster,
    engine: Option<Engine>,
    generation: Arc<AtomicU64>,
    panel: Panel,
    dpi: u32,
    fonts: Fonts,
    brushes: [HBRUSH; 4], // theme::background(), theme::sidebar(), theme::surface(), theme::raised()
    shapes: Vec<Shape>,
    // The sidebar.
    new_session: HWND,
    settings_button: HWND,
    side: HWND,
    side_rows: Vec<SideRow>,
    side_hover: Option<usize>,
    /// The Files view of the panel on the right (`files_panel`): the tree of the workspace, the list that shows its rows, the row selected and the one under the pointer, what the tree
    /// is of, the watcher of its folder (only while it is in view), and whether what it shows may be old (it was out of view, unwatched).
    tree: files::Tree,
    tree_list: HWND,
    tree_rows: Vec<files::Row>,
    tree_selected: Option<String>,
    tree_hover: Option<usize>,
    tree_for: Option<PathBuf>,
    tree_watcher: Option<files::Watcher>,
    tree_stale: bool,
    tree_dpi: u32,
    /// The tree's `version` when the list was last made from it.
    tree_version: Option<u64>,
    /// The little menu of a row, and the row it is of.
    tree_menu: HWND,
    tree_menu_row: Option<files::Row>,
    tree_menu_hover: Option<usize>,
    /// What the little list on screen offers and whose it is (a row of the tree, the shells a terminal can be started in, or what a right click on a terminal offers).
    panel_menu: files_panel::PanelMenu,
    /// The Terminal view of the panel on the right (`term_panel`): its windows and its terminals.
    term: term_panel::TermView,
    /// The words over the panel on the right, where a press goes to the view each names.
    right_tab_hits: Vec<(RECT, RightView)>,
    collapsed: HashSet<String>,
    all_sessions: Vec<session::Summary>,
    /// The session on screen, once it has a file.
    current_session: Option<PathBuf>,
    // The chat.
    transcript: HWND,
    composer: HWND,
    composer_cue: HWND,
    send: HWND,
    model_chip: HWND,
    mode_chip: HWND,
    effort_chip: HWND,
    help_button: HWND,
    doc: HWND,
    /// The icons over the transcript, drawn in its own paint where each stood in that frame: for hit-testing, which is the pointer's, and for the hidden controls. Which of them
    /// the pointer is over, which is pressed, and which has just copied (shown as a check mark till the timer).
    overlay_hits: Vec<OverlayHit>,
    overlay_hover: Option<OverlayKey>,
    overlay_press: Option<OverlayKey>,
    overlay_copied: Option<OverlayKey>,
    overlay_tip_for: Option<(OverlayKey, RECT)>,
    proxies: Vec<HWND>,
    proxy_keys: Vec<Option<OverlayKey>>,
    /// Where each answer ends in the transcript, and its text.
    answers: Vec<(i32, String)>,
    /// Each card of what the person wrote, in order: its corners are drawn over.
    user_cards: Vec<UserCard>,
    /// The person's messages, in order, for the rail of turns.
    rail_turns: Vec<TurnMark>,
    /// A message's pictures are on the rail, and its words, coming next, are its words there.
    turn_words_due: bool,
    /// The rail beside the transcript, when it is shown: two turns or more, and room at the column's side.
    rail: Option<RECT>,
    /// The turn being read, and the one the pointer is on.
    rail_reading: usize,
    rail_hover: Option<usize>,
    /// Each mark on the rail, and its turn.
    rail_hits: Vec<(RECT, usize)>,
    /// The marks as their tips stand now.
    rail_tips: Vec<(RECT, usize)>,
    /// Where the text column ends at the right, in the window.
    column_right: i32,
    transcript_area: RECT,
    /// The transcript's scrollbar as this window draws it: the control's own is cut off by its region, a wide dark band in the page.
    thumb: Option<RECT>,
    /// Being dragged: how far below the thumb's top it was taken.
    thumb_grab: Option<i32>,
    shown: Vec<Shown>,
    replaying: bool,
    /// The text's place in the transcript's window: left, right, height.
    transcript_rect: (i32, i32, i32),
    /// The column's width, which a block in the transcript is as wide as.
    text_twips: i32,
    code_language: String,
    /// An IME is composing in the composer.
    composing: bool,
    composer_procedure: isize,
    straydog: bool,
    toggle_language: HWND,
    toggle_display: HWND,
    folder_chip: HWND,
    /// How many lines the composer last had: the card grows with them.
    composer_lines: i32,
    provider_up: HWND,
    provider_down: HWND,
    /// What a dialog stands on: over the whole of the window, with a picture of the app under it, darkened. The dialog's controls are its children, so
    /// that what is behind takes neither the focus nor a click, and the transcript behind goes on being written.
    modal: HWND,
    /// The part of a dialog that scrolls.
    pane: HWND,
    modal_close: HWND,
    nav_buttons: Vec<HWND>,
    modal_open: bool,
    modal_backdrop: HBITMAP,
    modal_backdrop_size: (i32, i32),
    /// What had the focus when it opened, to have it back.
    modal_return: HWND,
    modal_shapes: Vec<Shape>,
    pane_shapes: Vec<Shape>,
    modal_card: RECT,
    /// Pressed outside the dialog: let go there too, it closes.
    modal_pressed_outside: bool,
    help_section: usize,
    theme_segments: [HWND; 3],
    mode_select: HWND,
    key_replace: HWND,
    key_remove: HWND,
    advanced_button: HWND,
    variable_toggle: HWND,
    mcp_import: HWND,
    /// A switch for each MCP server, made when the Tools page first needs it.
    mcp_trust: Vec<HWND>,
    /// The sizes of the conversation's words, on the Appearance page.
    size_segments: [HWND; TEXT_SIZES.len()],
    /// A switch for each skill found, made when the Skills page first needs it.
    skill_toggles: Vec<HWND>,
    /// What the model keeps between sessions, as the Memory page last read it, and a button to take out each.
    memory_notes: Vec<(&'static str, String)>,
    memory_remove: Vec<HWND>,
    /// The app's own file and its engine's, as they were when it started: a new build changes them.
    builds: Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>,
    /// A new build seen once, to be seen the same again before it is offered.
    update_seen: Option<Vec<Option<(std::time::SystemTime, u64)>>>,
    /// A new build is here, and the notice at the top of the main screen asks whether to restart into it. Its button (also the one that says how far the fetch of a release has come, and
    /// updates once the setup is here) and, for the page of a release, its other one; and Later, that sends the notice away.
    update_offer: bool,
    update_offer_button: HWND,
    notice_later: HWND,
    /// The build the person said no to, with the cross: they are asked again only for a newer one.
    update_declined: Option<Vec<Option<(std::time::SystemTime, u64)>>>,
    /// They said yes while work was going on: it restarts when the work is done.
    restart_when_done: bool,
    /// The setup that waits is being checked before it is run (a thread reads all of its bytes: a second or two): the buttons that restart into it say so, and cannot be pressed again.
    checking_setup: bool,
    /// A newer release on GitHub than this one, found on the app's own look: the notice at the top of the main screen tells of it (`notice`), with a button for its page while nothing of its
    /// setup is fetched, and for the update itself once it is. The version the tray has told of, once (`notice::balloon_due`).
    release_offer: Option<updates::Release>,
    release_offer_button: HWND,
    tray_told_release: Option<String>,
    /// A look at GitHub is under way (a thread asks; the answer comes as a note), and what the last one came to, as the About page says it.
    release_checking: bool,
    release_outcome: Option<updates::Outcome>,
    /// The folder this copy runs from when the installer put it there: only such a copy is updated through the installer. A copy unpacked from the zip is given the release's page.
    install_folder: Option<PathBuf>,
    /// The setup of a newer release: fetched on its own in the background and checked, then offered; and what the last try came to, as About says it, and whether that is a trouble.
    release_update: ReleaseUpdate,
    release_update_said: Option<(String, bool)>,
    /// Later was pressed on the notice of the setup that is here: it is offered again at the next start.
    release_ready_closed: bool,
    /// The switch on the About page for the app's own looks.
    toggle_updates: HWND,
    /// "Remember on this PC", for the key of encrypted conversations.
    toggle_remember: HWND,
    read_folder_add: HWND,
    read_folder_stop: HWND,
    read_folder_remove: Vec<HWND>,
    /// The folders commands can read, as the page last looked.
    read_folders: Vec<read_folders::Folder>,
    read_folder_places: Vec<read_folders::Located>,
    /// Work going on in a folder's tree: which way, where, and what stops it. Only an adding can be stopped: a taking back goes to its end.
    read_folder_work: Option<ReadFolderWork>,
    /// What the last of that came to.
    read_folder_said: (String, bool),
    setup_again: HWND,
    remove_all: HWND,
    /// A stored key is being replaced: the field is open for the new one.
    key_editing: bool,
    /// The ways of giving a key that most people will not need are showing.
    advanced_open: bool,
    /// The key is to be read from a variable.
    variable_on: bool,
    /// Which line the dog says while this chat is empty.
    greeting: usize,
    // Finding in the conversation.
    find_open: bool,
    find_field: HWND,
    find_previous: HWND,
    find_next: HWND,
    find_close: HWND,
    find_matches: Vec<(i32, i32)>,
    find_at: usize,
    /// What to find in the session being opened, once it is on screen.
    find_after_open: Option<String>,
    // Searching all sessions.
    side_search: HWND,
    side_query: String,
    side_hits: Option<Vec<session::Hit>>,
    search_generation: u64,
    suggest: HWND,
    suggestions: Vec<Suggestion>,
    suggest_hover: Option<usize>,
    files: Vec<String>,
    skills: Vec<dogmind_agent::skills::Skill>,
    mcp: Arc<dogmind_agent::mcp::Hub>,
    /// Whether conversations are written encrypted, shared with every agent: while the key is not unlocked, none is opened or written.
    policy: Arc<Policy>,
    crypt: Crypt,
    branch: Option<String>,
    session_title: String,
    totals: (u64, u64, u64, u32), // prompt, cached, completion, requests
    side_target: Option<usize>,
    rename_target: Option<PathBuf>,
    rename_seed: Option<String>,
    resume_paths: Vec<PathBuf>,
    parked: Vec<Parked>,
    /// What the session on screen has going or waiting in the background, as its agent last said, and when that was said (the times of the panel run on from it).
    waiting: Waiting,
    waiting_taken: std::time::Instant,
    /// A turn that nobody sent is at work: what it asks, and where it ends, are told to someone who may not be looking.
    woken_turn: bool,
    /// The stop of each row of the list of what waits, in the panel on the right (its id, or `*` for all of them), and where the time of each row is drawn.
    waiting_hits: Vec<(RECT, String)>,
    waiting_times: Vec<(RECT, WaitRow)>,
    /// What a session opened beside a working one will connect with.
    provider: Option<(Arc<dyn Provider>, String, u64)>,
    /// What sessions said after they stopped being on screen, until they are opened.
    held_notices: HashMap<PathBuf, Vec<(String, Style)>>,
    /// Chats out of sight whose last turn failed, until they are opened.
    failed_sessions: HashSet<PathBuf>,
    /// The turn on screen failed.
    turn_failed: bool,
    /// The session's turns, the rows of the rewind panel, and the rows of the rules panel.
    turns: Vec<dogmind_agent::checkpoint::Turn>,
    /// What each turn's edits changed (Event::TurnChanges), by the turn: the data of the card under its answer.
    turn_changes: std::collections::BTreeMap<u64, Vec<dogmind_agent::tools::FileChange>>,
    rewind_rows: Vec<u64>,
    shown_rules: Vec<rules::Rule>,
    changes: Vec<FileChange>,
    /// The file to show first when the changes come, chosen in the panel on the right.
    change_wanted: Option<usize>,
    /// The files of one answer, which the Changes page lists in place of the workspace's, when it was opened from the card under that answer.
    turn_view: Option<Vec<FileChange>>,
    /// A session opened again: where each person's words begin in the message list and in what was written (`shown`), and where the end is: the places the cards of the files each
    /// answer changed go to. The turns whose changes were asked for and have not come, each with its place, and the cards that have come.
    turn_slots: Vec<(usize, usize)>,
    turn_end_slot: usize,
    turn_cards_waiting: HashMap<u64, usize>,
    turn_cards_came: Vec<(usize, u64)>,
    /// The bar under the title with the session's goal: its button, which clears it.
    goal_clear: HWND,
    // The panel on the right.
    right_width: i32,
    right_toggle: HWND,
    right_hits: Vec<(RECT, usize)>,
    /// What Windows sends when the taskbar comes back, and the tray has to be told again.
    taskbar_created: u32,
    /// What an installer sends to ask this copy to close: `dogmind-setup` has the same name.
    please_quit: u32,
    /// The session the person is being asked about, and its title.
    delete_target: Option<(PathBuf, String)>,
    /// The file shown beside the conversation, and when it was last written. The browser exists only while one is shown.
    preview: Option<Preview>,
    preview_file: Option<PathBuf>,
    /// The lines of it a link named, to be shown at: for a browser that is still starting too.
    preview_lines: Option<(usize, usize)>,
    preview_written: Option<std::time::SystemTime>,
    /// How many files the page had asked for when `preview_written` was taken: a page that asks for more has not changed, it has shown what it holds.
    preview_loaded: usize,
    /// The file shown is not there any more, and the panel says so.
    preview_gone: bool,
    preview_making: bool,
    preview_blocked: u32,
    preview_hits: Vec<(RECT, PathBuf)>,
    /// The right panel's way to the engine's dashboard, and where it goes.
    dashboard_hit: Option<(RECT, String)>,
    preview_reload: HWND,
    preview_browser: HWND,
    preview_close: HWND,
    todos: Vec<serde_json::Value>,
    git: Option<GitInfo>,
    /// Web content is in this conversation, so a command that gets the network asks.
    web_seen: bool,
    toggle_web: HWND,
    toggle_compact: HWND,
    context_auto: Option<u64>,
    /// Going through the first setup: a model folder or a provider leads on to the workspace.
    onboarding: bool,
    setup_kind: Kind,
    sidebar_width: i32,
    /// The Changes page: its window, the file shown as rows, where the view is, and what the pointer has chosen.
    diff_view: HWND,
    /// The trajectory page, when the Changes page's window shows it: the table's rows and which turns are open.
    trajectory: Option<TrajectoryPage>,
    diff_previous: HWND,
    diff_next: HWND,
    diff_close: HWND,
    diff_rows: Vec<diffview::Row>,
    diff_starts: Vec<usize>,
    diff_scroll: i32,
    diff_across: i32,
    /// Rows chosen, first and last as they were pointed at.
    diff_chosen: Option<(usize, usize)>,
    diff_thumb_grab: Option<i32>,
    /// What stands in place of rows.
    diff_words: String,
    diff_file: Option<usize>,
    note: HWND,
    approve: HWND,
    always: HWND,
    reject: HWND,
    feedback: HWND,
    activity_area: RECT,
    status_area: RECT,
    // The other panels share these.
    list: HWND,
    list_rows: Vec<ListRow>,
    list_hover: Option<usize>,
    /// The first ones as a panel says; `KEY_FIELD` and `VARIABLE_FIELD` on the pages with a key.
    fields: [HWND; 5],
    labels: [String; 4],
    buttons: [HWND; 5],
    kind_chip: HWND,
    drafter_chip: HWND,
    /// Speed first for a PC short of memory: the engine's next start leaves the CPU's low-weight routes out.
    speed_chip: HWND,
    /// Whether the model reads pictures: a switch in a dialog, a choice on the first screens.
    vision_chip: HWND,
    vision_delete: HWND,
    vision_offer_button: HWND,
    /// For a model not yet there: whether to fetch its vision file with it.
    vision_wanted: bool,
    /// This download is for a vision file and no more.
    vision_fetch: bool,
    /// What stands over the composer when a picture cannot be read and something can be done.
    vision_offer: Option<VisionOffer>,
    vision_offer_close: Option<RECT>,
    toggle_thinking: HWND,
    toggle_safe: HWND,
    panel_title: String,
    panel_log: String,
    progress: Option<f64>,
    kind: Kind,
    /// For a model not yet there: whether to fetch its drafter with it. Of the model that is there, the settings say.
    drafter: bool,
    /// The folder of a drafter being fetched on its own for the model that is there: this download is for it and no more.
    drafter_fetch: Option<PathBuf>,
    /// The faster-writing data is in the settings, and the engine that is running began without it: it starts again once no work is under way.
    drafter_restart_waits: bool,
    // State.
    pending: Option<Approval>,
    tools_running: HashMap<String, (String, String)>,
    running: bool,
    has_messages: bool,
    queued: Vec<Outgoing>,
    /// Where the view stood when the thumb was last placed.
    thumb_at: i32,
    /// What of the transcript control shows: its size less the scrollbar's strip, and the rail's place in it, cut out.
    transcript_region: ((i32, i32), Option<(i32, i32, i32, i32)>),
    /// Words sent to the turn under way that have not reached it yet.
    interjected: Vec<String>,
    /// The model's question that waits for the person (Event::Question).
    question: Option<AskedQuestion>,
    /// Where each card keeps its room, with what is drawn over it (in the transcript's paint, with the icons), and the number the next card has.
    card_slots: Vec<CardSlot>,
    card_next: u64,
    /// The plan that waits for the person's decision (Event::PlanReview).
    plan_review: Option<AskedPlan>,
    /// What this session works toward, as the agent last told it (Event::Goal).
    goal: Option<String>,
    /// The buttons on what waits above the input: where, which message, and what each does.
    queue_hits: Vec<(RECT, usize, QueueAct)>,
    /// Those buttons as their tips stand now.
    queue_tips: Vec<(RECT, QueueAct)>,
    /// The blocks of code in the transcript, each with a copy icon over it while in view.
    blocks: Vec<CodeBlock>,
    /// Tool calls in the transcript, one row to each run.
    runs: Vec<ToolRun>,
    /// The run still going on, at the end of the transcript.
    run_open: Option<usize>,
    /// A row is being written, which ends no run.
    run_writing: bool,
    /// Calls still running when something else was said: they go on in the next row.
    run_carry: Vec<RunEntry>,
    /// The person had to allow these: they stand on rows of their own.
    asked_calls: HashSet<String>,
    /// The run the pointer is on.
    run_hover: Option<usize>,
    /// A call being written, with no run to show it in.
    plain_writing: Option<RunEntry>,
    /// When a request under way last said how it was going.
    request_live: Option<std::time::Instant>,
    /// Where the block being written is kept in `shown`.
    open_block: Option<usize>,
    /// A block has begun and its first row is still to come: that row is where its card is found.
    block_card_due: bool,
    /// Where the last run of text went in the transcript.
    last_inserted: (i32, i32),
    /// The links in the transcript, in order: where they are and where they lead.
    links: Vec<(i32, i32, String)>,
    /// The link under the pointer.
    link_hover: Option<String>,
    /// The row of the panel on the right under the pointer.
    hover_row: Option<RECT>,
    /// How far a panel taller than the window has been scrolled, and how tall it is.
    panel_scroll: i32,
    panel_extent: i32,
    /// A dialog page's thumb, in its pane, with how high the pane is and how far it can scroll; and where it is held while it is dragged.
    pane_thumb: Option<(RECT, i32, i32)>,
    pane_thumb_grab: Option<i32>,
    /// What the panel says at its foot is something wrong.
    panel_problem: bool,
    /// Stop was pressed and the work has not ended yet.
    stop_asked: bool,
    /// The last try to connect failed: sending tries again, where for an engine of this app writing does (`engine_waits`).
    connect_failed: bool,
    /// The timer is what is connecting, so a failure it has seen is not said again.
    connect_retrying: bool,
    /// When the connection being made began, and when it first had something to wait for (the engine starting, or loading): for the trajectory.
    connect_began: Option<std::time::Instant>,
    connect_waiting_since: Option<std::time::Instant>,
    last_connect_error: String,
    /// What is in the composer is being put back: this is no one writing.
    restoring_draft: bool,
    /// When a key last went to the composer.
    composer_typed_at: Option<std::time::Instant>,
    // Pictures.
    attach_button: HWND,
    /// What goes with the next message, and the windows that show it.
    attachments: Vec<Attached>,
    attached_windows: Vec<HWND>,
    /// The pictures of the transcript's messages: drawn in its own paint, from the places that same frame gives their rooms.
    picture_blocks: Vec<PictureBlock>,
    /// The small copies the view wants now: they are not the ones to be forgotten.
    thumb_wanted: std::collections::HashSet<(PathBuf, (i32, i32))>,
    thumbs: HashMap<(PathBuf, (i32, i32)), KeptThumb>,
    thumb_clock: u64,
    /// Whether what is connected reads pictures; none until it has said.
    vision: Option<bool>,
    vision_told: bool,
    /// What the provider connected to offers, where it says: Codex.
    provider_models: Vec<String>,
    /// The folder a preview can reach, for a file outside the workspace.
    preview_root: Option<PathBuf>,
    tooltip: HWND,
    workspace: PathBuf,
    answer: String,
    last_answer: String,
    pending_text: String,
    /// What has arrived of the answer and is not on screen yet (`reveal_tick`).
    reveal: String,
    /// The reply's keys, hidden as they come in pieces: what is shown of it (the reply itself is kept as it was written).
    stream_mask: crate::mask::StreamMask,
    /// When text last went in from it.
    revealed_at: std::time::Instant,
    /// `reveal` is going in now, at the end of a transcript whose paragraphs before it stay as they are.
    revealing: bool,
    /// What goes in now is not kept in `shown`.
    unkept: bool,
    /// The transcript has changed size since text last began to go in.
    transcript_resized: bool,
    /// The transcript's text object model, for holding its layout while text goes in.
    transcript_document: *mut core::ffi::c_void,
    /// A session's transcript is going in, all at once.
    opening: bool,
    frames: Arc<FrameClock>,
    frame_thread: Option<std::thread::Thread>,
    /// The last frame, and how the following view is moving: its speed in pixels a second, and the part of a pixel not moved yet.
    frame_at: std::time::Instant,
    follow_speed: f32,
    follow_carried: f32,
    /// The view keeps to the end as text comes, until the person scrolls away from it.
    follow_end: bool,
    /// What a fine wheel has sent the transcript that has not added up to a pixel yet, in 120ths.
    wheel_rest: i32,
    /// When the person last did.
    scrolled_up_at: Option<std::time::Instant>,
    line_start: bool,
    /// Thinking has gone into the transcript and no answer has followed it yet: the answer begins on a line of its own, whether the thinking
    /// is shown by then or not.
    thinking_written: bool,
    in_code: bool,
    /// The level of the heading being written; none is 0.
    heading: u8,
    italic: bool,
    struck: bool,
    quote: bool,
    /// The character before the one being read, across pieces of a line.
    inline_before: Option<char>,
    para: Para,
    para_next: Option<Para>,
    code_comment: bool,
    table: Vec<String>,
    bold: bool,
    inline_code: bool,
    thinking: String,
    activity: String,
    /// When the work the activity line tells of began: what it shows, with a turning ring, so that waiting is seen to be waiting.
    activity_since: Option<std::time::Instant>,
    /// The seconds the activity line shows now.
    activity_second: u64,
    connected: bool,
    connection: String,
    /// The local engine is not started yet: opening the app costs no VRAM, RAM or GPU until the person begins to write.
    engine_waits: bool,
    /// When the engine last failed to start.
    engine_failed_at: Option<std::time::Instant>,
    /// The transcript has said the engine stopped: the turn and the timer each find out, in either order, and it is said once.
    engine_stop_told: bool,
    model: String,
    used: u64,
    window_tokens: u64,
    rate: Option<f64>,
    catalog: Vec<CatalogEntry>,
    /// This PC, as Straydog sees it, for who is choosing a model.
    machine_line: String,
    /// When this PC cannot run a model of its own.
    refusal: Option<Refusal>,
    /// Leaving this PC: under way, or done, and then nothing is written any more.
    leaving: Leaving,
    start_folders: Vec<PathBuf>,
    setting_up: bool,
    /// What the running setup has said of the whole, for one bar across its stages.
    setup_tally: engine::SetupTally,
    /// The chosen model's licence, as a link.
    licence_link: HWND,
    /// Ends the setup that is running.
    setup_cancel: Arc<AtomicBool>,
    /// Its last word, for when the panel is opened again.
    setup_status: String,
    selected_provider: Option<usize>,
    list_procedure: isize,
    transcript_procedure: isize,
    /// The transcript has asked to be told when the pointer leaves it.
    pointer_tracked: bool,
    /// About's line for the engine, once it has been read.
    engine_version: Option<String>,
    /// Where the session's name stands over the conversation, and when it was last clicked: twice names it.
    title_area: RECT,
    title_clicked: Option<std::time::Instant>,
    /// The folder the folder chip has said in full, on its tooltip.
    folder_told: PathBuf,
    /// What Always would do, when that is more than this one call.
    always_line: String,
    /// What the card says in sentences, above the command: why it asks, and the model's purpose. Drawn in the body face; the command is in the control under it, in a face of its own.
    approval_head: String,
    /// The page and kind of connection the fields' empty words are for.
    cues_for: Option<(Panel, Kind)>,
    doc_procedure: isize,
    note_procedure: isize,
    /// The control whose thin scrollbar's thumb is being dragged, and where on the thumb it was taken.
    bar_grab: Option<(HWND, i32)>,
    /// How fast the words of an answer are going in, in characters a second, and the part of one left over.
    reveal_rate: f32,
    reveal_carried: f32,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

/// Leaving this PC, from Settings.
enum Leaving {
    No,
    Going,
    /// What it comes to.
    Gone(String),
}

/// What a change of every conversation is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CryptJob {
    On,
    Off,
    /// A change that was stopped before it was done, finished at this start.
    FinishOn,
    FinishOff,
}

struct CryptWork {
    job: CryptJob,
    done: usize,
    total: usize,
}

/// What a change of every conversation came to, back from the thread that ran it.
struct CryptOutcome {
    job: CryptJob,
    /// The key, once there is one, even where the change did not go through.
    cipher: Option<Arc<Cipher>>,
    /// The recovery code, shown once.
    code: Option<String>,
    changed: usize,
    /// What could not be carried over, and was left out.
    lost: Vec<String>,
    error: Option<String>,
}

/// What the app holds of the encryption of conversations (`dogmind_agent::conversation_key`): the key file, and the key itself once it is unlocked, in memory only.
struct Crypt {
    keystore: Option<Keystore>,
    cipher: Option<Arc<Cipher>>,
    /// The key file is there and cannot be read: said at the start, and nothing is opened.
    damaged: Option<String>,
    /// At the unlock page: the recovery code is typed, with a new passphrase, in place of the passphrase.
    by_code: bool,
    work: Option<CryptWork>,
    /// What the work page says when nothing is going on, and the recovery code it shows once.
    said: String,
    said_is_a_problem: bool,
    code: Option<String>,
    /// The box "Remember on this PC", as the page that has it shows it.
    remember: bool,
    /// The session the copy before this one had open, held while the key is asked for.
    resume: Option<PathBuf>,
    /// Where the work page goes when it is done.
    back: Panel,
    /// What the start left for after the key: the look at what earlier runs left writable reads the conversations' workspaces.
    sweep_due: bool,
}

impl Crypt {
    /// What the data folder says: no key file, conversations as they are; one, and the key is asked for unless Windows has it for this user.
    fn open(folder: &Path) -> (Crypt, Arc<Policy>) {
        let mut crypt = Crypt { keystore: None, cipher: None, damaged: None, by_code: false, work: None, said: String::new(), said_is_a_problem: false, code: None, remember: true, resume: None, back: Panel::Chat, sweep_due: false };
        let policy = match Keystore::load(folder) {
            Err(KeyError::Missing) => Policy::off(),
            Err(error) => {
                crypt.damaged = Some(error.to_string());
                Policy::locked()
            }
            Ok(keystore) => {
                let policy = match keystore.unlock_remembered() {
                    Ok(cipher) => {
                        crypt.cipher = Some(cipher.clone());
                        Policy::on(cipher)
                    }
                    Err(_) => Policy::locked(),
                };
                crypt.remember = keystore.is_remembered();
                crypt.keystore = Some(keystore);
                policy
            }
        };
        crypt.sweep_due = crypt.is_locked();
        (crypt, policy)
    }

    /// Nothing is opened or written: the key is asked for, or its file cannot be read.
    fn is_locked(&self) -> bool {
        self.damaged.is_some() || (self.keystore.is_some() && self.cipher.is_none())
    }
}

/// How a download goes on after the network drops, before it says so and stops: an hour and more, in all, of a drop being waited out.
const SETUP_TRIES: u32 = 40;
const SETUP_RETRY_SECONDS: u64 = 90;

const PATH_TOO_LONG: &str = "This folder's path is too long for Windows to run a program in it, so commands will not start here. Files can still be read and edited. A folder with a shorter path has neither limit.";

/// Codex is someone else's program, and has to come from them. Not by way of `npm`, which a stranger has no reason to have.
const CODEX_MISSING: &str = "The Codex program is not on this PC. Install it from OpenAI (github.com/openai/codex), run  codex login  in a terminal, then press Continue.";

/// The row of the panel on the right that opens the dashboard, among those that open a changed file.

/// The default browser, for an address this app made.
unsafe fn open_in_browser(url: &str) {
    let (verb, target) = (wide("open"), wide(url));
    ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
}

fn agent_config(settings: &Settings, folder: &Path, workspace: PathBuf, mode: Mode, mcp: Arc<dogmind_agent::mcp::Hub>, policy: Arc<Policy>) -> agent::Config {
    // SAFETY: it takes nothing, and only reads a setting.
    let japanese_system = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() } & 0x3FF == 0x11;
    agent::Config {
        workspace,
        sessions: folder.join("sessions"),
        rules: folder.join("permissions.json"),
        mode,
        effort: settings.effort.clone(),
        web: web_settings(settings),
        language: settings.language.clone(),
        japanese_system,
        instructions: Some(folder.join("instructions.md")),
        home: std::env::var_os("USERPROFILE").map(PathBuf::from),
        skills_off: settings.skills_off.clone(),
        memory: Some(folder.join("memory")),
        mcp: Some(mcp),
        policy,
    }
}

/// What the agents read the web with. A key that cannot be read leaves the free indexes, and /status says so.
fn web_settings(settings: &Settings) -> WebSettings {
    let holder = ProviderConfig { name: "Search key".into(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: settings.search_key.trim().to_string() };
    WebSettings { enabled: settings.web_tools, search_key: holder.key().ok().flatten().filter(|key| !key.is_empty()), allow_local: false }
}

unsafe fn control(parent: HWND, class: &str, text: &str, style: u32, ex: u32, id: isize) -> HWND {
    let (class, text) = (wide(class), wide(text));
    // (none of them draws over a dialog that lies on top of it)
    CreateWindowExW(ex, class.as_ptr(), text.as_ptr(), WS_CHILD | WS_CLIPSIBLINGS | style, 0, 0, 0, 0, parent, id as HMENU, GetModuleHandleW(std::ptr::null()), std::ptr::null())
}

unsafe fn set_text(window: HWND, text: &str) {
    let text = wide(text);
    SetWindowTextW(window, text.as_ptr());
}

unsafe fn get_text(window: HWND) -> String {
    let length = GetWindowTextLengthW(window);
    let mut buffer = vec![0u16; length as usize + 1];
    let read = GetWindowTextW(window, buffer.as_mut_ptr(), length + 1);
    String::from_utf16_lossy(&buffer[..read.max(0) as usize]).replace("\r\n", "\n").replace('\r', "\n")
}

/// WM_SETREDRAW, on a window that is showing only. Turned back on it sets WS_VISIBLE, so a transcript hidden behind Settings would come up over
/// them with every word of an answer, until the next layout hid it again. A hidden window has nothing to draw anyway.
unsafe fn redraw(window: HWND, on: bool) {
    static WAS_SHOWING: AtomicBool = AtomicBool::new(false);
    if !on {
        WAS_SHOWING.store(GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_VISIBLE != 0, Ordering::Relaxed);
    }
    if WAS_SHOWING.load(Ordering::Relaxed) {
        SendMessageW(window, WM_SETREDRAW, usize::from(on), 0);
    }
}

unsafe fn show(window: HWND, visible: bool) {
    ShowWindow(window, if visible { SW_SHOWNA } else { SW_HIDE });
}

/// The words of the button of an update that is on its way or here. They follow the stage the update is in and what was asked of it, and never how far a fetch has come: a note of
/// progress changes the percent in the line above the button, and the button is not touched.
fn update_button_words_of(update: &ReleaseUpdate, setup_is_here: bool, checking_setup: bool, restart_when_done: bool) -> Option<&'static str> {
    match update {
        ReleaseUpdate::Fetching { .. } => Some(t("Cancel")),
        ReleaseUpdate::Ready(_) if setup_is_here => Some(if checking_setup { t("Checking…") } else if restart_when_done { t("Not now") } else { t("Restart to update") }),
        _ => None,
    }
}

fn update_button_kind_of(update: &ReleaseUpdate, restart_when_done: bool) -> ButtonKind {
    match update {
        ReleaseUpdate::Ready(_) if restart_when_done => ButtonKind::Secondary,
        ReleaseUpdate::Ready(_) => ButtonKind::Primary,
        _ => ButtonKind::Link,
    }
}

/// A debug build with `DOGMIND_TEST_ICONLOG` set to a file writes there, a line each and with the time in microseconds, what the transcript's host does to its pixels (moves them,
/// asks for a part of them again, draws a part) and where the icons stood in each frame it drew: what a test reads to say whether any frame had an icon away from its answer.
static TEST_LOG: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();

pub(crate) fn test_log_on() -> bool {
    cfg!(debug_assertions) && TEST_LOG.get_or_init(|| std::env::var_os("DOGMIND_TEST_ICONLOG").and_then(|path| std::fs::File::create(path).ok()).map(std::sync::Mutex::new)).is_some()
}

pub(crate) fn test_log(line: &str) {
    use std::io::Write;
    if !test_log_on() {
        return;
    }
    if let Some(Some(file)) = TEST_LOG.get() {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_micros());
        let _ = writeln!(file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()), "{at} {line}");
    }
}

/// Whether the icons are drawn in a colour that nothing else of the window has, for a test to find them by: a debug build with `DOGMIND_TEST_ICONCOLOR` set.
fn icons_marked() -> bool {
    static MARKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cfg!(debug_assertions) && *MARKED.get_or_init(|| std::env::var_os("DOGMIND_TEST_ICONCOLOR").is_some())
}

/// The transcript's host has drawn its text (and its fades) on `device`: the icons over the text are drawn on the same device now, at the places the text has in this frame.
pub(crate) unsafe fn paint_overlays(window: HWND, device: HDC, dx: i32, dy: i32) {
    let Some(app) = app() else { return };
    // (the composer and the side page are hosts of the same kind: the icons are over the transcript's text and no other's)
    if window == app.transcript {
        app.paint_overlay_icons(device, dx, dy);
    }
}

/// What the notice says, and the words on its button.
fn notice_text(shown: &notice::Notice) -> (String, String) {
    use notice::Notice;
    match shown {
        Notice::Page { version } => (tf("Dogmind {0} is available.", &[version]), t("Open the release page").to_string()),
        Notice::Fetching { version, percent } => (tf("Dogmind {0} is available.", &[version]), tf("Downloading… {0}%", &[percent])),
        Notice::UpdateNow { waiting: true, checking, .. } => (t("It restarts into the new version when this work is done.").to_string(), if *checking { t("Checking…") } else { t("Not now") }.to_string()),
        Notice::UpdateNow { version: Some(version), checking, .. } => (tf("Dogmind {0} is ready to install.", &[version]), if *checking { t("Checking…") } else { t("Update now (Dogmind restarts)") }.to_string()),
        Notice::UpdateNow { version: None, checking, .. } => (t("A new version is here.").to_string(), if *checking { t("Checking…") } else { t("Update now (Dogmind restarts)") }.to_string()),
    }
}

/// A control where it now goes. Without SWP_NOCOPYBITS the system moves its old pixels along and asks it to draw only what it thinks is new, and what
/// was covered or not yet drawn at the old place comes along as holes; one that has not moved is left alone.
unsafe fn place(window: HWND, x: i32, y: i32, width: i32, height: i32) {
    SetWindowPos(window, std::ptr::null_mut(), x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS);
}

/// Where a control stands in its parent, and how large it is.
unsafe fn child_area(window: HWND, parent: HWND) -> (i32, i32, i32, i32) {
    let mut area: RECT = std::mem::zeroed();
    GetWindowRect(window, &mut area);
    let mut corner = POINT { x: area.left, y: area.top };
    ScreenToClient(parent, &mut corner);
    (corner.x, corner.y, area.right - area.left, area.bottom - area.top)
}

/// How large the conversation's words are, in percent of the sizes written here: what the settings chose. The app's own chrome keeps its sizes, as its rows are laid out for them.
static TEXT_SCALE: AtomicU32 = AtomicU32::new(100);

pub(crate) fn text_scale() -> i32 {
    TEXT_SCALE.load(Ordering::Relaxed) as i32
}

fn char_format(color: u32, effects: u32, face: &str, points: i32, back: Option<u32>) -> CharFormat {
    let mut names = [0u16; 32];
    for (slot, unit) in names.iter_mut().zip(face.encode_utf16()) {
        *slot = unit;
    }
    CharFormat {
        size: std::mem::size_of::<CharFormat>() as u32,
        mask: CFM_COLOR | CFM_BOLD | CFM_ITALIC | CFM_UNDERLINE | CFM_STRIKEOUT | CFM_FACE | CFM_SIZE | CFM_BACKCOLOR,
        effects: effects | if back.is_none() { CFE_AUTOBACKCOLOR } else { 0 },
        height: points * 20 * text_scale() / 100,
        offset: 0,
        color,
        char_set: 1, // DEFAULT_CHARSET
        pitch_and_family: 0,
        face: names,
        weight: 0,
        spacing: 0,
        back_color: back.unwrap_or(0),
        lcid: 0,
        reserved: 0,
        style: 0,
        kerning: 0,
        underline_type: 0,
        animation: 0,
        rev_author: 0,
        underline_color: 0,
    }
}

/// Where a paragraph of an answer stands among the others.
#[derive(Clone, Copy, PartialEq)]
enum Para {
    Normal,
    /// An item of a list, this far in: its lines keep to its text, and the items stay together.
    Item(i32),
    AfterList,
    AfterBlock,
    Quote,
}

/// Paragraphs at a height of their own: left to itself RichEdit gives Yu Gothic UI nearly two lines' height for each.
fn para_format(points: i32, indent: i32, before: i32, after: i32) -> ParaFormat {
    // SAFETY: all zeros is a valid value of this plain structure.
    let mut format: ParaFormat = unsafe { std::mem::zeroed() };
    format.size = std::mem::size_of::<ParaFormat>() as u32;
    // (the offset too, at none: a paragraph after a list item would keep the item's hanging indent on its wrapped lines)
    // (and at the left, said each time: a new paragraph takes the one before it's, and a time stands at the right)
    format.mask = PFM_STARTINDENT | PFM_RIGHTINDENT | PFM_OFFSET | PFM_SPACEBEFORE | PFM_SPACEAFTER | PFM_LINESPACING | PFM_ALIGNMENT;
    format.alignment = PFA_LEFT;
    format.start_indent = indent;
    format.space_before = before;
    format.space_after = after;
    format.line_spacing = points * 32 * text_scale() / 100; // twips: 1.6 times the type
    format.line_spacing_rule = 4; // exactly
    format
}

/// A RichEdit's ITextDocument, as `EM_GETOLEINTERFACE` leads to it. Null where it has none.
unsafe fn text_document(edit: HWND) -> *mut core::ffi::c_void {
    let mut ole: *mut core::ffi::c_void = std::ptr::null_mut();
    if SendMessageW(edit, WM_USER + 60, 0, std::ptr::addr_of_mut!(ole) as LPARAM) == 0 || ole.is_null() {
        return std::ptr::null_mut();
    }
    // {8CC497C0-A1DF-11CE-8098-00AA0047BE5D}
    let iid = windows_sys::core::GUID { data1: 0x8CC4_97C0, data2: 0xA1DF, data3: 0x11CE, data4: [0x80, 0x98, 0x00, 0xAA, 0x00, 0x47, 0xBE, 0x5D] };
    let vtable = *(ole as *const *const usize);
    let query: unsafe extern "system" fn(*mut core::ffi::c_void, *const windows_sys::core::GUID, *mut *mut core::ffi::c_void) -> i32 = std::mem::transmute(*vtable);
    let release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 = std::mem::transmute(*vtable.add(2));
    let mut document: *mut core::ffi::c_void = std::ptr::null_mut();
    let found = query(ole, &iid, &mut document);
    release(ole);
    if found < 0 { std::ptr::null_mut() } else { document }
}

/// What `text_document` gave, given back.
unsafe fn release_document(document: *mut core::ffi::c_void) {
    if document.is_null() {
        return;
    }
    let vtable = *(document as *const *const usize);
    let release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 = std::mem::transmute(*vtable.add(2));
    release(document);
}

/// While text goes in piece by piece, the control lays nothing out: `ITextDocument::Freeze`, and `Unfreeze`.
unsafe fn hold_layout(document: *mut core::ffi::c_void, hold: bool) {
    if document.is_null() {
        return;
    }
    let vtable = *(document as *const *const usize);
    let call: unsafe extern "system" fn(*mut core::ffi::c_void, *mut i32) -> i32 = std::mem::transmute(*vtable.add(if hold { 18 } else { 19 }));
    let mut count = 0;
    call(document, &mut count);
}

/// A RichEdit of this class: Msftedit's window, or the windowless one drawn with Direct2D (`textview`), which falls back to the first where it cannot be made.
unsafe fn rich_edit_of(class: &str, parent: HWND, style: u32, background: u32, points: i32) -> HWND {
    let style = style | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_NOOLEDRAGDROP;
    let mut edit = control(parent, class, "", style, 0, 0);
    if edit.is_null() && class != "RICHEDIT50W" {
        edit = control(parent, "RICHEDIT50W", "", style, 0, 0);
    }
    SendMessageW(edit, EM_SETBKGNDCOLOR, 0, background as LPARAM);
    SendMessageW(edit, EM_EXLIMITTEXT, 0, 256 << 20);
    let format = char_format(theme::text(), 0, UI_FACE, points, None);
    SendMessageW(edit, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
    associate_japanese(edit, points);
    edit
}

/// What is Japanese in this control is in this face, whatever Latin one is set over it after: RichEdit keeps it through every later format. Left to itself it takes Yu Gothic, which is not the UI cut.
unsafe fn associate_japanese(edit: HWND, points: i32) {
    let mut format = char_format(theme::text(), 0, japanese_face(), points, None);
    format.mask = CFM_FACE | CFM_SIZE | CFM_CHARSET;
    format.char_set = 128; // SHIFTJIS_CHARSET
    SendMessageW(edit, EM_SETCHARFORMAT, SCF_ASSOCIATEFONT as WPARAM, std::ptr::addr_of!(format) as LPARAM);
}

impl App {
    /// Scrollbars and the like, in the theme.
    unsafe fn theme_controls(&self) {
        let light = theme::is_light();
        let name = wide(if light { "Explorer" } else { "DarkMode_Explorer" });
        for window in [self.transcript, self.composer, self.list, self.side, self.note, self.suggest, self.doc, self.tooltip] {
            allow_dark(window, !light);
            SetWindowTheme(window, name.as_ptr(), std::ptr::null());
        }
    }

    /// Windows, or the person, has changed the theme: everything that keeps a colour moves to it. The transcript is written again.
    unsafe fn apply_theme(&mut self) {
        let light = self.settings.is_light();
        if light == theme::is_light() {
            return;
        }
        theme::set_light(light);
        prefer_theme(light);
        title_bar(self.window);
        let old = self.brushes;
        self.brushes = [CreateSolidBrush(theme::background()), CreateSolidBrush(theme::sidebar()), CreateSolidBrush(theme::surface()), CreateSolidBrush(theme::raised())];
        SetClassLongPtrW(self.window, GCLP_HBRBACKGROUND, self.brushes[0] as isize);
        for brush in old {
            DeleteObject(brush);
        }
        self.theme_controls();
        retheme_buttons(self.window, light);
        for (edit, back, points) in [(self.transcript, theme::background(), 11), (self.composer, theme::surface(), 11), (self.doc, theme::background(), 10)] {
            SendMessageW(edit, EM_SETBKGNDCOLOR, 0, back as LPARAM);
            let format = char_format(theme::text(), 0, UI_FACE, points, None);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
        }
        for item in &mut self.shown {
            if let Shown::Row { color, back, .. } = item {
                *color = theme::translate(*color, light);
                *back = theme::translate(*back, light);
            }
        }
        if !self.modal_backdrop.is_null() {
            DeleteObject(self.modal_backdrop);
            self.modal_backdrop = std::ptr::null_mut();
        }
        self.layout();
        self.reflow();
        repaint_all(self.window);
        // (a page this app wrote for a file is in the theme's colours)
        if let Some(shown) = &self.preview {
            shown.retheme();
        }
    }

    /// The conversation's words at the size the settings now say: what is typed, the transcript (written again) and the page beside.
    unsafe fn apply_text_size(&mut self) {
        TEXT_SCALE.store(self.settings.text_percent(), Ordering::Relaxed);
        for (edit, points) in [(self.transcript, 11), (self.composer, 11), (self.doc, 10)] {
            let format = char_format(theme::text(), 0, UI_FACE, points, None);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            associate_japanese(edit, points);
        }
        self.layout();
        self.reflow();
        repaint_all(self.window);
    }

    /// Follows Windows, then dark, then light.
    unsafe fn next_theme(&mut self) {
        self.settings.theme = match self.settings.theme.as_str() {
            "system" => "dark",
            "dark" => "light",
            _ => "system",
        }.to_string();
        self.save();
        self.apply_theme();
        self.update_toggles();
    }

    /// Where the transcript's last character is, in its window.
    unsafe fn end_point(&self) -> POINT {
        let length = SendMessageW(self.transcript, EM_GETTEXTLENGTHEX, std::ptr::addr_of!(TEXT_LENGTH_CHARS) as WPARAM, 0);
        let mut last = POINT { x: 0, y: 0 };
        SendMessageW(self.transcript, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(last) as WPARAM, length);
        last
    }

    /// Frames from the next time the screen is composed.
    unsafe fn want_frames(&mut self) {
        if !self.frames.wanted.swap(true, Ordering::Relaxed) {
            self.frame_at = std::time::Instant::now();
        }
        match &self.frame_thread {
            Some(thread) => thread.unpark(),
            None => {
                let (window, clock) = (self.window as isize, self.frames.clone());
                self.frame_thread = Some(std::thread::spawn(move || frame_loop(window, clock)).thread().clone());
            }
        }
    }

    /// Text goes in at the end of the transcript, and the view stays where the person has it. To insert is to move the caret
    /// there, and that scrolls: nothing is drawn until the view is back, or the page jumps and flickers with every token.
    unsafe fn at_end(&mut self, insert: impl FnOnce(HWND)) {
        if !self.has_messages {
            self.has_messages = true;
            self.layout();
        }
        if self.run_open.is_some() && !self.run_writing && !self.replaying {
            self.close_run();
        }
        let edit = self.transcript;
        let mut bar: SCROLLINFO = std::mem::zeroed();
        bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        bar.fMask = SIF_ALL;
        GetScrollInfo(edit, SB_VERT, &mut bar);
        let at_bottom = bar.nPage == 0 || bar.nPos + bar.nPage as i32 + 4 >= bar.nMax;
        let end_in_view = bar.nPage == 0 || bar.nPos + 2 * bar.nPage as i32 >= bar.nMax;
        let mut position = POINT { x: 0, y: 0 };
        SendMessageW(edit, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        let mut selection = CharRange { first: 0, last: 0 };
        SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(selection) as LPARAM);
        if self.replaying || self.opening {
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            insert(edit);
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            return;
        }
        // An answer going in changes its last line and what is below it. A last word or a closing mark can pull in the end of the line before, and no
        // more: a line of the largest type and the room above it are as far up as anything can have moved. Unless the text has grown out of
        // the window with this: its scrollbar comes, and every line above is wrapped again.
        let changed_from = self.revealing.then(|| self.end_point().y - scale(self.dpi, 48));
        self.transcript_resized = false;
        redraw(edit, false);
        hold_layout(self.transcript_document, true);
        SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
        insert(edit);
        SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(selection) as LPARAM);
        hold_layout(self.transcript_document, false);
        // The view stays where it was: the frames (reveal_tick) bring it down, a little at a time.
        SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        redraw(edit, true);
        // (the wheel takes the view away over some frames: until it has gone, it is still at the end)
        if at_bottom && self.scrolled_up_at.map_or(true, |at| at.elapsed() > SCROLL_UP_TAKES) {
            self.follow_end = true;
        }
        if self.follow_end {
            self.want_frames();
        }
        self.place_thumb();
        // Far above the end, what is on screen has not changed: only the scrollbar has.
        if at_bottom || end_in_view || self.follow_end {
            match changed_from.filter(|_| !self.transcript_resized) {
                Some(top) => {
                    let mut changed: RECT = std::mem::zeroed();
                    GetClientRect(edit, &mut changed);
                    changed.top = top.max(0);
                    if changed.top < changed.bottom {
                        InvalidateRect(edit, &changed, 0);
                    }
                }
                None => {
                    InvalidateRect(edit, std::ptr::null(), 0);
                }
            }
        } else {
            RedrawWindow(edit, std::ptr::null(), std::ptr::null_mut(), RDW_FRAME | RDW_INVALIDATE | RDW_NOERASE | RDW_NOCHILDREN | RDW_VALIDATE);
        }
    }

    unsafe fn append(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        // What acts on the text and shows nothing (a direction override, a zero width mark) is shown as its mark, in the muted colour, and does not reach the control: the rest of a line
        // is not shown backwards because of something a model wrote.
        if markdown::has_marks(text) {
            for (part, is_mark) in markdown::with_marks(text) {
                self.append(&part, if is_mark { Style::Quote } else { style });
            }
            return;
        }
        if !self.replaying && !self.unkept {
            match self.shown.last_mut() {
                Some(Shown::Text(held, kind, para)) if *kind == style && *para == self.para => held.push_str(text),
                _ => self.shown.push(Shown::Text(text.to_string(), style, self.para)),
            }
        }
        let (ui, mono) = (UI_FACE, "Consolas");
        let format = match style {
            Style::Body => char_format(theme::text(), 0, ui, 11, None),
            Style::Strong => char_format(theme::text(), CFE_BOLD, ui, 11, None),
            Style::Emphasis => char_format(theme::text(), CFE_ITALIC, ui, 11, None),
            Style::Struck => char_format(theme::muted(), CFE_STRIKEOUT, ui, 11, None),
            Style::Quote => char_format(theme::muted(), 0, ui, 11, None),
            Style::Link => char_format(theme::accent_hover(), CFE_UNDERLINE, ui, 11, None),
            Style::Title => char_format(theme::text(), CFE_BOLD, ui, 15, None),
            Style::Gap => char_format(theme::background(), 0, ui, 6, None),
            Style::Heading => char_format(theme::text(), CFE_BOLD, ui, 13, None),
            Style::Subheading => char_format(theme::text(), CFE_BOLD, ui, 12, None),
            Style::InlineCode => char_format(theme::accent_hover(), 0, mono, 10, None),
            Style::Thinking => char_format(theme::faint(), CFE_ITALIC, ui, 10, None),
            Style::Tool => char_format(theme::muted(), 0, ui, 9, None),
            Style::CopyRow => char_format(theme::background(), 0, ui, 13, None),
            Style::SaidAt | Style::AnsweredAt => char_format(theme::faint(), 0, ui, 9, None),
            Style::Output => char_format(theme::faint(), 0, mono, 9, None),
            Style::Warn => char_format(theme::warn(), 0, ui, 10, None),
            Style::Bad => char_format(theme::bad(), 0, ui, 10, None),
            Style::Said => char_format(theme::muted(), 0, ui, 9, None),
        };
        let paragraph = match style {
            Style::Output => para_format(9, 240, 0, 0),
            Style::Tool => para_format(9, 0, 60, 40),
            Style::SaidAt => {
                let mut said = para_format(9, 0, 60, 40);
                said.alignment = PFA_RIGHT;
                // (at the card's right edge: the control's text reaches past it by the room it keeps for its own bar)
                said.mask |= PFM_RIGHTINDENT;
                said.right_indent = GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi) * 3 / 5 * 1440 / self.dpi as i32;
                said
            }
            Style::CopyRow | Style::AnsweredAt => {
                let mut row = para_format(11, 0, 0, 120);
                row.mask |= PFM_TABSTOPS;
                row.tab_count = 1;
                row.tabs[0] = ANSWERED_AT_TAB;
                row
            }
            Style::Title => para_format(15, 0, 160, 100),
            Style::Gap => para_format(6, 0, 0, 0),
            Style::Heading => para_format(13, 0, 240, 80),
            Style::Subheading => para_format(12, 0, 180, 60),
            Style::Thinking | Style::Warn | Style::Bad => para_format(10, 0, 0, 100),
            Style::Said => para_format(9, 0, 0, 100),
            _ => match self.para {
                Para::Normal => para_format(11, 0, 0, 120),
                Para::AfterList => para_format(11, 0, 80, 120),
                Para::AfterBlock => para_format(11, 0, 300, 120),
                Para::Quote => para_format(11, 280, 40, 40),
                Para::Item(indent) => {
                    const TEXT_AT: i32 = 280;
                    let start = indent * 130;
                    let mut item = para_format(11, start, 0, 40);
                    item.mask |= PFM_OFFSET | PFM_TABSTOPS;
                    item.offset = TEXT_AT;
                    item.tab_count = 1;
                    item.tabs[0] = start + TEXT_AT;
                    item
                }
            },
        };
        let text = wide(text);
        let mut inserted = (0, 0);
        self.at_end(|edit| {
            let mut range = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
            SendMessageW(edit, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
            SendMessageW(edit, EM_REPLACESEL, 0, text.as_ptr() as LPARAM);
            // On what went in, as a format set on the caret does not always outlive a row of another font before it.
            let mut end = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(end) as LPARAM);
            range.last = end.last;
            SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            inserted = (range.first, range.last);
        });
        self.last_inserted = inserted;
    }

    /// Words that lead somewhere, in the transcript. What they lead to shows at the foot of the window while the pointer is on them.
    unsafe fn append_link(&mut self, words: &str, url: &str) {
        if !self.replaying {
            self.shown.push(Shown::Link(words.to_string(), url.to_string(), self.para));
        }
        self.link_run(words, url);
    }

    unsafe fn link_run(&mut self, words: &str, url: &str) {
        let held = std::mem::replace(&mut self.unkept, true);
        self.append(words, Style::Link);
        self.unkept = held;
        self.links.push((self.last_inserted.0, self.last_inserted.1, url.to_string()));
    }

    /// The pointer has moved on the transcript: whether it is on a link, and which.
    unsafe fn track_link(&mut self, lparam: LPARAM) {
        let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        let at_point = POINT { x, y };
        let at = SendMessageW(self.transcript, EM_CHARFROMPOS_RICH, 0, std::ptr::addr_of!(at_point) as LPARAM) as i32;
        let mut near = POINT { x: 0, y: 0 };
        SendMessageW(self.transcript, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(near) as WPARAM, at as LPARAM);
        // (the character nearest a pointer in empty space is not under it)
        let reach = scale(self.dpi, 32);
        let close = (x - near.x).abs() < reach && y >= near.y - scale(self.dpi, 4) && y < near.y + reach;
        self.run_hover = self.run_under(lparam);
        let found = match self.run_hover {
            Some(run) => Some(self.runs[run].hover(&self.workspace)).filter(|words| !words.is_empty()),
            None => close.then(|| link_at(&self.links, at)).flatten().map(str::to_string),
        };
        if found != self.link_hover {
            if self.link_hover.is_none() {
                let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.transcript, dwHoverTime: 0 };
                TrackMouseEvent(&mut track);
            }
            self.link_hover = found;
            InvalidateRect(self.window, &self.status_area, 0);
        }
    }

    /// The run whose first line is under this point of the transcript.
    unsafe fn run_under(&self, lparam: LPARAM) -> Option<usize> {
        if self.runs.is_empty() {
            return None;
        }
        let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        let at_point = POINT { x, y };
        let at = SendMessageW(self.transcript, EM_CHARFROMPOS_RICH, 0, std::ptr::addr_of!(at_point) as LPARAM) as i32;
        let run = self.runs.iter().rposition(|run| run.first >= 0 && run.first <= at && at <= run.head_end)?;
        // (the character nearest a pointer in empty space is not under it)
        let mut near = POINT { x: 0, y: 0 };
        SendMessageW(self.transcript, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(near) as WPARAM, at as LPARAM);
        let reach = scale(self.dpi, 32);
        ((x - near.x).abs() < reach && y >= near.y - scale(self.dpi, 4) && y < near.y + reach).then_some(run)
    }

    /// A click on a link: to the browser if it is on the web, beside the conversation if it is a file here (at the lines it names, as
    /// `src/app.js#L24`), and nowhere otherwise.
    unsafe fn open_link(&mut self, url: &str) {
        match markdown::web_address(url) {
            Some(address) => open_in_browser(&address),
            None => {
                let (path, lines) = markdown::lines_of_link(url);
                if self.workspace.join(path).is_file() {
                    self.open_preview_at(path, lines);
                } else if self.workspace.join(url).is_file() {
                    self.open_preview(url);
                }
            }
        }
    }

    /// One row of a shaded block.
    unsafe fn append_row(&mut self, row: &rtf::Row) {
        if !self.replaying {
            self.shown.push(Shown::Row { lines: row.lines.iter().map(|line| line.to_string()).collect(), face: row.face.to_string(), points: row.points, color: row.color, back: row.back, width: row.right.then_some(row.width), right: row.right, padding: row.padding });
        }
        let first = self.insert_rtf(rtf::row(row), Some(row.color));
        if !self.replaying && !self.opening {
            if let Some(pause) = test_rows_painted() {
                RedrawWindow(self.transcript, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_UPDATENOW);
                std::thread::sleep(std::time::Duration::from_millis(pause));
            }
        }
        if row.right {
            let start = text_start(self.transcript, first);
            // Words under the message's pictures are the lower part of one card, which begins at the pictures: two cards, each rounded, were
            // cut in the middle where they met.
            let after_pictures = std::mem::take(&mut self.turn_words_due);
            if !after_pictures {
                self.user_cards.push(UserCard { start, kind: CardKind::Person, shape: None, looked_at: None });
            }
            let prompt = rail_words(&row.lines.join(" "), 50);
            match self.rail_turns.last_mut() {
                Some(turn) if after_pictures => turn.prompt = prompt,
                _ => self.rail_turns.push(TurnMark { start, prompt }),
            }
            // (a whole conversation opened, or written again, has the rail placed once it is all in; the second turn's card makes room for it,
            // when it is in: show_user)
            if !self.replaying && !self.opening {
                self.place_rail();
            }
        } else if std::mem::take(&mut self.block_card_due) {
            // (a block's first row: the block's corners are rounded as a card's are, from where its first line begins)
            let start = text_start(self.transcript, first);
            self.user_cards.push(UserCard { start, kind: CardKind::Block, shape: None, looked_at: None });
        }
    }

    /// A table's cell is square, and RichEdit has no other: what is left of the page goes over the corners of each card in
    /// view. Where a card is comes from its pixels, as the control alone knows its row's edges. They are looked for once, as this runs for
    /// every paint and a pixel read from a window costs a tenth of a millisecond or more; a card seen whole keeps its shape from then on, and its
    /// corners are only drawn where this paint, whose area is `dirty`, can have drawn over them: when the view scrolls they move with the rest.
    unsafe fn round_user_cards(&mut self, given: HDC, dirty: RECT) {
        if self.user_cards.is_empty() || self.panel != Panel::Chat {
            return;
        }
        let edit = self.transcript;
        let device = if given.is_null() { GetDC(edit) } else { given };
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(edit, &mut client);
        let radius = scale(self.dpi, ds::radius::CARD_CORNER);
        let mut view: Option<ViewPixels> = None;
        for user_card in self.user_cards.iter_mut().rev() {
            let back = user_card.kind.back();
            let mut point = POINT { x: 0, y: 0 };
            SendMessageW(edit, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(point) as WPARAM, user_card.start as LPARAM);
            if point.y < client.bottom {
                // A card that has grown since its shape was taken (the rows of a command's output keep coming) has the card's colour at its old foot and under it: the corners drawn at that foot
                // are in the card now, and are filled with it; and the card is looked for again, as it is now.
                if let Some(shape) = user_card.shape {
                    let card = shape.at(point);
                    let x = card.left + radius;
                    if test_log_on() {
                        test_log(&format!("card kept {},{},{},{} foot pixels {:06x} {:06x}", card.left, card.top, card.right, card.bottom, GetPixel(device, x, card.bottom), GetPixel(device, x, card.bottom + 1)));
                    }
                    if !test_stale_shapes() && card.bottom >= client.top && card.bottom + 1 < client.bottom && card_has_grown(GetPixel(device, x, card.bottom), GetPixel(device, x, card.bottom + 1), back) {
                        let brush = CreateSolidBrush(back);
                        for area in foot_corners(&card, radius) {
                            FillRect(device, &area, brush);
                        }
                        DeleteObject(brush);
                        user_card.shape = None;
                        user_card.looked_at = None;
                    }
                }
                let found = match user_card.shape {
                    Some(shape) => {
                        let mut card = shape.at(point);
                        card.top_open = !overlaps(&RECT { left: card.left, top: card.top, right: card.right + 1, bottom: card.top + radius }, &dirty);
                        card.bottom_open = !overlaps(&RECT { left: card.left, top: card.bottom + 1 - radius, right: card.right + 1, bottom: card.bottom + 1 }, &dirty);
                        // (selected, it is not the colour of a card, and is left square as it was)
                        let middle = (card.top.max(client.top) + card.bottom.min(client.bottom - 1)) / 2;
                        let in_view = !(card.top_open && card.bottom_open) && card.bottom >= client.top && GetPixel(device, point.x - 2, middle) == back;
                        in_view.then_some((card, shape.page))
                    }
                    // (one pixel says when it is above the view, which is where it mostly is by now)
                    None if GetPixel(device, point.x - 2, point.y.max(client.top)) != back => None,
                    None if user_card.looked_at.is_some_and(|at| at.elapsed().as_millis() < 250) => None,
                    None => {
                        user_card.looked_at = Some(std::time::Instant::now());
                        let view = view.get_or_insert_with(|| ViewPixels::of(device, client));
                        find_card(client, point, radius, scale(self.dpi, 3), |x, y| view.at(x, y) == back).map(|mut card| {
                            let page = view.at(card.left - 1, card.top + radius);
                            // With the view at the top of the text, the top of the view is the top of a card that reaches it.
                            let mut scrolled = POINT { x: 0, y: 0 };
                            SendMessageW(edit, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(scrolled) as LPARAM);
                            card.top_open = card.top_open && scrolled.y > 0;
                            // (what goes on below where the card was found to end, or above where it begins, in a colour that is not the page's, is the card in the fade at an edge of the view: it is
                            // not rounded there, as its end is not in sight)
                            card.bottom_open |= card.bottom + 1 < client.bottom && view.at(card.left + radius, card.bottom + 1) != page;
                            card.top_open |= card.top > client.top && view.at(card.left + radius, card.top - 1) != page;
                            if test_log_on() {
                                test_log(&format!("card found {},{},{},{} top_open={} bottom_open={}", card.left, card.top, card.right, card.bottom, card.top_open, card.bottom_open));
                            }
                            if !card.top_open && !card.bottom_open {
                                user_card.shape = Some(CardShape::around(&card, point, page));
                            }
                            (card, page)
                        })
                    }
                };
                if let Some((card, page)) = found {
                    for j in 0..radius {
                        for i in 0..radius {
                            let cover = corner_cover(radius, i, j);
                            if cover < 1.0 {
                                let color = mix(page, back, cover);
                                if !card.top_open {
                                    SetPixelV(device, card.left + i, card.top + j, color);
                                    SetPixelV(device, card.right - i, card.top + j, color);
                                }
                                if !card.bottom_open {
                                    SetPixelV(device, card.left + i, card.bottom - j, color);
                                    SetPixelV(device, card.right - i, card.bottom - j, color);
                                }
                            }
                        }
                    }
                }
            }
            // Those above this one have scrolled out of view.
            if point.y < 0 {
                break;
            }
        }
        if given.is_null() {
            ReleaseDC(edit, device);
        }
    }

    /// RTF at the end of the transcript, its text in this colour.
    unsafe fn insert_rtf(&mut self, rtf: String, text_color: Option<u32>) -> i32 {
        let bytes: Vec<u8> = rtf.into_bytes().into_iter().chain(std::iter::once(0)).collect();
        let how = SetTextEx { flags: ST_SELECTION, code_page: 65001 };
        let mut color = char_format(text_color.unwrap_or(0), 0, "", 0, None);
        color.mask = CFM_COLOR;
        let mut first = 0;
        self.at_end(|edit| {
            let mut range = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
            first = range.first;
            SendMessageW(edit, EM_SETTEXTEX, std::ptr::addr_of!(how) as WPARAM, bytes.as_ptr() as LPARAM);
            // The control keeps a row's shading from its RTF and not the colour of its text: that is set on what went in.
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            let mut end = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(end) as LPARAM);
            range.last = end.last;
            if text_color.is_some() {
                SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
                SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(color) as LPARAM);
            }
        });
        first
    }

    /// The transcript again, at the column's width now, with the view where it was.
    unsafe fn reflow(&mut self) {
        if self.shown.is_empty() {
            return;
        }
        let edit = self.transcript;
        let mut bar: SCROLLINFO = std::mem::zeroed();
        bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        bar.fMask = SIF_ALL;
        GetScrollInfo(edit, SB_VERT, &mut bar);
        let at_bottom = bar.nPage == 0 || bar.nPos + bar.nPage as i32 + 4 >= bar.nMax;
        let share = f64::from(bar.nPos) / f64::from(bar.nMax.max(1));
        let held = std::mem::take(&mut self.shown);
        let para = self.para;
        self.replaying = true;
        redraw(edit, false);
        hold_layout(self.transcript_document, true);
        set_text(edit, "");
        self.answers.clear();
        self.user_cards.clear();
        self.rail_turns.clear();
        self.turn_words_due = false;
        self.picture_blocks.clear();
        self.blocks.clear();
        self.card_slots.clear();
        self.links.clear();
        self.link_hover = None;
        self.runs.iter_mut().for_each(|run| run.first = -1);
        for item in &held {
            match item {
                Shown::Text(text, style, para) => {
                    self.para = *para;
                    self.append(text, *style);
                }
                Shown::Link(words, url, para) => {
                    self.para = *para;
                    self.link_run(words, url);
                }
                Shown::Row { lines, face, points, color, back, width, right, padding } => {
                    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
                    // (a card wider than the column has become would stand out of it to the left)
                    let width = width.map_or(self.text_twips, |width| width.min((self.text_twips * 4 / 5).max(800)));
                    self.append_row(&rtf::Row { lines: &lines, face, points: *points, color: *color, back: *back, width, right: *right, column: self.text_twips, padding: *padding });
                }
                Shown::Answer(text, at) => self.answer_row(text, at),
                Shown::Table(rows) => self.table_rows(rows),
                Shown::Code { line, language, open } => {
                    self.code_row(line, language, *open);
                }
                Shown::Pictures { pictures, width, alone, right } => self.append_pictures(pictures, (*width).min((self.text_twips * 4 / 5).max(800)), *alone, *right),
                Shown::Run(at) => self.write_run(*at),
                Shown::Card(card) => self.write_card(card),
                Shown::Block(code) => {
                    self.begin_block();
                    if let Some(block) = self.blocks.last_mut() {
                        block.text = code.clone();
                    }
                }
            }
        }
        self.replaying = false;
        self.para = para;
        self.shown = held;
        hold_layout(self.transcript_document, false);
        GetScrollInfo(edit, SB_VERT, &mut bar);
        if at_bottom {
            SendMessageW(edit, WM_VSCROLL, SB_BOTTOM as WPARAM, 0);
        } else {
            let mut position = POINT { x: 0, y: (share * f64::from(bar.nMax)) as i32 };
            SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        }
        redraw(edit, true);
        InvalidateRect(edit, std::ptr::null(), 0);
    }

    /// The lines of a Markdown table have ended: as a table if they are one, and as they were written if they are not.
    unsafe fn flush_table(&mut self) {
        let lines = std::mem::take(&mut self.table);
        if lines.is_empty() {
            return;
        }
        let parsed: Vec<Vec<String>> = lines.iter().filter_map(|line| rtf::table_cells(line)).collect();
        if parsed.len() < 2 || !rtf::is_table_rule(&parsed[1]) {
            for line in &lines {
                self.append(&format!("{line}\n"), Style::Body);
            }
            return;
        }
        let rows: Vec<Vec<String>> = parsed.into_iter().enumerate().filter(|(at, _)| *at != 1).map(|(_, row)| row).collect();
        self.gap_after_a_block();
        if !self.replaying {
            self.shown.push(Shown::Table(rows.clone()));
        }
        self.table_rows(&rows);
        self.para_next = Some(Para::AfterBlock);
    }

    unsafe fn table_rows(&mut self, rows: &[Vec<String>]) {
        const POINTS: i32 = 10;
        let ends = rtf::table_ends(rows, POINTS, self.text_twips);
        for (at, cells) in rows.iter().enumerate() {
            let row = rtf::TableRow { cells, ends: &ends, face: UI_FACE, points: POINTS, color: theme::text(), back: if at == 0 { theme::code_back() } else { theme::background() }, line: theme::line(), bold: at == 0 };
            self.insert_rtf(rtf::table_row(&row), Some(theme::text()));
        }
    }

    /// A line of a block of code or of a command's output.
    unsafe fn block_line(&mut self, line: &str, color: u32, points: i32) {
        let shown = if line.is_empty() { " " } else { line };
        self.append_row(&rtf::Row { lines: &[shown], face: "Consolas", points, color, back: theme::code_back(), width: self.text_twips, right: false, column: 0, padding: (260, 26) });
    }

    /// Before a block or a table: right after another they would read as one, as rows of a table join.
    unsafe fn gap_after_a_block(&mut self) {
        if matches!(self.shown.last(), Some(Shown::Row { .. } | Shown::Code { .. } | Shown::Table(_))) {
            self.append("\n", Style::Gap);
        }
    }

    /// Room over a block of code or of a command's output: after another block, as for a table, and after words too, whose paragraph leaves
    /// little under it (a list's item hardly any), so that the block stood tight under them. Not under a run of calls, whose output it is.
    unsafe fn room_over_a_block(&mut self) {
        let room = match self.shown.last() {
            Some(Shown::Row { .. } | Shown::Code { .. } | Shown::Table(_) | Shown::Link(..)) => true,
            Some(Shown::Text(text, style, _)) => *style != Style::Gap && !text.trim().is_empty(),
            _ => false,
        };
        if room {
            self.append("\n", Style::Gap);
        }
    }

    /// A block is about to begin here.
    unsafe fn begin_block(&mut self) {
        let mut first = 0;
        self.at_end(|edit| {
            let mut range = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
            first = range.first;
        });
        self.blocks.push(CodeBlock { at: first, found: false, text: String::new() });
        self.block_card_due = true;
        if !self.replaying {
            self.shown.push(Shown::Block(String::new()));
            self.open_block = Some(self.shown.len() - 1);
        }
    }

    unsafe fn copy_block(&mut self, at: usize) {
        let Some(block) = self.blocks.get(at) else { return };
        let text = block.text.clone();
        self.copy_to_clipboard(text.trim_end_matches('\n'));
        self.copied(OverlayKey::Block(at));
    }

    /// What opens and closes a block: a little space, and what the code is in if it says.
    unsafe fn block_edge(&mut self, label: &str) {
        let shown = if label.is_empty() { " " } else { label };
        let points = if label.is_empty() { 6 } else { 8 };
        // (room enough over and under the lines, which the rounded corners also take some of)
        self.append_row(&rtf::Row { lines: &[shown], face: UI_FACE, points, color: theme::muted(), back: theme::code_back(), width: self.text_twips, right: false, column: 0, padding: (260, 80) });
    }

    /// A line of code in an answer; a diff in its colours.
    unsafe fn code_line(&mut self, line: &str) {
        if let Some(block) = self.blocks.last_mut() {
            block.text.push_str(line);
            block.text.push('\n');
        }
        if let Some(Shown::Block(code)) = self.open_block.and_then(|at| self.shown.get_mut(at)) {
            code.push_str(line);
            code.push('\n');
        }
        let diff = matches!(self.code_language.as_str(), "diff" | "patch");
        let color = match line.chars().next() {
            _ if diff && (line.starts_with("+++") || line.starts_with("---")) => theme::muted(),
            Some('+') if diff => theme::good(),
            Some('-') if diff => theme::bad(),
            Some('@') if diff => theme::accent_hover(),
            _ => theme::code(),
        };
        if diff {
            self.block_line(line, color, 10);
            return;
        }
        if !self.replaying {
            self.shown.push(Shown::Code { line: line.to_string(), language: self.code_language.clone(), open: self.code_comment });
        }
        let language = self.code_language.clone();
        self.code_comment = self.code_row(line, &language, self.code_comment);
    }

    /// One line of code, in the colours of its language when it has any. Returns whether a block comment runs on past it.
    unsafe fn code_row(&mut self, line: &str, language: &str, open: bool) -> bool {
        use crate::highlight::{self, Kind};
        let shown = if line.is_empty() { " " } else { line };
        let Some((runs, still_open)) = highlight::runs(shown, language, open) else {
            let row = rtf::Row { lines: &[shown], face: "Consolas", points: 10, color: theme::code(), back: theme::code_back(), width: self.text_twips, right: false, column: 0, padding: (260, 26) };
            // (its place in `shown` is the line itself)
            self.insert_rtf(rtf::row(&row), Some(row.color));
            return false;
        };
        let colored: Vec<(String, u32)> = runs.into_iter().map(|(text, kind)| {
            let color = match kind {
                Kind::Plain => theme::code(),
                Kind::Keyword => theme::accent_hover(),
                Kind::Text => theme::good(),
                Kind::Comment => theme::muted(),
                Kind::Number => theme::warn(),
            };
            (text, color)
        }).collect();
        // Its colours are in the RTF: nothing is set on it afterwards.
        self.insert_rtf(rtf::colored_row(&colored, "Consolas", 10, theme::code_back(), self.text_twips, (260, 26)), None);
        still_open
    }

    /// The caret stays out of the composer's way: its hint lies over the first line, and what an IME is composing goes there.
    unsafe fn update_composer_cue(&self) {
        let empty = GetWindowTextLengthW(self.composer) == 0;
        show(self.composer_cue, self.panel == Panel::Chat && empty && !self.composing);
    }

    /// An answer as it streams, with its Markdown rendered: what a marker means is known once the line has begun, so a
    /// line's first characters wait until they can be told.
    unsafe fn append_answer(&mut self, text: &str) {
        // A run ends where an answer begins, before its first word is read: going in, that word would close the run and be taken for the end of a line, and what came next
        // would start a line of its own, and lose the heading it was in.
        if self.run_open.is_some() && !self.run_writing && !self.replaying && !text.is_empty() {
            self.close_run();
        }
        self.pending_text.push_str(text);
        loop {
            let pending = std::mem::take(&mut self.pending_text);
            if pending.is_empty() {
                return;
            }
            if self.line_start {
                if !self.in_code && !self.table.is_empty() && !pending.trim_start_matches([' ', '\t']).starts_with('|') {
                    // Anything else on the line, a blank one too, ends the table.
                    self.flush_table();
                }
                if !self.in_code && pending.starts_with('\n') {
                    self.pending_text = pending[1..].to_string();
                    continue;
                }
                let newline = pending.find('\n');
                let head = newline.map_or(pending.as_str(), |at| &pending[..at]);
                let trimmed = head.trim_start();
                if newline.is_none() && markdown::undecided(trimmed) {
                    self.pending_text = pending;
                    return;
                }
                if trimmed.starts_with("```") {
                    let Some(at) = newline else {
                        self.pending_text = pending;
                        return;
                    };
                    self.in_code = !self.in_code;
                    self.pending_text = pending[at + 1..].to_string();
                    if self.in_code {
                        self.code_language = trimmed.trim_start_matches('`').trim().to_lowercase();
                        self.code_comment = false;
                        let label = self.code_language.clone();
                        self.room_over_a_block();
                        self.begin_block();
                        self.block_edge(&label);
                    } else {
                        self.block_edge("");
                        self.open_block = None;
                        self.para_next = Some(Para::AfterBlock);
                    }
                    continue;
                }
                if !self.in_code && newline.is_some() && markdown::is_rule(trimmed) {
                    self.pending_text = pending[newline.unwrap_or(0) + 1..].to_string();
                    self.rule_row();
                    continue;
                }
                if !self.in_code && trimmed.starts_with('|') {
                    // A table is drawn when it has ended, as a column's width depends on all its rows.
                    let Some(at) = newline else {
                        self.pending_text = pending;
                        return;
                    };
                    self.table.push(pending[..at].trim_end_matches('\r').to_string());
                    self.pending_text = pending[at + 1..].to_string();
                    continue;
                }
                if self.in_code {
                    // A block is added to by the row, so a line waits until it is whole.
                    let Some(at) = newline else {
                        self.pending_text = pending;
                        return;
                    };
                    let line = pending[..at].trim_end_matches('\r').to_string();
                    self.pending_text = pending[at + 1..].to_string();
                    self.code_line(&line);
                    continue;
                }
                self.line_start = false;
                self.heading = 0;
                self.inline_before = None;
                if !self.in_code {
                    let indent = pending.len() - pending.trim_start().len();
                    let marker = list_marker(trimmed);
                    if marker.is_none() {
                        // What follows a list or a block keeps a little away from it, for this one paragraph.
                        self.para = self.para_next.take().unwrap_or(if matches!(self.para, Para::Item(_)) { Para::AfterList } else { Para::Normal });
                    }
                    if let Some(level) = markdown::heading_level(trimmed) {
                        self.heading = level;
                        self.pending_text = pending[indent + level as usize..].trim_start_matches(' ').to_string();
                        continue;
                    }
                    if trimmed.starts_with('>') {
                        self.quote = true;
                        self.para = Para::Quote;
                        self.pending_text = markdown::quoted(&pending[indent..]).unwrap_or("").to_string();
                        // (an empty line of a quotation is a line)
                        if self.pending_text.is_empty() {
                            return;
                        }
                        continue;
                    }
                    if let Some((shown, length)) = marker {
                        self.para_next = None;
                        self.para = Para::Item(indent as i32);
                        self.append(&format!("{shown}\t"), Style::Body);
                        self.pending_text = pending[indent + length..].to_string();
                        continue;
                    }
                }
            }
            self.emit_inline(&pending);
            if self.pending_text.is_empty() || !self.line_start {
                return;
            }
        }
    }

    /// Text inside a line, up to its end: bold, emphasis, code and links lose their marks. What follows the line goes back to wait, and so does what cannot be told yet.
    unsafe fn emit_inline(&mut self, text: &str) {
        let mut run = String::new();
        let mut chars = text.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            if c == '\n' {
                run.push('\n');
                self.flush_run(&mut run);
                self.line_start = true;
                self.bold = false;
                self.italic = false;
                self.struck = false;
                self.inline_code = false;
                self.heading = 0;
                self.inline_before = None;
                if std::mem::take(&mut self.quote) {
                    self.para_next = Some(Para::AfterList);
                }
                self.pending_text = text[at + 1..].to_string();
                return;
            }
            if c == '`' {
                self.flush_run(&mut run);
                self.inline_code = !self.inline_code;
                self.inline_before = Some(c);
                continue;
            }
            if !self.inline_code {
                let next = chars.peek().map(|(_, next)| *next);
                let rest = &text[at..];
                // What may mean more than it says, and has its meaning in what has not come.
                let hold = |app: &mut App, run: &mut String| {
                    app.flush_run(run);
                    app.pending_text = rest.to_string();
                };
                match (c, next) {
                    ('\\' | '*' | '~' | '!', None) => {
                        hold(self, &mut run);
                        return;
                    }
                    ('\\', Some(escaped)) if markdown::is_escapable(escaped) => {
                        chars.next();
                        run.push(escaped);
                        self.inline_before = Some(escaped);
                        continue;
                    }
                    ('*', Some('*')) => {
                        chars.next();
                        self.flush_run(&mut run);
                        self.bold = !self.bold;
                        continue;
                    }
                    ('*', Some(_)) if self.italic && self.inline_before.is_some_and(|before| !before.is_whitespace()) => {
                        self.flush_run(&mut run);
                        self.italic = false;
                        self.inline_before = Some(c);
                        continue;
                    }
                    ('*', Some(after)) if !self.italic && markdown::opens_emphasis(self.inline_before, after) => {
                        self.flush_run(&mut run);
                        self.italic = true;
                        self.inline_before = Some(c);
                        continue;
                    }
                    ('~', Some('~')) => {
                        chars.next();
                        self.flush_run(&mut run);
                        self.struck = !self.struck;
                        continue;
                    }
                    ('[', _) | ('!', Some('[')) => {
                        let opening = usize::from(c == '!');
                        match markdown::parse_link(&rest[opening..]) {
                            markdown::Link::Waiting => {
                                hold(self, &mut run);
                                return;
                            }
                            markdown::Link::Whole { text: words, url, length } => {
                                self.flush_run(&mut run);
                                let end = at + opening + length;
                                // A picture the answer shows, there in the conversation as a tool's is, when it is one on this PC: what is
                                // after it goes on from a line of its own.
                                if opening == 1 {
                                    if let Some(picture) = self.answer_picture(url) {
                                        self.pending_text = text[end..].to_string();
                                        self.picture_block(&[picture], false);
                                        return;
                                    }
                                }
                                self.append_link(words, url);
                                while chars.peek().is_some_and(|(index, _)| *index < end) {
                                    chars.next();
                                }
                                self.inline_before = Some(')');
                                continue;
                            }
                            markdown::Link::No => {}
                        }
                    }
                    ('h', _) if !self.inline_before.is_some_and(|before| before.is_ascii_alphanumeric()) => match markdown::bare_url(rest) {
                        markdown::BareUrl::Waiting => {
                            hold(self, &mut run);
                            return;
                        }
                        markdown::BareUrl::Whole(length) => {
                            self.flush_run(&mut run);
                            let url = &rest[..length];
                            self.append_link(url, url);
                            let end = at + length;
                            while chars.peek().is_some_and(|(index, _)| *index < end) {
                                chars.next();
                            }
                            self.inline_before = Some('/');
                            continue;
                        }
                        markdown::BareUrl::No => {}
                    },
                    _ => {}
                }
            }
            run.push(c);
            self.inline_before = Some(c);
        }
        self.flush_run(&mut run);
    }

    unsafe fn flush_run(&mut self, run: &mut String) {
        let style = match self.heading {
            1 => Style::Title,
            2 => Style::Heading,
            3..=6 => Style::Subheading,
            _ if self.inline_code => Style::InlineCode,
            _ if self.bold => Style::Strong,
            _ if self.italic => Style::Emphasis,
            _ if self.struck => Style::Struck,
            _ if self.quote => Style::Quote,
            _ => Style::Body,
        };
        let text = std::mem::take(run);
        self.append(&text, style);
    }

    /// A line across the column, for `---`.
    unsafe fn rule_row(&mut self) {
        self.append("\n", Style::Gap);
        self.append_row(&rtf::Row { lines: &[" "], face: UI_FACE, points: 1, color: theme::line(), back: theme::line(), width: self.text_twips, right: false, column: 0, padding: (0, 0) });
        self.para_next = Some(Para::AfterBlock);
    }

    /// The end of an answer: what was waiting for more goes out as it is.
    unsafe fn finish_answer(&mut self) {
        let mut rest = std::mem::take(&mut self.pending_text);
        if !self.in_code && !self.table.is_empty() && rest.trim_start().starts_with('|') {
            // The answer ended on its table's last line.
            self.table.push(std::mem::take(&mut rest).trim_end().to_string());
        }
        self.flush_table();
        self.para_next = None;
        if self.in_code {
            // An answer that ended inside its block, or with the fence still on its last line.
            if !rest.is_empty() && !rest.trim_start().starts_with("```") {
                self.code_line(rest.trim_end_matches(['\r', '\n']));
            }
            self.block_edge("");
            self.open_block = None;
            self.line_start = true;
        } else if markdown::is_rule(rest.trim()) {
            self.rule_row();
        } else if let markdown::BareUrl::Whole(length) = markdown::bare_url(&format!("{rest} ")) {
            // (the answer ended on an address, which had been waiting to see where it ended)
            self.line_start = false;
            let (url, after) = rest.split_at(length);
            self.append_link(url, url);
            self.append(after, Style::Body);
        } else if rest != "*" && !rest.is_empty() {
            self.line_start = false;
            self.append(&rest, Style::Body);
        }
        self.end_line();
        self.in_code = false;
        self.bold = false;
        self.italic = false;
        self.struck = false;
        self.quote = false;
        self.inline_code = false;
        self.heading = 0;
    }

    /// A run begins right under an answer: that was a remark on the way, and its copy row goes. What ends a turn keeps one.
    unsafe fn run_follows(&mut self, at: usize) {
        if let Some(Shown::Answer(_, at)) = self.shown.last() {
            // (the row's tab and time, and its end)
            let length = if at.is_empty() { 1 } else { at.encode_utf16().count() as i32 + 2 };
            if let Some((first, _)) = self.answers.pop() {
                self.shown.pop();
                self.at_end(|edit| {
                    let row = CharRange { first, last: first + length };
                    SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(row) as LPARAM);
                    let nothing = wide("");
                    SendMessageW(edit, EM_REPLACESEL, 0, nothing.as_ptr() as LPARAM);
                });
            }
        }
        self.shown.push(Shown::Run(at));
    }

    /// Below an answer, a row for its copy button, and when it was given (`at`, as it is to read: or nothing).
    unsafe fn mark_answer(&mut self, text: &str, at: &str) {
        self.end_line();
        self.shown.push(Shown::Answer(text.to_string(), at.to_string()));
        self.answer_row(text, at);
    }

    unsafe fn answer_row(&mut self, text: &str, at: &str) {
        let mut end = CharRange { first: 0, last: 0 };
        self.at_end(|edit| {
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(end) as LPARAM);
        });
        self.answers.push((end.first, text.to_string()));
        // (the answer has its own place in `shown`, which writes this row again)
        self.unkept = true;
        if !at.is_empty() {
            // (a tab first: where the row begins is where its copy button stands, and the time goes past it)
            self.append("\t", Style::CopyRow);
            self.append(at, Style::AnsweredAt);
        }
        self.append("\n", Style::CopyRow);
        self.unkept = false;
        self.line_start = true;
    }

    /// The view has moved, by whatever moved it: the thumb follows. The wheel's scroll is the control's own, carried out over many timer ticks after the message that began it has
    /// returned, so that no message of ours can say when it ends. Asked after each paint and tick of the control, and only a move does anything. (What the text carries, the icons and
    /// the pictures of its messages, is drawn in its own paint and needs no following.)
    unsafe fn follow_the_thumb(&mut self) {
        let mut position = POINT { x: 0, y: 0 };
        SendMessageW(self.transcript, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        if position.y != self.thumb_at {
            self.thumb_at = position.y;
            self.place_thumb();
        }
    }

    /// A new session with the conversation through this answer, opened: to go on from there another way. The answer is found in the saved
    /// session by its words, and by how many answers before it have the same.
    unsafe fn fork_from_answer(&mut self, at: usize) {
        if at >= self.answers.len() {
            return;
        }
        let Some(path) = self.current_session.clone() else { return };
        let words = self.answers[at].1.clone();
        let same_before = self.answers[..at].iter().filter(|(_, other)| *other == words).count();
        let messages = session::read(&path).unwrap_or_default();
        let found = messages.iter().enumerate().filter(|(_, message)| message.role == "assistant" && message.content.as_deref().map(str::trim) == Some(words.trim())).nth(same_before).map(|(place, _)| place);
        let Some(through) = found else {
            self.say(t("This answer is not in the saved session yet, so there is nothing to branch from. Try again once it is."));
            return;
        };
        // (one with no name yet is named by its first message, as the original is)
        let title = (!self.session_title.trim().is_empty()).then(|| tf("{0} (branch)", &[&self.session_title.trim()]));
        match session::fork(&path, through, title.as_deref()) {
            Ok(branch) => {
                self.refresh_sessions();
                self.open_session(branch);
            }
            Err(error) => self.say(&tf("The branch could not be made: {0}", &[&error])),
        }
    }

    unsafe fn copy_answer(&mut self, at: usize) {
        let Some((_, text)) = self.answers.get(at) else { return };
        let text = text.clone();
        self.copy_to_clipboard(&text);
        self.copied(OverlayKey::Copy(at));
    }

    /// An icon has copied: it shows a check mark for a moment.
    unsafe fn copied(&mut self, key: OverlayKey) {
        self.overlay_copied = Some(key);
        self.invalidate_overlay(key);
        SetTimer(self.window, TIMER_COPIED, 1400, None);
    }

    /// The icon of `key`, as it was last drawn, drawn again.
    unsafe fn invalidate_overlay(&self, key: OverlayKey) {
        if let Some(hit) = self.overlay_hits.iter().find(|hit| hit.key == key) {
            InvalidateRect(self.transcript, &hit.area, 0);
        }
    }

    unsafe fn end_line(&mut self) {
        if !self.line_start {
            self.append("\n", Style::Body);
            self.line_start = true;
        }
    }

    /// What an edit changed, in colour, and how a command ended: its last lines.
    unsafe fn show_tool_output(&mut self, tool: &str, output: &str) {
        match tool {
            "web_search" | "web_read" if output.starts_with(WARNING_MARK) => {
                self.append(t("This has text that reads like instructions to an AI. It went to the model marked as data, with word not to act on it.\n"), Style::Warn);
            }
            "apply_patch" => self.show_diff(output, 40),
            "execute_command" => {
                let (above, lines) = toolrun::command_lines(output);
                if above == 0 && lines.is_empty() {
                    return;
                }
                // A block of its own, with room over and under it, as a block of code has.
                self.room_over_a_block();
                self.begin_block();
                self.block_edge("");
                if above > 0 {
                    self.block_line(&tf("… {0} lines above", &[&above]), theme::muted(), 9);
                }
                for line in &lines {
                    self.block_line(line, theme::code(), 9);
                }
                self.block_edge("");
                self.open_block = None;
                self.line_start = true;
            }
            _ => {}
        }
    }

    /// The room of a run's first line, in half-width characters of its type.
    fn run_room(&self) -> usize {
        ((self.text_twips - 500).max(0) / 95) as usize
    }

    /// A run goes in at the end of the transcript, or changes where it is. Only the words change: the row keeps its own line's end, and what comes after it
    /// is not touched.
    unsafe fn write_run(&mut self, at: usize) {
        let (first, last) = (self.runs[at].first, self.runs[at].last);
        let since = self.runs[at].began.map_or(0.0, |began| began.elapsed().as_secs_f64());
        let lines = self.runs[at].lines(&self.workspace, self.run_room(), since);
        let held = std::mem::replace(&mut self.run_writing, true);
        // (a row changing in its place has changed only itself: the frames of an answer's reveal are as careful)
        let revealing = std::mem::replace(&mut self.revealing, first >= 0 && lines.len() == 1);
        let mut written = (first, first, last);
        self.at_end(|edit| written = put_run_lines(edit, (first >= 0).then_some((first, last)), &lines));
        self.revealing = revealing;
        self.run_writing = held;
        let run = &mut self.runs[at];
        (run.first, run.head_end, run.last) = written;
        run.written = lines.into_iter().next().map_or(String::new(), |(line, _)| line);
        run.written_at = Some(std::time::Instant::now());
    }

    /// The run going on, or a new one with whatever was left running from the last.
    unsafe fn open_run(&mut self) -> usize {
        if let Some(at) = self.run_open {
            return at;
        }
        let mut run = ToolRun::new();
        run.open = true;
        run.began = Some(std::time::Instant::now());
        run.entries = std::mem::take(&mut self.run_carry);
        self.runs.push(run);
        let at = self.runs.len() - 1;
        self.run_follows(at);
        self.run_open = Some(at);
        at
    }

    /// The run has ended: it stays as a line that says what it came to, in the place and at the height it had. Its calls still running go on in the next.
    unsafe fn close_run(&mut self) {
        let Some(at) = self.run_open.take() else { return };
        let run = &mut self.runs[at];
        let (done, running): (Vec<RunEntry>, Vec<RunEntry>) = std::mem::take(&mut run.entries).into_iter().partition(|entry| entry.done);
        run.entries = done;
        run.open = false;
        run.took = run.began.zip(run.ended).map(|(began, ended)| (ended - began).as_secs_f64());
        self.run_carry.extend(running);
        if self.runs[at].entries.is_empty() {
            self.remove_run_row(at);
        } else {
            self.write_run(at);
        }
        self.line_start = true;
    }

    /// A row with nothing in it goes. It is the last thing in the transcript but for what is on its way in.
    unsafe fn remove_run_row(&mut self, at: usize) {
        let (first, last) = (self.runs[at].first, self.runs[at].last);
        if first >= 0 {
            let held = std::mem::replace(&mut self.run_writing, true);
            self.at_end(|edit| {
                let range = CharRange { first, last };
                SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
                let nothing = [0u16];
                SendMessageW(edit, EM_REPLACESEL, 0, nothing.as_ptr() as LPARAM);
            });
            self.run_writing = held;
        }
        self.runs[at].first = -1;
        if let Some(place) = self.shown.iter().rposition(|item| matches!(item, Shown::Run(run) if *run == at)) {
            self.shown.remove(place);
        }
    }

    /// A call out of the run going on or of those left running.
    fn take_run_entry(&mut self, id: &str) -> Option<RunEntry> {
        if let Some(open) = self.run_open {
            let entries = &mut self.runs[open].entries;
            if let Some(place) = entries.iter().position(|entry| entry.id == id) {
                return Some(entries.remove(place));
            }
        }
        let place = self.run_carry.iter().position(|entry| entry.id == id)?;
        Some(self.run_carry.remove(place))
    }

    /// A call on a row of its own: the row of the run going on if that has nothing else in it, so that nothing moves.
    unsafe fn own_row(&mut self, mut entry: RunEntry, tone: Tone, note: String) {
        if tone != Tone::Done {
            entry.failure = Some(note.clone());
        }
        let at = match self.run_open {
            Some(at) if self.runs[at].entries.is_empty() => {
                self.run_open = None;
                at
            }
            _ => {
                self.close_run();
                self.runs.push(ToolRun::new());
                self.run_follows(self.runs.len() - 1);
                self.runs.len() - 1
            }
        };
        let run = &mut self.runs[at];
        run.entries = vec![entry];
        run.open = false;
        run.tone = tone;
        run.note = note;
        self.write_run(at);
        self.line_start = true;
    }

    /// The answer has been said: what is still held back goes out, and it can be copied. Ends also with a call beginning to be written, whose row comes under it.
    unsafe fn end_of_answer(&mut self) {
        self.thinking.clear();
        // (a word held back, in case it was a key, goes out with the rest)
        let held = self.stream_mask.finish();
        self.reveal.push_str(&held);
        self.flush_reveal();
        self.finish_answer();
        if !self.answer.is_empty() {
            self.last_answer = std::mem::take(&mut self.answer);
            let answer = self.last_answer.clone();
            self.mark_answer(&answer, &clock_now());
        }
    }

    /// What the model has written so far of each call, for a session put aside.
    fn writing_now(&self) -> Vec<(usize, String, String)> {
        let open = self.run_open.map_or(&[][..], |open| &self.runs[open].entries[..]);
        open.iter().chain(&self.run_carry).filter_map(|entry| entry.writing.as_ref().map(|writing| (writing.index, entry.tool.clone(), writing.raw.clone()))).collect()
    }

    /// More of a call the model is writing. Only what is held grows, as this comes a few times a second: the row is written by `run_tick`.
    unsafe fn run_writes(&mut self, index: usize, tool: String, piece: &str) {
        let of_it = |entry: &RunEntry| entry.writing.as_ref().is_some_and(|writing| writing.index == index);
        let known = self.run_open.is_some_and(|open| self.runs[open].entries.iter().any(of_it)) || self.run_carry.iter().any(of_it);
        if !known {
            self.end_of_answer();
            self.end_line();
        }
        let at = self.open_run();
        let entries = &mut self.runs[at].entries;
        let place = entries.iter().position(of_it).unwrap_or_else(|| {
            entries.push(RunEntry::being_written(index, tool));
            entries.len() - 1
        });
        entries[place].more_written(piece);
        if !known {
            if !self.stop_asked {
                self.set_activity(String::new());
            }
            self.write_run(at);
        }
    }

    unsafe fn run_started(&mut self, id: String, tool: String, summary: String) {
        self.end_line();
        let at = self.open_run();
        match self.runs[at].entries.iter_mut().find(|entry| entry.writing.is_some() && entry.tool == tool) {
            // (the same call: the model has finished writing it)
            Some(entry) => {
                entry.writing = None;
                entry.id = id;
                entry.summary = summary;
            }
            None => self.runs[at].entries.push(RunEntry::running(id, tool, summary)),
        }
        // (the row says what is going on, and the line below it what the model is doing between calls)
        if !self.stop_asked {
            self.set_activity(String::new());
        }
        self.write_run(at);
    }

    unsafe fn run_ended(&mut self, id: &str, tool: String, summary: String, ok: bool, declined: bool, elapsed: f64, output: &str) {
        self.end_line();
        let mut entry = self.take_run_entry(id).unwrap_or_else(|| RunEntry::running(id.to_string(), tool.clone(), summary.clone()));
        entry.done = true;
        entry.elapsed = Some(elapsed);
        let asked = self.asked_calls.remove(id);
        let warned = matches!(tool.as_str(), "web_search" | "web_read") && output.starts_with(WARNING_MARK);
        let failed_command = if ok && tool == "execute_command" { toolrun::command_failed(output) } else { None };
        if declined {
            self.own_row(entry, Tone::Warn, t("Rejected").to_string());
        } else if !ok {
            let first = output.strip_prefix("error: ").unwrap_or(output).lines().next().unwrap_or("").chars().take(240).collect();
            self.own_row(entry, Tone::Bad, first);
        } else if let Some(how) = failed_command {
            self.own_row(entry, Tone::Bad, how);
            self.show_tool_output(&tool, output);
        } else if asked || warned {
            self.own_row(entry, Tone::Done, String::new());
            self.show_tool_output(&tool, output);
        } else {
            entry.output = toolrun::kept_output(&tool, output);
            let at = self.open_run();
            let run = &mut self.runs[at];
            run.entries.push(entry);
            // (those of a batch end in the order they began: the one under way stays the last)
            run.entries.sort_by_key(|entry| !entry.done);
            run.ended = Some(std::time::Instant::now());
            self.write_run(at);
            return;
        }
        // What was running beside it goes on below it.
        if !self.run_carry.is_empty() {
            let at = self.open_run();
            self.write_run(at);
        }
    }

    /// The call under way needs the person: it comes out of the run, and whatever else is in the run has ended.
    unsafe fn run_asks(&mut self) {
        let Some(open) = self.run_open else { return };
        let Some(place) = self.runs[open].entries.iter().rposition(|entry| !entry.done) else { return };
        self.asked_calls.insert(self.runs[open].entries[place].id.clone());
        if self.runs[open].entries.len() > 1 {
            let entry = self.runs[open].entries.remove(place);
            self.close_run();
            self.run_carry.retain(|carried| carried.id != entry.id);
            let at = self.open_run();
            self.runs[at].entries.push(entry);
            self.write_run(at);
        }
    }

    /// The row says what is going on as it goes on: its ring turns and its seconds count, and it is written only when that has changed what it says.
    unsafe fn run_tick(&mut self) {
        let Some(at) = self.run_open else { return };
        if self.panel != Panel::Chat {
            return;
        }
        let since = self.runs[at].began.map_or(0.0, |began| began.elapsed().as_secs_f64());
        if self.runs[at].head(&self.workspace, self.run_room(), since) == self.runs[at].written {
            return;
        }
        // (its list open, more of a page goes with it: once a second is plenty)
        if self.runs[at].expanded && self.runs[at].written_at.is_some_and(|written| written.elapsed().as_millis() < 1000) {
            return;
        }
        // (no one can see it from far above the end)
        let mut bar: SCROLLINFO = std::mem::zeroed();
        bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        bar.fMask = SIF_ALL;
        GetScrollInfo(self.transcript, SB_VERT, &mut bar);
        if bar.nPage == 0 || bar.nPos + 2 * bar.nPage as i32 >= bar.nMax {
            self.write_run(at);
        }
    }

    /// A request under way that has gone quiet is not going at the speed it last said.
    unsafe fn rate_tick(&mut self) {
        if self.rate.is_some() && self.request_live.is_some_and(|live| live.elapsed().as_secs() >= 3) {
            self.rate = None;
            InvalidateRect(self.window, &self.status_area, 0);
        }
    }

    /// A run is opened or closed where it stands, which may be anywhere in the transcript: what comes after it moves by as much, and the view stays.
    unsafe fn toggle_run(&mut self, at: usize) {
        if !self.runs.get(at).is_some_and(|run| run.expandable() && run.first >= 0) {
            return;
        }
        let edit = self.transcript;
        let mut bar: SCROLLINFO = std::mem::zeroed();
        bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        bar.fMask = SIF_ALL;
        GetScrollInfo(edit, SB_VERT, &mut bar);
        let at_bottom = bar.nPage == 0 || bar.nPos + bar.nPage as i32 + 4 >= bar.nMax;
        let mut position = POINT { x: 0, y: 0 };
        SendMessageW(edit, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        let mut selection = CharRange { first: 0, last: 0 };
        SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(selection) as LPARAM);
        self.runs[at].expanded = !self.runs[at].expanded;
        let (first, old_last) = (self.runs[at].first, self.runs[at].last);
        let since = self.runs[at].began.map_or(0.0, |began| began.elapsed().as_secs_f64());
        let lines = self.runs[at].lines(&self.workspace, self.run_room(), since);
        redraw(edit, false);
        let (_, head_end, last) = put_run_lines(edit, Some((first, old_last)), &lines);
        let delta = last - old_last;
        let moved = |place: &mut i32| {
            if *place >= old_last {
                *place += delta;
            }
        };
        moved(&mut selection.first);
        moved(&mut selection.last);
        self.answers.iter_mut().for_each(|(place, _)| moved(place));
        self.user_cards.iter_mut().for_each(|card| moved(&mut card.start));
        self.rail_turns.iter_mut().for_each(|turn| moved(&mut turn.start));
        self.picture_blocks.iter_mut().for_each(|block| moved(&mut block.start));
        self.blocks.iter_mut().for_each(|block| moved(&mut block.at));
        self.card_slots.iter_mut().for_each(|slot| moved(&mut slot.start));
        for (from, to, _) in &mut self.links {
            moved(from);
            moved(to);
        }
        for run in &mut self.runs {
            if run.first >= old_last {
                moved(&mut run.first);
                moved(&mut run.head_end);
                moved(&mut run.last);
            }
        }
        let run = &mut self.runs[at];
        run.head_end = head_end;
        run.last = last;
        SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(selection) as LPARAM);
        SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        redraw(edit, true);
        InvalidateRect(edit, std::ptr::null(), 0);
        if self.find_open {
            self.find_scan();
        }
        if at_bottom && self.runs[at].expanded {
            self.follow_end = true;
            self.want_frames();
        }
        self.place_thumb();
    }

    /// The calls of a session's file that went well, as one row.
    unsafe fn flush_told_calls(&mut self, told: &mut Vec<RunEntry>) {
        if told.is_empty() {
            return;
        }
        self.runs.push(ToolRun::new());
        let at = self.runs.len() - 1;
        self.runs[at].entries = std::mem::take(told);
        self.run_follows(at);
        self.write_run(at);
        self.line_start = true;
    }

    /// A call from a session's file, as it would have shown when it ended.
    unsafe fn told_call(&mut self, tool: &str, summary: &str, result: &str, told: &mut Vec<RunEntry>) {
        match toolrun::told_failure(tool, result) {
            Some((tone, note)) => {
                self.flush_told_calls(told);
                self.runs.push(ToolRun::new());
                let at = self.runs.len() - 1;
                let run = &mut self.runs[at];
                run.entries = vec![RunEntry::ended(tool.to_string(), summary.to_string(), Vec::new())];
                run.tone = tone;
                run.note = note;
                self.run_follows(at);
                self.write_run(at);
                self.line_start = true;
                if tool == "execute_command" {
                    self.show_tool_output(tool, result);
                }
            }
            None if matches!(tool, "web_search" | "web_read") && result.starts_with(WARNING_MARK) => {
                self.flush_told_calls(told);
                told.push(RunEntry::ended(tool.to_string(), summary.to_string(), Vec::new()));
                self.flush_told_calls(told);
                self.show_tool_output(tool, result);
            }
            None => told.push(RunEntry::ended(tool.to_string(), summary.to_string(), toolrun::kept_output(tool, result))),
        }
    }

    unsafe fn show_diff(&mut self, text: &str, limit: usize) {
        let lines: Vec<&str> = text.lines().filter(|line| !line.starts_with("--- applied_diff") && !line.starts_with("--- ") && !line.starts_with("+++ ")).collect();
        for line in lines.iter().take(limit) {
            let color = match line.chars().next() {
                Some('+') => theme::good(),
                Some('-') => theme::bad(),
                Some('@') => theme::accent_hover(),
                _ => theme::code(),
            };
            self.block_line(line, color, 9);
        }
        if lines.len() > limit {
            self.block_line(&tf("… {0} more lines", &[&(lines.len() - limit)]), theme::muted(), 9);
        }
        self.line_start = true;
    }

    /// What the person wrote: a card against the right edge, and what the model writes runs under it at full width.
    /// `at`: when it was said, as it is to read under the card, or nothing.
    unsafe fn show_user(&mut self, text: &str, images: &[(PathBuf, (i32, i32))], at: &str) {
        self.end_line();
        // (the first card keeps clear of the edge above it)
        if self.shown.is_empty() {
            self.append("\n", Style::Gap);
        }
        // (said in the middle of a run, it stands clear of the run's row; and of a card)
        if matches!(self.shown.last(), Some(Shown::Run(_) | Shown::Card(_))) {
            self.append("\n", Style::Gap);
        }
        let lines: Vec<&str> = text.trim().lines().collect();
        let device = GetDC(self.transcript);
        let previous = SelectObject(device, self.fonts.body);
        let widest = lines.iter().map(|line| width_on(device, self.fonts.body, line)).max().unwrap_or(0);
        SelectObject(device, previous);
        ReleaseDC(self.transcript, device);
        // The body font is drawn a size larger in the transcript than it was measured here, and at the size the settings chose.
        // (in twips: about 16 pixels at each side and 15 over and under at 100%, where the words had been tight against the card)
        let padding = (240, 220);
        let wanted = i64::from(widest) * 1440 * 115 * i64::from(text_scale()) / 10000 / i64::from(self.dpi) + i64::from(padding.0) * 2 + 120;
        let most = (self.text_twips * 4 / 5).max(800);
        let wanted = if images.is_empty() {
            wanted as i32
        } else {
            let room = i64::from(most - 2 * padding.0) * i64::from(self.dpi) / 1440;
            let sizes: Vec<(i32, i32)> = images.iter().map(|(_, size)| *size).collect();
            let tiles = attach::Tiles::of(&sizes, room as i32, self.dpi);
            let across = i64::from(tiles.total_width()) * 1440 / i64::from(self.dpi) + i64::from(padding.0) * 2 + 40;
            (if lines.is_empty() { across } else { across.max(wanted) }) as i32
        };
        let width = wanted.clamp(800, most);
        if !images.is_empty() {
            self.append_pictures(images, width, lines.is_empty(), true);
        }
        if !lines.is_empty() {
            self.append_row(&rtf::Row { lines: &lines, face: UI_FACE, points: 11, color: theme::text(), back: theme::user_back(), width, right: true, column: self.text_twips, padding });
        }
        if at.is_empty() {
            self.append("\n", Style::Tool);
        } else {
            self.append(&format!("{at}\n"), Style::SaidAt);
        }
        self.line_start = true;
        // The second turn brings the rail, whose room narrows the conversation's column: the conversation is written again at it now, before
        // an answer comes into it, and not on a timer, which would write it again in the middle of one.
        if self.rail_turns.len() == 2 && !self.opening {
            let before = self.text_twips;
            self.layout();
            if self.text_twips != before {
                KillTimer(self.window, TIMER_REFLOW);
                self.reflow();
                self.place_thumb();
            }
        }
    }

    /// A picture an answer shows with `![what it shows](path)`: a file of the workspace, or one elsewhere on this PC that a tool gave the model
    /// (check_page's), that is a picture, and its size. None: it stays a link, as one on the web does.
    fn answer_picture(&self, url: &str) -> Option<(PathBuf, (i32, i32))> {
        let url = url.trim();
        let url = url.strip_prefix("file:///").unwrap_or(url);
        let path = Path::new(url);
        let file = if path.is_absolute() { path.to_path_buf() } else { self.workspace.join(path) };
        if markdown::web_address(url).is_some() || !attach::is_image(&file) || !file.is_file() {
            return None;
        }
        let (width, height) = image::image_dimensions(&file).ok()?;
        Some((file, (width as i32, height as i32)))
    }

    /// Where a message's pictures are kept with this session, and their sizes.
    fn pictures_of(&self, images: &[dogmind_agent::message::ImageRef]) -> Vec<(PathBuf, (i32, i32))> {
        self.current_session.as_deref().map_or(Vec::new(), |session| images.iter().map(|image| (dogmind_agent::images::file_of(session, image), (image.width as i32, image.height as i32))).collect())
    }

    /// What a tool opened for the model to see, a picture the model read: under its call, at the left, where the person sees what the model saw.
    unsafe fn show_tool_pictures(&mut self, pictures: &[(PathBuf, (i32, i32))]) {
        self.picture_block(pictures, true);
    }

    /// Pictures in a block at the left. `under_a_call`: a tool's, with a line under them before what the model does next; an answer's go
    /// on to its next paragraph with a block's room, as a block of code does (with the line too there was twice a paragraph's gap).
    unsafe fn picture_block(&mut self, pictures: &[(PathBuf, (i32, i32))], under_a_call: bool) {
        if pictures.is_empty() {
            return;
        }
        self.end_line();
        const ACROSS: i32 = 200;
        let most = (self.text_twips * 3 / 5).max(800);
        let room = i64::from(most - 2 * ACROSS) * i64::from(self.dpi) / 1440;
        let sizes: Vec<(i32, i32)> = pictures.iter().map(|(_, size)| *size).collect();
        let tiles = attach::Tiles::of(&sizes, room as i32, self.dpi);
        let width = ((i64::from(tiles.total_width()) * 1440 / i64::from(self.dpi)) as i32 + 2 * ACROSS + 40).clamp(800, most);
        self.append_pictures(pictures, width, true, false);
        if under_a_call {
            self.append("\n", Style::Tool);
        }
        self.line_start = true;
        self.para_next = Some(Para::AfterBlock);
    }

    /// Room in a card for pictures, which come when they come into view: nothing is read now, however long the conversation.
    unsafe fn append_pictures(&mut self, pictures: &[(PathBuf, (i32, i32))], width: i32, alone: bool, right: bool) {
        if !self.replaying {
            self.shown.push(Shown::Pictures { pictures: pictures.to_vec(), width, alone, right });
        }
        const ACROSS: i32 = 200;
        const DOWN: i32 = 100;
        let room = i64::from(width - 2 * ACROSS) * i64::from(self.dpi) / 1440;
        let sizes: Vec<(i32, i32)> = pictures.iter().map(|(_, size)| *size).collect();
        let tiles = attach::Tiles::of(&sizes, room as i32, self.dpi);
        // (the room above them is in the height of this row, and what is under them is the text's, or if none, this row's too)
        let height = (i64::from(tiles.total_height()) * 1440 / i64::from(self.dpi)) as i32 + if alone { 2 * DOWN } else { DOWN };
        let back = if right { theme::user_back() } else { theme::code_back() };
        let first = self.insert_rtf(rtf::spacer_row(width, self.text_twips, height, back, (ACROSS, DOWN), right), None);
        let start = text_start(self.transcript, first);
        self.picture_blocks.push(PictureBlock { start, paths: pictures.iter().map(|(path, _)| path.clone()).collect(), tiles });
        // (a message's card begins at its pictures, whether words follow them or not)
        if alone || right {
            self.user_cards.push(UserCard { start, kind: if right { CardKind::Person } else { CardKind::Block }, shape: None, looked_at: None });
        }
        if right {
            self.rail_turns.push(TurnMark { start, prompt: tf("{0} pictures", &[&pictures.len()]) });
            self.turn_words_due = !alone;
            if alone && !self.replaying && !self.opening {
                self.place_rail();
            }
        }
    }

    /// A small copy of a file at a size, from what is kept, or asked for on another thread and not there yet.
    unsafe fn thumb_of(&mut self, path: &Path, room: (i32, i32)) -> Thumb {
        self.thumb_clock += 1;
        let key = (path.to_path_buf(), room);
        if let Some(kept) = self.thumbs.get_mut(&key) {
            kept.used = self.thumb_clock;
            return kept.thumb.clone();
        }
        if self.thumbs.len() >= THUMBS_KEPT {
            self.forget_a_thumb();
        }
        self.thumbs.insert(key, KeptThumb { thumb: Thumb::Waiting, used: self.thumb_clock });
        let (poster, path) = (self.poster.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            let decoded = attach::decode(&path, room.0, room.1);
            poster.send(Note::Thumb { path, room, decoded });
        });
        Thumb::Waiting
    }

    /// The copy asked for longest ago goes, as long as the view does not want it and it is not one of a picture waiting to go.
    unsafe fn forget_a_thumb(&mut self) {
        let attached_room = self.attached_room();
        let oldest = self.thumbs.iter().filter(|(key, kept)| {
            let (path, room) = *key;
            !matches!(kept.thumb, Thumb::Waiting)
                && !self.thumb_wanted.contains(*key)
                && !(*room == attached_room && self.attachments.iter().any(|attached| attached.path == *path))
        }).min_by_key(|(_, kept)| kept.used).map(|(key, _)| key.clone());
        if let Some(kept) = oldest.and_then(|key| self.thumbs.remove(&key)) {
            if let Thumb::Ready { bitmap, .. } = kept.thumb {
                DeleteObject(bitmap);
            }
        }
    }

    fn attached_room(&self) -> (i32, i32) {
        (scale(self.dpi, 64), scale(self.dpi, 64))
    }

    /// A copy has come: it is kept, and goes to whichever window was waiting for it.
    unsafe fn thumb_came(&mut self, path: PathBuf, room: (i32, i32), decoded: Result<attach::Decoded, attach::Undecoded>) {
        let thumb = match decoded {
            Ok(decoded) => bitmap_of(&decoded).map_or_else(|| Thumb::Missing(t("This picture cannot be shown here").to_string()), |bitmap| Thumb::Ready { bitmap, width: decoded.width, height: decoded.height }),
            Err(attach::Undecoded::Gone) => Thumb::Missing(t("File not found").to_string()),
            Err(attach::Undecoded::Unreadable) => Thumb::Missing(t("This picture cannot be shown here").to_string()),
        };
        self.thumbs.insert((path.clone(), room), KeptThumb { thumb: thumb.clone(), used: self.thumb_clock });
        // (the tiles of the transcript that were holding its place are drawn again)
        for hit in &self.overlay_hits {
            if let OverlayKey::Picture(block, at) = hit.key {
                let same = self.picture_blocks.get(block).is_some_and(|block| block.paths.get(at) == Some(&path) && (block.tiles.width, block.tiles.height) == room);
                if same {
                    InvalidateRect(self.transcript, &hit.area, 0);
                }
            }
        }
        if room == self.attached_room() {
            for (at, attached) in self.attachments.iter().enumerate() {
                if attached.path == path {
                    set_thumb(self.attached_windows[at], thumb.clone());
                }
            }
        }
    }

    /// Whether the model waits for the person: for an approval, an answer to its question, or a decision on its plan.
    fn waits_for_person(&self) -> bool {
        self.pending.is_some() || self.asking()
    }

    /// Whether the model waits for the person's words, as for a question or a plan, and not for a press on a card of approval.
    fn asking(&self) -> bool {
        self.question.is_some() || self.plan_review.is_some()
    }

    /// A row of a card's own, in its colour: as a block of code is written, but kept by the card and not by itself. `first`: the card's first row, from which its corners are rounded.
    unsafe fn card_row(&mut self, lines: &[&str], points: i32, color: u32, down: i32, first: bool) {
        self.block_card_due = first;
        let shown: Vec<&str> = if lines.is_empty() { vec![" "] } else { lines.to_vec() };
        self.append_row(&rtf::Row { lines: &shown, face: UI_FACE, points, color, back: theme::code_back(), width: self.text_twips, right: false, column: 0, padding: (260, down) });
    }

    /// Room for controls drawn over the card: a row of an exact height, with what goes on it. Without any, it is only room between two rows.
    unsafe fn card_room(&mut self, tall: i32, parts: Vec<CardPart>) {
        let twips = (i64::from(tall) * 1440 / i64::from(self.dpi)) as i32;
        let first = self.insert_rtf(rtf::spacer_row(self.text_twips, self.text_twips, twips, theme::code_back(), (260, 0), false), None);
        if !parts.is_empty() {
            let start = text_start(self.transcript, first);
            self.card_slots.push(CardSlot { start, parts, tall });
        }
    }

    /// A card, in the state it is in, at the end of the transcript: shaded as a block of code is, with its controls drawn over rows of room of an exact height. It is kept with what the
    /// transcript is written from, so that the transcript written again, at another width or in another theme, writes it as it stands then.
    unsafe fn write_card(&mut self, card: &Card) {
        let recording = !self.replaying;
        // (a question or a plan is for the person, and is seen: the view goes to it. The files an answer changed come under it, and the view goes there only if it was at the end; a notice is the same)
        let attention = !matches!(card, Card::Files { .. } | Card::Notice { .. });
        let at_the_end = recording && !self.opening && self.transcript_at_end();
        if recording {
            self.close_run();
            self.end_line();
            self.room_over_a_block();
            self.shown.push(Shown::Card(card.clone()));
        }
        // (what goes in is the card's own: none of it is a part of the transcript's record)
        let held = std::mem::replace(&mut self.replaying, true);
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        match card {
            Card::Question { question, options, state, .. } => {
                self.card_row(&[" "], 6, theme::muted(), 80, true);
                self.card_row(&[t("Question")], 8, theme::muted(), 26, false);
                let lines: Vec<&str> = question.lines().collect();
                self.card_row(&lines, 11, theme::text(), 40, false);
                match state {
                    AnswerState::Waiting => {
                        self.card_room(s(8), Vec::new());
                        for (at, (label, meaning)) in options.iter().enumerate() {
                            let suggested = if at == 0 { t("Suggested") } else { "" };
                            let part = CardPart { text: choice_text(at + 1, suggested, label, meaning), kind: ButtonKind::Choice, act: CardAct::Choose(label.clone()), width: 0, tip: String::new() };
                            self.card_room(s(if meaning.is_empty() { 36 } else { 52 }), vec![part]);
                            self.card_room(s(5), Vec::new());
                        }
                        let send = t("Send");
                        let width = text_width(self.fonts.strong, send) + s(ds::size::BUTTON_PADDING);
                        let frame = CardPart { text: t("Write your own answer…").to_string(), kind: ButtonKind::Frame, act: CardAct::FocusField, width: 0, tip: String::new() };
                        let button = CardPart { text: send.to_string(), kind: ButtonKind::Primary, act: CardAct::SendOwn, width, tip: String::new() };
                        self.card_room(s(ds::size::CONTROL + 2), vec![frame, button]);
                    }
                    AnswerState::Answered(words) => self.card_row(&[&tf("Your answer: {0}", &[words])], 10, theme::accent(), 60, false),
                    AnswerState::Unanswered => self.card_row(&[t("Not answered")], 10, theme::muted(), 60, false),
                }
                self.card_room(s(12), Vec::new());
            }
            Card::Plan { plan, state, .. } => {
                // The plan as the transcript writes an answer, Markdown and all, under a word that says what it is; and under it a card with what can be done with it, or what was.
                self.append(&format!("{}\n", t("Plan")), Style::Tool);
                self.line_start = true;
                self.append_answer(plan);
                self.finish_answer();
                self.append("\n", Style::Gap);
                self.card_row(&[" "], 6, theme::muted(), 80, true);
                match state {
                    PlanState::Waiting => {
                        let button = |text: &str, kind: ButtonKind, act: CardAct, fonts: &Fonts| CardPart { text: text.to_string(), kind, act, width: text_width(fonts.strong, text) + s(ds::size::BUTTON_PADDING), tip: String::new() };
                        let parts = vec![button(t("Carry it out"), ButtonKind::Primary, CardAct::Carry, &self.fonts), button(t("Change…"), ButtonKind::Secondary, CardAct::Change, &self.fonts)];
                        self.card_room(s(ds::size::CONTROL), parts);
                    }
                    PlanState::Approved => self.card_row(&[t("Approved")], 10, theme::accent(), 60, false),
                    PlanState::Changes(change) => self.card_row(&[&tf("Change requested: {0}", &[change])], 10, theme::warn(), 60, false),
                    PlanState::Undecided => self.card_row(&[t("Not decided")], 10, theme::muted(), 60, false),
                }
                self.card_room(s(12), Vec::new());
            }
            Card::Notice { wakes, at } => {
                self.card_row(&[" "], 6, theme::muted(), 80, true);
                let label = if at.is_empty() { t("Notice").to_string() } else { format!("{}  ·  {at}", t("Notice")) };
                self.card_row(&[&label], 8, theme::muted(), 26, false);
                for wake in wakes {
                    let row = cards::notice_row(wake);
                    self.card_row(&[&row.title], 10, theme::accent(), 34, false);
                    self.card_row(&[&row.what], 11, theme::text(), 30, false);
                    self.card_row(&[&row.how], 10, if row.failed { theme::warn() } else { theme::muted() }, 44, false);
                }
                self.card_room(s(12), Vec::new());
            }
            Card::Files { turn, rows, more, added, removed } => {
                let count = rows.len() + more;
                let files = if count == 1 { t("1 file").to_string() } else { tf("{0} files", &[&count]) };
                let words = format!("{}  ·  {files}  ·  +{added}  \u{2212}{removed}", t("Changed in this answer"));
                self.card_row(&[" "], 6, theme::muted(), 80, true);
                self.card_row(&[&words], 8, theme::muted(), 26, false);
                self.card_room(s(4), Vec::new());
                let widest = cards::most_changed(rows);
                for (at, row) in rows.iter().enumerate() {
                    let (green, red) = cards::bar(row.added, row.removed, widest);
                    // (a file that is gone, or that is a program, is shown in Explorer and not opened)
                    let (icon, tip) = if cards::opens_in_app(&row.path) && self.workspace.join(&row.path).is_file() { ("external-link", t("Open in its app")) } else { ("folder", t("Show in Explorer")) };
                    let file = CardPart { text: file_row_text(row.added, row.removed, green, red, &row.path), kind: ButtonKind::FileRow, act: CardAct::OpenDiff(*turn, at), width: 0, tip: String::new() };
                    let open = CardPart { text: icon.to_string(), kind: ButtonKind::Icon, act: CardAct::OpenFile(*turn, at), width: s(ds::size::ICON_BUTTON), tip: tip.to_string() };
                    self.card_room(s(30), vec![file, open]);
                }
                if *more > 0 {
                    let rest = CardPart { text: tf("and {0} more", &[more]), kind: ButtonKind::Ghost, act: CardAct::MoreFiles(*turn), width: 0, tip: String::new() };
                    self.card_room(s(28), vec![rest]);
                }
                self.card_room(s(12), Vec::new());
            }
        }
        // (a table that ends the document has its last row drawn apart from the rest: a line after it, which is the room under a card anyway)
        self.append("
", Style::Gap);
        self.replaying = held;
        self.line_start = true;
        self.para_next = Some(Para::AfterBlock);
        if recording && !self.opening {
            if attention || at_the_end {
                self.follow_end = true;
                self.scrolled_up_at = None;
                self.want_frames();
            }
            self.place_thumb();
        }
    }

    /// The card with this number in the state `settle` leaves it in: the transcript is written again with it.
    unsafe fn settle_card(&mut self, id: u64, settle: impl FnOnce(&mut Card)) {
        let found = self.shown.iter_mut().find_map(|item| match item {
            Shown::Card(card) if card.id() == Some(id) => Some(card),
            _ => None,
        });
        if let Some(card) = found {
            settle(card);
            self.reflow();
        }
    }

    /// Cards that still wait, when the turn that asked has ended: unanswered.
    unsafe fn settle_waiting_cards(&mut self) {
        let mut changed = false;
        for item in &mut self.shown {
            match item {
                Shown::Card(Card::Question { state, .. }) if *state == AnswerState::Waiting => {
                    *state = AnswerState::Unanswered;
                    changed = true;
                }
                Shown::Card(Card::Plan { state, .. }) if *state == PlanState::Waiting => {
                    *state = PlanState::Undecided;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.reset_composer_cue();
            self.reflow();
        }
    }

    /// The answer to the model's question: by a press on a choice, by what was written in the card's field, or by what was typed in the composer. `false`: nothing asks.
    unsafe fn answer_question(&mut self, words: &str) -> bool {
        let Some(asked) = self.question.take() else { return false };
        let words = words.trim().to_string();
        self.reset_composer_cue();
        let said = words.clone();
        self.settle_card(asked.card, |card| {
            if let Card::Question { state, .. } = card {
                *state = AnswerState::Answered(said);
            }
        });
        let sent = asked.answer.send(words).is_ok();
        self.set_activity(t("Thinking…").into());
        self.update_buttons();
        InvalidateRect(self.side, std::ptr::null(), 0);
        self.focus_composer();
        sent
    }

    /// The decision on the plan: by a press on Carry it out, or by what was typed. `Err`: what to change. `false`: no plan waits.
    unsafe fn decide_plan(&mut self, decision: Result<(), String>) -> bool {
        let Some(asked) = self.plan_review.take() else { return false };
        let state = match &decision {
            Ok(()) => PlanState::Approved,
            Err(change) => PlanState::Changes(change.trim().to_string()),
        };
        self.settle_card(asked.card, |card| {
            if let Card::Plan { state: kept, .. } = card {
                *kept = state;
            }
        });
        self.reset_composer_cue();
        let sent = asked.answer.send(decision).is_ok();
        self.set_activity(t("Thinking…").into());
        self.update_buttons();
        InvalidateRect(self.side, std::ptr::null(), 0);
        self.focus_composer();
        sent
    }

    /// Change, on the plan: the composer says what it is for, and what is written in it next is the change asked for.
    unsafe fn plan_change_asked(&mut self) {
        if let Some(asked) = &mut self.plan_review {
            asked.changing = true;
            set_text(self.composer_cue, t("What should change in the plan?"));
            self.update_composer_cue();
            self.focus_composer();
        }
    }

    /// The composer's own hint again.
    unsafe fn reset_composer_cue(&mut self) {
        set_text(self.composer_cue, t("Message Dogmind"));
    }

    /// What was written in the composer, as the answer; with nothing written it is where to write it.
    unsafe fn card_pressed_own(&mut self) {
        let words = get_text(self.composer);
        if words.trim().is_empty() {
            self.focus_composer();
        } else {
            set_text(self.composer, "");
            self.composer_typed_at = None;
            self.answer_question(&words);
        }
    }

    /// The turns of the session just opened: for each that saved files, what it changed is asked for, and its card goes where its answer ended, before the next person's words or at the end.
    unsafe fn turns_for_cards(&mut self, turns: &[dogmind_agent::checkpoint::Turn]) {
        self.turn_cards_waiting.clear();
        self.turn_cards_came.clear();
        for turn in turns.iter().filter(|turn| !turn.files.is_empty()) {
            // (one whose words are no longer in the conversation, as a compaction took them, has no place in it)
            let Some(slot) = turn.message.and_then(|message| cards::place_of_turn(&self.turn_slots, self.turn_end_slot, message)) else { continue };
            self.turn_cards_waiting.insert(turn.id, slot);
            let _ = self.agent.commands.send(Command::TurnChanges(turn.id));
        }
    }

    /// The card of what a turn of a session opened again changed, where its answer ended: put in what the transcript is written from, one reply at a time as they come (the places of
    /// the others are not moved by it: `cards::card_position`), and the transcript written again a moment after the last that came together.
    unsafe fn put_turn_card(&mut self, slot: usize, turn: u64, files: &[dogmind_agent::tools::FileChange]) {
        let at = cards::card_position(&self.turn_cards_came, slot, turn).min(self.shown.len());
        self.turn_cards_came.push((slot, turn));
        self.shown.insert(at, Shown::Card(Card::files(turn, files)));
        // (a block being written is a place in the list too)
        if let Some(open) = self.open_block.as_mut().filter(|open| **open >= at) {
            *open += 1;
        }
        SetTimer(self.window, TIMER_REFLOW, 60, None);
    }

    /// Whether the view is at the end of the transcript, or there is nothing to scroll.
    unsafe fn transcript_at_end(&self) -> bool {
        let mut bar: SCROLLINFO = std::mem::zeroed();
        bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        bar.fMask = SIF_ALL;
        GetScrollInfo(self.transcript, SB_VERT, &mut bar);
        bar.nPage == 0 || bar.nPos + bar.nPage as i32 + 4 >= bar.nMax
    }

    /// The files an answer changed, on the Changes page: with the diff of file `at` of its card in view, or from the first.
    unsafe fn open_turn_diff(&mut self, turn: u64, at: usize) {
        let Some(files) = self.turn_changes.get(&turn).cloned() else { return };
        self.turn_view = Some(files);
        self.change_wanted = Some(at);
        self.set_panel(Panel::Diff);
    }

    /// A file of an answer's card, in its own app, or shown in Explorer when it is a program (which the app does not run), or no longer there.
    unsafe fn open_turn_file(&mut self, turn: u64, at: usize) {
        let Some(path) = self.turn_changes.get(&turn).and_then(|files| files.get(at)).map(|file| file.path.clone()) else { return };
        let file = self.workspace.join(&path);
        let (verb, target, arguments) = if file.is_file() && cards::opens_in_app(&path) {
            ("open", file.to_string_lossy().replace('/', "\\"), String::new())
        } else if file.is_file() {
            ("open", "explorer.exe".to_string(), format!("/select,\"{}\"", file.to_string_lossy().replace('/', "\\")))
        } else {
            // (gone: the folder it was in)
            ("open", "explorer.exe".to_string(), format!("\"{}\"", file.parent().unwrap_or(&self.workspace).to_string_lossy().replace('/', "\\")))
        };
        let (verb, target, arguments) = (wide(verb), wide(&target), wide(&arguments));
        ShellExecuteW(self.window, verb.as_ptr(), target.as_ptr(), if arguments.len() > 1 { arguments.as_ptr() } else { std::ptr::null() }, std::ptr::null(), SW_SHOWNORMAL);
    }

    unsafe fn card_pressed(&mut self, slot: usize, part: usize) {
        let Some(act) = self.card_slots.get(slot).and_then(|room| room.parts.get(part)).map(|part| part.act.clone()) else { return };
        match act {
            CardAct::Choose(label) => {
                self.answer_question(&label);
            }
            CardAct::SendOwn => self.card_pressed_own(),
            CardAct::FocusField => self.focus_composer(),
            CardAct::Carry => {
                self.decide_plan(Ok(()));
            }
            CardAct::Change => self.plan_change_asked(),
            CardAct::OpenDiff(turn, at) => self.open_turn_diff(turn, at),
            CardAct::OpenFile(turn, at) => self.open_turn_file(turn, at),
            CardAct::MoreFiles(turn) => self.open_turn_diff(turn, 0),
        }
    }

    /// What an icon does.
    unsafe fn overlay_act(&mut self, key: OverlayKey) {
        match key {
            OverlayKey::Copy(at) => self.copy_answer(at),
            OverlayKey::Fork(at) => self.fork_from_answer(at),
            OverlayKey::Block(at) => self.copy_block(at),
            OverlayKey::Run(at) => self.toggle_run(at),
            OverlayKey::Card(slot, part) => self.card_pressed(slot, part),
            OverlayKey::Picture(block, at) => {
                if let Some(path) = self.picture_blocks.get(block).and_then(|block| block.paths.get(at)).cloned() {
                    self.open_picture(&path);
                }
            }
        }
    }

    /// What an icon says of itself: in a tip, and to a screen reader.
    fn overlay_words(&self, key: OverlayKey) -> String {
        match key {
            OverlayKey::Copy(_) => t("Copy this answer").to_string(),
            OverlayKey::Fork(_) => t("Branch from here: a new session with the conversation up to this answer").to_string(),
            OverlayKey::Block(_) => t("Copy this code").to_string(),
            OverlayKey::Run(_) => t("Show or hide these calls").to_string(),
            OverlayKey::Card(slot, part) => self.card_slots.get(slot).and_then(|room| room.parts.get(part)).map_or(String::new(), card_part_words),
            OverlayKey::Picture(block, at) => self.picture_blocks.get(block).and_then(|block| block.paths.get(at)).map_or(String::new(), |path| file_name_of(path)),
        }
    }

    /// The icons of the answers, code blocks and rows of calls in view, each where it stands now: the places come from the text, asked for from the last one up to the first above the view.
    /// What is in view is the same set whatever has moved it, which is the point of asking at the moment of drawing.
    unsafe fn overlays_in_view(&mut self) -> Vec<OverlayHit> {
        let mut found = Vec::new();
        if self.panel != Panel::Chat || !self.has_messages {
            return found;
        }
        let dpi = self.dpi;
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.transcript, &mut client);
        let height = client.bottom;
        let ask = |char_at: i32| {
            let mut point = POINT { x: 0, y: 0 };
            SendMessageW(self.transcript, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(point) as WPARAM, char_at as LPARAM);
            point
        };
        // The answers: a copy icon at the row's start, and the branch icon beside it.
        let size = scale(dpi, ds::size::ICON_BUTTON);
        let forks = self.current_session.is_some();
        for at in (0..self.answers.len()).rev() {
            let point = ask(self.answers[at].0);
            if point.y < -size {
                break;
            }
            if point.y < height {
                let left = point.x - scale(dpi, 2);
                found.push(OverlayHit { area: rect(left, point.y, size, size), key: OverlayKey::Copy(at) });
                if forks {
                    found.push(OverlayHit { area: rect(left + size, point.y, size, size), key: OverlayKey::Fork(at) });
                }
            }
        }
        // The blocks of code: at the top right of each, inside it.
        let size = scale(dpi, 24);
        let x = self.transcript_rect.1 - GetSystemMetricsForDpi(SM_CXVSCROLL, dpi) * 3 / 5 - size - scale(dpi, 4);
        for at in (0..self.blocks.len()).rev() {
            if !self.blocks[at].found {
                self.blocks[at].at = text_start(self.transcript, self.blocks[at].at);
                self.blocks[at].found = true;
            }
            let point = ask(self.blocks[at].at);
            if point.y < -size {
                break;
            }
            if point.y < height && !self.blocks[at].text.is_empty() {
                found.push(OverlayHit { area: rect(x, point.y + scale(dpi, 1), size, size), key: OverlayKey::Block(at) });
            }
        }
        // The rows of calls that have more to show: at the end of their words.
        let size = scale(dpi, 20);
        for at in (0..self.runs.len()).rev() {
            if self.runs[at].first < 0 {
                continue;
            }
            let point = ask(self.runs[at].head_end);
            if point.y < -size {
                break;
            }
            if self.runs[at].expandable() && point.y < height {
                found.push(OverlayHit { area: rect(point.x + scale(dpi, 4), point.y + scale(dpi, 4), size, size), key: OverlayKey::Run(at) });
            }
        }
        // The controls of the cards: each row of a card's room has its parts across it, from where the row begins, a part with no width of its own taking what is left. (The room's width is the
        // card's, less the space its rows keep at each side of their words: as much at the right as the first of its words stands from the card's left edge.)
        let (left_edge, right_edge) = (self.transcript_rect.0, self.column_right - self.transcript_area.left);
        let gap = scale(dpi, ds::space::S);
        for at in (0..self.card_slots.len()).rev() {
            let room = &self.card_slots[at];
            let point = ask(room.start);
            if point.y + room.tall <= 0 {
                break;
            }
            if point.y >= height {
                continue;
            }
            let total = (right_edge - (point.x - left_edge) - point.x).max(scale(dpi, 120));
            let fixed: i32 = room.parts.iter().map(|part| part.width).sum::<i32>() + gap * (room.parts.len() as i32 - 1);
            let rest = (total - fixed).max(scale(dpi, 60));
            let mut x = point.x;
            for (place, part) in room.parts.iter().enumerate() {
                let width = if part.width == 0 { rest } else { part.width };
                found.push(OverlayHit { area: rect(x, point.y, width, room.tall), key: OverlayKey::Card(at, place) });
                x += width + gap;
            }
        }
        // The pictures of the messages: each tile where its card's room is in this frame (the place of the room's character is its top, and the card has its padding there).
        let mut wanted: Vec<(PathBuf, (i32, i32))> = Vec::new();
        for at in (0..self.picture_blocks.len()).rev() {
            let block = &self.picture_blocks[at];
            let point = ask(block.start);
            if point.y + block.tiles.total_height() < 0 {
                break;
            }
            if point.y >= height {
                continue;
            }
            for (index, path) in block.paths.iter().enumerate() {
                let (x, y) = block.tiles.at(index);
                let area = picture_area(point, (x, y), (block.tiles.width, block.tiles.height), dpi);
                if tile_is_in_view(&area, height) && wanted.len() < PICTURES_IN_VIEW {
                    found.push(OverlayHit { area, key: OverlayKey::Picture(at, index) });
                    wanted.push((path.clone(), (block.tiles.width, block.tiles.height)));
                }
            }
        }
        // (what the view wants is asked for, or kept from being forgotten, before any of it is drawn)
        self.thumb_wanted = wanted.iter().cloned().collect();
        for (path, room) in wanted {
            self.thumb_of(&path, room);
        }
        found
    }

    /// The icons, in the transcript's paint: after the text, in the same device context, each where the text has put its place in this very frame. They are part of the pixels the host
    /// moves when it scrolls them, and the band that comes into view is drawn with its own. Whatever the pointer or the keys have done to one shows in its colour.
    /// `dx`, `dy`: where the transcript's own pixels are in `device` (the bitmap of the area drawn, or the window itself).
    unsafe fn paint_overlay_icons(&mut self, device: HDC, dx: i32, dy: i32) {
        self.overlay_hits = self.overlays_in_view();
        let dpi = self.dpi;
        let focused = {
            let focus = GetFocus();
            self.proxies.iter().position(|proxy| *proxy == focus).and_then(|at| self.proxy_keys[at])
        };
        let ring = focused.is_some() && keys_are_in_use();
        // (drawing a picture may ask for its small copy, which is the app's to keep: the list is taken out while it is drawn from)
        let hits = std::mem::take(&mut self.overlay_hits);
        for hit in &hits {
            let key = hit.key;
            let area = RECT { left: hit.area.left + dx, top: hit.area.top + dy, right: hit.area.right + dx, bottom: hit.area.bottom + dy };
            let pressed = self.overlay_press == Some(key) && self.overlay_hover == Some(key);
            let lit = self.overlay_hover == Some(key);
            if let OverlayKey::Card(slot, part) = key {
                if let Some(part) = self.card_slots.get(slot).and_then(|room| room.parts.get(part)) {
                    // (the card's field is the composer: its frame is lit while the composer has the focus and the question waits for what is written in it)
                    let composer = part.kind == ButtonKind::Frame && self.question.is_some() && GetFocus() == self.composer;
                    paint_button(device, area, part.kind, &part.text, theme::code_back(), lit || composer, pressed, ring && focused == Some(key));
                }
                continue;
            }
            if let OverlayKey::Picture(block, index) = key {
                let Some((path, room)) = self.picture_blocks.get(block).and_then(|block| block.paths.get(index).map(|path| (path.clone(), (block.tiles.width, block.tiles.height)))) else { continue };
                let thumb = self.thumb_of(&path, room);
                draw_tile(device, area, &thumb, false, &self.fonts);
                // (the picture's own edge, which is the tile's where it is still being read; it stands from the tile's top left)
                let edge = match thumb {
                    Thumb::Ready { width, height, .. } => RECT { left: area.left, top: area.top, right: area.left + width, bottom: area.top + height },
                    _ => area,
                };
                if lit || pressed {
                    rounded(device, edge, scale(dpi, ds::radius::SMALL), None, Some(theme::accent()));
                }
                if ring && focused == Some(key) {
                    // (two lines, as a picture's colours are anything: one would be lost against some)
                    for inset in [1, 2] {
                        rounded(device, RECT { left: edge.left + inset, top: edge.top + inset, right: edge.right - inset, bottom: edge.bottom - inset }, scale(dpi, ds::radius::SMALL) - inset, None, Some(theme::accent()));
                    }
                }
                continue;
            }
            let copied = self.overlay_copied == Some(key);
            let name = match key {
                OverlayKey::Copy(_) | OverlayKey::Block(_) if copied => "check",
                OverlayKey::Copy(_) | OverlayKey::Block(_) => "copy",
                OverlayKey::Fork(_) => "git-branch",
                OverlayKey::Run(at) if self.runs[at].expanded => "chevron-down",
                OverlayKey::Run(_) | OverlayKey::Card(..) | OverlayKey::Picture(..) => "chevron-right",
            };
            // As an icon button of the window draws: a shade under it when the pointer is on it, one up on a card of a block of code.
            if lit || pressed {
                rounded(device, area, scale(dpi, ds::radius::SMALL), Some(if pressed { theme::raised() } else { theme::surface() }), None);
            }
            let color = if icons_marked() { 0x00FF_00FF } else if name == "check" { theme::good() } else if lit || pressed { theme::text() } else { theme::faint() };
            icon::draw(device, name, area, scale(dpi, ds::size::ICON), color);
            if ring && focused == Some(key) {
                rounded(device, RECT { left: area.left + 1, top: area.top + 1, right: area.right - 1, bottom: area.bottom - 1 }, scale(dpi, ds::radius::SMALL), None, Some(theme::accent()));
            }
        }
        self.overlay_hits = hits;
        if test_log_on() {
            let mut position = POINT { x: 0, y: 0 };
            SendMessageW(self.transcript, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
            let drawn: Vec<String> = self.overlay_hits.iter().map(|hit| match hit.key {
                // (what a control of a card says: a test finds the one it presses by it)
                OverlayKey::Card(..) => format!("{:?}@{},{}<{}>", hit.key, hit.area.left, hit.area.top, self.overlay_words(hit.key)),
                OverlayKey::Picture(..) => format!("{:?}@{},{},{},{}", hit.key, hit.area.left, hit.area.top, hit.area.right - hit.area.left, hit.area.bottom - hit.area.top),
                _ => format!("{:?}@{},{}", hit.key, hit.area.left, hit.area.top),
            }).collect();
            test_log(&format!("icons scroll_y={} dpi={} area={},{},{} {} [hover {:?} press {:?} focus {:?}]", position.y, dpi, self.transcript_area.left, self.transcript_area.top, self.transcript_area.bottom, drawn.join(" "), self.overlay_hover, self.overlay_press, focused));
        }
        // (what stands over the transcript is told of a moment after the view has come to rest)
        SetTimer(self.window, TIMER_PROXIES, 150, None);
        self.keep_overlay_tip();
    }

    /// The icon under a point of the transcript, as it was drawn.
    fn overlay_at(&self, x: i32, y: i32) -> Option<OverlayKey> {
        icon_under(&self.overlay_hits, x, y)
    }

    /// The pointer is at a point of the transcript: the icon under it is lit, and the one it left is not.
    unsafe fn overlay_pointer(&mut self, x: i32, y: i32) {
        let over = self.overlay_at(x, y);
        if over == self.overlay_hover {
            return;
        }
        let was = std::mem::replace(&mut self.overlay_hover, over);
        if test_log_on() {
            test_log(&format!("hover {was:?} -> {over:?} at {x},{y}"));
        }
        for key in [was, over].into_iter().flatten() {
            self.invalidate_overlay(key);
        }
        self.keep_overlay_tip();
    }

    /// The pointer's place again, from where it is, after the view has moved under it.
    unsafe fn overlay_pointer_again(&mut self) {
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        ScreenToClient(self.transcript, &mut point);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.transcript, &mut client);
        let inside = point.x >= 0 && point.y >= 0 && point.x < client.right && point.y < client.bottom;
        if inside {
            self.overlay_pointer(point.x, point.y);
        }
    }

    /// The one tip of the icons stands over the icon the pointer is on, with its words; none where it is on none.
    unsafe fn keep_overlay_tip(&mut self) {
        let wanted = self.overlay_hover.and_then(|key| self.overlay_hits.iter().find(|hit| hit.key == key).map(|hit| (key, hit.area)));
        let same = |a: &Option<(OverlayKey, RECT)>, b: &Option<(OverlayKey, RECT)>| match (a, b) {
            (Some((key, area)), Some((other, place))) => key == other && same_rect(area, place),
            (None, None) => true,
            _ => false,
        };
        if same(&wanted, &self.overlay_tip_for) {
            return;
        }
        let mut tool: TTTOOLINFOW = std::mem::zeroed();
        tool.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        tool.hwnd = self.transcript;
        tool.uId = OVERLAY_TIP;
        if self.overlay_tip_for.is_some() {
            SendMessageW(self.tooltip, TTM_DELTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
        }
        if let Some((key, area)) = wanted {
            // (a control of a card says what it is by its own words, and has a tip only where it has one: an icon alone)
            let words = match key {
                OverlayKey::Card(slot, part) => self.card_slots.get(slot).and_then(|room| room.parts.get(part)).map_or(String::new(), |part| part.tip.clone()),
                _ => self.overlay_words(key),
            };
            if !words.is_empty() {
                if test_log_on() {
                    test_log(&format!("tip {key:?} <{words}>"));
                }
                let words = wide(&words);
                tool.uFlags = TTF_SUBCLASS;
                tool.rect = area;
                tool.lpszText = words.as_ptr() as *mut u16;
                SendMessageW(self.tooltip, TTM_ADDTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
            }
        }
        self.overlay_tip_for = wanted;
    }

    /// The hidden controls say what the icons in view are, top to bottom: Tab goes to each, a screen reader reads its words, and Enter or Space does what the icon does.
    unsafe fn sync_proxies(&mut self) {
        let keys = proxy_order(&self.overlay_hits);
        let (left, top) = (self.transcript_area.left + 1, self.transcript_area.top);
        for slot in 0..PROXY_POOL {
            let proxy = self.proxies[slot];
            match keys.get(slot) {
                Some(key) => {
                    let words = self.overlay_words(*key);
                    if get_text(proxy) != words {
                        set_text(proxy, &words);
                    }
                    // (a place in the order of the page's controls, and no size)
                    SetWindowPos(proxy, std::ptr::null_mut(), left, top + slot as i32, 0, 0, SWP_NOZORDER | SWP_NOACTIVATE);
                    self.proxy_keys[slot] = Some(*key);
                    if IsWindowVisible(proxy) == 0 {
                        show(proxy, true);
                    }
                }
                None => {
                    self.proxy_keys[slot] = None;
                    if IsWindowVisible(proxy) != 0 && GetFocus() != proxy {
                        show(proxy, false);
                    }
                }
            }
        }
    }

    /// No words for a control any more.
    unsafe fn untip(&self, window: HWND) {
        let mut tool: TTTOOLINFOW = std::mem::zeroed();
        tool.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        tool.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
        tool.hwnd = GetParent(window);
        tool.uId = window as usize;
        SendMessageW(self.tooltip, TTM_DELTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
    }

    /// Words for a control when the pointer rests on it.
    unsafe fn tip(&self, window: HWND, words: &str) {
        let text = wide(words);
        let mut tool: TTTOOLINFOW = std::mem::zeroed();
        tool.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        tool.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
        tool.hwnd = GetParent(window);
        tool.uId = window as usize;
        tool.lpszText = text.as_ptr() as *mut u16;
        SendMessageW(self.tooltip, TTM_DELTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
        SendMessageW(self.tooltip, TTM_ADDTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
    }

    /// Each button on the waiting messages says what it does, as a tip over its place in the window. Only a change is passed on: a tip begun
    /// stays while the window is laid out again under it.
    unsafe fn tip_queue_buttons(&mut self) {
        let now: Vec<(RECT, QueueAct)> = self.queue_hits.iter().map(|(area, _, act)| (*area, *act)).collect();
        if now.len() == self.queue_tips.len() && now.iter().zip(&self.queue_tips).all(|(a, b)| same_rect(&a.0, &b.0) && a.1 == b.1) {
            return;
        }
        // (ids far from any window's handle, which the other tips are known by)
        let id = |at: usize| usize::MAX - 64 - at;
        let mut tool: TTTOOLINFOW = std::mem::zeroed();
        tool.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        tool.hwnd = self.window;
        for at in 0..self.queue_tips.len() {
            tool.uId = id(at);
            SendMessageW(self.tooltip, TTM_DELTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
        }
        for (at, (area, act)) in now.iter().enumerate() {
            let text = wide(t(act.tip()));
            tool.uFlags = TTF_SUBCLASS;
            tool.uId = id(at);
            tool.rect = *area;
            tool.lpszText = text.as_ptr() as *mut u16;
            SendMessageW(self.tooltip, TTM_ADDTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
        }
        self.queue_tips = now;
    }

    /// These files are looked over, on another thread as a large one takes a while, and come to the next message. Why one cannot is said.
    unsafe fn attach_files(&mut self, paths: Vec<PathBuf>) {
        let poster = self.poster.clone();
        std::thread::spawn(move || {
            for path in paths {
                let result = dogmind_agent::images::attach_file(&path);
                poster.send(Note::Attached { path, result });
            }
        });
        self.focus_composer();
    }

    /// One has been looked over.
    unsafe fn file_attached(&mut self, path: PathBuf, result: Result<dogmind_agent::images::Attachment, String>) {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
        match result {
            Err(words) => self.warn(&format!("{name}: {}", lang::notice(&words))),
            Ok(_) if self.attachments.iter().any(|attached| attached.path == path) => {}
            Ok(_) if self.attachments.len() >= attach::MOST_IMAGES => self.warn(&tf("Up to {0} pictures can go with one message.", &[&attach::MOST_IMAGES])),
            Ok(picture) => {
                self.attachments.push(Attached { path, picture });
                self.show_attachments();
                self.tell_about_vision();
                self.message_begun();
            }
        }
    }

    /// The small copies above the input, as `attachments` now is.
    unsafe fn show_attachments(&mut self) {
        let room = self.attached_room();
        for at in 0..self.attached_windows.len() {
            let window = self.attached_windows[at];
            match self.attachments.get(at).map(|attached| attached.path.clone()) {
                Some(path) => {
                    let thumb = self.thumb_of(&path, room);
                    set_thumb(window, thumb);
                    let name = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
                    set_text(window, &name);
                    self.tip(window, &name);
                }
                None => show(window, false),
            }
        }
        self.update_send_enabled();
        self.layout();
    }

    unsafe fn remove_attachment(&mut self, at: usize) {
        if at < self.attachments.len() {
            self.attachments.remove(at);
            self.show_attachments();
            self.focus_composer();
        }
    }

    /// Files dropped on the window: a picture goes with the message, and anything else is named in it, as `@` does.
    unsafe fn files_came(&mut self, paths: Vec<PathBuf>) {
        if self.onboarding {
            return;
        }
        if self.panel != Panel::Chat {
            self.set_panel(Panel::Chat);
        }
        let (pictures, others): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|path| attach::is_image(path));
        if !others.is_empty() {
            let named: Vec<String> = others.iter().map(|path| match path.strip_prefix(&self.workspace) {
                Ok(inside) => format!("@{} ", inside.display()),
                Err(_) => format!("{} ", path.display()),
            }).collect();
            SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
            let text = wide(&named.concat());
            SendMessageW(self.composer, EM_REPLACESEL, 1, text.as_ptr() as LPARAM);
        }
        if pictures.is_empty() {
            self.focus_composer();
        } else {
            self.attach_files(pictures);
        }
    }

    /// Ctrl+V in the composer, where what is on the clipboard is a picture or files. `false`: it is text, for the control to paste.
    unsafe fn paste_pictures(&mut self) -> bool {
        const CF_BITMAP: u32 = 2;
        const CF_UNICODETEXT: u32 = 13;
        const CF_HDROP: u32 = 15;
        let kind = attach::paste_kind(IsClipboardFormatAvailable(CF_HDROP) != 0, IsClipboardFormatAvailable(CF_UNICODETEXT) != 0, IsClipboardFormatAvailable(CF_BITMAP) != 0);
        if matches!(kind, attach::Paste::Text | attach::Paste::Nothing) || OpenClipboard(self.window) == 0 {
            return false;
        }
        if kind == attach::Paste::Files {
            let files = dropped_files(GetClipboardData(CF_HDROP) as HDROP);
            CloseClipboard();
            self.files_came(files);
            return true;
        }
        let pixels = attach::bitmap_pixels(GetClipboardData(CF_BITMAP) as HBITMAP);
        CloseClipboard();
        let Some((width, height, pixels)) = pixels else {
            self.warn(t("The picture on the clipboard could not be read."));
            return true;
        };
        // A file of its own, which the transcript shows until the session keeps its copy. Made elsewhere: a large PNG takes a moment.
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let (poster, path) = (self.poster.clone(), attach::pasted_path(&self.folder, stamp));
        std::thread::spawn(move || {
            let result = attach::png_of(width, height, &pixels).and_then(|png| {
                let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, &png));
                saved.map_err(|error| format!("{}: {error}", path.display())).map(|()| png)
            }).and_then(|png| dogmind_agent::images::attach_bytes("pasted.png", png));
            poster.send(Note::Attached { path, result });
        });
        self.focus_composer();
        true
    }

    /// What was asked for has no way to see a picture: said once, and with what can be done about it where something can.
    unsafe fn tell_about_vision(&mut self) {
        let local = self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded);
        if !local || self.vision != Some(false) || self.vision_told {
            return;
        }
        self.vision_told = true;
        let Some(installed) = self.vision_installed() else {
            self.warn(t("This model can't read pictures."));
            return;
        };
        match crate::vision::state(&installed) {
            crate::vision::State::Off(_) => {
                self.vision_offer = Some(VisionOffer::TurnOn);
                self.layout();
            }
            crate::vision::State::On(_) => self.warn(t("This model's vision file did not load. Its log says why: /log")),
            // (whether this model has such a file at all, and how large, is Straydog's to say)
            crate::vision::State::Absent => match engine::find(&self.settings) {
                Some(straydog) => {
                    let poster = self.poster.clone();
                    std::thread::spawn(move || {
                        let size = engine::load_catalog(&straydog).ok().and_then(|catalog| catalog.entries.into_iter().find(|entry| entry.is(&installed.bundle))).and_then(|entry| entry.vision_bytes);
                        poster.send(Note::VisionSize(size));
                    });
                }
                None => self.warn(t("This model can't read pictures.")),
            },
        }
    }

    /// The Explorer's window for choosing pictures.
    unsafe fn choose_pictures(&mut self) {
        let chosen = preview::choose_images(self.window as isize, t("Choose pictures"), t("Pictures"));
        if !chosen.is_empty() {
            self.attach_files(chosen);
        }
    }

    /// A file as a command names it: from the session's folder unless the whole way is given.
    fn named_file(&self, name: &str) -> PathBuf {
        let path = Path::new(name);
        if path.is_absolute() { path.to_path_buf() } else { self.workspace.join(path) }
    }

    /// A line of the transcript about something that needs the person.
    unsafe fn warn(&mut self, text: &str) {
        self.end_line();
        self.append(&format!("{text}\n"), Style::Warn);
        self.line_start = true;
    }

    /// Opens a picture of the transcript or of the composer beside the conversation; a file that has gone is said to have.
    unsafe fn open_picture(&mut self, path: &Path) {
        if !path.is_file() {
            self.warn(&tf("{0} is no longer there.", &[&path.display()]));
            return;
        }
        // A picture of an encrypted session is sealed in its file, and the browser beside the conversation reads files: the one that is asked for is put, plain, in the folder
        // that is emptied at every start, and no other is.
        let shown = if dogmind_agent::images::is_sealed(path) {
            let folder = self.folder.join("attachments");
            let copy = folder.join(path.file_name().unwrap_or_default());
            let made = dogmind_agent::images::read_stored(path).and_then(|bytes| std::fs::create_dir_all(&folder).and_then(|()| std::fs::write(&copy, bytes)));
            if made.is_err() {
                self.warn(t("This picture cannot be opened here."));
                return;
            }
            copy
        } else {
            path.to_path_buf()
        };
        self.open_preview(&shown.display().to_string());
    }

    fn current_mode(&self) -> Mode {
        match self.settings.mode.as_str() {
            "accept_edits" => Mode::AcceptEdits,
            "auto" => Mode::Auto,
            "plan" => Mode::Plan,
            _ => Mode::Default,
        }
    }

    /// The command (or what is to be written) on the card, in a monospace face with a Japanese one for what is Japanese in it, and room between its lines.
    unsafe fn show_command(&self, text: &str) {
        set_text(self.note, "");
        let format = char_format(theme::text(), 0, "Consolas", ds::points::CODE, None);
        SendMessageW(self.note, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
        associate_japanese(self.note, ds::points::CODE);
        set_text(self.note, text);
        let mut paragraph = para_format(ds::points::CODE, 0, 0, 0);
        paragraph.line_spacing = command_line_twips();
        SendMessageW(self.note, EM_SETSEL, 0, -1);
        SendMessageW(self.note, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
        SendMessageW(self.note, EM_SETSEL, 0, 0);
    }

    /// The folder's files for `@`, and its branch: on another thread, as a large tree takes a while.
    unsafe fn look_at_workspace(&self) {
        let (poster, path) = (self.poster.clone(), self.workspace.clone());
        std::thread::spawn(move || {
            let files = dogmind_agent::tools::workspace_files(&path, 20000);
            let branch = dogmind_agent::tools::git_branch(&path);
            let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
            let skills = dogmind_agent::skills::one_each(dogmind_agent::skills::discover(&path, home.as_deref()));
            poster.send(Note::Workspace { path, files, branch, skills });
        });
    }

    /// What a skill needs that this app has not, if anything: tools of a server that is not configured, or a feature of the app it was written for.
    fn skill_missing(&self, skill: &dogmind_agent::skills::Skill) -> Option<dogmind_agent::skills::Missing> {
        skill.needs_what_is_not_here(&self.mcp.server_parts())
    }

    /// Whether a skill is on: as the person chose, and with no choice, as its kind says (off when it is written for another app's features).
    fn skill_enabled(&self, name: &str) -> bool {
        let foreign = self.skills.iter().find(|skill| skill.name.eq_ignore_ascii_case(name)).is_some_and(|skill| self.skill_missing(skill).is_some());
        self.settings.skill_is_on(name, foreign)
    }

    /// What the agents are told to leave out.
    fn skills_off_now(&self) -> Vec<String> {
        let foreign: Vec<String> = self.skills.iter().filter(|skill| self.skill_missing(skill).is_some()).map(|skill| skill.name.clone()).collect();
        self.settings.skills_off_for(&foreign)
    }

    /// The agents are told what is off, for a session begun from now on (and one under way keeps its list).
    fn send_skills_off(&self) {
        let off = self.skills_off_now();
        let _ = self.agent.commands.send(Command::SetSkillsOff(off.clone()));
        for parked in &self.parked {
            let _ = parked.agent.commands.send(Command::SetSkillsOff(off.clone()));
        }
    }

    /// What is offered above the composer for the word being typed: commands after `/`, files after `@`.
    unsafe fn update_suggestions(&mut self) {
        let text = get_text(self.composer);
        let mut found = Vec::new();
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            for (name, about) in COMMANDS {
                if name.starts_with(&text) {
                    found.push(Suggestion { title: name.to_string(), detail: t(about).to_string(), insert: format!("{name} ") });
                }
            }
            for skill in self.skills.iter().filter(|skill| skill.for_the_person && self.skill_enabled(&skill.name) && format!("/{}", skill.name).to_lowercase().starts_with(&text.to_lowercase())) {
                let about: String = skill.description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(90).collect();
                found.push(Suggestion { title: format!("/{}", skill.name), detail: about, insert: format!("/{} ", skill.name) });
            }
        } else if let Some(word) = text.rsplit(char::is_whitespace).next().filter(|word| word.starts_with('@')) {
            let wanted = word[1..].to_lowercase();
            for name in self.files.iter().filter(|name| name.to_lowercase().contains(&wanted)).take(30) {
                found.push(Suggestion { title: format!("@{name}"), detail: String::new(), insert: format!("@{name} ") });
            }
        }
        let same_size = found.len() == self.suggestions.len();
        self.suggestions = found;
        SendMessageW(self.suggest, LB_RESETCONTENT, 0, 0);
        for _ in &self.suggestions {
            SendMessageW(self.suggest, LB_ADDSTRING, 0, 0);
        }
        if !self.suggestions.is_empty() {
            SendMessageW(self.suggest, LB_SETCURSEL, 0, 0);
        }
        if !same_size {
            self.layout();
        }
        show(self.composer_cue, self.panel == Panel::Chat && text.is_empty());
    }

    /// The offered word in place of what was being typed.
    unsafe fn accept_suggestion(&mut self, at: usize) {
        let Some(chosen) = self.suggestions.get(at).cloned() else { return };
        let text = get_text(self.composer);
        let start = text.rfind(char::is_whitespace).map_or(0, |space| space + text[space..].chars().next().map_or(1, char::len_utf8));
        let replaced = format!("{}{}", &text[..start], chosen.insert);
        set_text(self.composer, &replaced);
        SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
        self.focus_composer();
        self.update_suggestions();
        self.suggestions.clear();
        SendMessageW(self.suggest, LB_RESETCONTENT, 0, 0);
        self.layout();
    }

    unsafe fn say(&mut self, text: &str) {
        self.end_line();
        self.append(&format!("{text}\n"), Style::Tool);
        self.line_start = true;
    }

    /// A line starting with `/`: `true` when it was a command, and has been done.
    unsafe fn run_command(&mut self, line: &str) -> bool {
        let Some(rest) = line.strip_prefix('/') else { return false };
        let (name, argument) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(name, argument)| (name, argument.trim()));
        match name {
            "help" => self.set_panel(Panel::Help),
            "report" => self.write_report(),
            "about" => self.set_panel(Panel::About),
            "status" => {
                let straydog = engine::find(&self.settings);
                let engine_line = straydog.as_deref().and_then(engine::version).map_or(t("not found").to_string(), |version| format!("{version}  ({})", straydog.as_deref().map_or(String::new(), |path| path.display().to_string())));
                let provider = self.settings.active_provider().map_or(String::new(), |provider| format!("{}  ·  {}", provider.name, t(provider.kind.label())));
                let mode = self.current_mode();
                let mode = t(MODES.iter().find(|(known, _)| *known == mode).map_or("", |(_, name)| name));
                let efforts = self.efforts();
                let effort = t(efforts.iter().find(|(known, _)| *known == self.settings.effort.as_deref()).map_or("Default", |(_, name)| name));
                let lines = [
                    format!("Dogmind  {}", env!("CARGO_PKG_VERSION")),
                    format!("Build        {}", readable_build(env!("DOGMIND_APP_BUILD"))),
                    format!("Dogmind      {}", dogmind_agent::prompt::DOGMIND_COMMIT.trim().chars().take(10).collect::<String>()),
                    format!("Tools        {}", self.current_session.as_deref().and_then(session::tools_of).map_or("this session has not saved its own".to_string(), |tools| session::describe_tools(&tools))),
                    status_line("Engine", &engine_line),
                    status_line("Provider", &provider),
                    status_line("Model", &self.model),
                    status_line("Connection", t(&self.connection)),
                    status_line("Folder", &self.workspace.display()),
                    status_line("Branch", self.branch.as_deref().unwrap_or(t("none"))),
                    status_line("Session", &self.current_session.as_ref().map_or(t("not saved yet").to_string(), |path| path.display().to_string())),
                    status_line("Approvals", &mode),
                    status_line("Web", &match (self.settings.web_tools, self.settings.search_key.trim().is_empty(), web_settings(&self.settings).search_key.is_some()) {
                        (false, _, _) => t("off"),
                        (true, true, _) => t("on, with DuckDuckGo and Bing"),
                        (true, false, true) => t("on, with Brave"),
                        (true, false, false) => t("on, with DuckDuckGo and Bing: the search key could not be read"),
                    }),
                    status_line("Effort", &effort),
                    status_line("Conversation length", &tf("{0} of {1} tokens", &[&self.used, &self.window_tokens])),
                ];
                self.end_line();
                for line in lines {
                    self.append(&format!("{line}\n"), Style::Output);
                }
                self.line_start = true;
            }
            "setup" => {
                self.onboarding = true;
                self.set_panel(Panel::Setup);
            }
            "new" | "clear" => {
                let workspace = self.workspace.clone();
                self.start_session(workspace);
            }
            "find" => {
                self.open_find();
                if !argument.is_empty() {
                    set_text(self.find_field, argument);
                    self.find_update();
                }
            }
            "search" => {
                set_text(self.side_search, argument);
                SetFocus(self.side_search);
                SendMessageW(self.side_search, EM_SETSEL, usize::MAX, -1);
                self.search_now();
            }
            "resume" => self.set_panel(Panel::Sessions),
            "rename" => {
                if let Some(path) = self.current_session.clone() {
                    if argument.is_empty() {
                        self.rename_target = Some(path);
                        self.set_panel(Panel::Rename);
                    } else {
                        let _ = session::rename(&path, argument);
                        self.session_title = argument.to_string();
                        self.refresh_sessions();
                        self.layout();
                    }
                } else {
                    self.say(t("This session has no messages yet."));
                }
            }
            "pin" => match self.current_session.clone() {
                Some(path) => self.toggle_pin(path),
                None => self.say(t("This session has no messages yet.")),
            },
            "delete" => match self.current_session.clone() {
                Some(path) => {
                    let title = self.session_title.clone();
                    self.ask_to_delete(path, title);
                }
                None => self.say(t("This session has no messages yet.")),
            },
            "compact" => self.on_menu(M_COMPACT),
            "rewind" => {
                if self.running {
                    self.say(t("Stop the work first."));
                } else {
                    let _ = self.agent.commands.send(Command::ListTurns);
                }
            }
            "permissions" => self.set_panel(Panel::Permissions),
            "retry" => {
                if !self.connected {
                    self.say(t("The engine is not ready yet: ask again once it is."));
                } else if !self.running {
                    self.running = true;
                    self.update_buttons();
                    self.say(t("Asking again."));
                    let _ = self.agent.commands.send(Command::Retry);
                }
            }
            "copy" => {
                if self.last_answer.is_empty() {
                    self.say(t("There is no answer to copy yet."));
                } else {
                    self.copy_to_clipboard(&self.last_answer.clone());
                    self.say(t("Copied the last answer."));
                }
            }
            "diff" => {
                self.trajectory = None;
                self.set_panel(Panel::Diff);
            }
            // (the bar under the title is where the goal is seen: none of the three writes a line in the conversation, but where there is no goal to show or to clear, which says how to set one)
            "goal" => match cards::goal_action(argument, self.goal.as_deref()) {
                cards::GoalAction::Set(goal) => {
                    let _ = self.agent.commands.send(Command::SetGoal(Some(goal)));
                }
                cards::GoalAction::Clear => {
                    let _ = self.agent.commands.send(Command::SetGoal(None));
                }
                // (the goal in the composer, as it was set, to read in full or to change and send)
                cards::GoalAction::Edit(goal) => {
                    set_text(self.composer, &format!("/goal {goal}"));
                    SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
                    self.focus_composer();
                }
                cards::GoalAction::None => self.say(t("No goal is set: /goal text sets one.")),
            },
            // (the trajectory of this session as a table: a page of its own, with the turns that open into their requests and tools)
            "trajectory" => match self.current_session.as_deref().map(dogmind_agent::trajectory::file_of) {
                Some(file) if file.exists() => match dogmind_agent::trajectory_report::summary_file(&file) {
                    Ok(summary) => self.show_trajectory(summary),
                    Err(error) => self.say(&tf("The trajectory cannot be read: {0}", &[&error])),
                },
                _ => self.say(t("This session has no trajectory yet: it is written as the session runs.")),
            },
            "usage" => {
                let (prompt, cached, completion, requests) = self.totals;
                let rate = self.rate.map_or(String::new(), |rate| tf(", last answer at {0} tok/s", &[&format!("{rate:.1}")]));
                self.say(&tf("{0} requests: {1} prompt tokens, {2} of them from the cache, {3} written{4}. The conversation is {5} of {6}.", &[&requests, &prompt, &cached, &completion, &rate, &self.used, &self.window_tokens]));
            }
            "init" => {
                self.send_or_wait(Outgoing { text: INIT_PROMPT.to_string(), images: Vec::new(), card: "/init".to_string() }, false);
            }
            "attach" => {
                if argument.is_empty() {
                    self.choose_pictures();
                } else {
                    self.attach_files(vec![self.named_file(argument.trim_matches('"'))]);
                }
            }
            "model" => {
                let found = self.settings.providers.iter().position(|provider| provider.name.eq_ignore_ascii_case(argument));
                let offered = self.provider_models.iter().position(|model| model.eq_ignore_ascii_case(argument));
                match (found, offered) {
                    (Some(at), _) => self.on_menu(M_PROVIDER + at),
                    (None, Some(at)) => self.on_menu(M_MODEL_CHOICE + at),
                    (None, None) => self.on_chip(ID_MODEL_CHIP),
                }
            }
            "effort" => {
                match effort_asked_for(argument) {
                    Some(at) => self.on_menu(M_EFFORT + at),
                    None => self.on_chip(ID_EFFORT_CHIP),
                }
            }
            "mode" => {
                let at = match argument.to_lowercase().as_str() {
                    "ask" | "default" => Some(0),
                    "edits" | "accept" => Some(1),
                    "auto" => Some(2),
                    "plan" => Some(3),
                    _ => None,
                };
                match at {
                    Some(at) => self.on_menu(M_MODE + at),
                    None => self.on_chip(ID_MODE_CHIP),
                }
            }
            "panel" => self.toggle_right_panel(),
            "files" => self.show_files_view(),
            "terminal" => self.terminal_command(),
            "preview" => {
                let wanted = if argument.is_empty() { self.preview_candidate() } else { Some(argument.to_string()) };
                match wanted {
                    Some(wanted) => self.open_preview(&wanted),
                    None => self.say(t("Nothing to show yet. Name a file: /preview page.html")),
                }
            }
            "theme" => self.next_theme(),
            "instructions" => self.set_panel(Panel::Instructions),
            "skills" => self.set_panel(Panel::Skills),
            "mcp" => {
                let file = self.folder.join("mcp.json");
                if argument.eq_ignore_ascii_case("import") {
                    let said = self.mcp_servers_imported();
                    self.say(&said);
                } else {
                    let lines = self.mcp.status();
                    self.say(&if lines.is_empty() { tf("No server yet. {0} lists them, and /mcp import copies Claude's and Codex's in.", &[&file.display()]) } else { lines.join("\n") });
                }
            }
            "plan" => self.on_menu(M_MODE + 3),
            "memory" => self.set_panel(Panel::Memory),
            "tools" => {
                let closed = dogmind_agent::sandbox::closed_tool_folders();
                if closed.is_empty() {
                    self.say(t("Every folder on PATH is open to sandboxed commands."));
                } else if let Some(command) = dogmind_agent::sandbox::open_folders_command(&closed) {
                    let names: Vec<String> = closed.iter().map(|folder| folder.display().to_string()).collect();
                    self.say(&tf("Windows will ask to let sandboxed commands read and run: {0}. Nothing else changes.", &[&names.join(", ")]));
                    let (verb, program, arguments) = (wide("runas"), wide("cmd.exe"), wide(&format!("/c {command}")));
                    ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), program.as_ptr(), arguments.as_ptr(), std::ptr::null(), SW_HIDE);
                }
            }
            "dashboard" => match self.dashboard_url() {
                Some(url) => open_in_browser(&url),
                None => self.say(t("Straydog is not what this session is connected to, so it has no dashboard to open.")),
            },
            "think" => {
                self.on_menu(M_THINKING);
                self.say(if self.settings.show_thinking { t("The thinking is shown.") } else { t("The thinking is hidden.") });
            }
            "safe" => {
                self.on_menu(M_SAFE);
                self.say(if self.settings.safe_commands { t("Safe commands run without asking.") } else { t("Every command asks.") });
            }
            "workspace" => self.folder_menu(),
            "providers" => self.set_panel(Panel::Providers),
            "models" => self.set_panel(Panel::Models),
            "settings" => self.set_panel(Panel::General),
            "log" => self.on_menu(M_LOG),
            "quit" | "exit" => {
                DestroyWindow(self.window);
            }
            known if self.skills.iter().any(|skill| skill.for_the_person && skill.name.eq_ignore_ascii_case(known)) => {
                if self.skill_enabled(known) {
                    self.send_or_wait(Outgoing::words(line.to_string()), false);
                } else {
                    self.say(t("That skill is turned off. Settings > Skills turns it on."));
                }
            }
            _ => return false,
        }
        true
    }

    unsafe fn update_chips(&self) {
        let provider = self.settings.active_provider().map_or(String::new(), |provider| provider.name.clone());
        // (which provider is in the menu: the name of the model is what changes, and what there is room for)
        let label = if self.model.is_empty() { provider } else { self.model.clone() };
        // (said again for every note of a download that goes by on the status line: a chip given the words it has is drawn again for nothing)
        set_text_if_changed(self.model_chip, &label);
        let mode = self.current_mode();
        let name = MODES.iter().find(|(known, _)| *known == mode).map_or("", |(_, name)| name);
        set_text_if_changed(self.mode_chip, t(name));
        let efforts = self.efforts();
        let effort = efforts.iter().find(|(known, _)| *known == self.settings.effort.as_deref()).map_or("Default", |(_, name)| name);
        set_text_if_changed(self.effort_chip, &tf("Effort: {0}", &[&t(effort)]));
    }

    unsafe fn update_status(&mut self) {
        self.update_chips();
        let title = if self.workspace == Path::new(".") { "Dogmind".to_string() } else { format!("Dogmind — {}", self.workspace.file_name().map_or_else(|| self.workspace.display().to_string(), |name| name.to_string_lossy().into_owned())) };
        set_text_if_changed(self.window, &title);
        InvalidateRect(self.window, &self.status_area, 0);
    }

    unsafe fn set_activity(&mut self, text: String) {
        // (whatever the turn says it is doing, what shows is that it is being stopped)
        let text = if self.stop_asked && !text.is_empty() { t("Stopping…").to_string() } else { text };
        if text.is_empty() {
            self.activity_since = None;
        } else if self.activity_since.is_none() {
            self.activity_since = Some(std::time::Instant::now());
        }
        if self.activity != text {
            self.activity = text;
            InvalidateRect(self.window, &self.activity_area, 0);
        }
    }

    unsafe fn save(&mut self) {
        // (what is being taken away is not written back)
        if !matches!(self.leaving, Leaving::No) {
            return;
        }
        if let Err(error) = self.settings.save(&self.folder.join("settings.json")) {
            self.append(&tf("The settings could not be saved: {0}\n", &[&error]), Style::Bad);
        }
    }

    unsafe fn layout(&mut self) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let (width, height) = (client.right, client.bottom);
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let mut shapes = Vec::new();
        let chat = self.panel == Panel::Chat;

        // The sidebar.
        let bare = self.onboarding;
        // (a small window gives what it can to the conversation)
        let looking_at_changes = self.panel == Panel::Diff;
        let squeezed = looking_at_changes || (chat && !bare && self.preview_file.is_some() && width - s(ds::size::SIDEBAR_SMALL) < s(760));
        let sidebar = if bare || (squeezed && !looking_at_changes) { 0 } else if width < s(960) { s(ds::size::SIDEBAR_SMALL) } else { s(ds::size::SIDEBAR) };
        // (the trajectory's table has the whole window: there is no sidebar to fill)
        self.sidebar_width = if looking_at_changes && self.trajectory.is_some() { 0 } else { sidebar };
        let inset = s(ds::space::S);
        let side_row = s(ds::size::SIDE_ROW);
        let side_width = sidebar - 2 * inset;
        let new_top = s(ds::space::S);
        place(self.new_session, inset, new_top, side_width, side_row);
        let search_top = new_top + side_row + s(ds::space::HAIR);
        if !bare && !squeezed {
            shapes.push(Shape::Icon { name: "search", area: rect(inset + s(ds::space::M), search_top, s(ds::size::ICON), side_row), size: s(ds::size::ICON), color: theme::soft() });
        }
        let text_x = inset + s(ds::space::M) + s(ds::size::ICON + ds::space::S);
        place(self.side_search, text_x, search_top + s(ds::size::FIELD_INSET), sidebar - text_x - inset - s(ds::space::XS), s(ds::size::FIELD_TEXT));
        show(self.side_search, !bare && !squeezed);
        let list_top = search_top + side_row + s(ds::space::M);
        place(self.side, inset, list_top, side_width, height - list_top - s(48));
        let foot = height - s(40);
        place(self.settings_button, inset, foot, side_width - side_row - s(ds::space::XS), side_row);
        place(self.help_button, sidebar - inset - side_row, foot, side_row, side_row);
        show(self.new_session, !bare && !squeezed);
        show(self.side, !bare && !squeezed);
        show(self.settings_button, !bare && !squeezed);

        // The panel on the right, when there is room for it beside a conversation.
        // A preview takes half of what the sidebar leaves, as a page needs the width.
        let room = width - sidebar;
        // (always there while it is on and there is room: it is also where the engine's dashboard is reached from)
        let right = if !chat || bare {
            0
        } else if self.preview_file.is_some() && room >= s(760) {
            (room / 2).max(s(380))
        } else if self.preview_file.is_none() && self.settings.right_panel && room - s(ds::size::RIGHT_PANEL) >= s(RIGHT_NEEDS) {
            // (a terminal wants columns: nearly half of what the sidebar leaves, as far as the conversation can spare it)
            let wanted = if self.settings.right_view == RightView::Terminal { (room * 45 / 100).clamp(s(ds::size::RIGHT_PANEL), s(TERMINAL_PANEL_MOST)) } else { s(ds::size::RIGHT_PANEL) };
            wanted.min(room - s(RIGHT_NEEDS))
        } else {
            0
        };
        self.right_width = right;
        self.right_hits.clear();
        self.right_tab_hits.clear();
        self.preview_hits.clear();
        self.waiting_hits.clear();
        self.waiting_times.clear();
        self.dashboard_hit = None;
        if !self.files_shown() {
            self.files_hidden();
        }
        if !self.terminal_shown() {
            self.terminal_hidden();
        }
        if self.preview_file.is_none() || right == 0 {
            if let Some(shown) = &self.preview {
                shown.hide();
            }
            for button in [self.preview_reload, self.preview_browser, self.preview_close] {
                ShowWindow(button, SW_HIDE);
            }
        }
        if right > 0 {
            self.layout_right(&mut shapes, width - right + s(ds::size::RIGHT_GAP), right - 2 * s(ds::size::RIGHT_GAP), height);
        }
        // (no switch for what could not show)
        let toggles = chat && !bare && (self.preview_file.is_some() || room - s(ds::size::RIGHT_PANEL) >= s(RIGHT_NEEDS));
        show(self.right_toggle, toggles);
        show(self.folder_chip, chat && !bare);

        // The column the content keeps to, however wide the window.
        let main_left = sidebar;
        let main_width = width - main_left - right;
        let column = main_width.min(if bare { s(ds::size::COLUMN_FIRST_RUN) } else { s(ds::size::COLUMN) }) - s(48);
        let left = main_left + (main_width - column) / 2;

        for window in [self.composer, self.send, self.attach_button, self.model_chip, self.mode_chip, self.effort_chip] {
            show(window, chat);
        }
        // The notice of a newer version, on the pages that have it; what is not there is hidden, once, and not hidden and shown again by every layout.
        let notice = if notice::is_on_page(chat, looking_at_changes, bare) { self.notice() } else { None };
        if notice.is_none() {
            for window in [self.update_offer_button, self.release_offer_button, self.notice_later] {
                show(window, false);
            }
        }
        let notice_room = if notice.is_some() { self.notice_height() + s(ds::space::S) } else { 0 };
        if !chat {
            for window in &self.attached_windows {
                show(*window, false);
            }
            show(self.vision_offer_button, false);
            show(self.goal_clear, false);
        }
        show(self.help_button, !bare && !squeezed);
        show(self.suggest, chat && !self.suggestions.is_empty());
        if !chat {
            show(self.composer_cue, false);
        }
        let band = chat && self.pending.is_some();
        for window in [self.note, self.approve, self.always, self.reject, self.feedback] {
            show(window, band);
        }
        show(self.transcript, chat && self.has_messages);
        let finding = chat && self.has_messages && self.find_open;
        for window in [self.find_field, self.find_previous, self.find_next, self.find_close] {
            show(window, finding);
        }
        if chat {
            let status_height = s(ds::size::STATUS);
            let inner = s(ds::size::COMPOSER_INSET);
            // Two lines' room to begin with; it grows with what is typed, up to about half the window, and then scrolls.
            let mut typed: RECT = std::mem::zeroed();
            GetClientRect(self.composer, &mut typed);
            if typed.right != column - 2 * inner {
                SetWindowPos(self.composer, std::ptr::null_mut(), 0, 0, column - 2 * inner, typed.bottom.max(s(60)), SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            self.composer_lines = SendMessageW(self.composer, EM_GETLINECOUNT, 0, 0) as i32;
            // Pictures waiting to go stand above what is typed, in the same card.
            let strip = if self.attachments.is_empty() { 0 } else { s(76) };
            let card_height = (s(70) + self.composer_lines * s(ds::space::XL) * text_scale() / 100).clamp(s(94), (height / 2).max(s(94))) + strip;
            let foot = height - s(ds::space::S) - status_height - s(ds::space::XS) - card_height;
            // What waits to be sent stands right above the input.
            let waiting = self.waiting_rows();
            let waiting_shown = waiting.len().min(3) as i32;
            let waiting_more = waiting.len() > 3;
            let waiting_height = if waiting.is_empty() { 0 } else { waiting_shown * s(ds::size::QUEUE_STEP) + if waiting_more { s(22) } else { 0 } + s(ds::space::XS) };
            // The composer stays at the foot, conversation or not. The dog's words, the dog and its name stand in the middle of what is
            // above it: the drawing ends 91% of the way down its square.
            let dog = s(95);
            let (bubble_height, dog_gap, mark_width) = (s(72), s(3), s(150));
            let mark_height = title_height(mark_width);
            let dog_bottom = dog * 91 / 100;
            let greeting_height = bubble_height + dog_gap + dog_bottom + mark_height + s(ds::space::XL);
            let card_top = foot;
            let status_top = card_top + card_height + s(ds::space::XS);
            let above = card_top - waiting_height;
            if !self.has_messages && above - greeting_height >= s(40) + notice_room {
                let title = GREETINGS[self.greeting];
                let bubble_top = s(40) + notice_room + (above - s(40) - notice_room - greeting_height) / 2;
                let bubble_width = (text_width(self.fonts.greeting, title) + s(60)).min(column);
                let area = rect(left + (column - bubble_width) / 2, bubble_top, bubble_width, bubble_height);
                // The drawing fills its square's width, and is about 0.82 of it high, centred. Its line is about 5% of its width.
                let (dog_x, dog_top) = (left + (column - dog) / 2, bubble_top + bubble_height + dog_gap);
                let tip = (dog_x + dog * 11 / 100 - s(ds::space::XL), bubble_top + bubble_height + s(13));
                shapes.push(Shape::Bubble { area, radius: s(26), tip, fill: theme::background(), line: theme::text(), thickness: dog as f32 * 0.05 });
                shapes.push(Shape::Text { area, text: title.into(), font: FontKind::Greeting, color: theme::text(), flags: DT_CENTER | ONE_LINE });
                shapes.push(Shape::Logo { x: dog_x, y: dog_top, size: dog });
                shapes.push(Shape::TitleMark { x: left + (column - mark_width) / 2, y: dog_top + dog_bottom, width: mark_width });
            }
            shapes.push(Shape::Round { area: rect(left, card_top, column, card_height), radius: s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
            if strip > 0 {
                let gap = s(ds::space::S);
                let count = self.attachments.len() as i32;
                let tile = ((column - 2 * inner + gap) / count - gap).min(s(64));
                for (at, window) in self.attached_windows.iter().enumerate() {
                    if (at as i32) < count {
                        place(*window, left + inner + at as i32 * (tile + gap), card_top + s(10), tile, s(64));
                        SetWindowPos(*window, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                        show(*window, true);
                    }
                }
            }
            place(self.composer, left + inner, card_top + s(ds::space::M) + strip, column - 2 * inner, card_height - s(58) - strip);
            place(self.composer_cue, left + inner + s(ds::space::HAIR), card_top + s(13) + strip, column - 2 * inner - s(20), s(ds::space::XL));
            self.update_composer_cue();
            SetWindowPos(self.composer_cue, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let row_top = card_top + card_height - s(42);
            let chip = s(ds::size::ICON_BUTTON);
            // They give way to the send button in a narrow window.
            let room = column - s(100);
            let want = |chip: HWND| text_width(self.fonts.small, &get_text(chip)) + s(ds::size::BUTTON_PADDING);
            // The short ones keep their words (the mode's is never cut: "Plan first"): the long name of a model is what gives way.
            // (less the gaps between the chips and a little to the send button)
            let (mode_width, effort_width, model_width) = chip_widths(room - s(ds::space::HAIR) - s(ds::space::XS) - s(ds::space::M), want(self.mode_chip), want(self.effort_chip).min(s(260)), want(self.model_chip), s(300), s(60));
            place(self.attach_button, left + s(ds::space::S), row_top + s(ds::size::CHIP_INSET), chip, chip);
            let chips = left + s(ds::space::S) + chip + s(ds::space::HAIR);
            place(self.model_chip, chips, row_top + s(ds::size::CHIP_INSET), model_width, chip);
            place(self.mode_chip, chips + model_width + s(ds::space::HAIR), row_top + s(ds::size::CHIP_INSET), mode_width, chip);
            place(self.effort_chip, chips + model_width + mode_width + s(ds::space::XS), row_top + s(ds::size::CHIP_INSET), effort_width, chip);
            let send = s(ds::size::SEND);
            place(self.send, left + column - send - s(ds::space::S), row_top, send, send);
            if !self.suggestions.is_empty() {
                let rows = self.suggestions.len().min(8) as i32;
                let list_height = rows * s(ds::size::MENU_ROW) + s(ds::space::XS);
                let top = card_top - list_height - s(ds::space::L);
                shapes.push(Shape::Round { area: rect(left, top, column, list_height + s(ds::space::M)), radius: s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
                place(self.suggest, left + s(6), top + s(6), column - s(ds::space::M), list_height);
                SetWindowPos(self.suggest, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            // The session's name, as it scrolls under nothing: at a glance, which one this is.
            let strip = s(ds::size::TITLE_STRIP);
            let toggle = s(ds::size::ICON_BUTTON);
            // (at the right end of the conversation's own area, beside the panel it opens, not at the end of the column the words keep to)
            let toggle_x = main_left + main_width - toggle - s(ds::space::M);
            place(self.right_toggle, toggle_x, (strip - toggle) / 2, toggle, toggle);
            // (an empty session has no name to tell it by, and the dog says what it is)
            let (name, color) = if !self.session_title.is_empty() { (self.session_title.clone(), theme::text()) } else if self.has_messages { (t("New session").to_string(), theme::faint()) } else { (String::new(), theme::faint()) };
            let title_room = if toggles { column.min(toggle_x - s(ds::space::S) - left) } else { column };
            self.title_area = rect(left, 0, title_room, strip);
            shapes.push(Shape::Text { area: self.title_area, text: name, font: FontKind::Body, color, flags: ONE_LINE });
            // (what scrolls goes under an edge, and not into the name)
            if self.has_messages {
                shapes.push(Shape::Fill { area: rect(main_left, strip - s(ds::space::PIXEL), main_width, s(ds::space::PIXEL)), color: theme::line() });
            }
            let name = self.workspace.file_name().map_or_else(|| self.workspace.display().to_string(), |name| name.to_string_lossy().into_owned());
            let chip_width = (text_width(self.fonts.small, &name) + s(66)).clamp(s(90), s(280));
            set_text_if_changed(self.folder_chip, &format!("folder|{name}"));
            if self.folder_told != self.workspace {
                self.folder_told = self.workspace.clone();
                self.tip(self.folder_chip, &self.workspace.display().to_string());
            }
            place(self.folder_chip, left, status_top, chip_width, status_height);
            let mut status_left = left + chip_width + s(6);
            if let Some(branch) = &self.branch {
                let mut branch = branch.clone();
                if let Some(git) = &self.git {
                    if git.ahead > 0 {
                        branch.push_str(&format!(" ↑{}", git.ahead));
                    }
                    if git.behind > 0 {
                        branch.push_str(&format!(" ↓{}", git.behind));
                    }
                }
                let branch = &branch;
                let branch_width = text_width(self.fonts.small, branch).min(s(180));
                shapes.push(Shape::Icon { name: "git-branch", area: rect(status_left, status_top, s(ds::size::ICON_SMALL), status_height), size: s(ds::size::ICON_SMALL), color: theme::faint() });
                shapes.push(Shape::Text { area: rect(status_left + s(ds::size::STATUS_ICON), status_top, branch_width + s(ds::space::XS), status_height), text: branch.clone(), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
                status_left += s(ds::size::STATUS_ICON) + branch_width + s(ds::space::M);
            }
            self.status_area = rect(status_left, status_top, (left + column - status_left).max(0), status_height);
            self.activity_area = rect(left, card_top - s(ds::size::TEXT_ROW), column, s(ds::size::TEXT_LINE));
            let mut bottom = card_top - s(ds::size::QUEUE_STEP);
            self.queue_hits.clear();
            self.vision_offer_close = None;
            show(self.vision_offer_button, self.vision_offer.is_some());
            if let Some(offer) = self.vision_offer {
                let tall = s(ds::size::QUEUE_STEP) + s(ds::space::XS);
                let row = rect(left, bottom - tall, column, tall);
                shapes.push(Shape::Round { area: row, radius: s(ds::radius::CONTROL), fill: Some(theme::surface()), border: None });
                let (words, action) = match offer {
                    _ if self.setting_up && self.vision_fetch => (t("Fetching the vision file.").to_string(), t("Cancel")),
                    VisionOffer::Download(bytes) => (tf("Reading pictures takes a file of {0} that this model does not have yet.", &[&engine::gb_file(bytes)]), t("Download")),
                    VisionOffer::TurnOn => (t("Reading pictures is turned off.").to_string(), t("Turn on")),
                };
                set_text_if_changed(self.vision_offer_button, action);
                let cross = s(ds::space::XL);
                let across = (text_width(self.fonts.body, action) + s(ds::space::XL)).max(s(72));
                let button_x = left + column - cross - s(ds::space::XS) - across;
                place(self.vision_offer_button, button_x, row.top + (tall - s(ds::size::ICON_BUTTON)) / 2, across, s(ds::size::ICON_BUTTON));
                SetWindowPos(self.vision_offer_button, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                shapes.push(Shape::Text { area: rect(left + s(ds::space::M), row.top, (button_x - left - s(ds::space::L)).max(0), tall), text: words, font: FontKind::Small, color: theme::soft(), flags: ONE_LINE });
                let close = rect(left + column - cross - s(ds::space::XS), row.top + (tall - cross) / 2, cross, cross);
                shapes.push(Shape::Icon { name: "x", area: close, size: s(ds::space::M), color: theme::muted() });
                self.vision_offer_close = Some(close);
                bottom -= tall + s(ds::space::XS);
            }
            if !waiting.is_empty() {
                let mut y = bottom - waiting_height;
                for (at, (words, when, now)) in waiting.iter().take(3).enumerate() {
                    let row = rect(left, y, column, s(ds::size::TEXT_ROW));
                    shapes.push(Shape::Round { area: row, radius: s(ds::radius::CONTROL), fill: Some(theme::surface()), border: None });
                    // Its buttons at the right end, the last of them where the one cross stood.
                    let acts = queue_acts(*now);
                    let step = s(26);
                    let buttons = s(28) + (acts.len() as i32 - 1) * step;
                    // (the words of when it goes keep a little way off the first button)
                    let buttons_room = buttons + if acts.len() > 1 { s(ds::space::S) } else { 0 };
                    for (place, act) in acts.iter().enumerate() {
                        let x = left + column - buttons + place as i32 * step;
                        let area = rect(x, y + s(ds::space::PIXEL), s(ds::space::XL), s(ds::space::XL));
                        shapes.push(Shape::Icon { name: act.icon(), area, size: s(ds::space::M), color: theme::muted() });
                        self.queue_hits.push((area, at, *act));
                    }
                    let when = t(when);
                    let when_width = (text_width(self.fonts.small, when) + s(ds::space::S)).min(column * 45 / 100);
                    shapes.push(Shape::Text { area: rect(left + column - buttons_room - when_width, y, when_width, s(ds::size::TEXT_ROW)), text: when.into(), font: FontKind::Small, color: theme::faint(), flags: DT_RIGHT | ONE_LINE });
                    shapes.push(Shape::Text { area: rect(left + s(ds::space::M), y, (column - buttons_room - s(ds::space::M) - when_width).max(0), s(ds::size::TEXT_ROW)), text: words.clone(), font: FontKind::Small, color: theme::soft(), flags: ONE_LINE });
                    y += s(ds::size::QUEUE_STEP);
                }
                if waiting_more {
                    shapes.push(Shape::Text { area: rect(left + s(ds::space::M), y - s(ds::space::XS), column - s(ds::space::XL), s(22)), text: tf("and {0} more", &[&(waiting.len() - 3)]), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
                }
                bottom -= waiting_height + s(ds::space::XS);
            }
            self.tip_queue_buttons();
            if band {
                // As the lines wrap at this width, however many the text has. It scrolls past what fits.
                SetWindowPos(self.note, std::ptr::null_mut(), 0, 0, column - s(ds::space::XXL), s(100), SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                let lines = SendMessageW(self.note, EM_GETLINECOUNT, 0, 0) as i32;
                // (the command's lines are as high as they were set to be, exactly: so many twips, of which a pixel is dpi / 1440)
                let line = (command_line_twips() * self.dpi as i32 / 1440).max(1);
                // What is said in sentences, above it: as high as its words are at this width, and no more than a few lines.
                let head_width = column - 2 * s(ds::space::L);
                let head_room = if self.approval_head.is_empty() { 0 } else { text_height(self.fonts.body, &self.approval_head, head_width).min(s(100)) + s(ds::space::S) };
                // A small window keeps a little of the conversation above the card, and the header clear of it.
                let most = s(320).min((bottom - s(200) - head_room).max(s(80)));
                let note_height = (lines * line + s(ds::size::FIELD_INSET)).clamp(s(ds::space::XXL), most);
                ShowScrollBar(self.note, SB_VERT, i32::from(lines * line + s(ds::size::FIELD_INSET) > most));
                let buttons = s(ds::size::HEAD);
                let line = if self.always_line.is_empty() { 0 } else { s(ds::size::TEXT_ROW) };
                let top = bottom - head_room - note_height - line - buttons - s(ds::size::MENU_ROW);
                let area = rect(left, top, column, head_room + note_height + line + buttons + s(ds::size::MENU_ROW));
                shapes.push(Shape::Round { area, radius: s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
                use dogmind_agent::safety::Risk;
                let high = self.pending.as_ref().is_some_and(|pending| pending.risk == Risk::High);
                let title = self.pending.as_ref().map_or(String::new(), |pending| {
                    let tool = t(tool_name(&pending.tool));
                    if matches!(pending.risk, Risk::Medium | Risk::High) { tf("{0}  ·  {1} risk", &[&tool, &t(pending.risk.name())]) } else { tool.to_string() }
                });
                shapes.push(Shape::Text { area: rect(left + s(ds::space::L), top + s(ds::space::S), column - s(ds::space::XXL), s(26)), text: title, font: FontKind::Strong, color: if high { theme::bad() } else { theme::text() }, flags: ONE_LINE });
                if head_room > 0 {
                    shapes.push(Shape::Text { area: rect(left + s(ds::space::L), top + s(ds::size::MENU_ROW), head_width, head_room - s(ds::space::S)), text: self.approval_head.clone(), font: FontKind::Body, color: theme::soft(), flags: DT_WORDBREAK });
                }
                place(self.note, left + s(13), top + s(ds::size::MENU_ROW) + head_room, column - s(29), note_height - s(ds::space::XS));
                if line > 0 {
                    shapes.push(Shape::Text { area: rect(left + s(ds::space::L), top + s(ds::size::MENU_ROW) + head_room + note_height - s(ds::space::HAIR), column - 2 * s(ds::space::L), line), text: self.always_line.clone(), font: FontKind::Small, color: theme::faint(), flags: (ONE_LINE & !DT_END_ELLIPSIS) | DT_PATH_ELLIPSIS });
                }
                let row = top + s(ds::size::MENU_ROW) + head_room + note_height + line;
                let button = s(96);
                place(self.approve, left + s(ds::space::L), row, button, s(ds::size::CONTROL));
                place(self.always, left + s(ds::space::L) + button + s(ds::space::S), row, button, s(ds::size::CONTROL));
                place(self.reject, left + s(ds::space::L) + 2 * (button + s(ds::space::S)), row, button, s(ds::size::CONTROL));
                let after = left + s(ds::space::L) + 3 * (button + s(ds::space::S));
                shapes.push(Shape::Round { area: rect(after, row, left + column - s(ds::space::L) - after, s(ds::size::CONTROL)), radius: s(ds::radius::CONTROL), fill: Some(theme::raised()), border: None });
                place(self.feedback, after + s(ds::space::M), row + s(ds::size::FIELD_INSET), left + column - s(ds::space::L) - after - 2 * s(ds::space::M), s(ds::size::FIELD_TEXT));
                bottom = top - s(ds::space::S);
            }
            // The conversation's own column: the input's, less what the rail of turns needs beside it at each side, where the panel's edge does not
            // leave that room already (with the scrollbar's strip, and as it stands centred, at both sides).
            let rail_room = if self.rail_turns.len() >= 2 {
                GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi) + s(RAIL_WIDTH) + s(ds::space::S)
            } else {
                0
            };
            let inset = (rail_room - (main_width - column) / 2).max(0);
            let (text_left, text_column) = (left + inset, column - 2 * inset);
            // The width text goes in at, whether or not there is any yet: what is written first would have to be written again, as soon as it had gone in.
            let twips = (i64::from(text_column) * 1440 / i64::from(self.dpi)) as i32;
            if twips != self.text_twips {
                self.text_twips = twips;
                // Once the window has stopped moving: it writes the whole conversation again.
                if self.has_messages {
                    SetTimer(self.window, TIMER_REFLOW, 150, None);
                }
            }
            let mut top = strip;
            // The session's goal, as a thin bar under its name, with a cross that clears it.
            show(self.goal_clear, cards::goal_bar(self.goal.as_deref()).is_some());
            if let Some(goal) = cards::goal_bar(self.goal.as_deref()) {
                let bar = s(ds::size::TEXT_ROW);
                top += s(ds::space::XS);
                shapes.push(Shape::Round { area: rect(left, top, column, bar), radius: s(ds::radius::CONTROL), fill: Some(theme::surface()), border: None });
                let label = t("Goal");
                let label_width = text_width(self.fonts.small, label);
                shapes.push(Shape::Text { area: rect(left + s(ds::space::M), top, label_width, bar), text: label.to_string(), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
                let cross = s(ds::size::ICON_BUTTON) - s(ds::space::XS);
                let words_left = left + s(ds::space::M) + label_width + s(ds::space::S);
                shapes.push(Shape::Text { area: rect(words_left, top, (left + column - cross - words_left).max(0), bar), text: goal, font: FontKind::Small, color: theme::soft(), flags: ONE_LINE });
                place(self.goal_clear, left + column - cross - s(ds::space::XS), top + (bar - cross) / 2, cross, cross);
                top += bar + s(ds::space::XS);
            }
            if let Some(notice) = &notice {
                top += s(ds::space::XS);
                top += self.place_notice(&mut shapes, notice, left, column, top);
            }
            if finding {
                top += s(ds::space::XS);
                let bar = s(ds::size::MENU_ROW);
                shapes.push(Shape::Round { area: rect(left, top, column, bar), radius: s(ds::radius::CONTROL), fill: Some(theme::surface()), border: Some(theme::line()) });
                shapes.push(Shape::Icon { name: "search", area: rect(left + s(10), top, s(ds::space::L), bar), size: s(ds::size::ICON_SMALL), color: theme::faint() });
                let button = s(26);
                let right = left + column - s(6);
                place(self.find_close, right - button, top + s(5), button, button);
                place(self.find_next, right - 2 * button - s(ds::space::HAIR), top + s(5), button, button);
                place(self.find_previous, right - 3 * button - s(ds::space::XS), top + s(5), button, button);
                let count = if self.find_matches.is_empty() {
                    if get_text(self.find_field).trim().is_empty() { String::new() } else { t("No matches").to_string() }
                } else {
                    format!("{} / {}", self.find_at + 1, self.find_matches.len())
                };
                let count_right = right - 3 * button - s(10);
                shapes.push(Shape::Text { area: rect(count_right - s(96), top, s(96), bar), text: count, font: FontKind::Small, color: theme::muted(), flags: DT_RIGHT | ONE_LINE });
                place(self.find_field, left + s(34), top + s(ds::space::S), count_right - s(96) - left - s(40), s(20));
                top += bar + s(6);
            }
            if self.has_messages {
                // As wide as the panel, with the text kept to the column: the scrollbar belongs at the panel's edge.
                self.transcript_area = rect(main_left, top, main_width, bottom - top);
                place(self.transcript, main_left, top, main_width, bottom - top);
                let bar = GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi);
                // (the control keeps back this much of its text's width for the bar it has: measured, three fifths of the bar's own)
                let area = RECT { left: text_left - main_left, top: 0, right: text_left - main_left + text_column + bar * 3 / 5, bottom: bottom - top };
                if self.transcript_rect != (area.left, area.right, area.bottom) {
                    self.transcript_rect = (area.left, area.right, area.bottom);
                    SendMessageW(self.transcript, EM_SETRECT, 0, std::ptr::addr_of!(area) as LPARAM);
                }
                // (where its text ends: the control's room for its own bar is not text)
                self.column_right = text_left + text_column;
                // (the rail's place goes into the control's region, and that is set here too)
                self.place_rail();
                self.set_transcript_region();
            }
        }

        // A dialog: all that is behind it is hidden, and a picture of it is what shows.
        if self.modal_open {
            self.layout_modal(width, height);
            self.shapes = shapes;
            InvalidateRect(self.window, std::ptr::null(), 0);
            return;
        }
        show(self.modal, false);

        // The Changes page has the window to itself: the files where the sidebar is, and the diff beside them.
        for window in [self.diff_view, self.diff_previous, self.diff_next, self.diff_close] {
            show(window, looking_at_changes);
        }
        if looking_at_changes {
            for window in self.shared_controls() {
                show(window, false);
            }
            let table = self.trajectory.is_some();
            show(self.list, !table && !self.list_rows.is_empty());
            // (nothing to go between)
            for window in [self.diff_previous, self.diff_next] {
                show(window, !table && !self.diff_starts.is_empty());
            }
            let head = s(ds::size::DIFF_HEAD);
            shapes.push(Shape::Text { area: rect(inset + s(ds::space::M), 0, if table { text_width(self.fonts.strong, &self.panel_title) + s(ds::space::XS) } else { sidebar - 2 * inset - s(ds::space::M) }, head), text: self.panel_title.clone(), font: FontKind::Strong, color: theme::text(), flags: ONE_LINE });
            // (under the head, across the window; the files and the diff stand under it)
            let under = head + notice.as_ref().map_or(0, |notice| self.place_notice(&mut shapes, notice, inset, width - 2 * inset, head));
            place(self.list, inset, under, side_width, height - under - s(ds::space::S));
            let button = s(ds::size::ICON_BUTTON);
            let middle = (head - button) / 2;
            let mut x = width - s(ds::space::M) - button;
            for window in [self.diff_close, self.diff_next, self.diff_previous] {
                place(window, x, middle, button, button);
                x -= button + s(ds::space::XS);
            }
            let file = if table { self.session_title.clone() } else { self.diff_file.and_then(|at| self.changes.get(at)).map_or(String::new(), |change| change.path.clone()) };
            let from = if table { inset + s(ds::space::M) + text_width(self.fonts.strong, &self.panel_title) + s(ds::space::XL) } else { sidebar + s(ds::space::L) };
            shapes.push(Shape::Text { area: rect(from, 0, (x + button - from - s(ds::space::XL)).max(0), head), text: file, font: FontKind::Body, color: theme::soft(), flags: (ONE_LINE & !DT_END_ELLIPSIS) | DT_PATH_ELLIPSIS });
            let across = if table { 0 } else { sidebar };
            place(self.diff_view, across, under, width - across, height - under);
            self.clamp_diff();
            InvalidateRect(self.diff_view, std::ptr::null(), 0);
            self.shapes = shapes;
            InvalidateRect(self.window, std::ptr::null(), 0);
            return;
        }

        // The other panels.
        let panel = !chat;
        let has_list = match self.panel {
            Panel::Start => !self.start_folders.is_empty(),
            Panel::Setup => true,
            // (an empty card says nothing: what the panel says below it is the news)
            Panel::Models => !self.list_rows.is_empty(),
            _ => false,
        };
        show(self.list, has_list);
        show(self.kind_chip, false);
        show(self.provider_up, false);
        show(self.provider_down, false);
        // (a PC that cannot run a model is not asked where to keep one)
        let refused = self.panel == Panel::Models && self.refusal.is_some();
        show(self.drafter_chip, self.panel == Panel::Models && !refused);
        show(self.speed_chip, self.panel == Panel::Models && !refused);
        show(self.vision_chip, self.panel == Panel::Models && !refused && self.vision_row().is_some());
        show(self.vision_delete, false);
        show(self.licence_link, self.panel == Panel::Models && !refused && !get_text(self.licence_link).is_empty());
        show(self.toggle_display, matches!(self.panel, Panel::Setup | Panel::Welcome));
        let field_count: usize = match self.panel {
            Panel::Models if refused => 0,
            Panel::Models => 3,
            Panel::SetupDetails => match self.setup_kind {
                Kind::Server => 3,
                Kind::Anthropic => 2,
                _ => 0,
            },
            _ => 0,
        };
        for (at, field) in self.fields.iter().enumerate() {
            show(*field, at < field_count);
        }
        let button_count: usize = match self.panel {
            Panel::Models => 3,
            Panel::Start if self.start_folders.is_empty() => 2,
            Panel::Start => 3,
            Panel::Setup => 2,
            Panel::SetupDetails => 2,
            Panel::Welcome => 1,
            _ => 0,
        };
        for (at, button) in self.buttons.iter().enumerate() {
            show(*button, at < button_count);
        }
        self.panel_extent = 0;
        if self.panel == Panel::Welcome {
            // The dog, what it says, and one thing to do.
            let dog = s(112);
            let (bubble_height, dog_gap, mark_width) = (s(68), s(3), s(160));
            let dog_bottom = dog * 91 / 100;
            let across = (text_width(self.fonts.strong, &get_text(self.buttons[0])) + s(ds::size::BUTTON_PADDING)).max(s(140));
            let total = bubble_height + dog_gap + dog_bottom + title_height(mark_width) + s(ds::space::XXL) + s(ds::size::CONTROL);
            let top = ((height - total) / 2).max(s(ds::space::L));
            let bubble_width = (text_width(self.fonts.greeting, WELCOME) + s(60)).min(column);
            let area = rect(left + (column - bubble_width) / 2, top, bubble_width, bubble_height);
            let (dog_x, dog_top) = (left + (column - dog) / 2, top + bubble_height + dog_gap);
            let tip = (dog_x + dog * 11 / 100 - s(ds::space::XL), top + bubble_height + s(13));
            shapes.push(Shape::Bubble { area, radius: s(26), tip, fill: theme::background(), line: theme::text(), thickness: dog as f32 * 0.04 });
            shapes.push(Shape::Text { area, text: WELCOME.into(), font: FontKind::Greeting, color: theme::text(), flags: DT_CENTER | ONE_LINE });
            shapes.push(Shape::Logo { x: dog_x, y: dog_top, size: dog });
            shapes.push(Shape::TitleMark { x: left + (column - mark_width) / 2, y: dog_top + dog_bottom, width: mark_width });
            place(self.buttons[0], left + (column - across) / 2, dog_top + dog_bottom + title_height(mark_width) + s(ds::space::XXL), across, s(ds::size::CONTROL));
            let shown = get_text(self.toggle_display);
            let chip = text_width(self.fonts.small, shown.split_once('|').map_or(shown.as_str(), |(_, words)| words)) + s(ds::size::BUTTON_PADDING) + s(21);
            place(self.toggle_display, width - chip - s(ds::space::M), s(ds::space::M), chip, s(ds::size::ICON_BUTTON));
        }
        if panel && self.panel != Panel::Welcome {
            let mut top = if bare { ((height - s(460)) / 2).max(s(ds::space::L)) } else { s(ds::size::ICON_BUTTON) } - self.panel_scroll;
            if bare {
                // The first screens: the mark, and the one question.
                let dog = s(72);
                shapes.push(Shape::Logo { x: left + (column - dog) / 2, y: top, size: dog });
                top += dog + s(ds::space::XS);
                let mark = s(112);
                shapes.push(Shape::TitleMark { x: left + (column - mark) / 2, y: top, width: mark });
                top += title_height(mark) + s(ds::space::L);
            }
            if bare {
                shapes.push(Shape::Text { area: rect(left, top, column, s(ds::size::MENU_ROW)), text: self.panel_title.clone(), font: FontKind::Heading, color: theme::text(), flags: DT_CENTER | ONE_LINE });
                top += s(ds::size::HEAD);
            } else {
                shapes.push(Shape::Text { area: rect(left, top, column, s(ds::size::HEAD)), text: self.panel_title.clone(), font: FontKind::Title, color: theme::text(), flags: ONE_LINE });
                top += s(52);
            }
            if self.panel == Panel::Setup {
                let shown = get_text(self.toggle_display);
                let chip = text_width(self.fonts.small, shown.split_once('|').map_or(shown.as_str(), |(_, words)| words)) + s(ds::size::BUTTON_PADDING) + s(21);
                place(self.toggle_display, width - chip - s(ds::space::M), s(ds::space::M) - self.panel_scroll, chip, s(ds::size::ICON_BUTTON));
            }
            let row = s(ds::size::CONTROL);
            // A label as wide as the longest of them, and over its field where that would leave a field too narrow.
            let mut names: Vec<&str> = self.labels[..field_count].iter().map(String::as_str).collect();
            if self.panel == Panel::Models {
                names.push(t("Faster-writing data (5 GB)"));
            }
            if self.panel == Panel::Models {
                let about = self.about_this_pc();
                if !about.is_empty() {
                    let tall = text_height(self.fonts.small, &about, column);
                    shapes.push(Shape::Text { area: rect(left, top, column, tall + s(ds::space::XS)), text: about, font: FontKind::Small, color: theme::muted(), flags: DT_WORDBREAK | if bare { DT_CENTER } else { 0 } });
                    top += tall + s(ds::space::S);
                }
            }
            let needed = names.iter().map(|name| text_width(self.fonts.body, name)).max().unwrap_or(0) + s(20);
            let stacked = needed > column * 45 / 100;
            let label_width = if stacked { column } else { needed.max(s(120)) };
            let label_row = if stacked { s(ds::size::TEXT_ROW) } else { 0 };
            let (field_left, field_width) = if stacked { (left, column) } else { (left + label_width, column - label_width) };
            let widths: Vec<i32> = (0..button_count).map(|at| (text_width(self.fonts.strong, &get_text(self.buttons[at])) + s(ds::size::BUTTON_PADDING)).max(s(96))).collect();
            let places = wrap_row(&widths, s(ds::space::S), column);
            let button_rows = places.last().map_or(1, |(row, _)| row + 1);
            let below = field_count as i32 * (row + s(ds::space::S) + label_row) + if self.panel == Panel::Models { 2 * (row + s(30) + label_row) + s(ds::size::LINK) + s(ds::size::TEXT_LINE) } else { 0 } + button_rows * s(ds::size::HEAD) + s(70);
            if has_list {
                // (what the window does not have, scrolling does)
                let list_height = if self.panel == Panel::Setup { self.list_rows.len() as i32 * s(47) + s(ds::space::M) } else { (height - top - self.panel_scroll - below - s(ds::space::L)).max(s(100)) };
                shapes.push(Shape::Round { area: rect(left, top, column, list_height), radius: s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
                place(self.list, left + s(6), top + s(6), column - s(ds::space::M), list_height - s(ds::space::M));
                top += list_height + s(14);
                if self.panel == Panel::Models && !refused && !get_text(self.licence_link).is_empty() {
                    place(self.licence_link, left, top - s(ds::space::S), column, s(ds::size::LINK));
                    top += s(ds::size::LINK);
                }
            }
            for at in 0..field_count {
                shapes.push(Shape::Text { area: rect(left, top, label_width, if stacked { label_row } else { row }), text: self.labels[at].clone(), font: FontKind::Body, color: theme::muted(), flags: ONE_LINE });
                top += label_row;
                let focused = GetFocus() == self.fields[at];
                shapes.push(Shape::Round { area: rect(field_left, top, field_width, row), radius: s(ds::radius::CONTROL), fill: Some(theme::surface()), border: Some(if focused { theme::accent() } else { theme::line() }) });
                place(self.fields[at], field_left + s(ds::space::M), top + s(ds::size::FIELD_INSET), field_width - 2 * s(ds::space::M), s(ds::size::FIELD_TEXT));
                top += row + s(ds::space::M);
            }
            if self.panel == Panel::Models && !refused {
                shapes.push(Shape::Text { area: rect(left, top, label_width, if stacked { label_row } else { row }), text: t("Faster-writing data (5 GB)").into(), font: FontKind::Body, color: theme::muted(), flags: ONE_LINE });
                top += label_row;
                place(self.drafter_chip, field_left, top + s(ds::space::XS), s(380).min(field_width), s(30));
                top += row + s(ds::space::XS);
                shapes.push(Shape::Text { area: rect(left, top, label_width, if stacked { label_row } else { row }), text: t("Speed first (for a PC short of memory)").into(), font: FontKind::Body, color: theme::muted(), flags: ONE_LINE });
                top += label_row;
                place(self.speed_chip, field_left, top + s(ds::space::XS), s(380).min(field_width), s(30));
                top += row + s(ds::space::XS);
                if let Some(words) = self.vision_row() {
                    shapes.push(Shape::Text { area: rect(left, top, label_width, if stacked { label_row } else { row }), text: words, font: FontKind::Body, color: theme::muted(), flags: ONE_LINE });
                    top += label_row;
                    place(self.vision_chip, field_left, top + s(ds::space::XS), s(380).min(field_width), s(30));
                    top += row + s(ds::space::XS);
                }
                if let Some(total) = self.download_total() {
                    shapes.push(Shape::Text { area: rect(left, top, column, s(ds::size::TEXT_LINE)), text: total, font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
                    top += s(ds::size::TEXT_LINE);
                }
                if let Some(done) = self.progress {
                    shapes.push(Shape::Round { area: rect(left, top + s(6), column, s(ds::size::THUMB)), radius: s(ds::size::THUMB) / 2, fill: Some(theme::raised()), border: None });
                    shapes.push(Shape::Round { area: rect(left, top + s(6), ((column as f64) * done.clamp(0.02, 1.0)) as i32, s(ds::size::THUMB)), radius: s(ds::size::THUMB) / 2, fill: Some(theme::accent()), border: None });
                }
                top += s(ds::size::TEXT_LINE);
            }
            // (on the first screens all stands on the middle)
            let shift = if bare && button_rows == 1 { (column - widths.iter().sum::<i32>() - (button_count as i32 - 1).max(0) * s(ds::space::S)) / 2 } else { 0 };
            for (at, (line, x)) in places.iter().enumerate() {
                place(self.buttons[at], left + shift + x, top + line * (s(ds::size::CONTROL) + s(ds::space::S)), widths[at], s(ds::size::CONTROL));
            }
            if refused {
                show(self.buttons[0], false);
                show(self.buttons[1], false);
                place(self.buttons[2], left + (column - widths[2]) / 2, top, widths[2], s(ds::size::CONTROL));
            }
            top += button_rows * (s(ds::size::CONTROL) + s(ds::space::S)) + s(ds::space::XS);
            let log_height = if self.panel_log.is_empty() { 0 } else { text_height(self.fonts.small, &self.panel_log, column) };
            shapes.push(Shape::Text { area: rect(left, top, column, log_height + s(ds::space::XS)), text: self.panel_log.clone(), font: FontKind::Small, color: if self.panel_problem { theme::bad() } else { theme::muted() }, flags: DT_WORDBREAK | if bare { DT_CENTER } else { 0 } });
            // A panel taller than the window scrolls: what would have been below its edge could not be reached at all.
            self.panel_extent = top + log_height + s(ds::space::L) + self.panel_scroll;
            let most = (self.panel_extent - height).max(0);
            if self.panel_scroll > most {
                self.panel_scroll = most;
                self.layout();
                return;
            }
            if most > 0 {
                let tall = thumb_height(height, height, self.panel_extent, s(ds::space::XXL));
                let at = thumb_offset(height - tall, self.panel_scroll, most);
                shapes.push(Shape::Round { area: rect(width - s(9), at, s(ds::size::THUMB), tall), radius: s(ds::size::THUMB) / 2, fill: Some(theme::line()), border: None });
            }
        }
        show(self.doc, false);
        // A row kept lit is one that is still there.
        if let Some(row) = self.hover_row {
            if !self.preview_hits.iter().map(|(area, _)| area).chain(self.right_hits.iter().map(|(area, _)| area)).chain(self.right_tab_hits.iter().map(|(area, _)| area)).chain(self.queue_hits.iter().map(|(area, _, _)| area)).chain(self.waiting_hits.iter().map(|(area, _)| area)).any(|area| same_rect(area, &row)) {
                self.hover_row = None;
            }
        }
        self.shapes = shapes;
        InvalidateRect(self.window, std::ptr::null(), 0);
    }

    /// Whether this panel is shown as a dialog over the app. The first run has the window to itself.
    fn is_dialog(&self, panel: Panel) -> bool {
        matches!(panel, Panel::Rename | Panel::ConfirmDelete | Panel::ConfirmRemove | Panel::Sessions | Panel::Rewind | Panel::CryptoOn | Panel::CryptoPassphrase | Panel::CryptoCode | Panel::CryptoOff | Panel::CryptoWork | Panel::Unlock) || (!self.onboarding && (matches!(panel, Panel::Help | Panel::About) || is_settings_place(panel)))
    }

    /// The controls that move between the window and a dialog as they are needed, and those that only a dialog has.
    fn shared_controls(&self) -> Vec<HWND> {
        let mut all = vec![self.list, self.doc, self.kind_chip, self.drafter_chip, self.speed_chip, self.vision_chip, self.licence_link, self.toggle_display, self.provider_up, self.provider_down];
        all.extend(self.fields);
        all.extend(self.buttons);
        all
    }

    fn pane_controls(&self) -> Vec<HWND> {
        let mut all = self.shared_controls();
        all.extend(self.theme_segments);
        all.extend(self.size_segments);
        all.extend(self.mcp_trust.iter().copied());
        all.extend(self.skill_toggles.iter().copied());
        all.extend(self.memory_remove.iter().copied());
        all.push(self.vision_delete);
        all.extend(self.read_folder_remove.iter().copied());
        all.extend([self.read_folder_add, self.read_folder_stop]);
        all.extend([self.toggle_thinking, self.toggle_safe, self.toggle_web, self.toggle_compact, self.toggle_updates, self.toggle_remember, self.toggle_language, self.mode_select, self.key_replace, self.key_remove, self.advanced_button, self.variable_toggle, self.mcp_import, self.setup_again, self.remove_all]);
        all
    }

    /// A picture of the app as it is now, darkened, for the dialog to stand on. It stays as it is while the dialog is open: what comes in the meantime is there when it closes.
    unsafe fn open_modal(&mut self) {
        self.modal_return = GetFocus();
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let screen = GetDC(self.window);
        let device = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, client.right, client.bottom);
        let previous = SelectObject(device, bitmap);
        // What is still to be drawn, first: what has just closed is not to show behind this. The composed picture (PW_RENDERFULLCONTENT) is for a preview's browser alone, which
        // draws nowhere else: it goes through the GPU, and with the engine on it has to wait its turn.
        RedrawWindow(self.window, std::ptr::null(), std::ptr::null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
        PrintWindow(self.window, device, 1 | if self.preview_file.is_some() { 2 } else { 0 }); // PW_CLIENTONLY, PW_RENDERFULLCONTENT
        dim(device, client);
        SelectObject(device, previous);
        DeleteDC(device);
        ReleaseDC(self.window, screen);
        self.modal_backdrop = bitmap;
        self.modal_backdrop_size = (client.right, client.bottom);
        self.modal_open = true;
        for control in self.shared_controls() {
            show(control, false);
            SetParent(control, self.pane);
        }
    }

    unsafe fn close_modal(&mut self) {
        self.modal_open = false;
        if !self.modal_backdrop.is_null() {
            DeleteObject(self.modal_backdrop);
            self.modal_backdrop = std::ptr::null_mut();
        }
        for control in self.shared_controls() {
            show(control, false);
            SetParent(control, self.window);
        }
        show(self.modal, false);
        self.modal_shapes.clear();
        self.pane_shapes.clear();
    }

    /// What a dialog's window draws.
    unsafe fn paint_modal(&self, window: HWND, device: HDC, client: RECT) {
        if window == self.pane {
            let brush = CreateSolidBrush(theme::background());
            FillRect(device, &client, brush);
            DeleteObject(brush);
            for shape in &self.pane_shapes {
                self.draw_shape(device, shape);
            }
            return;
        }
        if self.modal_backdrop.is_null() {
            // (the theme has changed under it: a picture in the old colours would be wrong)
            let brush = CreateSolidBrush(theme::background());
            FillRect(device, &client, brush);
            DeleteObject(brush);
            dim(device, client);
        } else {
            let source = CreateCompatibleDC(device);
            let previous = SelectObject(source, self.modal_backdrop);
            if self.modal_backdrop_size == (client.right, client.bottom) {
                BitBlt(device, 0, 0, client.right, client.bottom, source, 0, 0, SRCCOPY);
            } else {
                SetStretchBltMode(device, HALFTONE);
                StretchBlt(device, 0, 0, client.right, client.bottom, source, 0, 0, self.modal_backdrop_size.0, self.modal_backdrop_size.1, SRCCOPY);
            }
            SelectObject(source, previous);
            DeleteDC(source);
        }
        for shape in &self.modal_shapes {
            self.draw_shape(device, shape);
        }
    }

    /// The rows of a dialog's list, and which of them is chosen.
    fn nav_rows(&self) -> (Vec<(&'static str, String)>, usize) {
        if self.panel == Panel::Help {
            let rows = help_sections().into_iter().map(|(name, _)| ("", name)).collect();
            return (rows, self.help_section);
        }
        if !is_settings_place(self.panel) {
            return (Vec::new(), 0);
        }
        let rows = SETTINGS_PLACES.iter().map(|(_, icon, name)| (*icon, t(name).to_string())).collect();
        (rows, SETTINGS_PLACES.iter().position(|(panel, _, _)| *panel == self.panel).unwrap_or(0))
    }

    unsafe fn layout_modal(&mut self, width: i32, height: i32) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        place(self.modal, 0, 0, width, height);
        let margin = s(ds::DIALOG_MARGIN);
        let (rows, chosen) = self.nav_rows();
        let (card_width, wanted_height) = match self.panel {
            Panel::Rename | Panel::ConfirmDelete | Panel::ConfirmRemove | Panel::CryptoPassphrase | Panel::CryptoCode | Panel::CryptoOff => (s(ds::size::SHORT_DIALOG).min(width - 2 * margin), 0),
            Panel::Sessions | Panel::Rewind => (s(ds::size::LIST_DIALOG).min(width - 2 * margin), 0),
            Panel::Help => ds::dialog_size((width, height), 80, (s(640), s(420)), (s(800), s(640)), margin),
            _ => ds::dialog_size((width, height), 80, (s(720), s(460)), (s(900), s(640)), margin),
        };
        let nav = if rows.is_empty() { 0 } else if card_width < s(800) { s(ds::size::DIALOG_NAV_SMALL) } else { s(ds::size::DIALOG_NAV) };
        let gutter = s(ds::space::XL);
        let head = s(56);
        let foot = s(ds::space::L);
        let pane_x = nav + gutter;
        let content = card_width - pane_x - gutter;
        if self.cues_for != Some((self.panel, self.kind)) {
            self.cues_for = Some((self.panel, self.kind));
            self.settings_cues();
        }
        let view = (wanted_height - head - foot).max(0);
        let mut form = Form::new(content, -self.panel_scroll, self.fonts, dpi);
        self.build_page(&mut form, view);
        let extent = form.y + self.panel_scroll + s(ds::space::XS);
        let card_height = if wanted_height > 0 { wanted_height } else { (extent + head + foot).min(height - 2 * margin) };
        let view = (card_height - head - foot).max(0);
        // Scrolled past what there now is.
        let most = (extent - view).max(0);
        self.panel_extent = extent;
        if self.panel_scroll > most {
            self.panel_scroll = most;
            self.layout_modal(width, height);
            return;
        }
        let card = rect((width - card_width) / 2, (height - card_height) / 2, card_width, card_height);
        self.modal_card = card;
        let corner = s(ds::radius::DIALOG);
        let mut shapes = vec![Shape::Round { area: card, radius: corner, fill: Some(theme::background()), border: None }];
        if nav > 0 {
            shapes.push(Shape::Round { area: rect(card.left, card.top, nav + 2 * corner, card_height), radius: corner, fill: Some(theme::sidebar()), border: None });
            shapes.push(Shape::Fill { area: rect(card.left + nav, card.top, 2 * corner, card_height), color: theme::background() });
            shapes.push(Shape::Fill { area: rect(card.left + nav, card.top, 1, card_height), color: theme::line() });
            // (the mark says what About is)
            let heading = if self.panel == Panel::About { String::new() } else { rows.get(chosen).map_or(String::new(), |(_, name)| name.clone()) };
            shapes.push(Shape::Text { area: rect(card.left + pane_x, card.top + s(ds::space::L), content - s(40), s(ds::size::ICON_BUTTON)), text: heading, font: FontKind::Heading, color: theme::text(), flags: ONE_LINE });
        }
        if nav == 0 {
            shapes.push(Shape::Text { area: rect(card.left + pane_x, card.top + s(ds::space::L), content, s(ds::size::ICON_BUTTON)), text: self.panel_title.clone(), font: FontKind::Heading, color: theme::text(), flags: ONE_LINE });
        }
        shapes.push(Shape::Round { area: card, radius: corner, fill: None, border: Some(theme::line()) });
        self.modal_shapes = shapes;
        let close = s(ds::size::ICON_BUTTON);
        place(self.modal_close, card.right - s(ds::space::M) - close, card.top + s(ds::space::M), close, close);
        // (what has its own way out needs no cross)
        show(self.modal_close, nav > 0);
        let row_height = s(ds::size::NAV_ROW);
        for (at, button) in self.nav_buttons.iter().enumerate() {
            match rows.get(at) {
                Some((icon, name)) => {
                    let inset = s(ds::space::S);
                    place(*button, card.left + inset, card.top + s(ds::space::M) + at as i32 * (row_height + s(ds::space::HAIR)), nav - 2 * inset, row_height);
                    set_text_if_changed(*button, &if icon.is_empty() { name.clone() } else { format!("{icon}|{name}") });
                    set_button_style(*button, if at == chosen { ButtonKind::NavOn } else { ButtonKind::Nav }, theme::sidebar());
                    show(*button, true);
                }
                None => show(*button, false),
            }
        }
        // (it reaches to the dialog's edge, for the thumb to stand there)
        place(self.pane, card.left + pane_x, card.top + head, card_width - pane_x - s(ds::space::HAIR), view);
        for control in self.pane_controls() {
            show(control, form.used.contains(&control));
        }
        self.pane_thumb = None;
        if most > 0 {
            let tall = thumb_height(view, view, extent, s(ds::space::XXL));
            let at = thumb_offset(view - tall, self.panel_scroll, most);
            let thumb = rect(card_width - pane_x - s(11), at, s(ds::size::THUMB), tall);
            let held = self.pane_thumb_grab.is_some();
            form.shapes.push(Shape::Round { area: thumb, radius: s(ds::size::THUMB) / 2, fill: Some(if held { theme::muted() } else { theme::line() }), border: None });
            self.pane_thumb = Some((thumb, view, most));
        }
        self.pane_shapes = form.shapes;
        if IsWindowVisible(self.modal) == 0 {
            SetWindowPos(self.modal, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            show(self.modal, true);
        }
        InvalidateRect(self.modal, std::ptr::null(), 0);
        InvalidateRect(self.pane, std::ptr::null(), 0);
    }

    /// What the fields of a settings page say while they are empty. Words under them say no more than this would.
    unsafe fn settings_cues(&self) {
        for field in self.fields {
            field_cue(field, "");
        }
        match self.panel {
            Panel::Providers => {
                field_cue(self.fields[KEY_FIELD], t("Kept in Windows Credential Manager"));
                match self.kind {
                    Kind::Server => {
                        field_cue(self.fields[1], "http://192.168.0.100:8080/v1");
                        field_cue(self.fields[2], t("The first it offers"));
                    }
                    Kind::Anthropic => {
                        field_cue(self.fields[1], t("Only for a gateway"));
                        field_cue(self.fields[2], anthropic::DEFAULT_MODEL);
                    }
                    _ => {}
                }
            }
            Panel::Models => {
                field_cue(self.fields[1], t("Automatic"));
                field_cue(self.fields[2], t("Automatic"));
            }
            Panel::Tools => field_cue(self.fields[KEY_FIELD], t("Optional. Kept in Credential Manager")),
            _ => {}
        }
    }

    /// The rows for a key: pasted, in a variable, or (from before) in a file. Where it is, is all that ever shows.
    unsafe fn key_rows(&self, form: &mut Form, setting: &str, label: &str) {
        let source = KeySource::of(setting);
        if self.variable_on {
            let name = get_text(self.fields[VARIABLE_FIELD]);
            let said = if name.trim().is_empty() { t("Give the variable's name below.").to_string() } else { tf("Read from the variable {0}", &[&name.trim()]) };
            form.value(label, &said);
        } else if let (KeySource::Stored(_), false) = (&source, self.key_editing) {
            let height = form.s(ds::size::CONTROL);
            form.label(label, height);
            let x = form.value_x();
            let dots = form.s(112);
            form.shapes.push(Shape::Round { area: rect(x, form.y, dots, height), radius: form.s(ds::radius::CONTROL), fill: Some(theme::surface()), border: Some(theme::line()) });
            form.shapes.push(Shape::Text { area: rect(x + form.s(ds::space::M), form.y, dots - form.s(ds::space::L), height), text: "••••••••".into(), font: FontKind::Body, color: theme::muted(), flags: ONE_LINE });
            let saved_y = form.y;
            form.buttons(x + dots + form.s(ds::space::S), &[self.key_replace, self.key_remove]);
            form.y = form.y.max(saved_y + height + std::mem::take(&mut form.extra) + form.s(ds::space::M));
        } else if let KeySource::File(path) = &source {
            form.value(label, &tf("Read from this file: {0}", &[path]));
            form.buttons(form.value_x(), &[self.key_remove]);
        } else {
            form.field(label, self.fields[KEY_FIELD]);
        }
        // (only for a key already kept that way: nobody needs to be told of it to paste one)
        if self.advanced_open {
            form.toggle(t("Read from an environment variable"), self.variable_toggle);
            if self.variable_on {
                form.field(t("Variable name"), self.fields[VARIABLE_FIELD]);
            }
        }
    }

    /// What the page chosen holds. `view`: how high the pane is, for what fills it.
    unsafe fn build_page(&self, form: &mut Form, view: i32) {
        let s = |value: i32| scale(self.dpi, value);
        let log = self.panel_log.clone();
        let log_color = if self.panel_problem { theme::bad() } else { theme::muted() };
        // (this button is every page's first: what its tip said on one page is not said on the next)
        self.untip(self.buttons[0]);
        match self.panel {
            Panel::General => {
                form.select(t("Display language"), self.toggle_display);
                form.select(t("Language of answers"), self.toggle_language);
                // (shortened to what fits, so the whole of it is on the row's tip)
                let folder = self.folder.display().to_string();
                self.tip(self.buttons[0], &folder);
                form.button_row(t("Data folder"), self.buttons[0], &folder);
                form.button_row(t("First setup"), self.setup_again, "");
                form.button_row(t("Remove this app from this PC"), self.remove_all, "");
            }
            Panel::Appearance => {
                let height = form.s(ds::size::CONTROL);
                form.label(t("Theme"), height);
                let (x, width) = (form.value_x(), form.value_width());
                form.shapes.push(Shape::Round { area: rect(x, form.y, width, height), radius: form.s(ds::radius::CONTROL), fill: Some(theme::surface()), border: Some(theme::line()) });
                let each = (width - form.s(ds::space::XS)) / 3;
                for (at, segment) in self.theme_segments.iter().enumerate() {
                    place(*segment, x + form.s(ds::space::HAIR) + at as i32 * each, form.y + form.s(ds::space::HAIR), each, height - form.s(ds::space::XS));
                    form.used.push(*segment);
                }
                form.end_row(height);
                form.label(t("Text size"), height);
                form.shapes.push(Shape::Round { area: rect(x, form.y, width, height), radius: form.s(ds::radius::CONTROL), fill: Some(theme::surface()), border: Some(theme::line()) });
                let each = (width - form.s(ds::space::XS)) / TEXT_SIZES.len() as i32;
                for (at, segment) in self.size_segments.iter().enumerate() {
                    place(*segment, x + form.s(ds::space::HAIR) + at as i32 * each, form.y + form.s(ds::space::HAIR), each, height - form.s(ds::space::XS));
                    form.used.push(*segment);
                }
                form.end_row(height);
                form.note(t("The conversation, what you type and the page beside it."), theme::muted());
                form.toggle(t("Show the model's thinking"), self.toggle_thinking);
                form.toggle(t("Group tool calls into one line"), self.toggle_compact);
            }
            Panel::Providers => {
                let rows = self.list_rows.len().clamp(1, 3) as i32;
                form.list(self.list, rows * s(ds::size::LIST_ROW) + s(ds::space::S));
                if self.list_rows.len() > 1 {
                    let arrows = s(ds::size::ICON_BUTTON);
                    let middle = form.y + (s(ds::size::CONTROL) - arrows) / 2;
                    place(self.provider_up, form.width - 2 * arrows - s(ds::space::HAIR), middle, arrows, arrows);
                    place(self.provider_down, form.width - arrows, middle, arrows, arrows);
                    form.used.extend([self.provider_up, self.provider_down]);
                }
                form.buttons(0, &[self.buttons[2]]);
                form.y += s(ds::space::S);
                form.select(t("How it runs"), self.kind_chip);
                form.field(t("Name"), self.fields[0]);
                let setting = self.selected_provider.and_then(|at| self.settings.providers.get(at)).map_or(String::new(), |provider| provider.api_key.clone());
                match self.kind {
                    Kind::Embedded | Kind::Codex => {}
                    Kind::Server => {
                        form.field(t("Address"), self.fields[1]);
                        form.field(t("Model"), self.fields[2]);
                        self.key_rows(form, &setting, t("API key"));
                    }
                    Kind::Anthropic => {
                        form.field(t("Model"), self.fields[2]);
                        self.key_rows(form, &setting, t("API key"));
                        form.field(t("Address"), self.fields[1]);
                    }
                }
                form.y += s(ds::space::XS);
                let in_use = self.selected_provider.and_then(|at| self.settings.providers.get(at)).is_some_and(|provider| provider.name == self.settings.active);
                if in_use {
                    form.buttons(0, &[self.buttons[1], self.buttons[3]]);
                } else {
                    form.buttons(0, &[self.buttons[0], self.buttons[1], self.buttons[3]]);
                }
                form.note(&log, log_color);
            }
            Panel::Models => {
                let about = self.about_this_pc();
                if !about.is_empty() {
                    form.note(&about, theme::muted());
                }
                if self.refusal.is_some() {
                    form.note(&log, log_color);
                    form.y += s(ds::space::S);
                    form.buttons(0, &[self.buttons[2], self.buttons[3], self.buttons[4]]);
                    return;
                }
                if !self.list_rows.is_empty() {
                    let rows = self.list_rows.len().clamp(1, 4) as i32;
                    form.list(self.list, rows * s(ds::size::LIST_ROW) + s(ds::space::S));
                }
                if !get_text(self.licence_link).is_empty() {
                    place(self.licence_link, 0, form.y - s(ds::space::XS), form.width, s(ds::size::LINK));
                    form.used.push(self.licence_link);
                    form.y += s(ds::size::LINK);
                }
                form.field_with_button(t("Where models are kept"), self.fields[0], self.buttons[1]);
                form.field(t("Graphics memory limit (GB)"), self.fields[1]);
                form.field(t("Longest conversation (tokens)"), self.fields[2]);
                form.toggle(t("Faster-writing data (5 GB)"), self.drafter_chip);
                form.toggle(t("Speed first (for a PC short of memory)"), self.speed_chip);
                if let Some(words) = self.vision_row() {
                    form.toggle(&words, self.vision_chip);
                }
                // Set aside, it can go, when asked.
                if let Some(bytes) = self.vision_set_aside() {
                    form.button_row(&tf("Its file: {0}", &[&engine::gb_file(bytes)]), self.vision_delete, "");
                }
                if let Some(total) = self.download_total() {
                    form.note(&total, theme::muted());
                }
                form.buttons(0, &[self.buttons[0]]);
                if let Some(done) = self.progress {
                    form.shapes.push(Shape::Round { area: rect(0, form.y, form.width, s(ds::size::THUMB)), radius: s(ds::size::THUMB) / 2, fill: Some(theme::raised()), border: None });
                    form.shapes.push(Shape::Round { area: rect(0, form.y, ((form.width as f64) * done.clamp(0.02, 1.0)) as i32, s(ds::size::THUMB)), radius: s(ds::size::THUMB) / 2, fill: Some(theme::accent()), border: None });
                    form.y += s(ds::space::L);
                }
                form.note(&log, log_color);
                form.y += s(ds::space::S);
                form.buttons(0, &[self.buttons[2], self.buttons[3], self.buttons[4]]);
            }
            Panel::Permissions => {
                form.select(t("When to ask"), self.mode_select);
                form.toggle(t("Don't ask for safe commands"), self.toggle_safe);
                form.section(t("What may always run here"));
                if !self.list_rows.is_empty() {
                    let rows = self.list_rows.len().clamp(1, 5) as i32;
                    form.list(self.list, rows * s(ds::size::LIST_ROW) + s(ds::space::S));
                    form.buttons(0, &[self.buttons[0]]);
                }
                form.note(&log, log_color);
                form.section(t("Folders commands can read"));
                // (it is the person's keys that are at stake: this much is said where it is done)
                form.note(t("Commands can read everything in these folders, and that includes any keys or passwords kept there. They cannot write there, and the network stays off unless you approve it. Files that look like keys are left out only if they are there when you add the folder, not ones made later."), theme::muted());
                for (at, folder) in self.read_folders.iter().enumerate() {
                    let Some(button) = self.read_folder_remove.get(at) else { break };
                    let whereabouts = self.read_folder_places.get(at);
                    let shown = if let Some(read_folders::Located::At(place)) = whereabouts { place.clone() } else { folder.path.clone() };
                    // (its own name first: the start of a path is what folders have in common)
                    form.button_row(&read_folder_name(&shown), *button, "");
                    form.note(&shown_path(&shown), theme::muted());
                    match whereabouts {
                        Some(read_folders::Located::Gone) => form.note(t("It is no longer on its drive. Remove it to take it off this list."), theme::muted()),
                        Some(read_folders::Located::DriveAway) => form.note(t("Its drive is not here."), theme::muted()),
                        _ => {}
                    }
                    if let Some(words) = read_folder_detail(folder) {
                        form.note(&words, theme::muted());
                    }
                }
                let working = self.read_folder_work.as_ref().map(ReadFolderWork::words);
                match &working {
                    Some(words) => {
                        form.note(words, theme::muted());
                        if self.read_folder_work.as_ref().is_some_and(|work| work.can_stop) {
                            form.buttons(0, &[self.read_folder_stop]);
                        }
                    }
                    None => {
                        form.buttons(0, &[self.read_folder_add]);
                        let (said, problem) = &self.read_folder_said;
                        if !said.is_empty() {
                            form.note(said, if *problem { theme::bad() } else { theme::muted() });
                        }
                    }
                }
            }
            Panel::Tools => {
                form.toggle(t("Web search and reading"), self.toggle_web);
                let setting = self.settings.search_key.clone();
                self.key_rows(form, &setting, t("Brave Search key"));
                form.section(t("MCP servers"));
                let lines = self.mcp.status();
                let servers = self.mcp.trust();
                for (at, (name, _)) in servers.iter().enumerate() {
                    let Some(toggle) = self.mcp_trust.get(at) else { break };
                    let line = lines.get(at).map_or(name.as_str(), String::as_str);
                    // (why one did not start needs the room of the page)
                    let failed = line.strip_prefix(&format!("{name}: did not start: "));
                    form.toggle_beside(name, *toggle, t("Trust what it returns"));
                    if let Some(why) = failed {
                        form.note(why, theme::muted());
                    }
                }
                if !servers.is_empty() {
                    // (it takes a safeguard away: this much is said where it is done)
                    form.note(t("What a trusted server returns no longer makes later commands ask. Its own tools still ask."), theme::muted());
                }
                form.buttons(0, &[self.mcp_import]);
                form.note(&log, log_color);
            }
            Panel::Memory => {
                if !self.memory_notes.is_empty() {
                    form.note(t("What the model keeps from one session to the next, and a new session reads. Remove what is wrong or should not be kept."), theme::muted());
                }
                for (scope, title) in [("project", t("This project")), ("user", t("You, in every project"))] {
                    let mine: Vec<(usize, &String)> = self.memory_notes.iter().enumerate().filter(|(_, (kept, _))| *kept == scope).map(|(at, (_, note))| (at, note)).collect();
                    if mine.is_empty() {
                        continue;
                    }
                    form.section(title);
                    for (at, note) in mine {
                        if let Some(button) = self.memory_remove.get(at) {
                            form.note_with_button(note, *button);
                        }
                    }
                }
                form.note(&log, log_color);
            }
            Panel::Skills => {
                if !self.skills.is_empty() {
                    form.note(t("A skill turned off is neither offered to the model nor started with /name. A session already under way keeps its list, but cannot read one that is off."), theme::muted());
                }
                for (skill, toggle) in self.skills.iter().zip(&self.skill_toggles) {
                    form.toggle_at_end(&skill.name, *toggle, if self.skill_enabled(&skill.name) { t("Enabled") } else { t("Disabled") });
                    // (why one is off before the person has said: what it needs is not here)
                    if let Some(missing) = self.skill_missing(skill) {
                        let words = match missing {
                            dogmind_agent::skills::Missing::Tools(server) => tf("Needs tools this app does not have: mcp__{0}__…", &[&server]),
                            dogmind_agent::skills::Missing::Something(what) => tf("Needs something of another app that this app does not have: {0}", &[&t(what)]),
                        };
                        form.note(&words, theme::warn());
                    }
                    // (about two lines: some are written for a model, a page long)
                    let about = skill.description.split_whitespace().collect::<Vec<_>>().join(" ");
                    let short: String = about.chars().take(160).collect();
                    form.note(&if short.len() < about.len() { format!("{}…", short.trim_end()) } else { short }, theme::muted());
                }
                form.note(&log, log_color);
            }
            Panel::Instructions => {
                let room = (view - (form.y + self.panel_scroll) - s(ds::size::CONTROL) - s(ds::space::XXL) - s(ds::space::M)).max(s(120));
                form.shapes.push(Shape::Round { area: rect(0, form.y, form.width, room), radius: s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
                place(self.doc, s(ds::space::M), form.y + s(ds::space::S), form.width - 2 * s(ds::space::M), room - 2 * s(ds::space::S));
                form.used.push(self.doc);
                form.y += room + s(ds::space::M);
                form.buttons(0, &[self.buttons[0]]);
                form.note(&log, log_color);
            }
            Panel::Help => {
                place(self.doc, 0, 0, form.width + s(ds::space::L), view.max(s(100)));
                form.used.push(self.doc);
                form.y = 0;
            }
            Panel::Rename => {
                form.frame(0, form.width, self.fields[0]);
                form.end_row(s(ds::size::CONTROL));
                form.y += s(ds::space::XS);
                form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                form.note(&log, log_color);
            }
            Panel::ConfirmDelete => {
                let title = self.delete_target.as_ref().map_or(String::new(), |(_, title)| title.clone());
                form.shapes.push(Shape::Text { area: rect(0, form.y, form.width, s(ds::space::XL)), text: title, font: FontKind::Body, color: theme::text(), flags: ONE_LINE });
                form.y += s(ds::space::XL);
                form.note(t("This cannot be undone. Files in the folder stay."), theme::muted());
                form.y += s(ds::space::XS);
                form.buttons(0, &[self.buttons[0], self.buttons[1]]);
            }
            Panel::ConfirmRemove => {
                match &self.leaving {
                    Leaving::No => {
                        form.note(t("This takes away what this app has left on this PC: its settings, sessions and logs, the keys it saved in Credential Manager, and what it opened to its sandbox. Your workspaces stay as they are."), theme::muted());
                        form.note(&self.model_in_the_data_folder().map_or_else(|| t("The downloaded model is not in the app's folder, and stays.").to_string(), |model| tf("The downloaded model is in {0}. It goes too, unless you keep it.", &[&shown_path(&model)])), theme::muted());
                        form.y += s(ds::space::XS);
                        form.buttons(0, &[self.buttons[0], self.buttons[1], self.buttons[2]]);
                    }
                    Leaving::Going => form.note(t("Removing..."), theme::muted()),
                    Leaving::Gone(words) => {
                        form.note(words, theme::text());
                        form.y += s(ds::space::XS);
                        form.buttons(0, &[self.buttons[0]]);
                    }
                }
            }
            Panel::Privacy => {
                form.note(t("If you forget the passphrase and lose the recovery code, the conversations cannot be read by anyone."), theme::muted());
                if self.crypt.keystore.is_none() {
                    form.note(t("Conversations, and the notes the model keeps, are stored as plain text in this app's folder. They can be encrypted, under a passphrase that only you know."), theme::muted());
                    form.buttons(0, &[self.buttons[0]]);
                } else {
                    form.note(t("Conversations are encrypted: each message, each picture and each note the model keeps. The key is kept only under your passphrase, under a recovery code and, if you chose it, under your Windows account."), theme::muted());
                    form.toggle(t("Remember on this PC"), self.toggle_remember);
                    form.note(t("With this on, Dogmind asks for nothing when it starts on this PC. The passphrase and the recovery code still open the conversations on another PC, or after Windows is installed again."), theme::muted());
                    form.buttons(0, &[self.buttons[0], self.buttons[1], self.buttons[2]]);
                }
                form.note(t("Not encrypted: the files of your workspaces and the copies kept to undo an edit (they lie in the workspace as they are), the timing records of a session (counts, times and names, never what was said), and the settings."), theme::muted());
                form.note(&log, log_color);
            }
            Panel::CryptoOn => {
                form.note(t("Choose a passphrase for your conversations. Then you are shown a recovery code, once. If you forget the passphrase, the recovery code is the only way back: with neither, the conversations cannot be read by anyone."), theme::muted());
                form.field(t("Passphrase"), self.fields[0]);
                form.field(t("Again"), self.fields[1]);
                form.toggle(t("Remember on this PC"), self.toggle_remember);
                form.note(t("Every conversation and note you have is encrypted now, one at a time, and new ones as they begin. With the box ticked, Dogmind asks for nothing when it starts on this PC."), theme::muted());
                form.y += s(ds::space::XS);
                form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                form.note(&log, log_color);
            }
            Panel::CryptoPassphrase => {
                form.field(t("New passphrase"), self.fields[0]);
                form.field(t("Again"), self.fields[1]);
                form.note(t("The conversations are not touched, and the recovery code still works."), theme::muted());
                form.y += s(ds::space::XS);
                form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                form.note(&log, log_color);
            }
            Panel::CryptoCode => {
                form.note(t("A new recovery code replaces the old one, which stops working. The passphrase stays as it is. The new code is shown once."), theme::muted());
                form.y += s(ds::space::XS);
                form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                form.note(&log, log_color);
            }
            Panel::CryptoOff => {
                form.note(t("Every conversation and note is written back as plain text, and the key file is removed. Anyone who can open this app's folder can then read them."), theme::muted());
                form.y += s(ds::space::XS);
                form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                form.note(&log, log_color);
            }
            Panel::CryptoWork => {
                let said = self.crypt_words();
                form.note(&said, if self.crypt.said_is_a_problem { theme::bad() } else { theme::text() });
                if self.crypt.code.is_some() {
                    form.field(t("Recovery code"), self.fields[0]);
                    form.note(t("Write this code down and keep it somewhere safe. It is shown once, and nothing else keeps it. If you forget the passphrase, it is the only way back to your conversations; if you lose both, they cannot be read by anyone."), theme::warn());
                }
                if self.crypt.work.is_none() {
                    form.y += s(ds::space::XS);
                    form.buttons(0, &[self.buttons[0]]);
                }
            }
            Panel::Unlock => {
                if let Some(why) = &self.crypt.damaged {
                    form.note(&tf("{0} It is conversation-key.json in {1}. Put back a copy of it, and start Dogmind again.", &[&t(why), &self.folder.display()]), theme::bad());
                } else if !self.crypt.by_code {
                    form.note(t("Your conversations are encrypted. Type your passphrase to open them."), theme::muted());
                    form.field(t("Passphrase"), self.fields[0]);
                    form.y += s(ds::space::XS);
                    form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                } else {
                    form.note(t("Type the recovery code you were shown, and choose a new passphrase."), theme::muted());
                    form.field(t("Recovery code"), self.fields[0]);
                    form.field(t("New passphrase"), self.fields[1]);
                    form.field(t("Again"), self.fields[2]);
                    form.y += s(ds::space::XS);
                    form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                }
                form.note(&log, log_color);
            }
            Panel::Sessions | Panel::Rewind => {
                if !self.list_rows.is_empty() {
                    form.list(self.list, self.list_rows.len().clamp(1, 6) as i32 * s(ds::size::LIST_ROW) + s(ds::space::S));
                }
                form.note(&log, log_color);
                form.y += s(ds::space::XS);
                if self.panel == Panel::Sessions {
                    form.buttons(0, &[self.buttons[0], self.buttons[1]]);
                } else {
                    form.buttons(0, &[self.buttons[0], self.buttons[1], self.buttons[2], self.buttons[3]]);
                }
            }
            Panel::About => {
                // The mark, the two versions, whether there is a newer one, and what the law asks for: in the middle of the page, with nothing to read but those.
                let (dog, mark) = (s(96), s(136));
                let block = dog + s(ds::space::XS) + title_height(mark) + s(ds::space::XL) + 2 * s(ds::size::TEXT_LINE) + s(ds::space::M)
                    + s(ds::size::CONTROL) + s(ds::space::S) + s(ds::size::TEXT_LINE) + s(ds::size::CONTROL) + s(ds::space::M) + s(ds::size::LINK);
                form.y += ((view - block) * 2 / 5 - s(ds::space::XL)).max(0);
                form.shapes.push(Shape::Logo { x: (form.width - dog) / 2, y: form.y, size: dog });
                form.y += dog + s(ds::space::XS);
                form.shapes.push(Shape::TitleMark { x: (form.width - mark) / 2, y: form.y, width: mark });
                form.y += title_height(mark) + s(ds::space::XL);
                // The two things this page is for, as a person would report them.
                let engine = self.engine_version.clone().unwrap_or_else(|| "Straydog".to_string());
                let build = readable_build(env!("DOGMIND_APP_BUILD")).replace(" (", "  ").replace(')', "");
                for line in [format!("Dogmind  {}  ({build})", env!("CARGO_PKG_VERSION")), engine] {
                    form.shapes.push(Shape::Text { area: rect(0, form.y, form.width, s(ds::size::TEXT_LINE)), text: line, font: FontKind::Body, color: theme::soft(), flags: DT_CENTER | ONE_LINE });
                    form.y += s(ds::size::TEXT_LINE);
                }
                form.y += s(ds::space::M);
                // Whether a newer release is out: asked for here, and said here. Once its setup is here, there is nothing left to look for: the button that stands in the look's place restarts into it.
                let setup_here = self.ready_setup().is_some();
                form.button_centered(if setup_here { self.buttons[3] } else { self.buttons[1] });
                if let Some((words, problem)) = self.release_words() {
                    let tall = text_height(self.fonts.small, &words, form.width);
                    form.shapes.push(Shape::Text { area: rect(0, form.y, form.width, tall + s(ds::space::HAIR)), text: words, font: FontKind::Small, color: if problem { theme::bad() } else { theme::soft() }, flags: DT_CENTER | DT_WORDBREAK });
                    form.y += tall + s(ds::space::HAIR) + s(ds::space::S);
                    if matches!(self.release_outcome, Some(updates::Outcome::Newer(_))) {
                        form.link_centered(self.buttons[2]);
                    }
                }
                // The update to it, as far as it has come: fetching, waiting to be run, or what became of the last try. With its setup here, the release's page is under it, to be read before restarting.
                if let Some((words, problem)) = self.update_words() {
                    let tall = text_height(self.fonts.small, &words, form.width);
                    form.shapes.push(Shape::Text { area: rect(0, form.y, form.width, tall + s(ds::space::HAIR)), text: words, font: FontKind::Small, color: if problem { theme::bad() } else { theme::soft() }, flags: DT_CENTER | DT_WORDBREAK });
                    form.y += tall + s(ds::space::HAIR) + s(ds::space::S);
                    if setup_here {
                        form.link_centered(self.buttons[2]);
                    } else if self.update_button_words().is_some() {
                        form.link_centered(self.buttons[3]);
                    }
                }
                form.toggle_centered(t("Update automatically"), self.toggle_updates);
                let what_it_does = t("It downloads in the background and asks before restarting.");
                let tall = text_height(self.fonts.small, what_it_does, form.width);
                form.shapes.push(Shape::Text { area: rect(0, form.y, form.width, tall + s(ds::space::HAIR)), text: what_it_does.to_string(), font: FontKind::Small, color: theme::muted(), flags: DT_CENTER | DT_WORDBREAK });
                form.y += tall + s(ds::space::HAIR) + s(ds::space::S);
                form.link_centered(self.buttons[0]);
                if !log.is_empty() {
                    let tall = text_height(self.fonts.small, &log, form.width);
                    form.shapes.push(Shape::Text { area: rect(0, form.y, form.width, tall + s(ds::space::HAIR)), text: log, font: FontKind::Small, color: log_color, flags: DT_CENTER | DT_WORDBREAK });
                    form.y += tall + s(ds::space::HAIR);
                }
            }
            _ => {}
        }
    }

    /// Leaving a page of the settings keeps what was typed there. `false`: it cannot be kept as it is, and the page says why.
    unsafe fn leave_page(&mut self) -> bool {
        match self.panel {
            Panel::Providers => {
                // (leaving with what was typed lost is no way to end)
                let typed = self.provider_from_fields();
                let key_typed = !get_text(self.fields[KEY_FIELD]).trim().is_empty();
                let known = self.selected_provider.and_then(|at| self.settings.providers.get(at));
                let same = !key_typed && match known {
                    Some(known) => *known == typed,
                    None => typed.name.is_empty() && typed.base_url.is_empty() && typed.api_key.is_empty(),
                };
                same || self.save_provider()
            }
            Panel::Models => match self.store_engine_fields() {
                Ok(()) => true,
                Err(error) => {
                    self.problem_in_panel(&error);
                    false
                }
            },
            Panel::Tools => self.keep_search_key(),
            Panel::Instructions => {
                let text = get_text(self.doc).replace("\r\n", "\n").replace('\r', "\n");
                let known = std::fs::read_to_string(self.folder.join("instructions.md")).unwrap_or_default();
                if text.trim_end() == known.trim_end() {
                    return true;
                }
                match std::fs::write(self.folder.join("instructions.md"), text) {
                    Ok(()) => true,
                    Err(error) => {
                        self.problem_in_panel(&error.to_string());
                        false
                    }
                }
            }
            _ => true,
        }
    }

    /// The search service's key, from the Tools page.
    unsafe fn keep_search_key(&mut self) -> bool {
        let typed = get_text(self.fields[KEY_FIELD]).trim().to_string();
        let variable = get_text(self.fields[VARIABLE_FIELD]).trim().to_string();
        if self.variable_on && !settings::is_variable_name(&variable) && !variable.is_empty() {
            self.problem_in_panel(t(settings::VARIABLE_NAME_ONLY));
            return false;
        }
        let setting = key_setting(&self.settings.search_key, self.variable_on, &variable, &typed, settings::SEARCH_KEY_TARGET);
        if !typed.is_empty() && !self.variable_on {
            if let Err(error) = crate::keys::store().write(settings::SEARCH_KEY_TARGET, &typed) {
                self.problem_in_panel(&error);
                return false;
            }
            set_text(self.fields[KEY_FIELD], "");
            self.key_editing = false;
        }
        if setting != self.settings.search_key {
            self.settings.search_key = setting;
            self.save();
            self.apply_web();
        }
        true
    }

    /// Fields and switches for a key, as a setting has it.
    unsafe fn show_key_setting(&mut self, setting: &str) {
        let source = KeySource::of(setting);
        self.variable_on = matches!(source, KeySource::Variable(_));
        self.key_editing = matches!(source, KeySource::None);
        self.advanced_open = !matches!(source, KeySource::None | KeySource::Stored(_));
        set_text(self.fields[KEY_FIELD], "");
        set_text(self.fields[VARIABLE_FIELD], &if let KeySource::Variable(name) = &source { name.clone() } else { String::new() });
        set_button_style(self.variable_toggle, if self.variable_on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        set_text(self.advanced_button, if self.advanced_open { t("chevron-down|Other ways to give the key") } else { t("chevron-right|Other ways to give the key") });
    }

    /// The setting for the key of what this page is about, and where it goes when it changes.
    unsafe fn forget_key(&mut self) {
        let setting = match self.panel {
            Panel::Tools => self.settings.search_key.clone(),
            _ => self.selected_provider.and_then(|at| self.settings.providers.get(at)).map_or(String::new(), |provider| provider.api_key.clone()),
        };
        // (the setting stays as it is while the key is still kept: said "removed" with the key there, it would have no way left to be found)
        if let Some(words) = settings::remove_key(crate::keys::store(), &setting) {
            self.problem_in_panel(&words);
            return;
        }
        match self.panel {
            Panel::Tools => self.settings.search_key.clear(),
            _ => {
                if let Some(provider) = self.selected_provider.and_then(|at| self.settings.providers.get_mut(at)) {
                    provider.api_key.clear();
                }
            }
        }
        self.save();
        if self.panel == Panel::Tools {
            self.apply_web();
        }
        self.show_key_setting("");
        self.say_in_panel(t("The key was removed."));
    }

    /// Tasks, what has changed and how full the context is, all painted by the window; a changed file can be clicked.
    unsafe fn layout_preview(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, height: i32) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        // (level with the session's name, and with the same edge under it)
        let bar = s(ds::size::TITLE_STRIP);
        let button = s(ds::size::ROW_ACTION);
        let mut at = x + width;
        let margin = s(ds::size::RIGHT_GAP);
        shapes.push(Shape::Fill { area: rect(x - margin, bar - s(ds::space::PIXEL), width + 2 * margin, s(ds::space::PIXEL)), color: theme::line() });
        for window in [self.preview_close, self.preview_browser, self.preview_reload] {
            at -= button + s(ds::space::HAIR);
            SetWindowPos(window, std::ptr::null_mut(), at, (bar - button) / 2, button, button, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        let name = self.preview_file.as_deref().and_then(Path::file_name).map_or(String::new(), |name| name.to_string_lossy().into_owned());
        let name = if self.preview.is_some() { name } else { tf("{0}: starting...", &[&name]) };
        shapes.push(Shape::Text { area: rect(x, 0, at - x - s(ds::space::S), bar), text: name, font: FontKind::Strong, color: theme::text(), flags: ONE_LINE });
        let note = if self.preview_blocked > 0 || self.preview_gone { s(26) } else { 0 };
        if note > 0 {
            let words = if self.preview_gone {
                tf("{0} is gone. Nothing is shown until it is written again.", &[&self.preview_file.as_deref().and_then(Path::file_name).map_or(String::new(), |name| name.to_string_lossy().into_owned())])
            } else {
                tf("{0} requests to outside were blocked here. Open in browser to let them through.", &[&self.preview_blocked])
            };
            shapes.push(Shape::Text { area: rect(x, height - note, width, note), text: words, font: FontKind::Small, color: theme::warn(), flags: ONE_LINE });
        }
        if let Some(shown) = &self.preview {
            // (to the panel's edges: a page brings its own margins)
            shown.place(x - margin, bar, x + width + margin, height - note);
        }
    }

    /// Shows a file of the workspace beside the conversation. The first takes a moment: the browser is made now, not before.
    unsafe fn open_preview(&mut self, wanted: &str) {
        self.open_preview_at(wanted, None);
    }

    /// The preview of a file, at these lines of it when they are given (a source file's: lit, and in view).
    unsafe fn open_preview_at(&mut self, wanted: &str, lines: Option<(usize, usize)>) {
        self.preview_lines = lines;
        let file = if Path::new(wanted).is_absolute() { PathBuf::from(wanted) } else { self.workspace.join(wanted) };
        if !file.is_file() {
            self.say(&tf("There is no {0} in this folder.", &[&wanted]));
            return;
        }
        if !preview::can_show(wanted) {
            self.say(t("The preview shows web pages, pictures, PDFs, Markdown and source files."));
            return;
        }
        // (a picture that went with a message may lie anywhere: the browser then reaches its folder, and it has no script to reach with)
        let root = if file.starts_with(&self.workspace) { self.workspace.clone() } else if attach::is_image(&file) { file.parent().map_or_else(|| self.workspace.clone(), Path::to_path_buf) } else { self.workspace.clone() };
        let Some(_) = preview::address_of(&root, &file) else {
            self.say(t("The preview shows what is in this folder."));
            return;
        };
        self.preview_written = preview::newest_beside(&file);
        self.preview_loaded = 0;
        self.preview_gone = false;
        self.preview_file = Some(file.clone());
        self.preview_root = Some(root.clone());
        self.preview_blocked = 0;
        if let Some(shown) = &self.preview {
            if let Err(error) = shown.open(&root, &file, lines) {
                self.close_preview();
                self.say(&tf("The preview did not open: {0}", &[&error]));
                return;
            }
        } else if !self.preview_making {
            let window = self.window as isize;
            match preview::make(window, &self.folder.join("preview"), move || {
                PostMessageW(window as HWND, WM_PREVIEW_MADE, 0, 0);
            }) {
                Ok(()) => self.preview_making = true,
                Err(error) => {
                    self.close_preview();
                    self.say(&tf("The preview needs the WebView2 runtime of Windows, which did not start: {0}", &[&error]));
                    return;
                }
            }
        }
        self.layout();
        // (a preview needs the width: without it there is nothing to see, and nothing else would say why)
        if self.right_width == 0 && self.panel == Panel::Chat {
            self.say(t("The window is too narrow for the preview: make it wider."));
        }
        InvalidateRect(self.window, std::ptr::null(), 0);
    }

    /// The browser has come up, or could not.
    unsafe fn preview_made(&mut self) {
        self.preview_making = false;
        match preview::take_made() {
            Some(Ok(made)) => {
                // Closed while it started: it goes, with its memory.
                let Some(file) = self.preview_file.clone() else { return };
                let root = self.preview_root.clone().unwrap_or_else(|| self.workspace.clone());
                let opened = made.open(&root, &file, self.preview_lines);
                self.preview = Some(made);
                if let Err(error) = opened {
                    self.close_preview();
                    self.say(&tf("The preview did not open: {0}", &[&error]));
                    return;
                }
            }
            Some(Err(error)) => {
                self.close_preview();
                self.say(&tf("The preview needs the WebView2 runtime of Windows, which did not start: {0}", &[&error]));
                return;
            }
            None => return,
        }
        self.layout();
        repaint_all(self.window);
    }

    /// The browser goes with it, and what it held.
    unsafe fn close_preview(&mut self) {
        self.preview = None;
        self.preview_file = None;
        self.preview_written = None;
        self.preview_loaded = 0;
        self.preview_gone = false;
        self.preview_blocked = 0;
        self.layout();
        InvalidateRect(self.window, std::ptr::null(), 0);
    }

    /// A session in another folder: what is shown no longer belongs to it.
    unsafe fn preview_follows_workspace(&mut self) {
        if self.preview_file.as_ref().is_some_and(|file| !file.starts_with(&self.workspace)) {
            self.close_preview();
        }
    }

    /// Once a second: what was shown has been written again, or the page has had something turned away.
    unsafe fn watch_preview(&mut self) {
        let Some(file) = &self.preview_file else { return };
        let Some(shown) = &self.preview else { return };
        // What the page asked for is what it is watched by, and not only what lies near it.
        let written = shown.newest_loaded(file);
        let loaded = shown.loaded_count();
        let mut relayout = false;
        match preview::seen_since(self.preview_written, written) {
            preview::Seen::Gone if !self.preview_gone => {
                // (the old page does not stay: it would show what is no more)
                self.preview_gone = true;
                self.preview_written = None;
                shown.clear();
                relayout = true;
            }
            preview::Seen::Gone => {}
            _ if loaded != self.preview_loaded && !self.preview_gone => {
                (self.preview_loaded, self.preview_written) = (loaded, written);
            }
            preview::Seen::Changed => {
                self.preview_written = written;
                self.preview_loaded = loaded;
                relayout = std::mem::take(&mut self.preview_gone);
                shown.reload();
            }
            preview::Seen::Same => {}
        }
        let blocked = shown.blocked.get();
        if blocked != self.preview_blocked || relayout {
            self.preview_blocked = blocked;
            self.layout();
            InvalidateRect(self.window, std::ptr::null(), 0);
        }
    }

    /// What this session has changed that the preview can show, the first of them: what `/preview` alone means.
    fn preview_candidate(&self) -> Option<String> {
        self.changes.iter().map(|change| change.path.clone()).find(|path| preview::is_page(path)).or_else(|| self.workspace.join("index.html").is_file().then(|| "index.html".to_string()))
    }

    /// How many rows the list of what waits takes in the panel on the right: its heading, a row for each command and each wait (a second for what a command waits to read), what has stopped
    /// waking, and the line that says these end with the app. At most so many are listed.
    fn background_rows(&self) -> usize {
        if self.waiting.is_empty() {
            return 0;
        }
        let listed = |count: usize| count.min(WAITING_LISTED);
        let commands = self.waiting.commands.iter().take(WAITING_LISTED).map(|command| 1 + usize::from(command.watching_for.is_some())).sum::<usize>();
        let (more_commands, more_waits) = (self.waiting.commands.len() > WAITING_LISTED, self.waiting.waits.len() > WAITING_LISTED);
        1 + commands + listed(self.waiting.waits.len()) + usize::from(more_commands) + usize::from(more_waits) + if self.waiting.paused.is_some() { 3 } else { 0 } + 1
    }

    /// The list of what the session has going or waiting in the background: each command and wait with a stop of its own, what stopped it from waking, and that these end with the app.
    /// The times are drawn as the window paints them (`paint`), so that they run on without the panel being laid out again. Returns where the next section begins.
    unsafe fn layout_waiting(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, mut y: i32) -> i32 {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let row = s(ds::size::TEXT_ROW);
        let (stop_wide, time_wide) = (s(ds::space::XL), s(76));
        let indent = s(ds::size::RIGHT_INDENT);
        shapes.push(Shape::Text { area: rect(x, y, width, s(ds::space::XL)), text: t("In the background").to_string(), font: FontKind::Strong, color: theme::muted(), flags: ONE_LINE });
        // (more than one: all of them at a press)
        if self.waiting.commands.len() + self.waiting.waits.len() > 1 {
            let words = t("Stop all");
            let across = text_width(self.fonts.small, words);
            let area = rect(x + width - across, y + (s(ds::space::XL) - s(ds::size::TEXT_LINE)) / 2, across, s(ds::size::TEXT_LINE));
            shapes.push(Shape::Text { area, text: words.to_string(), font: FontKind::Small, color: theme::accent_hover(), flags: DT_RIGHT | ONE_LINE });
            self.waiting_hits.push((rect(area.left - s(6), y, across + s(12), s(ds::space::XL)), "*".to_string()));
        }
        y += s(ds::size::QUEUE_STEP);
        let stop_at = |this: &mut App, shapes: &mut Vec<Shape>, y: i32, id: &str| {
            let area = rect(x + width - stop_wide, y, stop_wide, row);
            shapes.push(Shape::Icon { name: "x", area, size: s(ds::size::ICON_SMALL), color: theme::faint() });
            this.waiting_hits.push((rect(area.left - s(4), y, stop_wide + s(4), row), id.to_string()));
        };
        let commands = self.waiting.commands.clone();
        for (at, command) in commands.iter().enumerate().take(WAITING_LISTED) {
            shapes.push(Shape::Icon { name: "terminal", area: rect(x, y, s(ds::space::L), row), size: s(ds::size::ICON_SMALL), color: theme::muted() });
            let text_wide = width - indent - stop_wide - time_wide;
            shapes.push(Shape::Text { area: rect(x + indent, y, text_wide, row), text: command.command.clone(), font: FontKind::Small, color: theme::text(), flags: ONE_LINE });
            self.waiting_times.push((rect(x + width - stop_wide - time_wide, y, time_wide, row), WaitRow::Command(at)));
            stop_at(self, shapes, y, &command.id);
            y += row;
            if let Some(pattern) = &command.watching_for {
                shapes.push(Shape::Text { area: rect(x + indent, y, width - indent - stop_wide, row), text: tf("waits for {0}", &[pattern]), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
                y += row;
            }
        }
        if commands.len() > WAITING_LISTED {
            shapes.push(Shape::Text { area: rect(x + indent, y, width - indent, row), text: tf("and {0} more", &[&(commands.len() - WAITING_LISTED)]), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
            y += row;
        }
        let waits = self.waiting.waits.clone();
        for (at, wait) in waits.iter().enumerate().take(WAITING_LISTED) {
            shapes.push(Shape::Icon { name: "clock", area: rect(x, y, s(ds::space::L), row), size: s(ds::size::ICON_SMALL), color: theme::muted() });
            shapes.push(Shape::Text { area: rect(x + indent, y, width - indent - stop_wide - time_wide, row), text: wait.note.clone(), font: FontKind::Small, color: theme::text(), flags: ONE_LINE });
            self.waiting_times.push((rect(x + width - stop_wide - time_wide, y, time_wide, row), WaitRow::Wait(at)));
            stop_at(self, shapes, y, &wait.id);
            y += row;
        }
        if waits.len() > WAITING_LISTED {
            shapes.push(Shape::Text { area: rect(x + indent, y, width - indent, row), text: tf("and {0} more", &[&(waits.len() - WAITING_LISTED)]), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
            y += row;
        }
        if let Some(paused) = self.waiting.paused {
            shapes.push(Shape::Text { area: rect(x, y, width, 3 * row), text: lang::notice(paused.sentence()), font: FontKind::Small, color: theme::warn(), flags: DT_WORDBREAK });
            y += 3 * row;
        }
        shapes.push(Shape::Text { area: rect(x, y, width, row), text: t("These end when Dogmind quits.").to_string(), font: FontKind::Small, color: theme::faint(), flags: ONE_LINE });
        y + row
    }

    /// What a row of the list says of its time now: how long a command has run, or has to run, and how long until a wait is up. Counted on from when the agent last said.
    fn waiting_time_words(&self, row: WaitRow) -> String {
        let since = self.waiting_taken.elapsed().as_secs();
        match row {
            WaitRow::Command(at) => match self.waiting.commands.get(at) {
                Some(command) if command.ended => t("ended").to_string(),
                Some(command) => cards::duration_words(command.seconds + since),
                None => String::new(),
            },
            WaitRow::Wait(at) => self.waiting.waits.get(at).map_or(String::new(), |wait| tf("in {0}", &[&cards::duration_words(wait.in_seconds.saturating_sub(since))])),
        }
    }

    /// Once a second: the times in the list of what waits run on, where it is in view.
    unsafe fn waiting_tick(&self) {
        if self.waiting_times.is_empty() || self.panel != Panel::Chat || self.right_width == 0 {
            return;
        }
        for (area, _) in &self.waiting_times {
            InvalidateRect(self.window, area, 0);
        }
    }

    /// A press on the stop of a row of the list, or on "Stop all": it ends that command or wait, which wakes nothing. The agent is not asked: what is going on is the window's to end, and what is left is read from it.
    unsafe fn stop_waiting(&mut self, id: &str) {
        if id == "*" {
            self.agent.background.stop_all();
        } else {
            self.agent.background.stop(id);
        }
        self.waiting = self.agent.background.snapshot();
        self.waiting_taken = std::time::Instant::now();
        InvalidateRect(self.side, std::ptr::null(), 0);
        self.layout();
    }

    unsafe fn layout_right(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, height: i32) {
        if self.preview_file.is_some() {
            self.layout_preview(shapes, x, width, height);
            return;
        }
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let row = s(ds::size::TEXT_ROW);
        // The views of the panel, as words over it (level with the session's name): the one shown is underlined.
        let words_top = (s(ds::size::TITLE_STRIP) - s(ds::space::XL)) / 2;
        let mut across_to = x;
        for (words, view) in [(t("Overview"), RightView::Overview), (t("Files"), RightView::Files), (t("Terminal"), RightView::Terminal)] {
            let across = text_width(self.fonts.strong, words);
            let on = self.settings.right_view == view;
            shapes.push(Shape::Text { area: rect(across_to, words_top, across, s(ds::space::XL)), text: words.to_string(), font: FontKind::Strong, color: if on { theme::text() } else { theme::muted() }, flags: ONE_LINE });
            if on {
                shapes.push(Shape::Fill { area: rect(across_to, words_top + s(ds::space::XL) + s(ds::space::HAIR), across, s(ds::space::HAIR)), color: theme::accent() });
            }
            self.right_tab_hits.push((rect(across_to - s(ds::space::S), words_top - s(ds::space::XS), across + 2 * s(ds::space::S), s(ds::space::XL) + s(ds::space::M)), view));
            across_to += across + s(ds::space::XL);
        }
        // (what is under them begins past the strip the session's name has)
        let mut y = s(ds::size::TITLE_STRIP) + s(ds::space::S);
        let heading = |shapes: &mut Vec<Shape>, y: &mut i32, text: String| {
            shapes.push(Shape::Text { area: rect(x, *y, width, s(ds::space::XL)), text, font: FontKind::Strong, color: theme::muted(), flags: ONE_LINE });
            *y += s(ds::size::QUEUE_STEP);
        };
        let plain = |shapes: &mut Vec<Shape>, y: &mut i32, text: String, color: u32| {
            shapes.push(Shape::Text { area: rect(x, *y, width, s(ds::size::TEXT_LINE)), text, font: FontKind::Small, color, flags: ONE_LINE });
            *y += s(ds::space::XL);
        };

        // The terminal has the panel to the foot of the window: no engine line under it.
        if self.settings.right_view == RightView::Terminal {
            self.dashboard_hit = None;
            self.layout_terminal_view(shapes, x, width, s(ds::size::TITLE_STRIP), height);
            return;
        }
        // The engine, at the foot: what it is and the way to its dashboard.
        self.dashboard_hit = None;
        let foot = height - s(ds::space::L);
        let mut engine_top = foot;
        if let Some(url) = self.dashboard_url() {
            let link = rect(x, foot - row, width, row);
            shapes.push(Shape::Icon { name: "external-link", area: rect(x, link.top, s(ds::space::L), row), size: s(ds::size::ICON_SMALL), color: theme::accent_hover() });
            shapes.push(Shape::Text { area: rect(x + s(ds::size::RIGHT_INDENT), link.top, width - s(ds::size::RIGHT_INDENT), row), text: t("Open the dashboard").to_string(), font: FontKind::Small, color: theme::accent_hover(), flags: ONE_LINE });
            self.dashboard_hit = Some((rect(x - s(6), link.top, width + s(ds::space::M), row), url));
            engine_top = link.top;
        }
        if !self.model.is_empty() {
            engine_top -= row;
            shapes.push(Shape::Text { area: rect(x, engine_top, width, row), text: self.model.clone(), font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
        }
        if engine_top < foot {
            engine_top -= s(ds::size::QUEUE_STEP);
            shapes.push(Shape::Text { area: rect(x, engine_top, width, s(ds::space::XL)), text: t("Engine").to_string(), font: FontKind::Strong, color: theme::muted(), flags: ONE_LINE });
        }
        // The files of the folder: the list takes what is between the two words and the engine's lines.
        if self.settings.right_view == RightView::Files {
            let margin = s(ds::size::RIGHT_GAP);
            let top = s(ds::size::TITLE_STRIP);
            self.files_place(x - margin + s(ds::space::S), top, width + 2 * margin - 2 * s(ds::space::S), (engine_top - top - s(ds::space::S)).max(s(ds::size::TEXT_ROW)));
            return;
        }
        // What waits in the background has its place after the tasks, whatever else there is: it is what will wake the session, and it ends with the app.
        let background_rows = self.background_rows();
        // What fits, past the two headings and above the engine: nothing is drawn off the bottom of the window.
        let (max_tasks, max_changes) = share_rows((((engine_top - y - s(60)) / row) - background_rows as i32).max(2), self.todos.len(), self.changes.len());
        if !self.todos.is_empty() {
            heading(shapes, &mut y, t("Tasks").to_string());
        }
        for item in self.todos.iter().take(max_tasks) {
            let status = item["status"].as_str().unwrap_or("pending");
            let done = matches!(status, "done" | "completed");
            let doing = matches!(status, "doing" | "in_progress");
            if done {
                shapes.push(Shape::Icon { name: "check", area: rect(x, y, s(ds::space::L), row), size: s(ds::size::ICON_SMALL), color: theme::good() });
            } else {
                let dot = s(9);
                let area = rect(x + s(3), y + (row - dot) / 2, dot, dot);
                shapes.push(Shape::Round { area, radius: dot / 2, fill: doing.then_some(theme::accent()), border: (!doing).then_some(theme::faint()) });
            }
            let color = if done { theme::faint() } else if doing { theme::text() } else { theme::muted() };
            shapes.push(Shape::Text { area: rect(x + s(ds::size::RIGHT_INDENT), y, width - s(ds::size::RIGHT_INDENT), row), text: item["text"].as_str().unwrap_or("").to_string(), font: FontKind::Small, color, flags: ONE_LINE });
            y += row;
        }
        if self.todos.len() > max_tasks {
            plain(shapes, &mut y, tf("and {0} more", &[&(self.todos.len() - max_tasks)]), theme::faint());
        }
        if !self.todos.is_empty() {
            y += s(ds::size::RIGHT_GAP);
        }
        if !self.waiting.is_empty() {
            y = self.layout_waiting(shapes, x, width, y);
            y += s(ds::size::RIGHT_GAP);
        }
        if self.changes.is_empty() {
            return;
        }

        let (added, removed) = self.changes.iter().fold((0, 0), |(added, removed), change| (added + change.added, removed + change.removed));
        heading(shapes, &mut y, changes_heading(added, removed));
        if let Some(git) = self.git.as_ref().filter(|git| git.conflicted > 0) {
            plain(shapes, &mut y, tf("{0} in conflict", &[&git.conflicted]), theme::bad());
        }
        for (at, change) in self.changes.iter().enumerate().take(max_changes) {
            let name = change.path.rsplit(['/', '\\']).next().unwrap_or(&change.path).to_string();
            let numbers = s(76);
            // A file that can be looked at has an eye for that; the row itself is its diff.
            let eye = s(ds::space::XL);
            if preview::can_show(&change.path) {
                let area = rect(x + width - numbers - eye, y, eye, row);
                shapes.push(Shape::Icon { name: "eye", area, size: s(ds::size::ICON_SMALL), color: theme::faint() });
                self.preview_hits.push((area, self.workspace.join(&change.path)));
            }
            shapes.push(Shape::Text { area: rect(x, y, width - numbers - eye, row), text: name, font: FontKind::Small, color: theme::text(), flags: ONE_LINE });
            let half = numbers / 2;
            if change.added > 0 {
                shapes.push(Shape::Text { area: rect(x + width - numbers, y, half, row), text: format!("+{}", change.added), font: FontKind::Small, color: theme::good(), flags: DT_RIGHT | ONE_LINE });
            }
            if change.removed > 0 {
                shapes.push(Shape::Text { area: rect(x + width - half, y, half, row), text: format!("−{}", change.removed), font: FontKind::Small, color: theme::bad(), flags: DT_RIGHT | ONE_LINE });
            }
            self.right_hits.push((rect(x - s(6), y, width + s(ds::space::M), row), at));
            y += row;
        }
        if self.changes.len() > max_changes {
            plain(shapes, &mut y, tf("and {0} more: /diff", &[&(self.changes.len() - max_changes)]), theme::faint());
        }
    }

    /// What has changed, on another thread: git may take a moment on a large tree.
    unsafe fn refresh_changes(&self) {
        // What git says, and what the agent itself changed where git does not look: outside a repository, or in what it ignores.
        let (poster, path, current) = (self.poster.clone(), self.workspace.clone(), self.current_session.clone());
        std::thread::spawn(move || {
            poster.send(Note::Git(path.clone(), dogmind_agent::tools::git_info(&path)));
            let own = current.map_or(Vec::new(), |current| dogmind_agent::checkpoint::Checkpoints::open(&current).session_changes(&path));
            let found = match dogmind_agent::tools::workspace_changes(&path) {
                Ok(mut changes) => {
                    let same = |a: &str, b: &str| a.replace('\\', "/").eq_ignore_ascii_case(&b.replace('\\', "/"));
                    for change in own {
                        if !changes.iter().any(|known| same(&known.path, &change.path)) {
                            changes.push(change);
                        }
                    }
                    Ok(changes)
                }
                Err(_) if !own.is_empty() => Ok(own),
                Err(_) => Err(t("No changes.").to_string()),
            };
            poster.send(Note::Changes(path, found));
        });
    }

    fn font(&self, kind: FontKind) -> HFONT {
        match kind {
            FontKind::Body => self.fonts.body,
            FontKind::Small => self.fonts.small,
            FontKind::Strong => self.fonts.strong,
            FontKind::Title => self.fonts.title,
            FontKind::Heading => self.fonts.heading,
            FontKind::Greeting => self.fonts.greeting,
        }
    }

    /// Where the ring turns in the activity line.
    fn spinner_area(&self) -> RECT {
        let (area, size) = (self.activity_area, scale(self.dpi, 15));
        rect(area.left + scale(self.dpi, ds::size::THUMB), (area.top + area.bottom - size) / 2, size, size)
    }

    /// A turn of the ring: only it is drawn again, and the words with it when the seconds they show have changed.
    unsafe fn spinner_tick(&mut self) {
        if self.activity.is_empty() || self.panel != Panel::Chat || self.waits_for_person() {
            return;
        }
        let second = self.activity_since.map_or(0, |since| since.elapsed().as_secs());
        if second != self.activity_second {
            self.activity_second = second;
            InvalidateRect(self.window, &self.activity_area, 0);
        } else {
            let area = self.spinner_area();
            let around = RECT { left: area.left - 2, top: area.top - 2, right: area.right + 2, bottom: area.bottom + 2 };
            InvalidateRect(self.window, &around, 0);
        }
    }

    unsafe fn draw_shape(&self, device: HDC, shape: &Shape) {
        match shape {
            Shape::Round { area, radius, fill, border } => rounded(device, *area, *radius, *fill, *border),
            Shape::Text { area, text, font, color, flags } => draw_text(device, text, *area, self.font(*font), *color, *flags),
            Shape::Logo { x, y, size } => logo(device, *x, *y, *size),
            Shape::TitleMark { x, y, width } => title_mark(device, *x, *y, *width),
            Shape::Bubble { area, radius, tip, fill, line, thickness } => bubble(device, *area, *radius, *tip, *fill, *line, *thickness),
            Shape::Icon { name, area, size, color } => icon::draw(device, name, *area, *size, *color),
            Shape::Fill { area, color } => {
                let brush = CreateSolidBrush(*color);
                FillRect(device, area, brush);
                DeleteObject(brush);
            }
        }
    }

    /// `dirty` is all that shows of this paint: nothing else is drawn, as this runs for each turn of the ring and each move of the scrollbar's
    /// thumb, and all of the window takes several milliseconds of the time the answer has to come in.
    unsafe fn paint(&mut self, device: HDC, client: RECT, dirty: RECT) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        FillRect(device, &client, self.brushes[0]);
        let side = RECT { left: 0, top: 0, right: self.sidebar_width, bottom: client.bottom };
        FillRect(device, &side, self.brushes[1]);
        if self.right_width > 0 {
            let right = RECT { left: client.right - self.right_width, top: 0, right: client.right, bottom: client.bottom };
            FillRect(device, &right, self.brushes[1]);
        }
        if let Some(row) = self.hover_row.filter(|row| overlaps(row, &dirty)) {
            rounded(device, row, s(ds::radius::SMALL), Some(theme::raised()), None);
        }
        for shape in &self.shapes {
            if !overlaps(&shape.bounds(), &dirty) {
                continue;
            }
            self.draw_shape(device, shape);
        }
        // (the times of what waits: counted on from what the agent last said, drawn now)
        if self.panel == Panel::Chat {
            for (area, row) in &self.waiting_times {
                if overlaps(area, &dirty) {
                    draw_text(device, &self.waiting_time_words(*row), *area, self.fonts.small, theme::muted(), DT_RIGHT | ONE_LINE);
                }
            }
        }
        if self.panel == Panel::Chat {
            if !self.activity.is_empty() && !self.waits_for_person() {
                let area = self.activity_area;
                let elapsed = self.activity_since.map_or(0.0, |since| since.elapsed().as_secs_f32());
                spinner(device, self.spinner_area(), (elapsed * 300.0) % 360.0, theme::line(), theme::accent(), s(ds::space::HAIR) as f32);
                let words = RECT { left: area.left + s(30), ..area };
                if overlaps(&words, &dirty) && !self.activity.trim().is_empty() {
                    let text = if elapsed >= 3.0 { format!("{}   {}", self.activity, tf("{0} s", &[&(elapsed as u32)])) } else { self.activity.clone() };
                    draw_text(device, &text, words, self.fonts.body, theme::text(), ONE_LINE);
                }
            }
            if self.rail.is_some_and(|rail| overlaps(&rail, &dirty)) {
                self.paint_rail(device);
            }
            if let Some(thumb) = self.thumb.filter(|shown| self.has_messages && overlaps(shown, &dirty)) {
                let color = if self.thumb_grab.is_some() { theme::muted() } else { theme::line() };
                rounded(device, thumb, (thumb.right - thumb.left) / 2, Some(color), None);
            }
            if overlaps(&self.status_area, &dirty) {
                self.paint_status(device);
            }
        }
    }

    /// The row under the pointer in the panel on the right, drawn lit: a row that can be clicked should look it.
    unsafe fn hover_on(&mut self, row: Option<RECT>) {
        let same = match (&self.hover_row, &row) {
            (Some(a), Some(b)) => same_rect(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        for area in [self.hover_row, row].into_iter().flatten() {
            InvalidateRect(self.window, &area, 0);
        }
        if self.hover_row.is_none() {
            let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.window, dwHoverTime: 0 };
            TrackMouseEvent(&mut track);
        }
        self.hover_row = row;
    }

    /// The strip the transcript's scrollbar has at the panel's edge.
    unsafe fn thumb_strip(&self) -> RECT {
        let area = self.transcript_area;
        RECT { left: area.right - GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi), ..area }
    }

    /// The transcript's scrollbar: its range, what shows of it, and where it is.
    unsafe fn transcript_bar(&self) -> SCROLLINFO {
        let mut bar: SCROLLINFO = std::mem::zeroed();
        bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        bar.fMask = SIF_ALL;
        GetScrollInfo(self.transcript, SB_VERT, &mut bar);
        bar
    }

    /// What of the transcript control shows: not the strip at the panel's edge, where its scrollbar's thumb is drawn, nor the rail of turns.
    /// Setting it redraws the control and what is around it: only when it changes.
    unsafe fn set_transcript_region(&mut self) {
        let area = self.transcript_area;
        let bar = GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi);
        let size = (area.right - area.left - bar, area.bottom - area.top);
        let rail = self.rail.map(|rail| (rail.left - area.left, rail.top - area.top, rail.right - area.left, rail.bottom - area.top));
        if (size, rail) == self.transcript_region {
            return;
        }
        self.transcript_region = (size, rail);
        let region = CreateRectRgn(0, 0, size.0, size.1);
        if let Some((left, top, right, bottom)) = rail {
            let cut = CreateRectRgn(left, top, right, bottom);
            CombineRgn(region, region, cut, RGN_DIFF);
            DeleteObject(cut);
        }
        SetWindowRgn(self.transcript, region, 1);
    }

    /// Where the rail of turns stands: two turns or more, at the panel's edge beside the scrollbar's strip, in the middle of the transcript's height,
    /// where the text column leaves room for it.
    unsafe fn rail_frame(&self) -> Option<RECT> {
        if self.rail_turns.len() < 2 || !self.has_messages || self.panel != Panel::Chat {
            return None;
        }
        let s = |value: i32| scale(self.dpi, value);
        let area = self.transcript_area;
        let right = area.right - GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi);
        let left = right - s(RAIL_WIDTH);
        if left < self.column_right + s(ds::space::S) {
            return None;
        }
        let pitch = s(RAIL_PITCH);
        let most = ((area.bottom - area.top) / 2).max(3 * pitch);
        let tall = (self.rail_turns.len() as i32 * pitch + 2 * s(RAIL_INSET)).min(most);
        let top = (area.top + area.bottom - tall) / 2;
        Some(RECT { left, top, right, bottom: top + tall })
    }

    /// The turn being read: the last whose message begins above a third of the way down the view, and at the end of the conversation, the last.
    unsafe fn reading_turn(&self) -> usize {
        if self.rail_turns.is_empty() {
            return 0;
        }
        let bar = self.transcript_bar();
        if bar.nPage == 0 || bar.nPos + bar.nPage as i32 + 4 >= bar.nMax {
            return self.rail_turns.len() - 1;
        }
        let area = self.transcript_area;
        let point = POINT { x: (self.column_right - area.left) / 2, y: (area.bottom - area.top) / 3 };
        let at = SendMessageW(self.transcript, EM_CHARFROMPOS_RICH, 0, std::ptr::addr_of!(point) as LPARAM) as i32;
        self.rail_turns.iter().rposition(|turn| turn.start <= at).unwrap_or(0)
    }

    /// The rail where the view now is: which turn is being read, and where each mark stands. Only what changed is drawn again.
    unsafe fn place_rail(&mut self) {
        let frame = self.rail_frame();
        let moved = match (&self.rail, &frame) {
            (Some(a), Some(b)) => !same_rect(a, b),
            (None, None) => false,
            _ => true,
        };
        if moved {
            for area in [self.rail, frame].into_iter().flatten() {
                InvalidateRect(self.window, &area, 0);
            }
            self.rail = frame;
            self.set_transcript_region();
            if frame.is_none() {
                self.rail_hover = None;
            }
        }
        let reading = self.reading_turn();
        let pitch = scale(self.dpi, RAIL_PITCH);
        let hits: Vec<(RECT, usize)> = frame.map_or(Vec::new(), |frame| {
            rail_marks(self.rail_turns.len(), reading, frame.bottom - frame.top, pitch, scale(self.dpi, RAIL_INSET)).into_iter()
                .map(|(at, y)| (RECT { left: frame.left, top: frame.top + y - pitch / 2, right: frame.right, bottom: frame.top + y - pitch / 2 + pitch }, at)).collect()
        });
        let changed = reading != self.rail_reading || hits.len() != self.rail_hits.len() || hits.iter().zip(&self.rail_hits).any(|(a, b)| a.1 != b.1 || !same_rect(&a.0, &b.0));
        if changed {
            if let Some(frame) = frame {
                InvalidateRect(self.window, &frame, 0);
            }
        }
        self.rail_reading = reading;
        self.rail_hits = hits;
        self.tip_rail();
    }

    /// A turn's tip on the rail: what the person said, and the start of the last answer to it.
    fn rail_tip(&self, at: usize) -> String {
        let Some(turn) = self.rail_turns.get(at) else { return String::new() };
        let end = self.rail_turns.get(at + 1).map_or(i32::MAX, |next| next.start);
        let answer = self.answers.iter().rev().find(|(place, _)| *place > turn.start && *place < end).map(|(_, text)| rail_words(text, 120)).unwrap_or_default();
        if answer.is_empty() { turn.prompt.clone() } else { format!("{}\n{answer}", turn.prompt) }
    }

    /// Each mark's tip, as one over its place in the window: passed on when the marks have moved, or an answer has come.
    unsafe fn tip_rail(&mut self) {
        let now: Vec<(RECT, usize)> = self.rail_hits.iter().map(|(area, at)| (*area, *at + 1000 * self.answers.len())).collect();
        if now.len() == self.rail_tips.len() && now.iter().zip(&self.rail_tips).all(|(a, b)| a.1 == b.1 && same_rect(&a.0, &b.0)) {
            return;
        }
        // (ids far from any window's handle, and from the waiting messages' buttons)
        let id = |at: usize| usize::MAX - 4096 - at;
        let mut tool: TTTOOLINFOW = std::mem::zeroed();
        tool.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        tool.hwnd = self.window;
        for at in 0..self.rail_tips.len() {
            tool.uId = id(at);
            SendMessageW(self.tooltip, TTM_DELTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
        }
        for (place, (area, turn)) in self.rail_hits.iter().enumerate() {
            let text = wide(&self.rail_tip(*turn));
            tool.uFlags = TTF_SUBCLASS;
            tool.uId = id(place);
            tool.rect = *area;
            tool.lpszText = text.as_ptr() as *mut u16;
            SendMessageW(self.tooltip, TTM_ADDTOOLW, 0, std::ptr::addr_of!(tool) as LPARAM);
        }
        self.rail_tips = now;
    }

    /// The rail's marks, as thin bars from its right side: the one being read longer and brighter, the one the pointer is on in between.
    unsafe fn paint_rail(&self, device: HDC) {
        let Some(frame) = self.rail else { return };
        let s = |value: i32| scale(self.dpi, value);
        let thick = s(2).max(2);
        let right = frame.right - s(ds::space::XS);
        for (area, at) in &self.rail_hits {
            let reading = *at == self.rail_reading;
            let hover = self.rail_hover == Some(*at);
            let length = s(if reading { RAIL_MARK_READ } else if hover { (RAIL_MARK + RAIL_MARK_READ) / 2 } else { RAIL_MARK });
            let top = (area.top + area.bottom - thick) / 2;
            let color = if reading { theme::text() } else if hover { theme::soft() } else { theme::faint() };
            rounded(device, RECT { left: right - length, top, right, bottom: top + thick }, thick / 2, Some(color), None);
        }
    }

    /// The view to a turn: its message near the top, as far as the conversation goes.
    unsafe fn jump_to_turn(&mut self, at: usize) {
        let Some(turn) = self.rail_turns.get(at) else { return };
        let edit = self.transcript;
        let mut point = POINT { x: 0, y: 0 };
        SendMessageW(edit, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(point) as WPARAM, turn.start as LPARAM);
        let mut position = POINT { x: 0, y: 0 };
        SendMessageW(edit, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        let bar = self.transcript_bar();
        let last = (bar.nMax - bar.nPage as i32 + 1).max(0);
        // (above its first line: the card's own room, and a little of the page over it)
        position.y = (position.y + point.y - scale(self.dpi, 40)).clamp(0, last);
        self.follow_end = position.y >= last;
        if !self.follow_end {
            self.scrolled_up_at = Some(std::time::Instant::now());
        }
        SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        self.place_thumb();
    }

    /// The pointer over the rail: the mark it is on is drawn so, and the hand says it can be pressed.
    unsafe fn rail_hover_at(&mut self, x: i32, y: i32) {
        let on = self.rail_hits.iter().find(|(area, _)| x >= area.left && x < area.right && y >= area.top && y < area.bottom).map(|(_, at)| *at);
        if on == self.rail_hover {
            return;
        }
        if self.rail_hover.is_none() {
            let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.window, dwHoverTime: 0 };
            TrackMouseEvent(&mut track);
        }
        self.rail_hover = on;
        if let Some(frame) = self.rail {
            InvalidateRect(self.window, &frame, 0);
        }
    }

    /// The thumb where the view now is; only what moved is drawn again.
    unsafe fn place_thumb(&mut self) {
        let bar = self.transcript_bar();
        let strip = self.thumb_strip();
        let (range, page) = (bar.nMax - bar.nMin + 1, bar.nPage as i32);
        let track = strip.bottom - strip.top;
        // (with all of it in view the control only disables its bar, and what it says of the range is left from when there was more)
        let mut state: SCROLLBARINFO = std::mem::zeroed();
        state.cbSize = std::mem::size_of::<SCROLLBARINFO>() as u32;
        let disabled = GetScrollBarInfo(self.transcript, -5, &mut state) != 0 && state.rgstate[0] & 1 != 0; // OBJID_VSCROLL, STATE_SYSTEM_UNAVAILABLE
        let thumb = (self.has_messages && !disabled && page > 0 && range > page && track > 0).then(|| {
            let height = thumb_height(track, page, range, scale(self.dpi, 32));
            let top = strip.top + thumb_offset(track - height, bar.nPos - bar.nMin, range - page);
            let width = scale(self.dpi, ds::size::THUMB);
            let left = (strip.left + strip.right - width) / 2;
            RECT { left, top, right: left + width, bottom: top + height }
        });
        let same = match (&self.thumb, &thumb) {
            (Some(a), Some(b)) => a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom,
            (None, None) => true,
            _ => false,
        };
        if !same {
            let moved = match (&self.thumb, &thumb) {
                // (with what its antialiased edge reaches outside it)
                (Some(a), Some(b)) => RECT { left: a.left.min(b.left) - 2, top: a.top.min(b.top) - 2, right: a.right.max(b.right) + 2, bottom: a.bottom.max(b.bottom) + 2 },
                _ => strip,
            };
            self.thumb = thumb;
            InvalidateRect(self.window, &moved, 0);
        }
        self.place_rail();
    }

    /// Dragging: the view follows the pointer.
    unsafe fn drag_thumb(&mut self, y: i32, grab: i32) {
        let bar = self.transcript_bar();
        let strip = self.thumb_strip();
        let (range, page) = (bar.nMax - bar.nMin + 1, bar.nPage as i32);
        let height = thumb_height(strip.bottom - strip.top, page, range, scale(self.dpi, 32));
        let room = (strip.bottom - strip.top - height).max(1);
        let at = (y - grab - strip.top).clamp(0, room);
        let position = POINT { x: 0, y: bar.nMin + (i64::from(at) * i64::from(range - page) / i64::from(room)) as i32 };
        // (the end gives the following view back, as scrolling to it does)
        self.follow_end = at == room;
        SendMessageW(self.transcript, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        self.place_thumb();
    }

    /// The line under the composer: the engine's state, how full the context is, how fast the last answer came.
    unsafe fn paint_status(&self, device: HDC) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let area = self.status_area;
        if let Some(url) = &self.link_hover {
            draw_text(device, url, area, self.fonts.small, theme::accent_hover(), ONE_LINE);
            return;
        }
        let middle = (area.top + area.bottom) / 2;
        let mut x = area.left + s(ds::space::S);
        // Only what needs saying: at work, the stop button and the line above the input say so, and when all is well nothing does. (The engine that goes on while the faster-writing data
        // is fetched says how far that has come, and then that it starts again.)
        let beside = if self.drafter_restart_waits {
            Some(drafter_restart_words())
        } else {
            Some(self.setup_status.as_str()).filter(|words| self.drafter_fetch.is_some() && !words.is_empty())
        };
        if !self.connected {
            let dot = s(ds::space::S);
            let color = if matches!(self.connection.as_str(), "not connected" | "no engine") { theme::bad() } else { theme::warn() };
            rounded(device, rect(x, middle - dot / 2, dot, dot), dot / 2, Some(color), None);
            x += dot + s(ds::space::S);
            draw_text(device, t(self.connection.as_str()), RECT { left: x, right: (area.right - s(190)).max(x), ..area }, self.fonts.small, theme::muted(), ONE_LINE);
        } else if let Some(words) = beside {
            let tall = area.bottom - area.top;
            icon::draw(device, "info", rect(x, area.top, s(ds::size::ICON), tall), s(ds::size::ICON_SMALL), theme::muted());
            draw_text(device, words, RECT { left: x + s(ds::size::RIGHT_INDENT), right: (area.right - s(190)).max(x), ..area }, self.fonts.small, theme::muted(), ONE_LINE);
        } else if self.web_seen {
            let tall = area.bottom - area.top;
            icon::draw(device, "info", rect(x, area.top, s(ds::size::ICON), tall), s(ds::size::ICON_SMALL), theme::warn());
            draw_text(device, t("Web content is here: network commands ask"), RECT { left: x + s(ds::size::RIGHT_INDENT), right: (area.right - s(190)).max(x), ..area }, self.fonts.small, theme::warn(), ONE_LINE);
        }
        let wide = area.right - area.left;
        let mut right = area.right - s(ds::space::S);
        let share = if self.window_tokens > 0 { (self.used as f64 / self.window_tokens as f64).clamp(0.0, 1.0) } else { 0.0 };
        if self.window_tokens > 0 && self.used > 0 {
            // How much of the conversation's room is used, in numbers, quiet until it runs out; a ring beside them once there is something to see in it.
            let color = if share > 0.85 { theme::bad() } else if share > 0.6 { theme::warn() } else { theme::faint() };
            if share >= 0.1 {
                let size = s(ds::size::ICON_SMALL);
                ring_with_arc(device, rect(right - size, middle - size / 2, size, size), -90.0, share as f32 * 360.0, theme::raised(), color, s(ds::space::HAIR) as f32);
                right -= size + s(ds::space::S);
            }
            if wide >= s(250) {
                let words = format!("{}K / {}K", self.used / 1000, self.window_tokens / 1000);
                let across = text_width(self.fonts.small, &words);
                draw_text(device, &words, RECT { left: right - across, right, ..area }, self.fonts.small, color, DT_RIGHT | ONE_LINE);
                right -= across + s(ds::space::M);
            }
        }
        if let Some(rate) = self.rate.filter(|_| wide >= s(300)) {
            let block = s(70);
            draw_text(device, &format!("{rate:.1} tok/s"), RECT { left: right - block, right, ..area }, self.fonts.small, theme::faint(), DT_RIGHT | ONE_LINE);
        }
    }

    /// The effort asked for, or, left at the default, what suits the provider. Sent nothing, the engine thinks at its highest: the model reasoned
    /// for over 9000 tokens (4 minutes at 40 tok/s) before a word on a small demo page, against 95 s at low.
    fn effort_to_send(&self) -> Option<String> {
        let local = self.settings.providers.iter().any(|provider| provider.name == self.settings.active && provider.kind == Kind::Embedded);
        self.settings.effort.clone().or_else(|| local.then(|| "low".to_string()))
    }

    fn efforts(&self) -> [(Option<&'static str>, &'static str); 6] {
        efforts_for(model_of_effort(&self.settings, &self.model))
    }

    unsafe fn send_effort(&self) {
        let effort = self.effort_to_send();
        let _ = self.agent.commands.send(Command::SetEffort(effort.clone()));
        self.every_agent(|live| live.effort = effort.clone());
    }

    /// Each chat's agent, at work or not: a chip is the app's, and a turn under way reads it before its next step.
    fn every_agent(&self, change: impl Fn(&mut dogmind_agent::agent::Live)) {
        for handle in std::iter::once(&self.agent).chain(self.parked.iter().map(|parked| &parked.agent)) {
            change(&mut handle.live.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        }
    }

    /// To every agent, as the settings have changed.
    unsafe fn apply_web(&self) {
        let web = web_settings(&self.settings);
        let _ = self.agent.commands.send(Command::SetWeb(web.clone()));
        for parked in &self.parked {
            let _ = parked.agent.commands.send(Command::SetWeb(web.clone()));
        }
    }

    unsafe fn set_panel(&mut self, panel: Panel) {
        // (there is nothing to go back to)
        if !matches!(self.leaving, Leaving::No) {
            return;
        }
        // What was typed on a page is kept on the way out of it; what cannot be, stays there to be put right.
        if self.modal_open && panel != self.panel && !self.leave_page() {
            return;
        }
        let dialog = self.is_dialog(panel);
        let closing = self.modal_open && !dialog;
        if dialog && !self.modal_open {
            self.open_modal();
        } else if closing {
            self.close_modal();
        }
        // Only its own panel writes in the document.
        SendMessageW(self.doc, EM_SETREADONLY, 1, 0);
        SendMessageW(self.doc, EM_SETBKGNDCOLOR, 0, theme::background() as LPARAM);
        // (a key is the only thing that is not shown)
        for field in self.fields {
            SendMessageW(field, EM_SETPASSWORDCHAR, 0, 0);
            InvalidateRect(field, std::ptr::null(), 0);
        }
        self.panel = panel;
        // (the files of one answer are listed on the Changes page that its card opened, and are not what the next opening of it lists)
        if panel != Panel::Diff && self.turn_view.take().is_some() {
            self.refresh_changes();
        }
        // (and the trajectory page is gone with the page that held it)
        if panel != Panel::Diff {
            self.trajectory = None;
        }
        // (the About page's button waits while a look is going on, and is not to stay that way on another page)
        EnableWindow(self.buttons[1], 1);
        self.panel_log.clear();
        self.panel_problem = false;
        self.panel_scroll = 0;
        self.machine_line.clear();
        self.refusal = None;
        self.list_rows.clear();
        self.progress = if self.setting_up { self.progress } else { None };
        for field in self.fields {
            field_cue(field, "");
        }
        match panel {
            Panel::Chat => {}
            Panel::Welcome => {
                self.panel_title = String::new();
                self.onboarding = true;
                self.update_toggles();
                set_button_style(self.toggle_display, ButtonKind::Chip, theme::background());
                set_text(self.buttons[0], t("Let's go"));
                self.set_button_kinds(&[ButtonKind::Primary]);
            }
            Panel::Setup => {
                self.panel_title = t("How should the model run?").into();
                self.onboarding = true;
                self.update_toggles();
                crate::machine::refresh_path();
                let straydog = engine::find(&self.settings);
                let codex = dogmind_agent::codex::find_executable();
                let model_ready = engine::is_set_up(&self.folder);
                let claude_key = crate::machine::fresh_variable("ANTHROPIC_API_KEY").is_some();
                // Only what bears on the choice.
                let this_pc = match (straydog.is_some(), model_ready) {
                    (false, _) => engine::not_found_short(),
                    (true, true) => "",
                    (true, false) => t("Needs an NVIDIA GPU. Downloads a model of about 75 GB"),
                };
                // Whether it can, which takes a look at the GPU: the row changes when that has been done.
                if let Some(straydog) = straydog.clone().filter(|_| !model_ready) {
                    let poster = self.poster.clone();
                    std::thread::spawn(move || poster.send(Note::Catalog(engine::load_catalog(&straydog))));
                }
                self.list_rows = vec![
                    ListRow { title: t("On this PC, with Straydog").into(), detail: this_pc.into(), active: false },
                    ListRow { title: t("Another computer, or an online service").into(), detail: t("Anything with an OpenAI-compatible API").into(), active: false },
                    ListRow { title: t("Codex, with a ChatGPT plan").into(), detail: if codex.is_some() { String::new() } else { t("Needs the Codex program from OpenAI, which is not on this PC").into() }, active: false },
                    ListRow { title: t("Claude, with an API key").into(), detail: if claude_key { String::new() } else { t("A key from console.anthropic.com, paid by use").into() }, active: false },
                ];
                set_button_style(self.toggle_display, ButtonKind::Chip, theme::background());
                set_text(self.buttons[0], t("Continue"));
                set_text(self.buttons[1], t("Skip for now"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Link]);
                self.fill_list();
                SendMessageW(self.list, LB_SETCURSEL, suggested_way(straydog.is_some(), model_ready, codex.is_some(), claude_key), 0);
            }
            Panel::SetupDetails => {
                match self.setup_kind {
                    Kind::Server => {
                        self.panel_title = t("Where is the server?").into();
                        self.labels = [t("Address").into(), t("Model").into(), t("API key").into(), String::new()];
                        field_cue(self.fields[1], t("The first it offers"));
                        field_cue(self.fields[2], t("If it needs one"));
                        set_text(self.fields[0], "http://");
                        set_text(self.fields[1], "");
                        set_text(self.fields[2], "");
                        SendMessageW(self.fields[2], EM_SETPASSWORDCHAR, 0x2022, 0);
                    }
                    Kind::Anthropic => {
                        self.panel_title = t("Your Claude API key").into();
                        self.labels = [t("API key").into(), t("Model").into(), String::new(), String::new()];
                        field_cue(self.fields[0], t("Kept in Windows Credential Manager"));
                        field_cue(self.fields[1], anthropic::DEFAULT_MODEL);
                        set_text(self.fields[0], "");
                        set_text(self.fields[1], "");
                        SendMessageW(self.fields[0], EM_SETPASSWORDCHAR, 0x2022, 0);
                        self.panel_log = if crate::machine::fresh_variable("ANTHROPIC_API_KEY").is_some() {
                            t("Found ANTHROPIC_API_KEY in the environment: leave this empty to use it.").into()
                        } else {
                            t("A key comes from console.anthropic.com, under API keys, and is paid by use: a Claude plan does not include it.").into()
                        };
                    }
                    _ => {
                        self.panel_title = "Codex".into();
                        crate::machine::refresh_path();
                        self.panel_log = if dogmind_agent::codex::find_executable().is_some() {
                            t("If you have not signed in yet, run  codex login  in a terminal first.").into()
                        } else {
                            t(CODEX_MISSING).into()
                        };
                    }
                }
                set_text(self.buttons[0], t("Continue"));
                set_text(self.buttons[1], t("Back"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::Help => {
                self.panel_title = t("Help").into();
                self.help_section = 0;
                self.show_help_section();
            }
            Panel::About => {
                self.panel_title = t("About").into();
                self.read_engine_version();
                set_text(self.buttons[0], t("Licenses"));
                self.set_button_kinds(&[ButtonKind::Link, ButtonKind::Secondary, ButtonKind::Link, ButtonKind::Link]);
                self.update_toggles();
                self.show_release_controls();
                self.tip(self.toggle_updates, t("Dogmind asks GitHub for the newest release every six hours, and when you bring the window back from the tray after an hour. It sends only its name and version. When there is a newer one, it says so on the main screen, downloads the setup in the background and checks it, and restarts into it only when you press Restart to update."));
            }
            Panel::General => {
                self.panel_title = t("Settings").into();
                self.update_toggles();
                set_button_style(self.toggle_display, ButtonKind::Select, theme::background());
                set_text(self.buttons[0], t("Open"));
                self.set_button_kinds(&[ButtonKind::Secondary]);
                set_text(self.setup_again, t("Run again"));
                set_text(self.remove_all, t("Remove..."));
            }
            Panel::Appearance => {
                self.panel_title = t("Settings").into();
                self.update_toggles();
            }
            Panel::Tools => {
                self.panel_title = t("Settings").into();
                self.update_toggles();
                let setting = self.settings.search_key.clone();
                self.show_key_setting(&setting);
                SendMessageW(self.fields[KEY_FIELD], EM_SETPASSWORDCHAR, 0x2022, 0);
                set_text(self.key_replace, t("Replace"));
                set_text(self.key_remove, t("Remove"));
                set_text(self.mcp_import, t("Import from Claude and Codex"));
                let servers = self.mcp.trust().len();
                while self.mcp_trust.len() < servers {
                    let id = ID_MCP_TRUST + self.mcp_trust.len() as isize;
                    self.mcp_trust.push(make_button(self.pane, id, "", ButtonKind::Toggle, theme::background()));
                }
                for toggle in &self.mcp_trust {
                    self.tip(*toggle, t("What this server returns no longer counts as web content, so later commands stop asking on its account. Its tools still ask, and in a session that already has web content they keep asking. What it returns is still marked as data."));
                }
                self.update_toggles();
            }
            Panel::ConfirmRemove => {
                self.panel_title = t("Remove this app from this PC?").into();
                let kept_too = self.model_in_the_data_folder().is_some();
                set_text(self.buttons[0], if kept_too { t("Remove all") } else { t("Remove") });
                set_text(self.buttons[1], if kept_too { t("Remove, keep the model") } else { t("Cancel") });
                set_text(self.buttons[2], t("Cancel"));
                if kept_too {
                    self.set_button_kinds(&[ButtonKind::Danger, ButtonKind::Secondary, ButtonKind::Secondary]);
                } else {
                    self.set_button_kinds(&[ButtonKind::Danger, ButtonKind::Secondary]);
                    show(self.buttons[2], false);
                }
            }
            Panel::ConfirmDelete => {
                self.panel_title = t("Delete this session?").into();
                set_text(self.buttons[0], t("Delete"));
                set_text(self.buttons[1], t("Keep it"));
                self.set_button_kinds(&[ButtonKind::Danger, ButtonKind::Secondary]);
            }
            Panel::Rename => {
                self.panel_title = t("Name this session").into();
                self.labels = [t("Name").into(), String::new(), String::new(), String::new()];
                let seed = self.rename_seed.take().unwrap_or_else(|| self.session_title.clone());
                set_text(self.fields[0], &seed);
                set_text(self.buttons[0], t("Save"));
                set_text(self.buttons[1], t("Cancel"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::Sessions => {
                self.panel_title = t("Open a session").into();
                set_text(self.buttons[0], t("Open"));
                set_text(self.buttons[1], t("Back"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
                let here = self.workspace.to_string_lossy().to_lowercase();
                let found: Vec<session::Summary> = self.all_sessions.iter().filter(|summary| summary.workspace.to_string_lossy().to_lowercase() == here).cloned().collect();
                self.resume_paths = found.iter().map(|summary| summary.path.clone()).collect();
                self.list_rows = found.iter().map(|summary| ListRow { title: summary.title.clone(), detail: tf("{0}  ·  {1} messages", &[&summary.created.get(..16).unwrap_or(""), &summary.messages]), active: self.current_session.as_ref() == Some(&summary.path) }).collect();
                if self.list_rows.is_empty() {
                    self.panel_log = t("No session in this folder yet.").into();
                }
            }
            Panel::Rewind => {
                self.panel_title = t("Go back to the beginning of").into();
                for (at, text) in [t("Files and chat"), t("Files only"), t("Chat only"), t("Back")].iter().enumerate() {
                    set_text(self.buttons[at], text);
                }
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary, ButtonKind::Secondary, ButtonKind::Secondary]);
                self.rewind_rows.clear();
                let mut changed: HashSet<PathBuf> = HashSet::new();
                for turn in self.turns.iter().rev() {
                    changed.extend(turn.files.iter().map(|saved| saved.path.clone()));
                    if turn.message.is_none() && turn.files.is_empty() {
                        continue;
                    }
                    self.rewind_rows.push(turn.id);
                    let files = match changed.len() {
                        0 => t("no file changed since").to_string(),
                        1 => t("1 file changed since").to_string(),
                        count => tf("{0} files changed since", &[&count]),
                    };
                    let chat = if turn.message.is_some() { "" } else { t("  ·  files only") };
                    self.list_rows.push(ListRow { title: turn.title.clone(), detail: format!("{}  ·  {files}{chat}", turn.at.get(..16).unwrap_or("")), active: false });
                }
                self.panel_log = t("Only the agent's own edits come back. What this overwrites is saved first.").into();
            }
            Panel::Memory => {
                self.panel_title = t("Memory").into();
                let memory_folder = self.folder.join("memory");
                self.memory_notes = dogmind_agent::memory::notes(&memory_folder, &self.workspace);
                while self.memory_remove.len() < self.memory_notes.len() {
                    let id = ID_MEMORY_REMOVE + self.memory_remove.len() as isize;
                    self.memory_remove.push(make_button(self.pane, id, "", ButtonKind::Secondary, theme::background()));
                }
                for button in &self.memory_remove {
                    set_text(*button, t("Remove"));
                }
                // (a file of notes that cannot be read is said so, and is not shown as none; and where what the model forgot is kept)
                let mut said: Vec<String> = dogmind_agent::memory::unreadable(&memory_folder, &self.workspace).into_iter().map(|(file, error)| tf("The notes in {0} could not be read ({1}). The file is left as it is.", &[&file.display(), &error])).collect();
                if self.memory_notes.is_empty() && said.is_empty() {
                    said.push(t("Nothing is kept yet. The model keeps what later sessions would want, and you can ask it to.").into());
                }
                let forgotten = dogmind_agent::memory::forgotten_file(&memory_folder);
                if forgotten.is_file() {
                    if dogmind_agent::conversation_key::is_sealed_text(&std::fs::read(&forgotten).unwrap_or_default()) {
                        // (encrypted: no editor shows it, so what it holds is here, the latest, for a note to be got back by hand)
                        match dogmind_agent::memory::forgotten_text(&memory_folder) {
                            Ok(text) => {
                                let latest: Vec<&str> = text.lines().filter(|line| line.starts_with("- ")).collect();
                                let shown: Vec<&str> = latest.iter().rev().take(10).rev().copied().collect();
                                said.push(tf("Notes the model forgot are kept, encrypted, in {0}. The latest:\n{1}", &[&forgotten.display(), &shown.join("\n")]));
                            }
                            Err(error) => said.push(tf("The notes the model forgot, in {0}, could not be read ({1}). The file is left as it is.", &[&forgotten.display(), &error])),
                        }
                    } else {
                        said.push(tf("Notes the model forgot are kept in {0}, so that you can get one back.", &[&forgotten.display()]));
                    }
                }
                self.panel_log = said.join("\n");
            }
            Panel::Privacy => {
                self.panel_title = t("Settings").into();
                let on = self.crypt.keystore.is_some();
                if let Some(keystore) = &self.crypt.keystore {
                    self.crypt.remember = keystore.is_remembered();
                }
                set_text(self.buttons[0], if on { t("Change passphrase...") } else { t("Encrypt conversations...") });
                set_text(self.buttons[1], t("New recovery code..."));
                set_text(self.buttons[2], t("Turn off encryption..."));
                self.set_button_kinds(&[if on { ButtonKind::Secondary } else { ButtonKind::Primary }, ButtonKind::Secondary, ButtonKind::Danger]);
                self.update_toggles();
            }
            Panel::CryptoOn => {
                self.panel_title = t("Encrypt conversations").into();
                self.crypt_fields_ready(2);
                self.crypt.remember = true;
                set_text(self.buttons[0], t("Encrypt"));
                set_text(self.buttons[1], t("Cancel"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
                self.update_toggles();
            }
            Panel::CryptoPassphrase => {
                self.panel_title = t("Change the passphrase").into();
                self.crypt_fields_ready(2);
                set_text(self.buttons[0], t("Change"));
                set_text(self.buttons[1], t("Cancel"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::CryptoCode => {
                self.panel_title = t("New recovery code").into();
                set_text(self.buttons[0], t("Make a new code"));
                set_text(self.buttons[1], t("Cancel"));
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::CryptoOff => {
                self.panel_title = t("Turn off encryption?").into();
                set_text(self.buttons[0], t("Turn off encryption"));
                set_text(self.buttons[1], t("Cancel"));
                self.set_button_kinds(&[ButtonKind::Danger, ButtonKind::Secondary]);
            }
            Panel::CryptoWork => {
                self.panel_title = t(match self.crypt.work.as_ref().map(|work| work.job) {
                    Some(CryptJob::On | CryptJob::FinishOn) => "Encrypting conversations",
                    Some(CryptJob::Off | CryptJob::FinishOff) => "Decrypting conversations",
                    None if self.crypt.code.is_some() => "Recovery code",
                    None => "Done",
                }).into();
                self.crypt_fields_ready(0);
                // (the code is shown to be read and copied, and is nowhere else once this page is gone)
                if let Some(code) = &self.crypt.code {
                    set_text(self.fields[0], code);
                    SendMessageW(self.fields[0], EM_SETREADONLY, 1, 0);
                }
                set_text(self.buttons[0], t("Done"));
                self.set_button_kinds(&[ButtonKind::Primary]);
            }
            Panel::Unlock => {
                self.panel_title = t("Unlock your conversations").into();
                self.crypt_fields_ready(if self.crypt.by_code { 3 } else { 1 });
                // (the code is typed as it was shown, so it is not hidden; the passphrases are)
                if self.crypt.by_code {
                    SendMessageW(self.fields[0], EM_SETPASSWORDCHAR, 0, 0);
                }
                set_text(self.buttons[0], t("Unlock"));
                set_text(self.buttons[1], if self.crypt.by_code { t("Use the passphrase instead") } else { t("Use the recovery code instead") });
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Link]);
                if self.crypt.damaged.is_some() {
                    show(self.buttons[0], false);
                    show(self.buttons[1], false);
                }
            }
            Panel::Skills => {
                self.panel_title = t("Skills").into();
                while self.skill_toggles.len() < self.skills.len() {
                    let id = ID_SKILL_TOGGLE + self.skill_toggles.len() as isize;
                    self.skill_toggles.push(make_button(self.pane, id, "", ButtonKind::Toggle, theme::background()));
                }
                self.panel_log = if self.skills.is_empty() { t("No skills yet.").into() } else { String::new() };
                self.update_toggles();
            }
            Panel::Instructions => {
                self.panel_title = t("Your instructions").into();
                let text = std::fs::read_to_string(self.folder.join("instructions.md")).unwrap_or_default();
                self.show_document(&text.replace('\n', "\r\n"));
                SendMessageW(self.doc, EM_SETREADONLY, 0, 0);
                SendMessageW(self.doc, EM_SETBKGNDCOLOR, 0, theme::surface() as LPARAM);
                set_text(self.buttons[0], t("Save"));
                self.set_button_kinds(&[ButtonKind::Primary]);
            }
            Panel::Diff if self.trajectory.is_some() => {
                self.panel_title = t("Trajectory").into();
                self.diff_rows.clear();
                self.diff_starts.clear();
                self.diff_file = None;
                self.diff_words = String::new();
            }
            Panel::Diff => {
                self.panel_title = t("Changes").into();
                self.diff_rows.clear();
                self.diff_starts.clear();
                self.diff_file = None;
                self.diff_words = t("Looking...").to_string();
                self.refresh_changes();
            }
            Panel::Permissions => {
                self.panel_title = t("What may always run here").into();
                set_text(self.buttons[0], t("Remove"));
                self.set_button_kinds(&[ButtonKind::Secondary]);
                self.update_toggles();
                let here = self.workspace.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
                self.shown_rules = rules::list(&self.folder.join("permissions.json")).into_iter()
                    .filter(|rule| rule.workspace.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase() == here).collect();
                self.list_rows = self.shown_rules.iter().map(|rule| ListRow { title: t(&rule.label()).to_string(), detail: if rule.exact { t("This exact command").into() } else if rule.prefix.is_empty() { t("Without asking").into() } else { t("Any command that starts so").into() }, active: false }).collect();
                self.panel_log = if self.shown_rules.is_empty() { t("No rules yet.").into() } else { String::new() };
                self.read_folders = read_folders::list(&read_folders::file_in(&self.folder));
                self.read_folder_places = read_folders::whereabouts_soon(&self.read_folders);
                while self.read_folder_remove.len() < self.read_folders.len() {
                    let id = ID_READ_FOLDER_REMOVE + self.read_folder_remove.len() as isize;
                    self.read_folder_remove.push(make_button(self.pane, id, "", ButtonKind::Secondary, theme::background()));
                }
                for button in &self.read_folder_remove {
                    set_text(*button, t("Remove"));
                }
                set_text(self.read_folder_add, t("Add a folder..."));
                set_text(self.read_folder_stop, t("Stop"));
            }
            Panel::Models => {
                self.panel_title = t("Settings").into();
                self.labels = [t("Where models are kept").into(), t("Graphics memory limit (GB)").into(), t("Longest conversation (tokens)").into(), String::new()];
                let folder = self.settings.models_folder.clone().map_or(String::new(), |folder| folder.display().to_string());
                set_text(self.fields[0], &folder);
                self.context_auto = None;
                set_text(self.fields[1], &settings::show_vram(self.settings.vram));
                set_text(self.fields[2], &self.settings.context.map_or(String::new(), |context| context.to_string()));
                self.update_drafter_chip();
                self.update_speed_chip();
                self.update_vision_chip();
                set_text(self.vision_delete, t("Delete"));
                set_text(self.buttons[1], t("Browse..."));
                set_text(self.buttons[2], if !self.onboarding { t("Connect again") } else if self.setting_up { t("Continue while it downloads") } else { t("Back") });
                set_text(self.buttons[3], t("Engine's log"));
                set_text(self.buttons[4], t("Dashboard"));
                self.show_setup_button();
                self.panel_log = if self.setting_up { self.setup_status.clone() } else { String::new() };
                if self.onboarding {
                    self.panel_title = t("Which model?").into();
                }
                match engine::find(&self.settings) {
                    Some(straydog) => {
                        self.list_rows.push(ListRow { title: t("Asking Straydog...").into(), detail: String::new(), active: false });
                        let poster = self.poster.clone();
                        std::thread::spawn(move || poster.send(Note::Catalog(engine::load_catalog(&straydog))));
                    }
                    None => self.panel_log = engine::not_found_words().into(),
                }
            }
            Panel::Providers => {
                self.panel_title = t("Settings").into();
                for (at, text) in [t("Use this"), t("Save"), t("New"), t("Delete")].iter().enumerate() {
                    set_text(self.buttons[at], text);
                }
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary, ButtonKind::Secondary, ButtonKind::Secondary]);
                set_text(self.key_replace, t("Replace"));
                set_text(self.key_remove, t("Remove"));
                SendMessageW(self.fields[KEY_FIELD], EM_SETPASSWORDCHAR, 0x2022, 0);
                self.fill_providers();
            }
            Panel::Start => {
                self.panel_title = t("Where shall we work?").into();
                self.start_folders = self.settings.workspaces.iter().filter(|folder| folder.is_dir()).cloned().collect();
                if self.onboarding && self.start_folders.is_empty() {
                    // (there is always somewhere to begin: made when it is chosen)
                    self.start_folders.push(self.first_folder());
                }
                if self.start_folders.is_empty() {
                    set_text(self.buttons[0], t("Choose a folder..."));
                    set_text(self.buttons[1], t("Back"));
                    self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
                } else {
                    set_text(self.buttons[0], if self.onboarding { t("Begin") } else { t("Start here") });
                    set_text(self.buttons[1], t("Choose a folder..."));
                    set_text(self.buttons[2], t("Back"));
                    self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary, ButtonKind::Secondary]);
                }
                self.list_rows = self.start_folders.iter().map(|folder| ListRow {
                    title: folder.file_name().map_or_else(|| folder.display().to_string(), |name| name.to_string_lossy().into_owned()),
                    detail: folder.display().to_string(),
                    active: false,
                }).collect();
            }
        }
        if !matches!(panel, Panel::Providers | Panel::Setup) {
            self.fill_list();
        }
        if (panel == Panel::Start && !self.start_folders.is_empty()) || (panel == Panel::Permissions && !self.shown_rules.is_empty()) || (panel == Panel::Sessions && !self.list_rows.is_empty()) {
            SendMessageW(self.list, LB_SETCURSEL, 0, 0);
        }
        self.layout();
        // A new page is drawn whole: a control that was told to draw while another was still over it has nothing left to draw, and shows what lay under it.
        RedrawWindow(self.window, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
        // A dialog begins at its list, from where the arrows go through it and Tab into what it shows; and when it goes, the focus goes back.
        if self.modal_open {
            let (rows, chosen) = self.nav_rows();
            SetFocus(match panel {
                Panel::Rename | Panel::CryptoOn | Panel::CryptoPassphrase => self.fields[0],
                Panel::Unlock if self.crypt.damaged.is_none() => self.fields[0],
                Panel::Sessions | Panel::Rewind if !self.list_rows.is_empty() => self.list,
                _ if rows.is_empty() => self.modal,
                _ => self.nav_buttons[chosen],
            });
            if panel == Panel::Rename {
                SendMessageW(self.fields[0], EM_SETSEL, 0, -1);
            }
            return;
        }
        if closing && !self.modal_return.is_null() && panel == Panel::Chat && IsWindowVisible(self.modal_return) != 0 && self.modal_return != self.window {
            SetFocus(self.modal_return);
            return;
        }
        // Where the keys are for: to a hidden composer they would go nowhere.
        let has_field = panel == Panel::SetupDetails && matches!(self.setup_kind, Kind::Server | Kind::Anthropic);
        let focus = match panel {
            Panel::Chat => self.composer,
            _ if has_field => self.fields[0],
            Panel::Setup | Panel::Start | Panel::Models if !self.list_rows.is_empty() => self.list,
            Panel::Diff => self.list,
            _ => self.window,
        };
        SetFocus(focus);
        if has_field {
            // (whole, to be typed over, but a server's address goes on after what it starts with)
            let all = !(panel == Panel::SetupDetails && self.setup_kind == Kind::Server);
            SendMessageW(focus, EM_SETSEL, if all { 0 } else { usize::MAX }, -1);
        }
    }

    /// One section of the help: what comes after a tab keeps to its column when it wraps.
    unsafe fn show_help_section(&self) {
        let sections = help_sections();
        let Some((_, body)) = sections.get(self.help_section) else { return };
        self.show_help(body);
    }

    unsafe fn show_help(&self, text: &str) {
        const COLUMN: i32 = 1800;
        set_text(self.doc, "");
        for line in text.lines() {
            let (words, format, paragraph) = if line.contains('\t') {
                let mut paragraph = para_format(10, 0, 0, 30);
                paragraph.mask |= PFM_TABSTOPS;
                paragraph.offset = COLUMN;
                paragraph.tab_count = 1;
                paragraph.tabs[0] = COLUMN;
                (line, char_format(theme::text(), 0, UI_FACE, 10, None), paragraph)
            } else {
                (line, char_format(theme::soft(), 0, UI_FACE, 10, None), para_format(10, 0, 0, 60))
            };
            SendMessageW(self.doc, EM_SETSEL, usize::MAX, -1);
            SendMessageW(self.doc, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
            SendMessageW(self.doc, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            let text = wide(&format!("{words}\r"));
            SendMessageW(self.doc, EM_REPLACESEL, 0, text.as_ptr() as LPARAM);
        }
        SendMessageW(self.doc, EM_SETSEL, 0, 0);
        SendMessageW(self.doc, WM_VSCROLL, SB_TOP as WPARAM, 0);
    }

    /// Plain text in the panels' document, with one column for what a tab starts.
    unsafe fn show_document(&self, text: &str) {
        set_text(self.doc, "");
        let format = char_format(theme::text(), 0, UI_FACE, 10, None);
        SendMessageW(self.doc, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
        set_text(self.doc, text);
        let mut paragraph = para_format(10, 0, 0, 0);
        paragraph.mask |= PFM_TABSTOPS;
        paragraph.tab_count = 1;
        paragraph.tabs[0] = 1800;
        SendMessageW(self.doc, EM_SETSEL, 0, -1);
        SendMessageW(self.doc, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
        SendMessageW(self.doc, EM_SETSEL, 0, 0);
    }

    /// One file's diff, coloured: runs of a kind of line go in together, so that a long diff takes no long.
    unsafe fn show_change(&mut self, at: usize) {
        const MOST_ROWS: usize = 30000;
        let Some(change) = self.changes.get(at) else { return };
        self.diff_rows = diffview::rows(&change.diff, &change.path, MOST_ROWS);
        self.diff_starts = diffview::change_starts(&self.diff_rows);
        self.diff_words = if self.diff_rows.is_empty() { t("No changes.").to_string() } else { String::new() };
        self.diff_file = Some(at);
        self.diff_scroll = 0;
        self.diff_across = 0;
        self.diff_chosen = None;
        // (a file that begins lower down is shown from its first change, with what stands before it)
        if let Some(first) = self.diff_starts.first() {
            self.diff_scroll = (*first as i32 - 3).max(0) * scale(self.dpi, ds::size::DIFF_ROW);
        }
        self.layout();
    }

    /// The trajectory page, on the Changes page's window, with every turn shut.
    unsafe fn show_trajectory(&mut self, summary: dogmind_agent::trajectory_report::Summary) {
        let open = std::collections::BTreeSet::new();
        let rows = crate::trajectory::rows(&summary, &open);
        self.trajectory = Some(TrajectoryPage { summary, open, rows });
        self.diff_scroll = 0;
        self.set_panel(Panel::Diff);
    }

    /// A turn's row opened or shut: the rows are made again.
    unsafe fn toggle_trajectory_turn(&mut self, turn: u64) {
        let Some(page) = &mut self.trajectory else { return };
        if !page.open.remove(&turn) {
            page.open.insert(turn);
        }
        page.rows = crate::trajectory::rows(&page.summary, &page.open);
        self.clamp_diff();
        InvalidateRect(self.diff_view, std::ptr::null(), 0);
    }

    /// How many rows the Changes page's window has: those of the diff, or of the trajectory's table.
    fn diff_len(&self) -> usize {
        self.trajectory.as_ref().map_or(self.diff_rows.len(), |page| page.rows.len())
    }

    /// How far the view can go, as the window and the file are now.
    unsafe fn diff_room(&self) -> i32 {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.diff_view, &mut client);
        (self.diff_len() as i32 * scale(self.dpi, ds::size::DIFF_ROW) - client.bottom).max(0)
    }

    unsafe fn clamp_diff(&mut self) {
        self.diff_scroll = self.diff_scroll.clamp(0, self.diff_room());
    }

    unsafe fn diff_to(&mut self, scroll: i32) {
        self.diff_scroll = scroll.clamp(0, self.diff_room());
        InvalidateRect(self.diff_view, std::ptr::null(), 0);
    }

    /// To the next change, and past the last of a file to the first of the next one.
    unsafe fn diff_jump(&mut self, forward: bool) {
        let row = scale(self.dpi, ds::size::DIFF_ROW);
        // (the one it went to last stands three rows under the top)
        let top = (self.diff_scroll / row + 3) as usize;
        match diffview::next_change(&self.diff_starts, top, forward) {
            Some(at) => self.diff_to((at as i32 - 3).max(0) * row),
            None => {
                let Some(file) = self.diff_file else { return };
                let next = if forward { file + 1 } else { file.wrapping_sub(1) };
                if next < self.changes.len() {
                    SendMessageW(self.list, LB_SETCURSEL, next, 0);
                    self.show_change(next);
                    if !forward {
                        if let Some(last) = self.diff_starts.last().copied() {
                            self.diff_to((last as i32 - 3).max(0) * row);
                        }
                    }
                }
            }
        }
    }

    /// The rows chosen, or the whole of the file, as text.
    unsafe fn copy_from_diff(&self) {
        let chosen = self.diff_chosen.map(|(a, b)| (a.min(b), a.max(b)));
        let text: Vec<String> = self.diff_rows.iter().enumerate().filter(|(at, row)| chosen.map_or(true, |(a, b)| *at >= a && *at <= b) && !matches!(row.kind, RowKind::Hunk | RowKind::Note)).map(|(_, row)| {
            let mark = match row.kind {
                RowKind::Added => "+",
                RowKind::Removed => "-",
                _ => " ",
            };
            format!("{mark}{}", row.text())
        }).collect();
        self.copy_to_clipboard(&text.join("\r\n"));
    }

    /// Sideways, as far as the longest line of the file wants and no further.
    unsafe fn diff_sideways(&mut self, steps: i32) {
        let room = child_area(self.diff_view, self.window).2;
        // (what goes before a line's words, as near as it matters here)
        let most = (diffview::longest(&self.diff_rows) as i32 * text_width(self.fonts.code, "0") + scale(self.dpi, 100) - room).max(0);
        self.diff_across = (self.diff_across + steps * 2 * scale(self.dpi, ds::space::XXL)).clamp(0, most);
        InvalidateRect(self.diff_view, std::ptr::null(), 0);
    }

    /// Up and down, or with Shift across what is too long for the window.
    unsafe fn diff_wheel(&mut self, wparam: WPARAM) {
        let notches = i32::from(((wparam >> 16) & 0xFFFF) as u16 as i16) / 120;
        if GetKeyState(VK_SHIFT as i32) < 0 {
            self.diff_sideways(-notches);
        } else {
            self.diff_to(self.diff_scroll - notches * 3 * scale(self.dpi, ds::size::DIFF_ROW));
        }
    }

    /// The trajectory's table: each column at its own place across, in the characters of the monospace face, the header kept at the top, the rows in view under it.
    unsafe fn paint_trajectory(&self, device: HDC, client: RECT) {
        use crate::trajectory::{column_starts, RowKind, COLUMNS};
        let Some(page) = &self.trajectory else { return };
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let row = s(ds::size::DIFF_ROW);
        let face = self.fonts.code;
        let character = text_width(face, "0").max(1);
        let (starts, notes) = column_starts();
        let bar = s(ds::space::M);
        let width = client.right - bar;
        SetBkMode(device, TRANSPARENT as _);
        let cell = |device: HDC, text: &str, column: usize, y: i32, color: u32| {
            let (left, room) = (starts[column] as i32 * character, COLUMNS[column].chars as i32 * character);
            let flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | if COLUMNS[column].right { DT_RIGHT } else { 0 };
            draw_text(device, text, RECT { left, right: left + room, top: y, bottom: y + row }, face, color, flags);
        };
        // (the header's row is the first, and stays where it is)
        let first = ((self.diff_scroll / row) as usize).max(1);
        let mut y = first as i32 * row - self.diff_scroll;
        for (at, line) in page.rows.iter().enumerate().skip(first) {
            if y >= client.bottom {
                break;
            }
            {
                let band = rect(0, y, width, row);
                let back = match line.kind {
                    RowKind::Turn { .. } if at % 2 == 1 => Some(theme::surface()),
                    RowKind::Total => Some(theme::raised()),
                    _ => None,
                };
                if let Some(color) = back {
                    let brush = CreateSolidBrush(color);
                    FillRect(device, &band, brush);
                    DeleteObject(brush);
                }
                let color = match line.kind {
                    RowKind::Turn { .. } | RowKind::Total => theme::text(),
                    _ => theme::muted(),
                };
                if let RowKind::Turn { open, .. } = line.kind {
                    icon::draw(device, if open { "chevron-down" } else { "chevron-right" }, rect(0, y, s(ds::size::ICON), row), s(ds::size::ICON_SMALL), theme::muted());
                }
                for (column, text) in line.cells.iter().enumerate() {
                    cell(device, text, column, y, color);
                }
                if !line.note.is_empty() {
                    draw_text(device, &line.note, RECT { left: notes as i32 * character, right: width, top: y, bottom: y + row }, face, theme::faint(), DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
                }
            }
            y += row;
        }
        // The header, over whatever has scrolled under it, with the line under it.
        let head = rect(0, 0, client.right, row);
        let brush = CreateSolidBrush(theme::background());
        FillRect(device, &head, brush);
        DeleteObject(brush);
        if let Some(line) = page.rows.first() {
            for (column, text) in line.cells.iter().enumerate() {
                cell(device, text, column, 0, theme::faint());
            }
        }
        let rule = rect(0, row - 1, client.right, 1);
        let brush = CreateSolidBrush(theme::line());
        FillRect(device, &rule, brush);
        DeleteObject(brush);
        // Where the view is in the whole of the table.
        let total = page.rows.len() as i32 * row;
        if total > client.bottom {
            let tall = thumb_height(client.bottom, client.bottom, total, s(ds::space::XXL));
            let at = thumb_offset(client.bottom - tall, self.diff_scroll, total - client.bottom);
            rounded(device, rect(client.right - s(ds::size::THUMB) - s(ds::size::THUMB) / 2, at, s(ds::size::THUMB), tall), s(ds::size::THUMB) / 2, Some(if self.diff_thumb_grab.is_some() { theme::muted() } else { theme::line() }), None);
        }
    }

    /// The window of the Changes page: only the rows in view are drawn, as long as a file may be.
    unsafe fn paint_diff(&self, device: HDC, client: RECT) {
        use crate::highlight::Kind;
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let brush = CreateSolidBrush(theme::background());
        FillRect(device, &client, brush);
        DeleteObject(brush);
        if self.trajectory.is_some() {
            return self.paint_trajectory(device, client);
        }
        if self.diff_rows.is_empty() {
            draw_text(device, &self.diff_words, RECT { left: s(ds::space::L), right: client.right - s(ds::space::L), top: 0, bottom: s(ds::size::DIFF_HEAD) }, self.fonts.body, theme::muted(), ONE_LINE);
            return;
        }
        let row = s(ds::size::DIFF_ROW);
        let bar = s(ds::space::M);
        let width = client.right - bar;
        let previous = SelectObject(device, self.fonts.code);
        let mut size = SIZE { cx: 0, cy: 0 };
        let zero = wide("0");
        GetTextExtentPoint32W(device, zero.as_ptr(), 1, &mut size);
        let digits = diffview::number_width(&self.diff_rows) as i32 * size.cx;
        let pad = s(ds::space::S);
        let old_right = pad + digits;
        let new_right = old_right + pad + digits;
        let mark_x = new_right + pad;
        let text_x = mark_x + size.cx + pad;
        let chosen = self.diff_chosen.map(|(a, b)| (a.min(b), a.max(b)));
        let first = (self.diff_scroll / row) as usize;
        let mut y = first as i32 * row - self.diff_scroll;
        let colour = |kind: Kind| match kind {
            Kind::Plain => theme::code(),
            Kind::Keyword => theme::accent_hover(),
            Kind::Text => theme::good(),
            Kind::Comment => theme::muted(),
            Kind::Number => theme::warn(),
        };
        SetBkMode(device, TRANSPARENT as _);
        for (at, line) in self.diff_rows.iter().enumerate().skip(first) {
            if y >= client.bottom {
                break;
            }
            let band = rect(0, y, width, row);
            let back = match line.kind {
                RowKind::Added => Some(theme::added_back()),
                RowKind::Removed => Some(theme::removed_back()),
                _ => None,
            };
            let lit = chosen.is_some_and(|(a, b)| at >= a && at <= b);
            if let Some(color) = if lit { Some(theme::raised()) } else { back } {
                let brush = CreateSolidBrush(color);
                FillRect(device, &band, brush);
                DeleteObject(brush);
            }
            match line.kind {
                RowKind::Hunk => {
                    // Where lines were passed over: a thin rule, with what the diff says of where it is now.
                    let rule = rect(0, y + row / 2, width, 1);
                    let brush = CreateSolidBrush(theme::line());
                    FillRect(device, &rule, brush);
                    DeleteObject(brush);
                    let words = line.text();
                    if !words.is_empty() {
                        let mut across = SIZE { cx: 0, cy: 0 };
                        let units: Vec<u16> = words.encode_utf16().collect();
                        GetTextExtentPoint32W(device, units.as_ptr(), units.len() as i32, &mut across);
                        let place = rect(text_x - pad, y, (across.cx + 2 * pad).min(width - text_x), row);
                        let brush = CreateSolidBrush(theme::background());
                        FillRect(device, &place, brush);
                        DeleteObject(brush);
                        draw_text(device, &words, RECT { left: text_x, right: width, top: y, bottom: y + row }, self.fonts.code, theme::muted(), DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
                    }
                }
                RowKind::Note => draw_text(device, &line.text(), RECT { left: text_x, right: width, top: y, bottom: y + row }, self.fonts.code, theme::muted(), DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS),
                _ => {
                    for (number, right) in [(line.old, old_right), (line.new, new_right)] {
                        if let Some(number) = number {
                            draw_text(device, &number.to_string(), RECT { left: right - digits, right, top: y, bottom: y + row }, self.fonts.code, theme::faint(), DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
                        }
                    }
                    let (mark, mark_color) = match line.kind {
                        RowKind::Added => ("+", theme::good()),
                        RowKind::Removed => ("−", theme::bad()),
                        _ => ("", theme::faint()),
                    };
                    draw_text(device, mark, RECT { left: mark_x, right: text_x, top: y, bottom: y + row }, self.fonts.code, mark_color, DT_SINGLELINE | DT_VCENTER);
                    // What is pushed aside sideways stays behind the numbers.
                    let clip = CreateRectRgn(text_x, y.max(0), width, (y + row).min(client.bottom));
                    SelectClipRgn(device, clip);
                    let mut x = text_x - self.diff_across;
                    for (text, kind) in &line.runs {
                        if x > width {
                            break;
                        }
                        let units: Vec<u16> = text.encode_utf16().collect();
                        let mut across = SIZE { cx: 0, cy: 0 };
                        GetTextExtentPoint32W(device, units.as_ptr(), units.len() as i32, &mut across);
                        if x + across.cx >= text_x {
                            draw_text(device, text, RECT { left: x, right: x + across.cx + s(ds::space::XS), top: y, bottom: y + row }, self.fonts.code, colour(*kind), DT_SINGLELINE | DT_VCENTER);
                        }
                        x += across.cx;
                    }
                    SelectClipRgn(device, std::ptr::null_mut());
                    DeleteObject(clip);
                }
            }
            y += row;
        }
        SelectObject(device, previous);
        // Where the changes are in the whole of the file, and where the view is.
        let total = self.diff_rows.len() as i32 * row;
        let track = client.bottom;
        if total > track {
            let strip = client.right - bar;
            for (at, line) in self.diff_rows.iter().enumerate() {
                let color = match line.kind {
                    RowKind::Added => theme::good(),
                    RowKind::Removed => theme::bad(),
                    _ => continue,
                };
                let tick = (i64::from(at as i32 * row) * i64::from(track) / i64::from(total)) as i32;
                let area = rect(strip + s(ds::space::XS), tick, s(ds::space::XS), s(ds::space::HAIR));
                let brush = CreateSolidBrush(color);
                FillRect(device, &area, brush);
                DeleteObject(brush);
            }
            let tall = thumb_height(track, track, total, s(ds::space::XXL));
            let at = thumb_offset(track - tall, self.diff_scroll, total - track);
            rounded(device, rect(client.right - s(ds::size::THUMB) - s(ds::size::THUMB) / 2, at, s(ds::size::THUMB), tall), s(ds::size::THUMB) / 2, Some(if self.diff_thumb_grab.is_some() { theme::muted() } else { theme::line() }), None);
        }
    }

    /// The first button of the models panel: to start a setup, or to end the one that is running.
    unsafe fn show_setup_button(&self) {
        let cancelling = self.setup_cancel.load(Ordering::Relaxed);
        set_text(self.buttons[0], if cancelling && self.setting_up { t("Cancelling...") } else if self.setting_up { t("Cancel download") } else { t("Download / verify") });
        self.set_button_kinds(&[if self.setting_up { ButtonKind::Secondary } else { ButtonKind::Primary }, ButtonKind::Secondary, ButtonKind::Secondary]);
        EnableWindow(self.buttons[0], i32::from(!(cancelling && self.setting_up) && self.refusal.is_none() && engine::find(&self.settings).is_some()));
    }

    /// What a running setup says goes where it can be seen, whichever panel is open.
    unsafe fn show_setup_status(&mut self, text: String) {
        self.setup_status = text.clone();
        if matches!(self.panel, Panel::Models | Panel::Setup) {
            self.say_in_panel(&text);
        } else if self.drafter_fetch.is_some() {
            // (the engine's own state stays what it is: this goes beside it, on the status line)
            self.update_status();
        } else {
            self.connection = text;
            self.update_status();
        }
    }

    /// The words of the chosen model's licence on its link, before its download is asked for.
    unsafe fn update_licence_link(&self) {
        let line = self.list_selection().and_then(|at| self.catalog.get(at)).and_then(engine::CatalogEntry::licence_line);
        set_text(self.licence_link, line.as_deref().unwrap_or(""));
    }

    /// The context field follows the chosen model's largest, until something else is typed there.
    unsafe fn offer_context(&mut self, at: Option<usize>) {
        let Some(context) = at.and_then(|at| self.catalog.get(at)).and_then(|entry| entry.context) else { return };
        let typed = get_text(self.fields[2]);
        let typed = typed.trim();
        let untouched = typed.is_empty() || typed.parse::<u64>().ok() == self.context_auto;
        if untouched {
            set_text(self.fields[2], &context.to_string());
            self.context_auto = Some(context);
        }
    }

    unsafe fn update_toggles(&self) {
        for (toggle, on) in [(self.toggle_thinking, self.settings.show_thinking), (self.toggle_compact, self.settings.compact_tools), (self.toggle_safe, self.settings.safe_commands), (self.toggle_web, self.settings.web_tools), (self.toggle_updates, self.settings.check_for_updates), (self.toggle_remember, self.crypt.remember)] {
            set_button_style(toggle, if on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        }
        for (at, (segment, (known, name))) in self.theme_segments.iter().zip(THEMES).enumerate() {
            set_text(*segment, t(if at == 0 { "System" } else { name }));
            set_button_style(*segment, if self.settings.theme == known { ButtonKind::SegmentOn } else { ButtonKind::Segment }, theme::surface());
        }
        for (segment, (percent, name)) in self.size_segments.iter().zip(TEXT_SIZES) {
            set_text(*segment, t(name));
            set_button_style(*segment, if self.settings.text_percent() == percent { ButtonKind::SegmentOn } else { ButtonKind::Segment }, theme::surface());
        }
        for (toggle, (_, on)) in self.mcp_trust.iter().zip(self.mcp.trust()) {
            set_button_style(*toggle, if on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        }
        for (toggle, skill) in self.skill_toggles.iter().zip(&self.skills) {
            set_button_style(*toggle, if self.skill_enabled(&skill.name) { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        }
        let mode = self.current_mode();
        set_text(self.mode_select, t(MODES.iter().find(|(known, _)| *known == mode).map_or("", |(_, name)| name)));
        set_text(self.toggle_language, match self.settings.language.as_str() {
            "ja" => t("Japanese, always"),
            "en" => t("English, always"),
            _ => t(auto_answer_words()),
        });
        let display = DISPLAY_LANGUAGES.iter().find(|(known, _)| *known == self.settings.display_language).map_or("As Windows is set", |(_, name)| name);
        // (on the first screens, for someone who may not read what it is set to)
        set_text(self.toggle_display, &if matches!(self.panel, Panel::Setup | Panel::Welcome) { format!("globe|{}", t(display)) } else { t(display).to_string() });
    }

    /// The way chosen on the first screen.
    unsafe fn setup_choose(&mut self) {
        let Some(at) = self.list_selection() else {
            self.problem_in_panel(t("Choose one of these."));
            return;
        };
        self.setup_kind = [Kind::Embedded, Kind::Server, Kind::Codex, Kind::Anthropic][at.min(3)];
        if self.setup_kind == Kind::Embedded {
            if engine::find(&self.settings).is_none() {
                self.problem_in_panel(engine::not_found_words());
                return;
            }
            if self.settings.models_folder.is_none() {
                self.settings.models_folder = Some(self.folder.join("models"));
            }
            self.settings.use_provider(Kind::Embedded, "", "", "");
            self.save();
            if engine::is_set_up(&self.folder) {
                // (it starts when something is written, as it does after any start of the app)
                self.engine_waits = true;
                self.connection = "starts when you write".into();
                self.set_panel(Panel::Start);
            } else {
                self.set_panel(Panel::Models);
            }
        } else {
            self.set_panel(Panel::SetupDetails);
        }
    }

    /// The details of a server, of Codex or of Claude are in: on to the workspace, connecting on the way.
    unsafe fn setup_details_done(&mut self) {
        // (no Codex: a folder and an empty conversation later is no place to find that out)
        if self.setup_kind == Kind::Codex {
            crate::machine::refresh_path();
            if dogmind_agent::codex::find_executable().is_none() {
                self.problem_in_panel(t(CODEX_MISSING));
                return;
            }
        }
        let field = |at: usize| get_text(self.fields[at]).trim().to_string();
        let (url, model, typed) = match self.setup_kind {
            Kind::Server => (field(0), field(1), field(2)),
            Kind::Anthropic => (String::new(), field(1), field(0)),
            _ => (String::new(), String::new(), String::new()),
        };
        let settings_now: Vec<&str> = self.settings.providers.iter().map(|provider| provider.api_key.as_str()).chain([self.settings.search_key.as_str()]).collect();
        let target = settings::new_key_target(if self.setup_kind == Kind::Anthropic { "Claude" } else { "Server" }, &settings_now);
        let key = if !typed.is_empty() {
            format!("{}{target}", settings::CREDENTIAL)
        } else if self.setup_kind == Kind::Anthropic && crate::machine::fresh_variable("ANTHROPIC_API_KEY").is_some() {
            "env:ANTHROPIC_API_KEY".to_string()
        } else {
            String::new()
        };
        let candidate = ProviderConfig { name: "x".into(), kind: self.setup_kind, base_url: url.clone(), model: model.clone(), api_key: key.clone() };
        if let Some(problem) = candidate.problem() {
            self.problem_in_panel(&problem);
            return;
        }
        if !typed.is_empty() {
            if let Err(error) = crate::keys::store().write(&target, &typed) {
                self.problem_in_panel(&error);
                return;
            }
        }
        self.settings.use_provider(self.setup_kind, &url, &model, &key);
        self.save();
        self.connect();
        self.set_panel(Panel::Start);
    }

    /// The tray's icon, whose messages come to the window. Called again if the taskbar comes back after Explorer restarts.
    unsafe fn tray_add(&self) {
        let mut data = self.tray_data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = window_icon(GetSystemMetrics(SM_CXSMICON));
        for (slot, unit) in data.szTip.iter_mut().zip("Dogmind".encode_utf16()) {
            *slot = unit;
        }
        Shell_NotifyIconW(NIM_ADD, &data);
        DestroyIcon(data.hIcon);
    }

    unsafe fn tray_data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.window;
        data.uID = 1;
        data
    }

    /// Where the window is, for the next time. Nowhere on a screen is nowhere to come back to.
    unsafe fn remember_place(&mut self) {
        let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(self.window, &mut placement) == 0 {
            return;
        }
        let area = placement.rcNormalPosition;
        if !MonitorFromRect(&area, MONITOR_DEFAULTTONULL).is_null() {
            self.settings.window = Some([area.left, area.top, area.right, area.bottom]);
            self.settings.maximized = placement.showCmd == SW_SHOWMAXIMIZED as u32;
        }
    }

    /// The taskbar button flashes: once. For what a turn that nobody sent asks, or says by ending, it goes on until the window has been looked at: when it came, no one was there.
    unsafe fn flash_for_attention(&self, nobody_sent_the_turn: bool) {
        // (a debug build told to by a test writes down that it did: the window is off the screen, and no one sees the button)
        if cfg!(debug_assertions) {
            if let Some(path) = std::env::var_os("DOGMIND_TEST_TRAYLOG") {
                use std::io::Write as _;
                if let std::result::Result::Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(file, "flash | {}", if nobody_sent_the_turn { "until it is looked at" } else { "once" });
                }
            }
        }
        if nobody_sent_the_turn {
            let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: self.window, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: u32::MAX, dwTimeout: 0 };
            FlashWindowEx(&info);
        } else {
            FlashWindow(self.window, 1);
        }
    }

    /// A word from the tray, only while the window is out of sight.
    unsafe fn tray_note(&self, title: &str, words: &str) {
        if IsWindowVisible(self.window) != 0 {
            return;
        }
        // (a debug build told to by a test writes down what the balloon says: off the screen, no one sees it)
        if cfg!(debug_assertions) {
            if let Some(path) = std::env::var_os("DOGMIND_TEST_TRAYLOG") {
                use std::io::Write as _;
                if let std::result::Result::Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(file, "{title} | {words}");
                }
            }
        }
        let mut data = self.tray_data();
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_NONE;
        for (slot, unit) in data.szInfoTitle.iter_mut().take(63).zip(title.encode_utf16()) {
            *slot = unit;
        }
        for (slot, unit) in data.szInfo.iter_mut().take(255).zip(words.encode_utf16()) {
            *slot = unit;
        }
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }

    /// The balloon of the tray for a version: once, and only while the window is hidden (`notice::balloon_due`). What the main screen has said to an open window counts as told.
    unsafe fn tell_the_tray_of(&mut self, version: &str, setup_is_here: bool) {
        let hidden = IsWindowVisible(self.window) == 0;
        if notice::balloon_due(version, hidden, self.tray_told_release.as_deref(), self.settings.dismissed_update.as_deref()) {
            let words = if setup_is_here { tf("Dogmind {0} is ready to install. Open the app to update.", &[&version]) } else { tf("Dogmind {0} is available. Open the app to update.", &[&version]) };
            self.tray_note(t("A new version of Dogmind is here"), &words);
        }
        if updates::is_to_be_told(version, self.settings.dismissed_update.as_deref()) {
            self.tray_told_release = Some(version.to_string());
        }
    }

    /// Whether a page that has the notice is the one shown, with the notice on it.
    fn notice_is_up(&self) -> bool {
        notice::is_on_page(self.panel == Panel::Chat, self.panel == Panel::Diff, self.onboarding)
    }

    /// Back from the tray. A look at GitHub an hour old is made again, so that a release that came out while the window was hidden shows on the main
    /// screen as it comes back, and not only on the About page.
    unsafe fn tray_open(&mut self) {
        // (a test's copy shows itself where it is, and takes no one's focus)
        if crate::machine::kept_from_the_real_machine() {
            ShowWindow(self.window, SW_SHOWNA);
        } else {
            ShowWindow(self.window, SW_SHOW);
            if IsIconic(self.window) != 0 {
                ShowWindow(self.window, SW_RESTORE);
            }
            SetForegroundWindow(self.window);
            SetFocus(if self.panel == Panel::Chat && self.pending.is_none() { self.composer } else { self.window });
        }
        self.look_for_release_after(updates::ON_RETURN);
    }

    unsafe fn tray_menu(&mut self) {
        let menu = CreatePopupMenu();
        let (open, quit) = (wide(t("Open Dogmind")), wide(t("Quit Dogmind")));
        AppendMenuW(menu, MF_STRING, M_TRAY_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, M_TRAY_QUIT, quit.as_ptr());
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        // Without this the menu stays when the person clicks elsewhere.
        SetForegroundWindow(self.window);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, at.x, at.y, 0, self.window, std::ptr::null());
        PostMessageW(self.window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen as usize {
            M_TRAY_OPEN => self.tray_open(),
            M_TRAY_QUIT => {
                DestroyWindow(self.window);
            }
            _ => {}
        }
    }

    /// Cut, copy and paste on the right button: RichEdit has no menu of its own. `lparam`: where, in the control.
    unsafe fn edit_menu(&mut self, edit: HWND, lparam: LPARAM, can_change: bool) {
        let mut range = CharRange { first: 0, last: 0 };
        SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
        let selected = range.first != range.last;
        let menu = CreatePopupMenu();
        let grayed = |on: bool| MF_STRING | if on { 0 } else { MF_GRAYED };
        let (cut, copy, paste, all) = (wide(t("Cut")), wide(t("Copy")), wide(t("Paste")), wide(t("Select all")));
        if can_change {
            AppendMenuW(menu, grayed(selected), M_EDIT_CUT, cut.as_ptr());
        }
        AppendMenuW(menu, grayed(selected), M_EDIT_COPY, copy.as_ptr());
        if can_change {
            let picture = edit == self.composer && attach::paste_kind(IsClipboardFormatAvailable(15) != 0, false, IsClipboardFormatAvailable(2) != 0) != attach::Paste::Nothing;
            AppendMenuW(menu, grayed(picture || SendMessageW(edit, 0x0400 + 50, 0, 0) != 0), M_EDIT_PASTE, paste.as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, M_EDIT_ALL, all.as_ptr());
        let mut at = POINT { x: (lparam & 0xFFFF) as i16 as i32, y: ((lparam >> 16) & 0xFFFF) as i16 as i32 };
        ClientToScreen(edit, &mut at);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN, at.x, at.y, 0, self.window, std::ptr::null());
        DestroyMenu(menu);
        match chosen as usize {
            M_EDIT_CUT => {
                SendMessageW(edit, WM_CUT, 0, 0);
            }
            M_EDIT_COPY => {
                SendMessageW(edit, WM_COPY, 0, 0);
            }
            M_EDIT_PASTE => {
                if edit != self.composer || !self.paste_pictures() {
                    SendMessageW(edit, WM_PASTE, 0, 0);
                }
            }
            M_EDIT_ALL => {
                SendMessageW(edit, EM_SETSEL, 0, -1);
            }
            _ => {}
        }
    }

    /// The composer has gained or lost a line: its card follows.
    unsafe fn composer_grow(&mut self) {
        let lines = SendMessageW(self.composer, EM_GETLINECOUNT, 0, 0) as i32;
        if lines != self.composer_lines {
            self.composer_lines = lines;
            self.layout();
            SendMessageW(self.composer, EM_SCROLLCARET, 0, 0);
        }
    }

    /// Where this session works, as a list: the folders used before and a way to any other. Beside the chip, on the side with room.
    unsafe fn folder_menu(&mut self) {
        let mut items: Vec<(usize, String, bool)> = Vec::new();
        for (at, folder) in self.settings.workspaces.iter().filter(|folder| folder.is_dir()).take(FOLDERS_SHOWN).enumerate() {
            let name = folder.file_name().map_or_else(|| folder.display().to_string(), |name| name.to_string_lossy().into_owned());
            items.push((M_FOLDER_RECENT + at, format!("{name}\t{}", folder.display()), *folder == self.workspace));
        }
        if !items.is_empty() {
            items.push((0, String::new(), false));
        }
        items.push((M_FOLDER_CHOOSE, t("Choose a folder...").to_string(), false));
        if self.has_messages {
            self.popup(self.folder_chip, &items);
        } else {
            let mut area: RECT = std::mem::zeroed();
            GetWindowRect(self.folder_chip, &mut area);
            self.popup_at(area.left, area.bottom + 2, TPM_TOPALIGN, &items);
        }
    }

    /// A new session in this folder. The folder it is in already stays as it is.
    unsafe fn go_to_folder(&mut self, folder: PathBuf) {
        if folder != self.workspace {
            self.start_session(folder);
        }
    }

    /// A new conversation, straight away, in the folder of the last one. Clicking the folder in the header chooses another.
    unsafe fn new_session(&mut self) {
        let on_screen = Some(self.workspace.clone()).filter(|folder| folder.is_dir());
        match on_screen.or_else(|| self.settings.workspace.clone().filter(|folder| folder.is_dir())) {
            Some(folder) => self.start_session(folder),
            None => self.finish_onboarding(),
        }
    }

    /// The first setup ends on the main screen. A session needs a folder, so it gets one to begin in, which the header changes.
    unsafe fn finish_onboarding(&mut self) {
        let chosen = self.settings.workspace.clone().filter(|folder| folder.is_dir());
        let folder = chosen.unwrap_or_else(|| self.first_folder());
        let _ = std::fs::create_dir_all(&folder);
        self.start_session(folder);
        // Skipped: what is written next says what is missing, and does not wait for a connection no one asked for.
        if !self.connected && !self.setting_up && self.engine.is_none() {
            if self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded) {
                self.engine_waits = true;
                self.connection = "starts when you write".into();
                self.update_status();
            } else {
                self.connect_failed = true;
            }
        }
    }

    /// An installer wants to replace these files. It closes as the tray's Quit does, unless it is working: then it stays, comes forward and says so, and the installer waits.
    /// Work that a restart would cut short: a turn, a setup, a session working in the background, a folder being opened to commands.
    fn at_work(&self) -> bool {
        self.running || self.setting_up || !self.parked.is_empty() || !self.waiting.is_empty() || self.read_folder_work.is_some() || self.pending.is_some()
    }

    /// A new build of the app or of its engine has come in over the files this copy runs from (`package.py` puts it beside the one in use, which
    /// Windows lets it rename): a row above the input asks whether to restart into it, and the tray says so if the window is out of sight. Nothing
    /// restarts unless the person says so. A file still being written is not taken for one: the same new size and time twice in a row is.
    unsafe fn look_for_update(&mut self) {
        // (the setup of a release, fetched and checked: it restarts when the work is done, if the person said so)
        if self.restart_when_done && !self.update_offer && self.ready_setup().is_some() {
            if !self.at_work() {
                self.hand_over_to_setup();
            }
            return;
        }
        let now: Vec<Option<(std::time::SystemTime, u64)>> = self.builds.iter().map(|(path, _)| stamp(path)).collect();
        let changed = self.builds.iter().zip(&now).any(|((_, was), is)| is.is_some() && is != was);
        if !changed || now.iter().any(Option::is_none) {
            self.update_seen = None;
            return;
        }
        if self.update_seen.as_ref() != Some(&now) {
            self.update_seen = Some(now);
            return;
        }
        if self.restart_when_done {
            if !self.at_work() {
                self.restart_into_update();
            }
            return;
        }
        if self.update_offer || self.update_declined.as_ref() == Some(&now) {
            return;
        }
        self.update_offer = true;
        self.tray_note(t("A new version of Dogmind is here"), t("Open the app to restart into it."));
        self.layout();
    }

    /// The offer's button: restart now, or, with work going on, when it is done; pressed again then, not after all.
    unsafe fn update_offer_pressed(&mut self) {
        if self.checking_setup {
            return;
        }
        if self.restart_when_done {
            self.restart_when_done = false;
        } else if self.at_work() {
            self.restart_when_done = true;
        } else {
            self.restart_into_newest();
            return;
        }
        self.layout();
    }

    /// Restart now: into the build that has come in over this one, or, if there is none, through the installer with the setup that was fetched.
    unsafe fn restart_into_newest(&mut self) {
        if self.update_offer || self.ready_setup().is_none() {
            self.restart_into_update();
        } else {
            self.hand_over_to_setup();
        }
    }

    /// The setup of a newer release fetched and checked, on a copy an installer put where it is.
    fn ready_setup(&self) -> Option<&updates::Ready> {
        match &self.release_update {
            ReleaseUpdate::Ready(ready) if self.install_folder.is_some() => Some(ready),
            _ => None,
        }
    }

    /// The notice of a newer version that the main screen has now, if there is one (see `notice::notice`).
    fn notice(&self) -> Option<notice::Notice> {
        let stage = match &self.release_update {
            ReleaseUpdate::None => notice::Stage::Nothing,
            ReleaseUpdate::Fetching { version, done, total, .. } => notice::Stage::Fetching { version, percent: done * 100 / (*total).max(1) },
            ReleaseUpdate::Ready(ready) => notice::Stage::Ready { version: &ready.version },
        };
        notice::notice(&notice::Situation {
            offered: self.release_offer.as_ref().map(|release| release.version.as_str()),
            stage,
            setup_runs: self.ready_setup().is_some(),
            dismissed: self.settings.dismissed_update.as_deref(),
            ready_closed: self.release_ready_closed,
            new_build: self.update_offer,
            restart_when_done: self.restart_when_done,
            checking: self.checking_setup,
        })
    }

    /// How high the notice is.
    fn notice_height(&self) -> i32 {
        scale(self.dpi, ds::size::ICON_BUTTON + 2 * ds::space::S)
    }

    /// The notice as a card of its own, across `width` from `left`, at `top`: what it says, its one button, and Later. The room it takes, with the gap under it.
    unsafe fn place_notice(&mut self, shapes: &mut Vec<Shape>, notice: &notice::Notice, left: i32, width: i32, top: i32) -> i32 {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let tall = self.notice_height();
        shapes.push(Shape::Round { area: rect(left, top, width, tall), radius: s(ds::radius::CARD), fill: Some(theme::surface()), border: Some(theme::line()) });
        let (words, label) = notice_text(notice);
        // (one width for every state the button goes through: one that says how far the fetch has come does not move as the number grows)
        let widest = [t("Update now (Dogmind restarts)").to_string(), t("Open the release page").to_string(), t("Not now").to_string(), t("Checking…").to_string(), tf("Downloading… {0}%", &[&100])]
            .iter()
            .map(|label| text_width(self.fonts.strong, label))
            .max()
            .unwrap_or(0);
        let across = (widest + s(ds::space::XL)).max(s(96)).min((width / 2).max(s(96)));
        let later = (text_width(self.fonts.small, t("Later")) + s(ds::space::XL)).max(s(56));
        let later_x = left + width - s(ds::space::S) - later;
        let main_x = later_x - s(ds::space::XS) - across;
        let y = top + (tall - s(ds::size::ICON_BUTTON)) / 2;
        let is_page = matches!(notice, notice::Notice::Page { .. });
        let (main, other) = if is_page { (self.release_offer_button, self.update_offer_button) } else { (self.update_offer_button, self.release_offer_button) };
        show(other, false);
        set_text_if_changed(main, &label);
        let (kind, enabled) = match notice {
            notice::Notice::Page { .. } => (ButtonKind::Secondary, true),
            notice::Notice::Fetching { .. } => (ButtonKind::Secondary, false),
            notice::Notice::UpdateNow { waiting, checking, .. } => (if *waiting { ButtonKind::Secondary } else { ButtonKind::Primary }, !*checking),
        };
        set_button_style(main, kind, theme::surface());
        EnableWindow(main, i32::from(enabled));
        place(main, main_x, y, across, s(ds::size::ICON_BUTTON));
        show(main, true);
        set_text_if_changed(self.notice_later, t("Later"));
        place(self.notice_later, later_x, y, later, s(ds::size::ICON_BUTTON));
        show(self.notice_later, true);
        shapes.push(Shape::Text { area: rect(left + s(ds::space::M), top, (main_x - left - s(ds::space::L)).max(0), tall), text: words, font: FontKind::Body, color: theme::text(), flags: ONE_LINE });
        tall + s(ds::space::S)
    }

    /// Later on the notice: this version is not told of again unprompted (a newer one is), and what was asked of the update is taken back.
    unsafe fn notice_sent_away(&mut self) {
        let Some(shown) = self.notice() else { return };
        let version = match &shown {
            notice::Notice::Page { version } | notice::Notice::Fetching { version, .. } => Some(version.clone()),
            notice::Notice::UpdateNow { version, .. } => version.clone(),
        };
        if let Some(version) = version {
            self.dismiss_release(&version);
        }
        if matches!(shown, notice::Notice::UpdateNow { .. }) {
            // (not this build: a newer one is offered again)
            self.update_offer = false;
            self.restart_when_done = false;
            self.release_ready_closed = true;
            self.update_declined = self.update_seen.clone();
        }
        self.layout();
    }

    /// The setup that was fetched, run to update this install: it waits for this app, and the engine it started, to end, puts the files of the new version in their places, and starts the
    /// app again. This quits as the tray's Quit does.
    /// Before it is run, all of its bytes are checked against the hash that was noted, through a handle that no one else can write through, held until the setup has started. That reads
    /// 560 MB (a second or two), so a thread does it, and the buttons say so and wait; it comes back as a note (`setup_checked`).
    unsafe fn hand_over_to_setup(&mut self) {
        if self.checking_setup {
            return;
        }
        let (Some(ready), Some(_)) = (self.ready_setup().cloned(), self.install_folder.clone()) else { return };
        self.restart_when_done = false;
        if !ready.is_there() {
            self.setup_is_gone();
            return;
        }
        self.checking_setup = true;
        self.release_update_changed();
        let (poster, folder) = (self.poster.clone(), self.folder.join(updates::FOLDER));
        std::thread::spawn(move || {
            let held = ready.hold_if_sound();
            // (not what was checked: it is not run, and it goes with its note)
            if matches!(held, Err(updates::NotRun::Changed)) {
                let _ = std::fs::remove_file(&ready.path);
                updates::clean_up(&folder, env!("CARGO_PKG_VERSION"));
            }
            poster.send(Note::SetupChecked(ready, held));
        });
    }

    /// What came of the check of the setup that waits: with the file sound and held, the setup is started, and this quits as the tray's Quit does; the handle is let go of once it has started.
    unsafe fn setup_checked(&mut self, ready: updates::Ready, held: Result<std::fs::File, updates::NotRun>) {
        self.checking_setup = false;
        let held = match held {
            Ok(held) if self.ready_setup() == Some(&ready) => held,
            // (it is not the one that waits any more)
            Ok(_) => return self.release_update_changed(),
            Err(updates::NotRun::Changed) => return self.setup_is_gone(),
            Err(updates::NotRun::Refused(error)) => return self.setup_not_started(&error),
        };
        // (work that began while it was read is not cut short: the press is kept, as it is when work is going on)
        if self.at_work() {
            drop(held);
            self.restart_when_done = true;
            return self.release_update_changed();
        }
        let Some(folder) = self.install_folder.clone() else { return };
        let waits_for: Vec<u32> = std::iter::once(std::process::id()).chain(self.engine.as_ref().map(Engine::id)).collect();
        match updates::run_held(&ready, held, &updates::setup_arguments(&folder, &waits_for)) {
            Ok(_) => {
                DestroyWindow(self.window);
            }
            Err(error) => self.setup_not_started(&error.to_string()),
        }
    }

    /// The setup that waits is not there, or is not what was checked: it is not offered, and About says so.
    unsafe fn setup_is_gone(&mut self) {
        self.release_update = ReleaseUpdate::None;
        self.release_update_said = Some((t("The downloaded update is gone. Check for updates to download it again.").to_string(), true));
        self.release_update_changed();
    }

    unsafe fn setup_not_started(&mut self, error: &str) {
        let words = tf("The update could not be started: {0}", &[&error]);
        if self.panel == Panel::Chat { self.say(&words) } else { self.problem_in_panel(&words) }
        self.release_update_changed();
    }

    /// About's line about the update to a newer release: how far its setup has come, that it waits to be run, or what became of the last try, and whether that is a trouble.
    fn update_words(&self) -> Option<(String, bool)> {
        match &self.release_update {
            ReleaseUpdate::Fetching { version, done, total, .. } => Some((tf("Downloading Dogmind {0}… {1}%", &[version, &(done * 100 / (*total).max(1))]), false)),
            ReleaseUpdate::Ready(ready) => Some((tf("Dogmind {0} is ready to install.", &[&ready.version]), false)),
            ReleaseUpdate::None => self.release_update_said.clone(),
        }
    }

    /// The button under that line, if it has one: a way to stop the download, or to restart into what was fetched.
    fn update_button_words(&self) -> Option<&'static str> {
        update_button_words_of(&self.release_update, self.ready_setup().is_some(), self.checking_setup, self.restart_when_done)
    }

    /// How that button looks: the page's main button when it restarts into the update, a plain one to take that back, and a link to stop a download.
    fn update_button_kind(&self) -> ButtonKind {
        update_button_kind_of(&self.release_update, self.restart_when_done)
    }

    /// An update of this install is on its way or here: About tells of it by its own lines, and not by those of the look that found the release.
    fn updating(&self) -> bool {
        self.install_folder.is_some() && !matches!(self.release_update, ReleaseUpdate::None)
    }

    /// The release About's link opens, with its page: the newer one the last look found; or, once its setup is here, that one, which a setup kept from before this start has had no
    /// look to name the page of. While the setup is on its way, none: it is told of by its percent.
    fn release_to_read(&self) -> Option<(String, String)> {
        if !self.updating() {
            return match self.release_outcome.as_ref() {
                Some(updates::Outcome::Newer(release)) => Some((release.version.clone(), release.page.clone())),
                _ => None,
            };
        }
        match (&self.release_update, self.release_outcome.as_ref()) {
            (ReleaseUpdate::Ready(ready), Some(updates::Outcome::Newer(release))) if release.version == ready.version => Some((release.version.clone(), release.page.clone())),
            (ReleaseUpdate::Ready(ready), _) => Some((ready.version.clone(), updates::release_page_of(&ready.version))),
            _ => None,
        }
    }

    /// What About and the row above the input show of the update has changed.
    unsafe fn release_update_changed(&mut self) {
        if self.panel == Panel::About {
            self.show_release_controls();
        }
        self.layout();
    }

    /// Fetches the setup of this release in the background, quietly: a thread tells how far it has come, and nothing in the window asks for anything until it is here and checked. The app's own
    /// try waits for a connection that is not metered; the person's own does not.
    unsafe fn fetch_release_setup(&mut self, release: updates::Release, on_its_own: bool) {
        let Some(setup) = release.setup.clone() else { return };
        if !matches!(self.release_update, ReleaseUpdate::None) {
            return;
        }
        // (asked here, on the window's thread, which Windows answers in a moment)
        if on_its_own && crate::machine::connection_is_metered() {
            self.release_update_said = Some(fetch_words(&updates::Failed::Metered));
            self.release_update_changed();
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.release_update = ReleaseUpdate::Fetching { version: release.version.clone(), done: 0, total: setup.size, stop: stop.clone() };
        self.release_update_said = None;
        let (poster, folder, version) = (self.poster.clone(), self.folder.join(updates::FOLDER), release.version);
        std::thread::spawn(move || {
            // (a note when the whole number of percent changes, and not more than five in a second)
            let mut last: Option<(std::time::Instant, u64)> = None;
            let result = updates::obtain(&version, &setup, &folder, &mut |done, total| {
                let percent = done * 100 / total.max(1);
                if done == total || last.is_none_or(|(at, was)| was != percent && at.elapsed() >= std::time::Duration::from_millis(200)) {
                    last = Some((std::time::Instant::now(), percent));
                    poster.send(Note::ReleaseProgress { done, total });
                }
            }, &stop);
            poster.send(Note::ReleaseFetched(result));
        });
        self.release_update_changed();
    }

    /// At the start: what earlier updates left that is done with goes from the `updates` folder, and a setup fetched and checked before, for a later version than this one, is offered
    /// again, and not fetched again. Its bytes are read again for that, off the window's thread.
    fn tidy_updates(&self) {
        let (poster, folder, installed) = (self.poster.clone(), self.folder.join(updates::FOLDER), self.install_folder.is_some());
        std::thread::spawn(move || {
            let running = env!("CARGO_PKG_VERSION");
            updates::clean_up(&folder, running);
            if !installed {
                return;
            }
            // (one that changed since it was checked is not offered, and is taken away)
            if let Some(ready) = updates::kept_and_sound(&folder, running) {
                poster.send(Note::ReleaseFetched(Ok(ready)));
            }
        });
    }

    /// After a fetch that did not come to an end: the setup that was fetched and checked before, if `kept` still finds it, is offered again, as it is at a start. Its bytes are read again for
    /// that, off the window's thread.
    fn offer_the_setup_kept(&self) {
        let (poster, folder) = (self.poster.clone(), self.folder.join(updates::FOLDER));
        std::thread::spawn(move || {
            if let Some(ready) = updates::kept_and_sound(&folder, env!("CARGO_PKG_VERSION")) {
                poster.send(Note::ReleaseFetched(Ok(ready)));
            }
        });
    }

    unsafe fn restart_into_update(&mut self) {
        // (the name it was started by, which is the new build's now: the file it runs from was moved aside)
        let Some(exe) = self.builds.first().map(|(path, _)| path.clone()) else { return };
        let mut command = std::process::Command::new(&exe);
        if self.workspace.is_dir() && self.workspace != Path::new(".") {
            command.arg(&self.workspace);
        }
        command.arg("--after").arg(std::process::id().to_string());
        if let Some(session) = &self.current_session {
            command.arg("--resume").arg(session);
        }
        if IsWindowVisible(self.window) == 0 {
            command.arg("--hidden");
        }
        if command.spawn().is_err() {
            // (it stays as it is, and looks again later)
            self.update_seen = None;
            return;
        }
        DestroyWindow(self.window);
    }

    /// The app's own look at GitHub for a newer release, when the timer says it may be time. It is made only if the person has not turned it off, if six hours have gone by since GitHub
    /// last answered, and if there is anywhere to ask; and not on the first screens, where a new install has just fetched the newest there is.
    unsafe fn look_for_release(&mut self) {
        self.look_for_release_after(updates::EVERY);
    }

    /// The same, when `every` seconds have gone by since GitHub last answered.
    unsafe fn look_for_release_after(&mut self, every: u64) {
        if !self.settings.check_for_updates || self.onboarding || !matches!(self.leaving, Leaving::No) || !updates::can_ask() {
            return;
        }
        if updates::is_due_after(self.settings.last_update_check, updates::now(), every) {
            self.start_release_look(true);
        }
    }

    /// Asks on a thread of its own (it may take ten seconds); what it comes to arrives as a note. `on_its_own`: the app's look, and not one the person asked for.
    unsafe fn start_release_look(&mut self, on_its_own: bool) {
        if self.release_checking {
            return;
        }
        self.release_checking = true;
        let poster = self.poster.clone();
        std::thread::spawn(move || poster.send(Note::ReleaseLooked { on_its_own, outcome: updates::check() }));
        if self.panel == Panel::About {
            self.show_release_controls();
            self.layout();
        }
    }

    /// The About page's button for the look: what it says, and that it waits while a look is going on.
    unsafe fn show_release_controls(&self) {
        set_text_if_changed(self.buttons[1], if self.release_checking { t("Checking…") } else { t("Check for updates") });
        set_text_if_changed(self.buttons[2], t("Open the release page"));
        if let Some(words) = self.update_button_words() {
            set_text_if_changed(self.buttons[3], words);
            set_button_style(self.buttons[3], self.update_button_kind(), theme::background());
        }
        EnableWindow(self.buttons[1], i32::from(!self.release_checking));
        EnableWindow(self.buttons[3], i32::from(!self.checking_setup));
    }

    /// What the About page says of the last look, and whether that is a trouble. Nothing while one is going on, and before any.
    fn release_words(&self) -> Option<(String, bool)> {
        use updates::{Outcome, Trouble};
        if self.release_checking {
            return None;
        }
        Some(match self.release_outcome.as_ref()? {
            Outcome::UpToDate => (tf("Dogmind {0} is up to date.", &[&env!("CARGO_PKG_VERSION")]), false),
            // (an update that is on its way or here has its own lines: the same release is not told of twice)
            Outcome::Newer(_) if self.updating() => return None,
            Outcome::Newer(release) => (tf("Dogmind {0} is available.", &[&release.version]), false),
            Outcome::NoRelease => (t("No release has been published yet.").to_string(), false),
            Outcome::NotAsked => (t("This test build asks no one.").to_string(), false),
            Outcome::Failed(Trouble::NotReached) => (t("Could not reach GitHub.").to_string(), true),
            Outcome::Failed(Trouble::TooSlow) => (t("GitHub did not answer in time.").to_string(), true),
            Outcome::Failed(Trouble::Refused(status)) => (tf("GitHub answered with an error ({0}). Try again later.", &[status]), true),
            Outcome::Failed(Trouble::Unreadable) => (t("GitHub's answer could not be read.").to_string(), true),
        })
    }

    /// The release's page, in the browser. The person has been told of this one, and is not told of it again on the app's own.
    unsafe fn open_release(&mut self, version: &str, page: &str) {
        open_in_browser(page);
        self.dismiss_release(version);
    }

    unsafe fn dismiss_release(&mut self, version: &str) {
        self.settings.dismissed_update = Some(version.to_string());
        self.save();
        if self.release_offer.as_ref().is_some_and(|offer| offer.version == version) {
            self.release_offer = None;
        }
    }

    /// The About page's switch for the app's own looks.
    unsafe fn updates_switched(&mut self) {
        self.settings.check_for_updates = !self.settings.check_for_updates;
        if !self.settings.check_for_updates {
            self.release_offer = None;
        }
        self.save();
        self.update_toggles();
        // (turned on, it looks at once if six hours have gone by: that is what turning it on asks for)
        self.look_for_release();
        self.layout();
    }

    unsafe fn asked_to_quit(&mut self) {
        if self.at_work() {
            // (a test's copy shows itself where it is, and takes no one's focus)
            if crate::machine::kept_from_the_real_machine() {
                ShowWindow(self.window, SW_SHOWNA);
            } else {
                self.tray_open();
            }
            self.say(t("The installer asks this app to close, to replace its files. It is still working, so it stays open: when it is done, quit it from the tray icon and press Try again in the installer."));
            return;
        }
        DestroyWindow(self.window);
    }

    /// Where a first session can begin.
    fn first_folder(&self) -> PathBuf {
        crate::machine::first_folder(&self.folder)
    }

    /// This PC as Straydog sees it, and the room where the model would go.
    unsafe fn about_this_pc(&self) -> String {
        let folder = get_text(self.fields[0]);
        let free = if self.refusal.is_some() || folder.trim().is_empty() { None } else { crate::machine::free_bytes(Path::new(folder.trim())) };
        let free = free.map(|free| tf("{0} GB free on that drive", &[&(free / 1_000_000_000)]));
        [Some(self.machine_line.clone()).filter(|line| !line.is_empty()), free].into_iter().flatten().collect::<Vec<_>>().join("  ·  ")
    }

    /// The model, when it lies in the folder this app removes.
    fn model_in_the_data_folder(&self) -> Option<PathBuf> {
        self.settings.models_folder.clone().filter(|folder| settings::same_or_inside(folder, &self.folder) && folder.is_dir())
    }

    /// What a stranger can send, written off this thread: asking Straydog about the GPU takes a moment.
    /// What the engine beside the app says it is, the first time anyone looks: it costs a process, and nothing until then.
    unsafe fn read_engine_version(&mut self) {
        if self.engine_version.is_some() {
            return;
        }
        let Some(straydog) = engine::find(&self.settings) else {
            self.engine_version = Some(t("Straydog: not found").to_string());
            return;
        };
        let poster = self.poster.clone();
        std::thread::spawn(move || {
            // (its version does not change from build to build: the day of the file says which this is)
            let made = std::fs::metadata(&straydog).and_then(|file| file.modified()).ok().and_then(|made| made.duration_since(std::time::UNIX_EPOCH).ok()).map(|since| since.as_secs());
            poster.send(Note::EngineVersion(engine::version(&straydog), made));
        });
    }

    unsafe fn write_report(&mut self) {
        let (poster, data, settings) = (self.poster.clone(), self.folder.clone(), self.settings.clone());
        std::thread::spawn(move || {
            let about = match engine::find(&settings) {
                None => "straydog.exe not found".to_string(),
                Some(straydog) => match engine::load_catalog(&straydog) {
                    Ok(catalog) => format!("{}{}", catalog.machine, catalog.refusal.map_or(String::new(), |refusal| format!("  ({})", refusal.short()))),
                    Err(error) => error,
                },
            };
            poster.send(Note::Report(crate::crash::write_report(&data, &settings, &about)));
        });
    }

    /// Takes away what this app has left on this PC. The folder it runs from it cannot take, and the person is told so. Nothing else of this run writes after this.
    unsafe fn leave_this_pc(&mut self, keep_model: bool) {
        self.leaving = Leaving::Going;
        self.engine = None;
        self.preview = None;
        self.agent.cancel.store(true, Ordering::Relaxed);
        let _ = self.agent.commands.send(Command::Quit);
        for parked in &self.parked {
            parked.agent.cancel.store(true, Ordering::Relaxed);
            let _ = parked.agent.commands.send(Command::Quit);
        }
        self.layout();
        RedrawWindow(self.window, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
        let (poster, data, settings) = (self.poster.clone(), self.folder.clone(), self.settings.clone());
        std::thread::spawn(move || {
            // (what the agents were doing ends with them, and only then is it not in use)
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let mut problems = Vec::new();
            // That this is this app's folder is the first thing, as it decides what else is its to take: a variable can name another.
            let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
            let its_own = match settings::what_remove_takes(&data, None, home.as_deref()) {
                Ok(_) => true,
                Err(error) => {
                    problems.push(error);
                    false
                }
            };
            let file = read_folders::file_in(&data);
            for folder in read_folders::list(&file) {
                if let Err(error) = read_folders::remove(&file, &folder.path, &mut |_| {}) {
                    problems.push(format!("{}: {error}", folder.path.display()));
                }
            }
            let mut named = settings.workspaces.clone();
            named.extend(session::list(&data.join("sessions")).into_iter().map(|summary| summary.workspace));
            dogmind_agent::workspace_grants::sweep(&data, &named);
            // (the list of what was given goes with the folder: what could not be taken back, the person hears of now)
            for folder in dogmind_agent::workspace_grants::still_granted(&data) {
                problems.push(tf("{0} may still be open to commands: what was given could not be taken back.", &[&shown_path(&folder)]));
            }
            let keep = if keep_model { settings.models_folder.clone() } else { None };
            let mut left = Vec::new();
            if its_own {
                if let Err(error) = crate::keys::store().remove_all() {
                    problems.push(error);
                }
                crate::machine::delete_profiles();
                for path in settings::what_remove_takes(&data, keep.as_deref(), home.as_deref()).unwrap_or_default() {
                    let gone = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                    if let Err(error) = gone {
                        problems.push(format!("{}: {error}", path.display()));
                    }
                }
                // (what this app did not make stays where it is: the folder goes only if that leaves it empty)
                left = settings::what_remove_leaves(&data, keep.as_deref());
                if keep.is_none() {
                    let _ = std::fs::remove_dir(&data);
                }
            }
            let program = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)).map_or(String::new(), |folder| shown_path(&folder));
            // (what the installer put there it takes away itself, with its Start menu entry; a copy from a zip is the person's own to delete)
            let mut words = if uninstaller_beside().is_some() { tf("It is done. Closing this takes away the program too: {0}", &[&program]) } else { tf("It is done. To finish, close this and delete the folder this app runs from: {0}", &[&program]) };
            match (keep, settings.models_folder.clone().filter(|folder| !settings::same_or_inside(folder, &data))) {
                (Some(model), _) | (None, Some(model)) => words.push_str(&format!("\n{}", tf("The model stays in {0}.", &[&shown_path(&model)]))),
                _ => {}
            }
            if !left.is_empty() {
                let names: Vec<String> = left.iter().map(|path| path.file_name().map_or(String::new(), |name| name.to_string_lossy().into_owned())).collect();
                words.push_str(&format!("\n{}\n{}", tf("This app did not make these, so they stay in {0}:", &[&shown_path(&data)]), names.join("\n")));
            }
            if !problems.is_empty() {
                words.push_str(&format!("\n{}\n{}", t("These could not be removed:"), problems.join("\n")));
            }
            poster.send(Note::Removed(words));
        });
    }

    unsafe fn set_button_kinds(&self, kinds: &[ButtonKind]) {
        for button in &self.buttons {
            EnableWindow(*button, 1);
        }
        for (button, kind) in self.buttons.iter().zip(kinds) {
            set_button_style(*button, *kind, theme::background());
        }
    }

    unsafe fn fill_list(&self) {
        SendMessageW(self.list, LB_RESETCONTENT, 0, 0);
        for _ in &self.list_rows {
            SendMessageW(self.list, LB_ADDSTRING, 0, 0);
        }
        InvalidateRect(self.list, std::ptr::null(), 1);
    }

    /// The model set up, as the settings say.
    fn vision_installed(&self) -> Option<crate::vision::Installed> {
        crate::vision::installed(&std::fs::read_to_string(engine::config_path(&self.folder)).ok()?)
    }

    /// The model the Models page is about is the one set up: its files say what the row says. Another one chosen is still only a choice.
    fn vision_for_installed(&self) -> Option<crate::vision::Installed> {
        let installed = self.vision_installed()?;
        match unsafe { self.list_selection() }.and_then(|at| self.catalog.get(at)) {
            Some(entry) if entry.bundle.is_some() && !entry.is(&installed.bundle) => None,
            _ => Some(installed),
        }
    }

    /// What the row says, with the file's size; none for a model that has no such file.
    fn vision_row(&self) -> Option<String> {
        let chosen = unsafe { self.list_selection() }.and_then(|at| self.catalog.get(at));
        // (only the list says what a model has)
        if chosen.is_some_and(|entry| entry.download_bytes.is_some() && entry.vision_bytes.is_none()) {
            return None;
        }
        Some(match chosen.and_then(|entry| entry.vision_bytes) {
            Some(bytes) => tf("Read pictures ({0})", &[&engine::gb_file(bytes)]),
            None => t("Read pictures").to_string(),
        })
    }

    /// The bytes of a vision file set aside, for the model set up.
    fn vision_set_aside(&self) -> Option<u64> {
        match crate::vision::state(&self.vision_for_installed()?) {
            crate::vision::State::Off(path) => std::fs::metadata(path).ok().map(|file| file.len()),
            _ => None,
        }
    }

    /// Whether the row stands on, as the files say or the choice was made.
    fn vision_is_on(&self) -> bool {
        if self.setting_up && self.vision_fetch {
            return true;
        }
        match self.vision_for_installed() {
            Some(installed) => matches!(crate::vision::state(&installed), crate::vision::State::On(_)),
            None => self.vision_wanted,
        }
    }

    /// All that a model yet to be set up takes to download, as chosen. The list's rows say it with everything.
    fn download_total(&self) -> Option<String> {
        if self.vision_for_installed().is_some() {
            return None;
        }
        let entry = unsafe { self.list_selection() }.and_then(|at| self.catalog.get(at))?;
        let mut total = entry.download_bytes?;
        if !self.vision_wanted {
            total = total.saturating_sub(entry.vision_bytes.unwrap_or(0));
        }
        if !self.drafter {
            total = total.saturating_sub(entry.drafter_bytes.unwrap_or(0));
        }
        Some(tf("{0} to download in all", &[&engine::gb(total)]))
    }

    unsafe fn update_vision_chip(&self) {
        let on = self.vision_is_on();
        if self.modal_open {
            set_button_style(self.vision_chip, if on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        } else {
            set_button_style(self.vision_chip, ButtonKind::Select, theme::background());
            set_text(self.vision_chip, if on { t("Get it too") } else { t("Leave it out") });
        }
    }

    /// Work is under way that the engine cannot be taken from.
    fn engine_is_busy(&self) -> bool {
        // (a session that only waits for a command to end is not using it: it would be woken by an engine that is there again)
        self.running || self.parked.iter().any(|parked| parked.running)
    }

    /// The row used: on the files of the model set up, or only as a choice for one to come.
    unsafe fn vision_clicked(&mut self) {
        if self.setting_up {
            return;
        }
        let Some(installed) = self.vision_for_installed() else {
            self.vision_wanted = !self.vision_wanted;
            self.update_vision_chip();
            self.layout();
            return;
        };
        match crate::vision::state(&installed) {
            crate::vision::State::On(_) => self.vision_switch(false),
            crate::vision::State::Off(_) => self.vision_switch(true),
            crate::vision::State::Absent => self.start_vision_setup(),
        }
    }

    /// The engine lets go of the file, it is moved, and the engine starts again if it is the one in use.
    unsafe fn vision_switch(&mut self, on: bool) {
        if self.engine_is_busy() {
            let words = t("That can be done once the work under way has ended.");
            if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
            return;
        }
        self.engine = None;
        let (poster, config) = (self.poster.clone(), engine::config_path(&self.folder));
        std::thread::spawn(move || {
            // (the engine's end lets go of the file a moment after it has been told)
            let mut result = crate::vision::switch(&config, on);
            for _ in 0..30 {
                if result.is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                result = crate::vision::switch(&config, on);
            }
            poster.send(Note::VisionSwitched(result));
        });
    }

    /// The vision file alone for the model that is there, the way the model's own download goes: its progress, its cancel, and on from where it stopped.
    unsafe fn start_vision_setup(&mut self) {
        if self.setting_up {
            // (the engine runs while the faster-writing data is fetched, so a picture can be asked about then: the file waits for that download)
            if self.drafter_fetch.is_some() {
                let words = t("That can be done once the work under way has ended.");
                if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
            }
            return;
        }
        if self.engine_is_busy() {
            let words = t("That can be done once the work under way has ended.");
            if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
            return;
        }
        let Some(installed) = self.vision_installed() else { return };
        let Some(straydog) = engine::find(&self.settings) else {
            let words = engine::not_found_words();
            if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
            return;
        };
        self.vision_fetch = true;
        let args = engine::vision_arguments(&installed, &engine::config_path(&self.folder));
        self.run_setup(straydog, args, t("Setting up the vision file...").to_string());
        self.update_vision_chip();
        if self.panel == Panel::Chat {
            self.layout();
        }
    }

    /// Speed first: on, the engine's next start skips the CPU's low-weight routes (faster where memory is short, answers change a little).
    unsafe fn update_speed_chip(&self) {
        let on = self.settings.speed_first;
        if self.modal_open {
            set_button_style(self.speed_chip, if on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        } else {
            set_button_style(self.speed_chip, ButtonKind::Select, theme::background());
            set_text(self.speed_chip, if on { t("On: faster, answers change a little") } else { t("Off") });
        }
    }

    /// A file alone is being fetched for the model that is there: its progress has a bar of its own, not one nearly full with the model.
    fn fetching_alone(&self) -> bool {
        self.vision_fetch || self.drafter_fetch.is_some()
    }

    /// The model set up, when the Models page is about it and it has a drafter: its settings say what the row says. Another one chosen is still only a choice, and only the list says
    /// which models have a drafter.
    fn drafter_for_installed(&self) -> Option<crate::vision::Installed> {
        let installed = self.vision_for_installed()?;
        self.catalog.iter().find(|entry| entry.is(&installed.bundle)).filter(|entry| entry.drafter_bytes.is_some()).map(|_| installed)
    }

    /// Whether the row stands on: as the settings of the model set up say, or the choice made.
    fn drafter_is_on(&self) -> bool {
        if self.drafter_fetch.is_some() {
            return true;
        }
        match self.drafter_for_installed() {
            Some(installed) => installed.drafter,
            None => self.drafter,
        }
    }

    unsafe fn update_drafter_chip(&self) {
        let on = self.drafter_is_on();
        if self.modal_open {
            set_button_style(self.drafter_chip, if on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
        } else {
            set_button_style(self.drafter_chip, ButtonKind::Select, theme::background());
            set_text(self.drafter_chip, if on { t("Build it too (about 5 GB more to download)") } else { t("Leave it out") });
        }
    }

    /// The row used: on the settings of the model set up, or only as a choice for one to come.
    unsafe fn drafter_clicked(&mut self) {
        let Some(installed) = self.drafter_for_installed() else {
            self.drafter = !self.drafter;
            self.update_drafter_chip();
            self.layout();
            return;
        };
        if self.setting_up {
            return;
        }
        if !installed.drafter {
            // (the engine goes on while it is fetched, and starts again once it has ended and no work is under way)
            self.start_drafter_setup(&installed);
            return;
        }
        if self.engine_is_busy() {
            let words = t("That can be done once the work under way has ended.");
            if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
            return;
        }
        self.start_drafter_off(&installed);
    }

    /// Whether a setup under way keeps an engine from running (see `setup_holds_off_the_engine`).
    fn setup_holds_off_the_engine(&self) -> bool {
        setup_holds_off_the_engine(self.setting_up, self.drafter_fetch.is_some())
    }

    /// The drafter alone for the model that is there, with the progress, the cancel and the going on from where it stopped of the model's own download: its shards are not read again. The
    /// engine keeps running meanwhile. The settings take the drafter in when it is built (`Note::SetupDone`), and the engine starts again with it, when no work is under way.
    unsafe fn start_drafter_setup(&mut self, installed: &crate::vision::Installed) {
        let Some(straydog) = engine::find(&self.settings) else {
            let words = engine::not_found_words();
            if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
            return;
        };
        let folder = drafter::directory(installed);
        let args = drafter::fetch_arguments(&folder);
        self.drafter_fetch = Some(folder);
        self.run_setup(straydog, args, t("Getting the faster-writing data...").to_string());
        self.update_drafter_chip();
        if self.panel == Panel::Chat {
            self.layout();
        }
    }

    /// The faster-writing data is in the settings. The engine that began before it has to start again to take it in, and does when no work is under way: a turn, a tool run or a
    /// session in the background is not cut short for it. One that has ended, or never began, needs nothing: its next start reads the settings.
    unsafe fn restart_for_drafter(&mut self) {
        let embedded = self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded);
        let engine_began = embedded && !self.engine_waits && !self.connect_failed;
        let waited = self.drafter_restart_waits;
        match drafter_restart(engine_began, self.engine_is_busy()) {
            DrafterRestart::Now => self.connect(),
            DrafterRestart::Not => {
                self.drafter_restart_waits = false;
                if waited {
                    self.update_status();
                }
            }
            DrafterRestart::WhenIdle => {
                self.drafter_restart_waits = true;
                if !waited {
                    // (the status line is under the conversation: another page says it where it says what it has to)
                    if self.panel != Panel::Chat {
                        self.say_in_panel(drafter_restart_words());
                    }
                    self.update_status();
                }
            }
        }
    }

    /// The engine's next start has no drafter, and it starts again now. Its file stays where it is: setup never takes one away, and turning the row on again needs no download.
    unsafe fn start_drafter_off(&mut self, installed: &crate::vision::Installed) {
        match drafter::turn_off(&engine::config_path(&self.folder)) {
            Ok(_) => {
                let words = tf("The faster-writing data is off. Its file stays in {0}, and turning it on again needs no download.", &[&shown_path(&drafter::directory(installed))]);
                if self.panel == Panel::Chat { self.say(&words) } else { self.say_in_panel(&words) }
            }
            Err(error) => {
                if self.panel == Panel::Chat { self.say(&error) } else { self.problem_in_panel(&error) }
                return;
            }
        }
        self.update_drafter_chip();
        self.layout();
        if self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded) {
            self.engine = None;
            self.connect();
        }
    }

    /// A new session in `folder`, on the chat. A folder too wide for commands still reads and edits, and says so.
    unsafe fn start_session(&mut self, folder: PathBuf) {
        self.park_if_working();
        self.queued_back_to_composer();
        self.settings.use_workspace(folder.clone());
        self.settings.set_up = true;
        self.onboarding = false;
        self.save();
        self.current_session = None;
        self.workspace = folder.clone();
        self.preview_follows_workspace();
        self.session_title.clear();
        self.totals = (0, 0, 0, 0);
        self.rate = None;
        let _ = self.agent.commands.send(Command::NewSession(folder.clone()));
        self.look_at_workspace();
        self.set_panel(Panel::Chat);
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
        if let Some(reason) = dogmind_agent::sandbox::refusal(&folder, home.as_deref()) {
            self.queued_notice(reason);
        } else if too_long_for_commands(&folder) {
            // (not `say`: the new session's transcript comes after this and would take it away)
            self.queued_notice(t(PATH_TOO_LONG).to_string());
        } else if let Some(words) = crate::machine::workspace_notice(&folder) {
            self.queued_notice(words);
        }
    }

    /// An agent for what comes on screen next, connected as the others are.
    unsafe fn spawn_agent(&self) -> (Handle, Receiver<Event>) {
        let (sender, events) = channel();
        let waker = self.poster.clone();
        let handle = agent::start(
            agent_config(&self.settings, &self.folder, self.workspace.clone(), self.current_mode(), self.mcp.clone(), self.policy.clone()),
            sender,
            Box::new(move || waker.wake()),
        );
        let _ = handle.commands.send(Command::SetSecrets(key_files(&self.settings)));
        let _ = handle.commands.send(Command::SetSafeCommands(self.settings.safe_commands));
        let _ = handle.commands.send(Command::SetEffort(self.effort_to_send()));
        if let Some((provider, model, window)) = &self.provider {
            let _ = handle.commands.send(Command::Provider(provider.clone(), model.clone(), *window));
        }
        (handle, events)
    }

    /// The session on screen is at work: it goes on out of sight, and the window gets a free agent.
    unsafe fn queued_back_to_composer(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut self.queued);
        let mut lines: Vec<String> = queued.iter().filter(|item| !item.text.is_empty()).map(|item| item.text.clone()).collect();
        let draft = get_text(self.composer);
        if !draft.trim().is_empty() {
            lines.push(draft);
        }
        // (putting it back is no one writing: it starts no engine)
        self.restoring_draft = true;
        set_text(self.composer, &lines.join("\r\n"));
        self.restoring_draft = false;
        for image in queued.into_iter().flat_map(|item| item.images) {
            if !self.attachments.iter().any(|attached| attached.path == image.path) && self.attachments.len() < attach::MOST_IMAGES {
                self.attachments.push(image);
            }
        }
        self.show_attachments();
    }

    /// Something waits to be sent to what is not connected yet.
    fn waiting_to_connect(&self) -> bool {
        !self.running && !self.connected && !self.queued.is_empty()
    }

    /// The square: the turn stops, or the wait for a connection does and what was written goes back in the composer.
    unsafe fn stop_work(&mut self) {
        if self.running {
            self.agent.cancel.store(true, Ordering::Relaxed);
            // (a turn that has not begun clears this when it does, behind a warm-up that cannot be cut short: TurnStarted asks again)
            self.stop_asked = true;
            self.set_activity(t("Stopping…").to_string());
        } else if self.waiting_to_connect() {
            KillTimer(self.window, TIMER_RECONNECT);
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.connect_retrying = false;
            if self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded) {
                // Dropped, it stops, and what it was loading leaves the memory.
                self.engine = None;
                self.engine_waits = true;
                self.connection = "starts when you write".into();
            } else {
                self.connect_failed = true;
                self.connection = "not connected".into();
            }
            self.queued_back_to_composer();
            self.set_activity(String::new());
            self.update_buttons();
            self.update_status();
            self.say(t("Not sent: what you wrote is back in the box."));
        }
    }

    /// The words for waiting: an engine of this app starts, and anything else is connected to.
    fn waiting_words(&self) -> &'static str {
        if self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded) { "Waiting for the engine…" } else { "Connecting…" }
    }

    unsafe fn park_if_working(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }
        self.flush_reveal();
        // (one that has a background command or a wait goes on out of sight when it is not at work too: what it waits for will wake it, and its agent must be there to be woken)
        if !self.running && self.waiting.is_empty() {
            return;
        }
        // (asked for above: what is on screen is about to go)
        let writing = self.writing_now();
        self.close_run();
        self.run_carry.clear();
        self.asked_calls.clear();
        self.plain_writing = None;
        self.request_live = None;
        let (handle, events) = self.spawn_agent();
        let agent = std::mem::replace(&mut self.agent, handle);
        let events = std::mem::replace(&mut self.events, events);
        self.parked.push(Parked {
            path: self.current_session.clone(),
            workspace: self.workspace.clone(),
            title: self.session_title.clone(),
            agent,
            events,
            pending: self.pending.take(),
            question: self.question.take().map(|asked| agent::Question { question: asked.question, options: asked.options, answer: asked.answer }),
            plan_review: self.plan_review.take().map(|asked| agent::PlanReview { plan: asked.plan, answer: asked.answer }),
            partial: std::mem::take(&mut self.answer),
            tools_running: std::mem::take(&mut self.tools_running),
            queued: std::mem::take(&mut self.queued),
            interjected: std::mem::take(&mut self.interjected),
            writing,
            used: self.used,
            window_tokens: self.window_tokens,
            rate: self.rate,
            totals: self.totals,
            notices: Vec::new(),
            failed: false,
            todos: std::mem::take(&mut self.todos),
            web_seen: self.web_seen,
            compacting: self.activity == t("Compacting the session…"),
            running: self.running,
            woken: std::mem::take(&mut self.woken_turn),
            waiting: std::mem::take(&mut self.waiting),
            waiting_taken: self.waiting_taken,
        });
        self.running = false;
        self.stop_asked = false;
        self.set_activity(String::new());
        self.update_buttons();
        InvalidateRect(self.side, std::ptr::null(), 0);
    }

    /// A session opened from the sidebar or `/resume`: whatever is on screen goes on working if it was.
    unsafe fn open_session(&mut self, path: PathBuf) {
        if self.current_session.as_ref() == Some(&path) {
            self.set_panel(Panel::Chat);
            self.find_opened();
            return;
        }
        self.park_if_working();
        self.queued_back_to_composer();
        self.failed_sessions.remove(&path);
        // (what has come in by now, so that the file and what is held of an answer do not both show it)
        self.drain();
        if let Some(at) = self.parked.iter().position(|parked| parked.path.as_ref() == Some(&path)) {
            self.take_up(at);
            self.find_opened();
            return;
        }
        self.totals = (0, 0, 0, 0);
        self.rate = None;
        self.current_session = Some(path.clone());
        let _ = self.agent.commands.send(Command::Resume(path));
        self.set_panel(Panel::Chat);
        InvalidateRect(self.side, std::ptr::null(), 0);
    }

    /// A session at work comes back on screen: the whole messages from its file, and then what is on its way.
    unsafe fn take_up(&mut self, at: usize) {
        let parked = self.parked.remove(at);
        let free = std::mem::replace(&mut self.agent, parked.agent);
        let _ = free.commands.send(Command::Quit);
        self.events = parked.events;
        self.current_session = parked.path;
        self.workspace = parked.workspace;
        self.preview_follows_workspace();
        self.session_title = parked.title;
        self.tools_running = parked.tools_running;
        self.queued = parked.queued;
        self.interjected = parked.interjected;
        self.used = parked.used;
        self.window_tokens = parked.window_tokens;
        self.rate = parked.rate;
        self.totals = parked.totals;
        // (one that stands by has no turn at work: it has what waits for it to wake)
        self.running = parked.running;
        self.woken_turn = parked.woken;
        self.waiting = parked.waiting;
        self.waiting_taken = parked.waiting_taken;
        self.todos = parked.todos;
        self.web_seen = parked.web_seen;
        self.changes.clear();
        self.refresh_changes();
        self.look_at_workspace();
        let messages = self.current_session.as_deref().map_or(Ok(Vec::new()), session::read).unwrap_or_default();
        self.set_panel(Panel::Chat);
        self.show_transcript(&messages);
        // (what it said when a turn of its own ended, in its time out of sight, and what it says now)
        for (notice, style) in self.held_notices.remove(self.current_session.as_ref().unwrap_or(&PathBuf::new())).unwrap_or_default().into_iter().chain(parked.notices) {
            self.append(&format!("{notice}\n"), style);
        }
        self.answer = parked.partial;
        if messages.last().and_then(|last| last.content.as_deref()) == Some(self.answer.as_str()) {
            self.answer.clear();
        }
        if !self.answer.is_empty() {
            let shown = crate::mask::hide_keys(&self.answer).into_owned();
            self.append_answer(&shown);
        }
        // (the calls it has under way, which its file does not know of yet)
        let running: Vec<RunEntry> = self.tools_running.iter().filter(|(_, (tool, _))| !cards::is_asking(tool)).map(|(id, (tool, summary))| RunEntry::running(id.clone(), tool.clone(), summary.clone())).collect();
        if self.settings.compact_tools && !running.is_empty() {
            let at = self.open_run();
            self.runs[at].entries = running;
            self.write_run(at);
        }
        for (index, tool, raw) in parked.writing.into_iter().filter(|(_, tool, _)| !cards::is_asking(tool)) {
            self.run_writes(index, tool, &raw);
        }
        self.set_activity(if !parked.running || parked.pending.is_some() || parked.question.is_some() || parked.plan_review.is_some() || (self.settings.compact_tools && !self.tools_running.is_empty()) { String::new() } else if !self.tools_running.is_empty() { t("Working").to_string() } else if parked.compacting { t("Compacting the session…").to_string() } else { t("Thinking…").to_string() });
        self.update_buttons();
        self.update_status();
        if let Some(approval) = parked.pending {
            self.on_event(Event::Approval(approval));
        }
        if let Some(question) = parked.question {
            self.on_event(Event::Question(question));
        }
        if let Some(review) = parked.plan_review {
            self.on_event(Event::PlanReview(review));
        }
        InvalidateRect(self.side, std::ptr::null(), 0);
    }

    /// Shown once the new session has cleared the transcript.
    unsafe fn queued_notice(&mut self, text: String) {
        self.panel_log = text;
    }

    /// Where Straydog's dashboard is, when Straydog is what is connected.
    fn dashboard_url(&self) -> Option<String> {
        if !self.straydog {
            return None;
        }
        let provider = self.settings.active_provider()?;
        // (in the app's own language: the dashboard takes it from `lang` and keeps it, and an engine from before that ignores it)
        let language = if lang::is_japanese() { "ja" } else { "en" };
        match provider.kind {
            Kind::Embedded => Some(format!("http://127.0.0.1:{}/dashboard?lang={language}", self.settings.port)),
            Kind::Server => Some(format!("{}/dashboard?lang={language}", provider.base_url.trim_end_matches('/').trim_end_matches("/v1"))),
            _ => None,
        }
    }

    unsafe fn toggle_right_panel(&mut self) {
        if self.preview_file.is_some() {
            self.close_preview();
            return;
        }
        // (a button that seems to do nothing is worse than one that says why)
        if self.right_width == 0 && self.settings.right_panel && self.panel == Panel::Chat {
            self.say(t("The window is too narrow for the panel: make it wider."));
            return;
        }
        self.settings.right_panel = !self.settings.right_panel;
        self.save();
        self.layout();
    }

    unsafe fn open_find(&mut self) {
        if self.panel != Panel::Chat {
            self.set_panel(Panel::Chat);
        }
        if !self.has_messages {
            self.say(t("There is nothing to find in yet."));
            return;
        }
        self.find_open = true;
        self.layout();
        SetFocus(self.find_field);
        SendMessageW(self.find_field, EM_SETSEL, 0, -1);
        self.find_scan();
    }

    unsafe fn close_find(&mut self) {
        self.find_open = false;
        self.find_matches.clear();
        let none = CharRange { first: -1, last: 0 };
        SendMessageW(self.transcript, EM_EXSETSEL, 0, std::ptr::addr_of!(none) as LPARAM);
        self.layout();
        self.focus_composer();
    }

    /// Every place the words are, in order, as the transcript is now.
    unsafe fn find_scan(&mut self) {
        const MAX_MATCHES: usize = 9999;
        self.find_matches.clear();
        let query = get_text(self.find_field);
        if !query.trim().is_empty() {
            let text = wide(query.trim());
            let mut search = FindText { range: CharRange { first: 0, last: -1 }, text: text.as_ptr(), found: CharRange { first: 0, last: 0 } };
            while self.find_matches.len() < MAX_MATCHES && SendMessageW(self.transcript, EM_FINDTEXTEXW, FR_DOWN, std::ptr::addr_of_mut!(search) as LPARAM) >= 0 {
                self.find_matches.push((search.found.first, search.found.last));
                search.range.first = search.found.last.max(search.found.first + 1);
            }
        }
        self.find_at = self.find_at.min(self.find_matches.len().saturating_sub(1));
    }

    /// The words have changed: the first place they are.
    unsafe fn find_update(&mut self) {
        self.find_at = 0;
        self.find_scan();
        self.find_show();
    }

    /// `forward`: the next place, or else the one before; past the end it starts again.
    unsafe fn find_go(&mut self, forward: bool) {
        self.find_scan();
        let count = self.find_matches.len();
        if count > 0 {
            self.find_at = if forward { (self.find_at + 1) % count } else { (self.find_at + count - 1) % count };
        }
        self.find_show();
    }

    unsafe fn find_show(&mut self) {
        if let Some((first, last)) = self.find_matches.get(self.find_at).copied() {
            let range = CharRange { first, last };
            SendMessageW(self.transcript, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
            SendMessageW(self.transcript, EM_SCROLLCARET, 0, 0);
        }
        self.layout();
    }

    /// The session opened from a search shows where the word is.
    unsafe fn find_opened(&mut self) {
        let Some(word) = self.find_after_open.take() else { return };
        self.open_find();
        set_text(self.find_field, &word);
        self.find_update();
    }

    /// All sessions are read on another thread; an answer to words since changed is dropped.
    unsafe fn search_now(&mut self) {
        KillTimer(self.window, TIMER_SEARCH);
        self.side_query = get_text(self.side_search).trim().to_string();
        self.search_generation += 1;
        if self.side_query.is_empty() {
            self.side_hits = None;
            self.fill_side();
            return;
        }
        let (poster, folder, query, generation) = (self.poster.clone(), self.folder.join("sessions"), self.side_query.clone(), self.search_generation);
        std::thread::spawn(move || poster.send(Note::Found(generation, session::search(&folder, &query))));
    }

    /// The sidebar's list, read on another thread: there may be many sessions.
    /// The fields a page of this kind types in, empty and with their text hidden; the others are shut. Nothing typed on one of these pages stays in a field when it goes.
    unsafe fn crypt_fields_ready(&self, typed: usize) {
        for (at, field) in self.fields.iter().enumerate() {
            set_text(*field, "");
            SendMessageW(*field, EM_SETREADONLY, 0, 0);
            SendMessageW(*field, EM_SETPASSWORDCHAR, if at < typed { 0x2022 } else { 0 }, 0);
        }
    }

    /// What the work page says: how far a change has come, or how it ended.
    fn crypt_words(&self) -> String {
        match &self.crypt.work {
            Some(CryptWork { job, done, total }) if *total > 0 => match job {
                CryptJob::On | CryptJob::FinishOn => tf("Encrypting {0} of {1}...", &[done, total]),
                CryptJob::Off | CryptJob::FinishOff => tf("Decrypting {0} of {1}...", &[done, total]),
            },
            Some(CryptWork { job: CryptJob::On | CryptJob::FinishOn, .. }) => t("Encrypting...").to_string(),
            Some(_) => t("Decrypting...").to_string(),
            None => self.crypt.said.clone(),
        }
    }

    /// Whether a conversation is being written: what a change of every file would take the ground from.
    fn conversations_are_in_use(&self) -> bool {
        self.running || self.parked.iter().any(|parked| parked.running)
    }

    /// The two passphrases typed from this field on, if they are one and not empty.
    unsafe fn typed_passphrase(&mut self, first: usize) -> Option<String> {
        let (one, again) = (get_text(self.fields[first]), get_text(self.fields[first + 1]));
        if one.is_empty() {
            self.problem_in_panel(t(conversation_key::EMPTY_PASSPHRASE));
            return None;
        }
        if one != again {
            self.problem_in_panel(t("The two passphrases are not the same."));
            return None;
        }
        Some(one)
    }

    unsafe fn crypt_turn_on(&mut self) {
        let Some(passphrase) = self.typed_passphrase(0) else { return };
        if self.conversations_are_in_use() {
            self.problem_in_panel(t("A conversation is at work. Let it finish or stop it first: the files are written to while it goes."));
            return;
        }
        let remember = self.crypt.remember;
        self.start_crypt(CryptJob::On, passphrase, remember);
    }

    unsafe fn crypt_turn_off(&mut self) {
        if self.conversations_are_in_use() {
            self.problem_in_panel(t("A conversation is at work. Let it finish or stop it first: the files are written to while it goes."));
            return;
        }
        self.start_crypt(CryptJob::Off, String::new(), false);
    }

    /// A change of every conversation, off this thread: it takes as long as the conversations are many and long. The page that shows it has no way out until it is done.
    unsafe fn start_crypt(&mut self, job: CryptJob, passphrase: String, remember: bool) {
        self.crypt.work = Some(CryptWork { job, done: 0, total: 0 });
        self.crypt.said.clear();
        self.crypt.said_is_a_problem = false;
        self.crypt.code = None;
        self.crypt.back = if matches!(job, CryptJob::FinishOn | CryptJob::FinishOff) { Panel::Chat } else { Panel::Privacy };
        self.set_panel(Panel::CryptoWork);
        let (data, poster, policy, cipher) = (self.folder.clone(), self.poster.clone(), self.policy.clone(), self.crypt.cipher.clone());
        std::thread::spawn(move || {
            let progress_poster = poster.clone();
            let mut progress = move |done: usize, total: usize| progress_poster.send(Note::CryptProgress(done, total));
            let mut hooks = conversion::Hooks::reporting(&mut progress);
            let mut outcome = CryptOutcome { job, cipher: cipher.clone(), code: None, changed: 0, lost: Vec::new(), error: None };
            let locked = || conversion::Ended::Failed(conversation_key::LOCKED.to_string());
            let result = match job {
                CryptJob::On => conversion::begin_on(&data, &policy, &passphrase, remember).and_then(|(mut keystore, made, code)| {
                    // (the key and the code are the window's from here, whether or not what follows is done: a stop leaves the rest to the next start)
                    outcome.cipher = Some(made.clone());
                    outcome.code = Some(code);
                    conversion::finish_on(&data, &mut keystore, &made, &mut hooks)
                }),
                CryptJob::Off => cipher.as_ref().ok_or_else(locked).and_then(|cipher| conversion::turn_off(&data, &policy, cipher, &mut hooks)),
                CryptJob::FinishOn | CryptJob::FinishOff => cipher.as_ref().ok_or_else(locked).and_then(|cipher| conversion::finish_pending(&data, &policy, cipher, &mut hooks)).map(|done| done.map(|(_, report)| report).unwrap_or_default()),
            };
            match result {
                Ok(report) => (outcome.changed, outcome.lost) = (report.changed, report.lost),
                Err(error) => outcome.error = Some(error.to_string()),
            }
            poster.send(Note::CryptDone(outcome));
        });
    }

    /// A change of every conversation has ended: what the key file and the key are now, and what is said of it.
    unsafe fn crypt_finished(&mut self, outcome: CryptOutcome) {
        self.crypt.work = None;
        // (the key file as it is now: made, changed, or gone)
        self.crypt.keystore = Keystore::load(&self.folder).ok();
        self.crypt.cipher = if self.crypt.keystore.is_some() { outcome.cipher.clone() } else { None };
        if self.crypt.cipher.is_some() && outcome.job == CryptJob::On {
            self.crypt.code = outcome.code.clone();
        }
        if let Some(keystore) = &self.crypt.keystore {
            self.crypt.remember = keystore.is_remembered();
        }
        let mut said = match (&outcome.error, outcome.job) {
            // (no key was made: nothing was changed, and there is nothing for a next start to finish)
            (Some(error), CryptJob::On) if outcome.cipher.is_none() => t(error).to_string(),
            (Some(error), _) => tf("It stopped: {0}. What was changed stays changed, and the next start of Dogmind finishes the rest.", &[&t(error)]),
            (None, CryptJob::On | CryptJob::FinishOn) => tf("{0} conversations and notes are encrypted.", &[&outcome.changed]),
            (None, CryptJob::Off | CryptJob::FinishOff) => tf("{0} conversations and notes are plain again, and the key file is removed.", &[&outcome.changed]),
        };
        if !outcome.lost.is_empty() {
            said = format!("{said}\n{}\n{}", t("These could not be carried over, and were left out:"), outcome.lost.join("\n"));
        }
        self.crypt.said = said;
        self.crypt.said_is_a_problem = outcome.error.is_some() || !outcome.lost.is_empty();
        self.set_panel(Panel::CryptoWork);
    }

    /// Done: the recovery code is gone from the window, and the page it came from is back.
    unsafe fn crypt_done(&mut self) {
        self.crypt.code = None;
        self.crypt.said.clear();
        self.crypt_fields_ready(0);
        let back = self.crypt.back;
        self.crypt.back = Panel::Chat;
        self.set_panel(back);
        self.after_unlock();
    }

    /// What waited for the key: the list of conversations, the look at what earlier runs left writable, and the session the app was left with.
    unsafe fn after_unlock(&mut self) {
        if self.crypt.is_locked() || self.crypt.work.is_some() {
            return;
        }
        self.refresh_sessions();
        if std::mem::take(&mut self.crypt.sweep_due) {
            self.sweep_grants();
        }
        if let Some(session) = self.crypt.resume.take().filter(|session| session.is_file()) {
            self.open_session(session);
        }
    }

    /// What earlier runs left writable, now that what this one uses is counted: the workspaces of the conversations are among what is counted, so it waits for the key.
    unsafe fn sweep_grants(&self) {
        let (data, mut named, poster) = (self.folder.clone(), self.settings.workspaces.clone(), self.poster.clone());
        std::thread::spawn(move || {
            named.extend(session::list(&data.join("sessions")).into_iter().map(|summary| summary.workspace));
            named.sort();
            named.dedup();
            let left: Vec<String> = dogmind_agent::workspace_grants::sweep(&data, &named).iter().map(|folder| format!("{}: {}", folder.display(), t(dogmind_agent::workspace_grants::NOT_TAKEN_BACK))).collect();
            if !left.is_empty() {
                poster.send(Note::ReadFolderDone(left.join("\n"), true));
            }
        });
    }

    /// The key is in memory: every agent may open and write, and a change that was stopped is finished.
    unsafe fn unlocked(&mut self, cipher: Arc<Cipher>) {
        self.policy.set_on(cipher.clone());
        self.crypt.cipher = Some(cipher);
        self.crypt_fields_ready(0);
        match self.crypt.keystore.as_ref().and_then(Keystore::pending) {
            Some(pending) => self.start_crypt(if pending == Pending::Encrypt { CryptJob::FinishOn } else { CryptJob::FinishOff }, String::new(), false),
            None => {
                self.set_panel(Panel::Chat);
                self.after_unlock();
            }
        }
    }

    unsafe fn crypt_unlock(&mut self) {
        if self.crypt.damaged.is_some() {
            return;
        }
        let by_code = self.crypt.by_code;
        let result = if by_code {
            let Some(new) = self.typed_passphrase(1) else { return };
            let code = get_text(self.fields[0]);
            match self.crypt.keystore.as_mut() {
                Some(keystore) => keystore.reset_passphrase_with_recovery(&code, &new),
                None => return,
            }
        } else {
            let passphrase = get_text(self.fields[0]);
            match &self.crypt.keystore {
                Some(keystore) => keystore.unlock(&passphrase),
                None => return,
            }
        };
        match result {
            Ok(cipher) => {
                self.crypt.by_code = false;
                self.unlocked(cipher);
            }
            Err(error) => {
                set_text(self.fields[0], "");
                self.problem_in_panel(t(&error.to_string()));
            }
        }
    }

    unsafe fn crypt_change_passphrase(&mut self) {
        let Some(new) = self.typed_passphrase(0) else { return };
        let (Some(keystore), Some(cipher)) = (self.crypt.keystore.as_mut(), self.crypt.cipher.clone()) else { return };
        match keystore.change_passphrase(&cipher, &new) {
            Ok(()) => {
                self.set_panel(Panel::Privacy);
                self.say_in_panel(t("The passphrase is changed."));
            }
            Err(error) => self.problem_in_panel(t(&error.to_string())),
        }
    }

    unsafe fn crypt_new_code(&mut self) {
        let (Some(keystore), Some(cipher)) = (self.crypt.keystore.as_mut(), self.crypt.cipher.clone()) else { return };
        match keystore.new_recovery_code(&cipher) {
            Ok(code) => {
                self.crypt.code = Some(code);
                self.crypt.said = t("A new recovery code is made. The old one does not work any more.").to_string();
                self.crypt.said_is_a_problem = false;
                self.crypt.back = Panel::Privacy;
                self.set_panel(Panel::CryptoWork);
            }
            Err(error) => self.problem_in_panel(t(&error.to_string())),
        }
    }

    /// The box "Remember on this PC": on the page of the settings it is the key file's, and changes it at once; on the page that turns encryption on it is only the choice.
    unsafe fn crypt_remember_switched(&mut self) {
        let on = !self.crypt.remember;
        if self.panel == Panel::Privacy {
            let (Some(keystore), Some(cipher)) = (self.crypt.keystore.as_mut(), self.crypt.cipher.clone()) else { return };
            if let Err(error) = keystore.set_remembered(&cipher, on) {
                self.problem_in_panel(t(&error.to_string()));
                return;
            }
        }
        self.crypt.remember = on;
        self.update_toggles();
        self.layout();
    }

    unsafe fn refresh_sessions(&self) {
        let (poster, folder) = (self.poster.clone(), self.folder.join("sessions"));
        std::thread::spawn(move || poster.send(Note::Sessions(session::list(&folder))));
    }

    unsafe fn fill_side(&mut self) {
        let top = SendMessageW(self.side, LB_GETTOPINDEX, 0, 0);
        let mut rows = Vec::new();
        let groups = if self.side_hits.is_some() { Vec::new() } else { session::by_workspace(self.all_sessions.clone()) };
        let today = today_text();
        let pinned: Vec<session::Summary> = if self.side_hits.is_some() { Vec::new() } else {
            self.settings.pinned.iter().filter_map(|path| self.all_sessions.iter().find(|summary| summary.path == *path).cloned()).collect()
        };
        if !pinned.is_empty() {
            rows.push(SideRow::Pinned);
            for summary in pinned {
                let title = if summary.title.is_empty() { t("(no title)").to_string() } else { summary.title };
                rows.push(SideRow::Session { path: summary.path, title, when: short_when(&summary.created, &today) });
            }
        }
        if let Some(hits) = &self.side_hits {
            rows.extend(hits.iter().map(|hit| SideRow::Session { path: hit.path.clone(), title: if hit.title.is_empty() { t("(no title)").to_string() } else { hit.title.clone() }, when: hit.snippet.clone() }));
            if hits.is_empty() {
                rows.push(SideRow::Hint(t("No session has that").into()));
            }
        }
        for (workspace, summaries) in groups {
            let name = workspace.file_name().map_or_else(|| workspace.display().to_string(), |name| name.to_string_lossy().into_owned());
            if summaries.iter().all(|summary| self.settings.pinned.contains(&summary.path)) {
                continue;
            }
            let collapsed = self.collapsed.contains(&workspace.to_string_lossy().to_lowercase());
            rows.push(SideRow::Workspace { path: workspace, name, collapsed });
            if !collapsed {
                for summary in summaries.into_iter().filter(|summary| !self.settings.pinned.contains(&summary.path)) {
                    let title = if summary.title.is_empty() { t("(no title)").to_string() } else { summary.title };
                    rows.push(SideRow::Session { path: summary.path, title, when: short_when(&summary.created, &today) });
                }
            }
        }
        self.side_rows = rows;
        SendMessageW(self.side, LB_RESETCONTENT, 0, 0);
        for _ in &self.side_rows {
            SendMessageW(self.side, LB_ADDSTRING, 0, 0);
        }
        SendMessageW(self.side, LB_SETTOPINDEX, top.max(0) as WPARAM, 0);
        InvalidateRect(self.side, std::ptr::null(), 1);
    }

    /// A click in the sidebar: a session opens, a workspace folds, and the plus at its end starts a new session there.
    unsafe fn on_side_click(&mut self, index: usize, x: i32) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.side, &mut client);
        match self.side_rows.get(index).cloned() {
            Some(SideRow::Workspace { path, .. }) => {
                if x > client.right - scale(self.dpi, 34) {
                    if path.is_dir() {
                        self.start_session(path);
                    }
                } else {
                    let key = path.to_string_lossy().to_lowercase();
                    if !self.collapsed.remove(&key) {
                        self.collapsed.insert(key);
                    }
                    self.fill_side();
                }
            }
            // (the three dots a row shows under the pointer: what the right button offers)
            Some(SideRow::Session { .. }) if x > client.right - scale(self.dpi, 34) => self.on_side_menu(index),
            Some(SideRow::Session { path, .. }) => {
                self.find_after_open = self.side_hits.as_ref().and_then(|_| self.side_query.split_whitespace().next().map(str::to_string));
                self.open_session(path);
            }
            _ => {}
        }
    }

    /// A right click in the sidebar.
    unsafe fn on_side_menu(&mut self, index: usize) {
        self.side_target = Some(index);
        let items: Vec<(usize, String, bool)> = match self.side_rows.get(index) {
            Some(SideRow::Session { path, .. }) => {
                let pin = t(if self.settings.pinned.contains(path) { "Unpin" } else { "Pin" });
                vec![(M_OPEN_SESSION, t("Open").into(), false), (M_PIN_SESSION, pin.into(), false), (M_RENAME_SESSION, t("Rename...").into(), false), (0, String::new(), false), (M_DELETE_SESSION, t("Delete...").into(), false)]
            }
            Some(SideRow::Workspace { .. }) => vec![(M_NEW_THERE, t("New session here").into(), false)],
            _ => return,
        };
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        self.popup_at(point.x, point.y, TPM_TOPALIGN, &items);
    }

    unsafe fn on_session_menu(&mut self, id: usize) {
        let Some(row) = self.side_target.and_then(|at| self.side_rows.get(at)).cloned() else { return };
        match (id, row) {
            (M_OPEN_SESSION, SideRow::Session { .. }) => {
                if let Some(at) = self.side_target {
                    self.on_side_click(at, 0);
                }
            }
            (M_RENAME_SESSION, SideRow::Session { path, title, .. }) => {
                self.rename_target = Some(path);
                self.session_title_for_rename(title);
                self.set_panel(Panel::Rename);
            }
            (M_PIN_SESSION, SideRow::Session { path, .. }) => self.toggle_pin(path),
            (M_DELETE_SESSION, SideRow::Session { path, title, .. }) => self.ask_to_delete(path, title),
            (M_NEW_THERE, SideRow::Workspace { path, .. }) => {
                if path.is_dir() {
                    self.start_session(path);
                }
            }
            _ => {}
        }
    }

    /// Pinned sessions stand at the top of the sidebar, the latest pinned last.
    unsafe fn toggle_pin(&mut self, path: PathBuf) {
        match self.settings.pinned.iter().position(|known| *known == path) {
            Some(at) => {
                self.settings.pinned.remove(at);
            }
            None => self.settings.pinned.push(path),
        }
        self.save();
        self.fill_side();
    }

    unsafe fn ask_to_delete(&mut self, path: PathBuf, title: String) {
        let working = self.parked.iter().any(|parked| parked.path.as_ref() == Some(&path)) || (self.running && self.current_session.as_ref() == Some(&path));
        if working {
            self.say(t("That session is at work: stop it first."));
            return;
        }
        self.delete_target = Some((path, title));
        self.set_panel(Panel::ConfirmDelete);
    }

    unsafe fn session_title_for_rename(&mut self, title: String) {
        if self.current_session != self.rename_target {
            self.rename_seed = Some(title);
        }
    }

    unsafe fn draw_suggestion(&self, item: &DRAWITEMSTRUCT) {
        let device = item.hDC;
        FillRect(device, &item.rcItem, self.brushes[2]);
        let Some(row) = self.suggestions.get(item.itemID as usize) else { return };
        let area = item.rcItem;
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        if item.itemState & ODS_SELECTED != 0 || self.suggest_hover == Some(item.itemID as usize) {
            rounded(device, RECT { left: area.left + s(ds::space::HAIR), right: area.right - s(ds::space::HAIR), top: area.top + s(ds::space::PIXEL), bottom: area.bottom - s(ds::space::PIXEL) }, s(ds::radius::CONTROL), Some(theme::raised()), None);
        }
        let split = s(180);
        draw_text(device, &row.title, RECT { left: area.left + s(ds::space::M), right: if row.detail.is_empty() { area.right - s(ds::space::M) } else { area.left + split }, ..area }, self.fonts.body, theme::text(), ONE_LINE);
        draw_text(device, &row.detail, RECT { left: area.left + split, right: area.right - s(ds::space::M), ..area }, self.fonts.small, theme::muted(), ONE_LINE);
    }

    unsafe fn draw_side_row(&self, item: &DRAWITEMSTRUCT) {
        let device = item.hDC;
        FillRect(device, &item.rcItem, self.brushes[1]);
        let Some(row) = self.side_rows.get(item.itemID as usize) else { return };
        let area = item.rcItem;
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let hover = self.side_hover == Some(item.itemID as usize);
        match row {
            SideRow::Workspace { name, collapsed, .. } => {
                let across = text_width(self.fonts.small, name).min(area.right - area.left - s(72));
                let x = area.left + s(ds::space::M);
                draw_text(device, name, RECT { left: x, right: x + across + s(ds::space::HAIR), ..area }, self.fonts.small, theme::muted(), ONE_LINE);
                if *collapsed || hover {
                    icon::draw(device, if *collapsed { "chevron-right" } else { "chevron-down" }, RECT { left: x + across + s(ds::space::XS), right: x + across + s(20), ..area }, s(ds::size::ICON_SMALL), theme::faint());
                }
                if hover {
                    let plus = RECT { left: area.right - s(ds::size::ROW_ACTION), right: area.right - s(ds::space::XS), top: area.top + s(ds::space::HAIR), bottom: area.bottom - s(ds::space::HAIR) };
                    rounded(device, plus, s(ds::radius::SMALL), Some(theme::surface()), None);
                    icon::draw(device, "plus", plus, s(ds::size::ICON_SMALL), theme::text());
                }
            }
            SideRow::Session { path, title, when } => {
                let current = self.current_session.as_ref() == Some(path);
                if current || hover {
                    let shade = RECT { left: area.left, right: area.right, top: area.top + s(ds::space::PIXEL), bottom: area.bottom - s(ds::space::PIXEL) };
                    rounded(device, shade, s(ds::radius::CONTROL), Some(if current { theme::raised() } else { theme::surface() }), None);
                }
                let parked = self.parked.iter().find(|parked| parked.path.as_ref() == Some(path));
                // How it stands, in a word, where its time would be; only one that needs saying has one.
                let state = side_state(parked.map(|parked| (parked.pending.is_some() || parked.question.is_some() || parked.plan_review.is_some(), parked.running)), current, self.waits_for_person(), self.running, !self.waiting.is_empty(), self.failed_sessions.contains(path)).map(|state| match state {
                    SideState::Waiting => (t("Waiting"), theme::warn()),
                    SideState::Working => (t("Working"), theme::accent()),
                    SideState::StandingBy => (t("Standing by"), theme::muted()),
                    SideState::Error => (t("Error"), theme::bad()),
                });
                let left = area.left + s(ds::space::M);
                let found = self.side_hits.is_some();
                let mut text_right = area.right - s(ds::space::M);
                if hover {
                    icon::draw(device, "ellipsis", RECT { left: area.right - s(ds::size::ROW_ACTION), right: area.right - s(ds::space::XS), ..area }, s(ds::size::ICON), theme::muted());
                    text_right = area.right - s(ds::space::XXL);
                } else if let Some((word, color)) = state {
                    let across = text_width(self.fonts.small, word);
                    draw_text(device, word, RECT { left: area.right - s(ds::space::M) - across, right: area.right - s(ds::space::M), ..area }, self.fonts.small, color, DT_RIGHT | ONE_LINE);
                    text_right = area.right - s(ds::space::M) - across - s(ds::space::S);
                } else if !found {
                    let across = text_width(self.fonts.small, when);
                    draw_text(device, when, RECT { left: area.right - s(ds::space::M) - across, right: area.right - s(ds::space::M), ..area }, self.fonts.small, theme::faint(), DT_RIGHT | ONE_LINE);
                    text_right = area.right - s(ds::space::M) - across - s(ds::space::S);
                }
                let color = if current { theme::text() } else { theme::soft() };
                if found {
                    let middle = (area.top + area.bottom) / 2;
                    draw_text(device, title, RECT { left, right: text_right, top: area.top + s(3), bottom: middle + s(ds::space::PIXEL) }, self.fonts.body, color, ONE_LINE);
                    draw_text(device, when, RECT { left, right: text_right, top: middle, bottom: area.bottom - s(3) }, self.fonts.small, theme::faint(), ONE_LINE);
                } else {
                    draw_text(device, title, RECT { left, right: text_right, ..area }, self.fonts.body, color, ONE_LINE);
                }
            }
            SideRow::Pinned => draw_text(device, t("Pinned"), RECT { left: area.left + s(ds::space::M), ..area }, self.fonts.small, theme::muted(), ONE_LINE),
            SideRow::Hint(text) => draw_text(device, text, RECT { left: area.left + s(ds::space::S), ..area }, self.fonts.small, theme::faint(), ONE_LINE),
        }
    }

    unsafe fn draw_list_row(&self, item: &DRAWITEMSTRUCT) {
        let device = item.hDC;
        FillRect(device, &item.rcItem, self.brushes[2]);
        let Some(row) = self.list_rows.get(item.itemID as usize) else { return };
        let area = item.rcItem;
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let selected = item.itemState & ODS_SELECTED != 0;
        if self.panel == Panel::Diff {
            FillRect(device, &area, self.brushes[1]);
            let hover = self.list_hover == Some(item.itemID as usize);
            if selected || hover {
                rounded(device, RECT { left: area.left, right: area.right, top: area.top + s(ds::space::PIXEL), bottom: area.bottom - s(ds::space::PIXEL) }, s(ds::radius::CONTROL), Some(if selected { theme::raised() } else { theme::surface() }), None);
            }
            let (added, removed) = self.changes.get(item.itemID as usize).map_or((0, 0), |change| (change.added, change.removed));
            let mut right = area.right - s(ds::space::M);
            if removed > 0 {
                let words = format!("−{removed}");
                let across = text_width(self.fonts.small, &words);
                draw_text(device, &words, RECT { left: right - across, right, ..area }, self.fonts.small, theme::bad(), DT_RIGHT | ONE_LINE);
                right -= across + s(ds::space::XS);
            }
            if added > 0 {
                let words = format!("+{added}");
                let across = text_width(self.fonts.small, &words);
                draw_text(device, &words, RECT { left: right - across, right, ..area }, self.fonts.small, theme::good(), DT_RIGHT | ONE_LINE);
                right -= across + s(ds::space::XS);
            }
            let name = row.title.rsplit(['/', '\\']).next().unwrap_or(&row.title);
            draw_text(device, name, RECT { left: area.left + s(ds::space::M), right, ..area }, self.fonts.body, if selected { theme::text() } else { theme::soft() }, ONE_LINE);
            return;
        }
        if selected || self.list_hover == Some(item.itemID as usize) {
            rounded(device, RECT { left: area.left + s(ds::space::HAIR), right: area.right - s(ds::space::HAIR), top: area.top + s(ds::space::PIXEL), bottom: area.bottom - s(ds::space::PIXEL) }, s(ds::radius::CONTROL), Some(theme::raised()), None);
        }
        let mut left = area.left + s(ds::space::M);
        if row.active {
            let dot = s(ds::space::S);
            rounded(device, rect(left, (area.top + area.bottom - dot) / 2, dot, dot), dot / 2, Some(theme::accent()), None);
            left += dot + s(ds::space::S);
        }
        let middle = (area.top + area.bottom) / 2;
        // (a model short of graphics memory here: greyed, and there to choose)
        let short = self.panel == Panel::Models && self.catalog.get(item.itemID as usize).is_some_and(|entry| !entry.runs);
        let (title_color, detail_color) = if short { (theme::faint(), theme::faint()) } else { (theme::text(), theme::muted()) };
        if row.detail.is_empty() {
            draw_text(device, &row.title, RECT { left, right: area.right - s(ds::space::M), ..area }, self.fonts.body, title_color, ONE_LINE);
        } else {
            draw_text(device, &row.title, RECT { left, right: area.right - s(ds::space::M), top: area.top + s(ds::space::XS), bottom: middle + s(ds::space::PIXEL) }, self.fonts.body, title_color, ONE_LINE);
            draw_text(device, &row.detail, RECT { left, right: area.right - s(ds::space::M), top: middle, bottom: area.bottom - s(ds::space::XS) }, self.fonts.small, detail_color, ONE_LINE);
        }
    }

    unsafe fn list_selection(&self) -> Option<usize> {
        let at = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        (at >= 0).then_some(at as usize)
    }

    unsafe fn fill_providers(&mut self) {
        self.list_rows = self.settings.providers.iter().map(|provider| ListRow {
            title: provider.name.clone(),
            detail: if provider.kind == Kind::Server { provider.base_url.clone() } else { t(provider.kind.label()).to_string() },
            active: provider.name == self.settings.active,
        }).collect();
        self.fill_list();
        let active = self.settings.providers.iter().position(|provider| provider.name == self.settings.active);
        self.select_provider(self.selected_provider.or(active));
    }

    unsafe fn select_provider(&mut self, at: Option<usize>) {
        // (what was typed for the one that was chosen is not left behind by choosing another)
        if self.panel == Panel::Providers && at != self.selected_provider && self.selected_provider.is_some() && !self.leave_page() {
            SendMessageW(self.list, LB_SETCURSEL, self.selected_provider.unwrap_or(0), 0);
            return;
        }
        self.selected_provider = at;
        let Some(at) = at.filter(|&at| at < self.settings.providers.len()) else { return };
        SendMessageW(self.list, LB_SETCURSEL, at, 0);
        let provider = self.settings.providers[at].clone();
        self.kind = provider.kind;
        set_text(self.kind_chip, t(provider.kind.label()));
        self.show_provider_fields(&provider);
    }

    /// A provider's fields, as far as its kind has any.
    unsafe fn show_provider_fields(&mut self, provider: &ProviderConfig) {
        set_text(self.fields[0], &provider.name);
        set_text(self.fields[1], &provider.base_url);
        set_text(self.fields[2], &provider.model);
        self.show_key_setting(&provider.api_key);
        self.panel_log.clear();
        self.layout();
    }

    /// What the key's setting will be: under the name of the one it has, if it has one, or a new one.
    fn provider_key_target(&self, name: &str) -> String {
        let known = self.selected_provider.and_then(|at| self.settings.providers.get(at));
        if let Some(KeySource::Stored(target)) = known.map(|provider| KeySource::of(&provider.api_key)) {
            return target;
        }
        let in_use: Vec<&str> = self.settings.providers.iter().map(|provider| provider.api_key.as_str()).chain([self.settings.search_key.as_str()]).collect();
        settings::new_key_target(name, &in_use)
    }

    unsafe fn provider_from_fields(&self) -> ProviderConfig {
        let name = get_text(self.fields[0]).trim().to_string();
        if matches!(self.kind, Kind::Embedded | Kind::Codex) {
            let model = self.selected_provider.and_then(|at| self.settings.providers.get(at)).filter(|known| known.kind == self.kind).map_or(String::new(), |known| known.model.clone());
            return ProviderConfig { name, kind: self.kind, base_url: String::new(), model, api_key: String::new() };
        }
        let current = self.selected_provider.and_then(|at| self.settings.providers.get(at)).map_or(String::new(), |known| known.api_key.clone());
        let (variable, typed) = (get_text(self.fields[VARIABLE_FIELD]), get_text(self.fields[KEY_FIELD]));
        let api_key = key_setting(&current, self.variable_on, variable.trim(), typed.trim(), &self.provider_key_target(&name));
        ProviderConfig { base_url: get_text(self.fields[1]).trim().to_string(), model: get_text(self.fields[2]).trim().to_string(), name, kind: self.kind, api_key }
    }

    unsafe fn say_in_panel(&mut self, text: &str) {
        self.panel_log = text.to_string();
        self.panel_problem = false;
        self.layout();
    }

    /// The same for what went wrong: it stands out, and in a panel scrolled from its foot it comes into view.
    unsafe fn problem_in_panel(&mut self, text: &str) {
        self.panel_log = text.to_string();
        self.panel_problem = true;
        self.panel_scroll = self.panel_extent;
        self.layout();
    }

    /// What Claude's and Codex's settings list goes in with this app's, and how that went.
    unsafe fn mcp_servers_imported(&self) -> String {
        let file = self.folder.join("mcp.json");
        let home = std::env::var_os("USERPROFILE").map_or(PathBuf::new(), PathBuf::from);
        let mut found = dogmind_agent::mcp::servers_of_claude(&std::fs::read_to_string(home.join(".claude.json")).unwrap_or_default());
        for (name, server) in dogmind_agent::mcp::servers_of_codex(&std::fs::read_to_string(home.join(".codex").join("config.toml")).unwrap_or_default()) {
            found.entry(name).or_insert(server);
        }
        match dogmind_agent::mcp::import(&file, found) {
            Ok(added) => tf("{0} servers added to {1}. They start the next time this app does.", &[&added, &file.display()]),
            Err(error) => error,
        }
    }

    unsafe fn import_mcp_servers(&mut self) {
        let said = self.mcp_servers_imported();
        self.say_in_panel(&said);
    }

    /// How high what shows of a panel is.
    unsafe fn panel_view(&self) -> i32 {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(if self.modal_open { self.pane } else { self.window }, &mut client);
        client.bottom
    }

    /// Scrolls a panel taller than the window.
    unsafe fn scroll_panel(&mut self, by: i32) {
        self.scroll_panel_to(self.panel_scroll + by);
    }

    /// The same, to a place. A dialog's page is laid out again alone: the window behind it does not change as it scrolls, and laying all of
    /// it out again for every notch of the wheel made the page slow to follow.
    unsafe fn scroll_panel_to(&mut self, to: i32) {
        let most = (self.panel_extent - self.panel_view()).max(0);
        let to = to.clamp(0, most);
        if to == self.panel_scroll {
            return;
        }
        self.panel_scroll = to;
        if self.modal_open {
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(self.window, &mut client);
            self.layout_modal(client.right, client.bottom);
        } else {
            self.layout();
        }
    }

    /// The page where its thumb, held this far from its top, is at the pointer.
    unsafe fn drag_pane_thumb(&mut self, y: i32) {
        let (Some((thumb, view, most)), Some(grab)) = (self.pane_thumb, self.pane_thumb_grab) else { return };
        let tall = thumb.bottom - thumb.top;
        let to = (i64::from(y - grab) * i64::from(most) / i64::from((view - tall).max(1))) as i32;
        self.scroll_panel_to(to);
    }

    /// A control Tab has reached comes into view.
    unsafe fn scroll_to(&mut self, control: HWND) {
        if self.panel == Panel::Chat {
            return;
        }
        // (the list of a dialog and its close button do not scroll)
        if self.modal_open && GetParent(control) != self.pane {
            return;
        }
        let view = self.panel_view();
        let (_, top, _, height) = child_area(control, if self.modal_open { self.pane } else { self.window });
        let margin = scale(self.dpi, 12);
        if top < 0 {
            self.scroll_panel(top - margin);
        } else if top + height > view {
            self.scroll_panel(top + height - view + margin);
        }
    }

    /// The chosen provider, a place up or down the list. Which one is in use does not change.
    unsafe fn move_provider(&mut self, by: i32) {
        let count = self.settings.providers.len() as i32;
        let Some(at) = self.selected_provider.filter(|&at| (at as i32) < count) else {
            self.problem_in_panel(t("Choose a connection to move."));
            return;
        };
        let to = at as i32 + by;
        if !(0..count).contains(&to) {
            return;
        }
        self.settings.providers.swap(at, to as usize);
        self.selected_provider = Some(to as usize);
        self.save();
        self.list_rows.swap(at, to as usize);
        self.fill_list();
        SendMessageW(self.list, LB_SETCURSEL, to as usize, 0);
    }

    unsafe fn save_provider(&mut self) -> bool {
        let provider = self.provider_from_fields();
        if let Some(problem) = provider.problem() {
            self.problem_in_panel(&problem);
            return false;
        }
        let clash = self.settings.providers.iter().enumerate().any(|(at, known)| known.name == provider.name && Some(at) != self.selected_provider);
        if clash {
            self.problem_in_panel(t("Another connection has that name."));
            return false;
        }
        // The key goes to the store, and from here on nothing holds it but the field, which is emptied.
        let typed = get_text(self.fields[KEY_FIELD]).trim().to_string();
        if let (KeySource::Stored(target), false) = (KeySource::of(&provider.api_key), typed.is_empty()) {
            if let Err(error) = crate::keys::store().write(&target, &typed) {
                self.problem_in_panel(&error);
                return false;
            }
            set_text(self.fields[KEY_FIELD], "");
        }
        // One that went from the store to a variable leaves nothing behind. (If it could not be taken away, that is said, once the page says it is saved.)
        let before = self.selected_provider.and_then(|at| self.settings.providers.get(at)).map(|known| known.api_key.clone());
        let left_behind = before.filter(|_| !matches!(KeySource::of(&provider.api_key), KeySource::Stored(_))).and_then(|setting| settings::remove_key(crate::keys::store(), &setting));
        match self.selected_provider.filter(|&at| at < self.settings.providers.len()) {
            Some(at) => {
                if self.settings.active == self.settings.providers[at].name {
                    self.settings.active = provider.name.clone();
                }
                self.settings.providers[at] = provider;
            }
            None => {
                self.settings.providers.push(provider);
                self.selected_provider = Some(self.settings.providers.len() - 1);
            }
        }
        self.save();
        self.fill_providers();
        match left_behind {
            Some(words) => self.problem_in_panel(&words),
            None => self.say_in_panel(t("Saved.")),
        }
        true
    }

    /// The person has begun a message: the engine starts now, and has the prompt read by the time it is sent. Never at launch: nothing is asked of the GPU until
    /// someone has something to say.
    unsafe fn message_begun(&mut self) {
        if starts_the_engine(self.engine_waits, self.setup_holds_off_the_engine(), self.engine_failed_at.map(|at| at.elapsed())) {
            self.connect();
        }
    }

    /// Starts using the active provider: stops what the last one needed and connects on another thread.
    unsafe fn connect(&mut self) {
        // (an engine that starts reads the settings as they are now)
        self.drafter_restart_waits = false;
        self.engine_waits = false;
        self.engine_stop_told = false;
        self.connect_failed = false;
        KillTimer(self.window, TIMER_RECONNECT);
        if !self.connect_retrying {
            self.last_connect_error.clear();
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.connected = false;
        self.vision = None;
        self.straydog = false;
        self.model.clear();
        self.provider_models.clear();
        self.rate = None;
        self.window_tokens = 0;
        let _ = self.agent.commands.send(Command::SetSecrets(key_files(&self.settings)));
        let Some(provider) = self.settings.active_provider().cloned() else { return };
        if provider.kind != Kind::Embedded {
            self.engine = None;
        } else if self.setup_holds_off_the_engine() {
            // It would want the files the setup is working on. When the setup ends, it connects.
            self.connection = "setting up the model".into();
            self.update_status();
            return;
        }
        self.connection = "connecting".into();
        self.connect_began = Some(std::time::Instant::now());
        self.connect_waiting_since = None;
        self.update_status();
        let (poster, generations) = (self.poster.clone(), self.generation.clone());
        let current = move || generations.load(Ordering::SeqCst) == generation;
        match provider.kind {
            Kind::Anthropic => {
                std::thread::spawn(move || {
                    let result = (|| {
                        let key = provider.key()?.ok_or(t("Claude needs an API key"))?;
                        let url = if provider.base_url.is_empty() { anthropic::DEFAULT_URL } else { &provider.base_url };
                        let client = Anthropic::new(url, key)?;
                        let model = if provider.model.is_empty() { anthropic::DEFAULT_MODEL.to_string() } else { provider.model.clone() };
                        let window = client.context_of(&model)?;
                        Ok::<_, String>((client, model, window))
                    })();
                    poster.send(match result {
                        Ok((client, model, window)) => Note::Connected { generation, model, window, straydog: false, vision: None, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
                    });
                });
            }
            Kind::Embedded => {
                let Some(straydog) = engine::find(&self.settings) else {
                    self.connection = "no engine".into();
                    // (once, however many times it is tried)
                    let words = format!("{}\n", engine::not_found_words());
                    if !matches!(self.shown.last(), Some(Shown::Text(held, _, _)) if held.ends_with(&words)) {
                        self.end_line();
                        self.append(&words, Style::Warn);
                    }
                    // (sending says this again, where it would otherwise wait for a connection no one is making)
                    self.connect_failed = true;
                    self.queued_back_to_composer();
                    self.update_status();
                    return;
                };
                if !engine::is_set_up(&self.folder) {
                    self.connection = "no model yet".into();
                    self.connect_failed = true;
                    self.update_status();
                    self.set_panel(Panel::Models);
                    return;
                }
                self.engine = None;
                let (settings, folder) = (self.settings.clone(), self.folder.clone());
                std::thread::spawn(move || {
                    let mut say = |text: String| poster.send(Note::Status(generation, text));
                    let mut client = match engine::start_alone(&straydog, &settings, &folder, &current, &mut say) {
                        Ok(started) => {
                            let client = engine::client(started.port);
                            poster.send(Note::EngineStarted(generation, started));
                            client
                        }
                        Err(error) => {
                            poster.send(Note::ConnectFailed(generation, tf("the engine did not start: {0}", &[&error])));
                            return;
                        }
                    };
                    let mut say = |text: String| poster.send(Note::Status(generation, text));
                    let result = engine::wait_ready(&client, &current, &mut say).and_then(|()| client.describe());
                    if let Ok(info) = &result {
                        client.suppresses_letters = info.suppresses_letters;
                    }
                    poster.send(match result {
                        Ok(info) => Note::Connected { generation, model: if provider.model.is_empty() { info.model } else { provider.model }, window: info.context, straydog: info.straydog, vision: info.vision, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, engine::in_plain_words(&error)),
                    });
                });
            }
            Kind::Server => {
                std::thread::spawn(move || {
                    let result = (|| {
                        let mut client = Client::new(&provider.base_url, provider.key()?)?;
                        // A server that has no /health still answers its models.
                        if let Ok(Health::Loading(_)) = client.health() {
                            let mut say = |text: String| poster.send(Note::Status(generation, text));
                            engine::wait_ready(&client, &current, &mut say)?;
                        }
                        let info = client.describe()?;
                        client.replay_reasoning = info.straydog;
                        client.suppresses_letters = info.suppresses_letters;
                        Ok::<_, String>((client, info))
                    })();
                    poster.send(match result {
                        Ok((client, info)) => Note::Connected { generation, model: if provider.model.is_empty() { info.model } else { provider.model }, window: info.context, straydog: info.straydog, vision: info.vision, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
                    });
                });
            }
            Kind::Codex => {
                crate::machine::refresh_path();
                std::thread::spawn(move || {
                    let client = Codex::default();
                    let models = client.models().map_err(|error| error.to_string());
                    if let Ok(models) = &models {
                        poster.send(Note::Models(generation, models.clone()));
                    }
                    // (one that has been chosen is tried whatever the list said, and says for itself if it has gone)
                    let model = if provider.model.is_empty() {
                        models.and_then(|models| models.into_iter().next().ok_or_else(|| t("this plan offers no models").to_string()))
                    } else {
                        Ok(provider.model)
                    };
                    poster.send(match model {
                        Ok(model) => Note::Connected { generation, model, window: codex::context_fallback(), straydog: false, vision: None, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
                    });
                });
            }
        }
    }

    unsafe fn send_message(&mut self) {
        self.send_typed(true);
    }

    /// What is typed goes. `interjects`: while the model works, words reach it at its next step (Enter); otherwise they wait for the work to end
    /// (Ctrl+Enter), to be sent into it from their row if wanted.
    unsafe fn send_typed(&mut self, interjects: bool) {
        let text = get_text(self.composer);
        let text = text.trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if reads_as_command(&text) {
            set_text(self.composer, "");
            if !self.run_command(&text) {
                // (nothing written is lost for a slip of the first word)
                set_text(self.composer, &text);
                SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
                self.say(&tf("No such command: {0}. /help lists them.", &[&text.split_whitespace().next().unwrap_or("")]));
            }
            return;
        }
        set_text(self.composer, "");
        self.composer_typed_at = None;
        // (a plan waits for a decision: yes carries it out, anything else is what to change; after Change, whatever is written is)
        if let Some(asked) = &self.plan_review {
            let decision = if !asked.changing && cards::approves(&text) { Ok(()) } else { Err(text.clone()) };
            if self.decide_plan(decision) {
                return;
            }
        }
        // (the model's question waits: what is typed is its answer, a choice's number standing for the choice)
        if let Some(asked) = &self.question {
            let labels: Vec<String> = asked.options.iter().map(|(label, _)| label.clone()).collect();
            let answer = cards::answer_text(&labels, &text);
            if self.answer_question(&answer) {
                return;
            }
        }
        let images = std::mem::take(&mut self.attachments);
        self.show_attachments();
        self.send_or_wait(Outgoing { card: text.clone(), text, images }, interjects);
    }

    /// To the model now if it can take it, and otherwise when it can, shown above the input until then: a card in the transcript would stand in the
    /// middle of an answer, or stay there for a message that goes back in the box. `interjects`: while it works this reaches it at its next step.
    unsafe fn send_or_wait(&mut self, out: Outgoing, interjects: bool) {
        if !out.images.is_empty() {
            self.tell_about_vision();
        }
        // (what reaches a turn under way is words: pictures wait for it to end)
        if self.running && self.connected && interjects && out.images.is_empty() {
            self.agent.interject.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(out.text.clone());
            self.interjected.push(out.text);
            self.layout();
            return;
        }
        if self.running || !self.connected {
            let running = self.running;
            self.queued.push(out);
            if !running {
                if self.engine_waits || self.connect_failed {
                    self.connect();
                }
                // (setting up, or no engine: connect has gone somewhere else, and what is said there stands)
                if self.waiting_to_connect() {
                    self.set_activity(t(self.waiting_words()).into());
                    self.update_buttons();
                }
            }
            self.layout();
            return;
        }
        self.dispatch(out);
    }

    unsafe fn dispatch(&mut self, out: Outgoing) {
        // Whatever the view was on, what has just been said and what answers it are what comes next.
        self.follow_end = true;
        self.scrolled_up_at = None;
        self.want_frames();
        let shown: Vec<(PathBuf, (i32, i32))> = out.images.iter().map(|attached| (attached.path.clone(), (attached.picture.width as i32, attached.picture.height as i32))).collect();
        self.show_user(&out.card, &shown, &clock_now());
        // (from now: behind a warm-up its turn may not begin for most of a minute)
        self.set_activity(t("Thinking…").into());
        self.running = true;
        self.stop_asked = false;
        self.update_buttons();
        let _ = self.agent.commands.send(send_command(out));
    }

    /// What waits above the input, in the order it will go: its words, when it goes, and whether it can go into the work under way now.
    fn waiting_rows(&self) -> Vec<(String, &'static str, bool)> {
        let then = if self.running { "after this work" } else if self.waiting_words() == "Connecting…" { "when connected" } else { "when the engine is ready" };
        // (what reaches a turn under way is words: pictures wait for it to end)
        let now = self.running && self.connected;
        self.interjected.iter().map(|text| (waiting_words(text, 0), "at its next step", false))
            .chain(self.queued.iter().map(|item| (waiting_words(&item.card, item.images.len()), then, now && item.images.is_empty()))).collect()
    }

    /// A button on a waiting message.
    unsafe fn queue_act(&mut self, at: usize, act: QueueAct) {
        match act {
            QueueAct::Now => self.send_into_work(at),
            QueueAct::Edit => self.call_back(at),
            QueueAct::Drop => {
                self.take_waiting(at);
                self.after_waiting_changed();
            }
        }
    }

    /// A message waiting for the work under way to end goes into it at its next step instead.
    unsafe fn send_into_work(&mut self, at: usize) {
        let Some(place) = at.checked_sub(self.interjected.len()) else { return };
        if self.running && self.connected && self.queued.get(place).is_some_and(|item| item.images.is_empty()) {
            let item = self.queued.remove(place);
            self.send_or_wait(item, true);
        }
        self.after_waiting_changed();
    }

    /// A waiting message taken out of the line: its words and pictures, or None for words that have just reached the turn, past calling back.
    fn take_waiting(&mut self, at: usize) -> Option<(String, Vec<Attached>)> {
        if at < self.interjected.len() {
            let words = self.interjected.remove(at);
            let mut waiting = self.agent.interject.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let place = waiting.iter().position(|known| *known == words)?;
            waiting.remove(place);
            Some((words, Vec::new()))
        } else if at - self.interjected.len() < self.queued.len() {
            let item = self.queued.remove(at - self.interjected.len());
            Some((item.text, item.images))
        } else {
            None
        }
    }

    unsafe fn after_waiting_changed(&mut self) {
        if self.queued.is_empty() && !self.running {
            self.set_activity(String::new());
        }
        self.update_buttons();
        self.layout();
        self.focus_composer();
    }

    /// The pencil on a waiting message: it does not go, and comes back to the box to be changed. One that has just reached the turn is past calling
    /// back.
    unsafe fn call_back(&mut self, at: usize) {
        if let Some((text, images)) = self.take_waiting(at) {
            let draft = get_text(self.composer);
            self.restoring_draft = true;
            set_text(self.composer, &if draft.trim().is_empty() { text } else { format!("{text}\r\n{draft}") });
            self.restoring_draft = false;
            SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
            for image in images {
                if !self.attachments.iter().any(|attached| attached.path == image.path) && self.attachments.len() < attach::MOST_IMAGES {
                    self.attachments.push(image);
                }
            }
        }
        self.show_attachments();
        self.after_waiting_changed();
    }

    /// Back to what is being written, unless a dialog is open: its keys stay with it.
    unsafe fn focus_composer(&self) {
        if !self.modal_open {
            SetFocus(self.composer);
        }
    }

    unsafe fn update_buttons(&self) {
        let stoppable = is_stoppable(self.running, self.asking(), self.waiting_to_connect());
        set_text(self.send, if stoppable { "square" } else { "arrow-up" });
        if stoppable {
            SetTimer(self.window, TIMER_DOTS, SPINNER_MS, None);
        } else {
            KillTimer(self.window, TIMER_DOTS);
        }
        self.update_send_enabled();
        InvalidateRect(self.window, &self.status_area, 0);
    }

    /// Nothing to send, nothing to stop: the button says so.
    unsafe fn update_send_enabled(&self) {
        let can = (self.running && !self.asking()) || self.waiting_to_connect() || !self.attachments.is_empty() || GetWindowTextLengthW(self.composer) > 0;
        EnableWindow(self.send, i32::from(can));
    }

    unsafe fn answer_approval(&mut self, decision: Decision) {
        let Some(approval) = self.pending.take() else { return };
        let feedback = get_text(self.feedback);
        set_text(self.feedback, "");
        let _ = approval.answer.send((decision, (!feedback.trim().is_empty()).then_some(feedback)));
        self.layout();
        self.focus_composer();
    }

    unsafe fn show_transcript(&mut self, messages: &[Message]) {
        set_text(self.transcript, "");
        self.shown.clear();
        self.answers.clear();
        self.user_cards.clear();
        self.rail_turns.clear();
        self.turn_words_due = false;
        self.picture_blocks.clear();
        self.blocks.clear();
        self.card_slots.clear();
        self.open_block = None;
        self.links.clear();
        self.link_hover = None;
        self.runs.clear();
        self.run_open = None;
        self.run_carry.clear();
        self.asked_calls.clear();
        self.run_hover = None;
        self.question = None;
        self.plan_review = None;
        self.turn_slots.clear();
        self.turn_end_slot = 0;
        self.turn_cards_waiting.clear();
        self.turn_cards_came.clear();
        // (a file that had gone may be back: it is looked for again)
        self.thumbs.retain(|_, kept| !matches!(kept.thumb, Thumb::Missing(_)));
        self.has_messages = false;
        if messages.is_empty() {
            self.greeting = next_greeting(self.greeting, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.subsec_nanos() as u64));
        }
        self.reveal.clear();
        self.follow_end = true;
        self.line_start = true;
        self.in_code = false;
        self.pending_text.clear();
        self.table.clear();
        self.heading = 0;
        self.bold = false;
        self.italic = false;
        self.struck = false;
        self.quote = false;
        let mut told: Vec<RunEntry> = Vec::new();
        // All of it at once, and nothing drawn until it is in: each piece asking where the view is, moving it back and saying what to draw again took half a second over a few long answers.
        self.opening = true;
        SendMessageW(self.transcript, WM_SETREDRAW, 0, 0);
        hold_layout(self.transcript_document, true);
        let results: HashMap<&str, &Message> = messages.iter().filter_map(|message| message.tool_call_id.as_deref().map(|id| (id, message))).collect();
        // (the questions and plans it holds, as cards, each with what came of it)
        let kept_cards = cards::of_messages(messages, self.card_next + 1);
        self.card_next += kept_cards.len() as u64;
        for (index, message) in messages.iter().enumerate() {
            match message.role.as_str() {
                "user" => {
                    if let Some(text) = message.content.as_deref() {
                        self.flush_told_calls(&mut told);
                        // (what the turn before it changed has its card here; a turn may begin with what the app said and not the person, which is not shown but is a place)
                        self.turn_slots.push((index, self.shown.len()));
                        // (what woke the session with no one writing: the app's notice, in a row of its own)
                        if let Some(wakes) = &message.notice {
                            self.write_card(&Card::Notice { wakes: wakes.clone(), at: written_words(message) });
                            continue;
                        }
                        if dogmind_agent::prompt::is_nudge(text) {
                            continue;
                        }
                        let pictures: Vec<(PathBuf, (i32, i32))> = self.current_session.as_deref().map_or(Vec::new(), |session| message.images.iter().map(|image| (dogmind_agent::images::file_of(session, image), (image.width as i32, image.height as i32))).collect());
                        self.show_user(typed_text(text), &pictures, &written_words(message));
                    }
                }
                "assistant" => {
                    if let Some(text) = &message.content {
                        if !text.trim().is_empty() {
                            self.flush_told_calls(&mut told);
                        }
                        self.append_answer(&crate::mask::hide_keys(text));
                        self.finish_answer();
                        self.last_answer = text.clone();
                        self.mark_answer(text, &written_words(message));
                    }
                    for call in message.tool_calls.iter().flatten() {
                        // A question put to the person, and a plan, are cards of their own, with what came of them.
                        if cards::is_asking(&call.function.name) {
                            if let Some((_, card)) = kept_cards.iter().find(|(id, _)| *id == call.id) {
                                self.flush_told_calls(&mut told);
                                self.write_card(card);
                            }
                            continue;
                        }
                        // A session at work has calls still running, or still waiting for an answer: they say so when they end.
                        if let Some(result) = results.get(call.id.as_str()) {
                            let arguments = serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
                            let summary: String = crate::mask::hide_keys(&markdown::show_hidden(&dogmind_agent::tools::summary(&call.function.name, &arguments))).chars().take(160).collect();
                            let pictures = self.pictures_of(&result.images);
                            if self.settings.compact_tools {
                                self.told_call(&call.function.name, &summary, &crate::mask::hide_keys(&markdown::show_hidden(result.content.as_deref().unwrap_or(""))), &mut told);
                                if !pictures.is_empty() {
                                    self.flush_told_calls(&mut told);
                                    self.show_tool_pictures(&pictures);
                                }
                                continue;
                            }
                            let name = t(tool_name(&call.function.name));
                            self.append(&if summary.is_empty() { format!("✓  {name}\n") } else { format!("✓  {name}  {summary}\n") }, Style::Tool);
                            self.show_tool_pictures(&pictures);
                        }
                    }
                }
                _ => {}
            }
        }
        self.flush_told_calls(&mut told);
        self.turn_end_slot = self.shown.len();
        self.opening = false;
        hold_layout(self.transcript_document, false);
        SendMessageW(self.transcript, WM_SETREDRAW, 1, 0);
        self.layout();
        if self.has_messages {
            SendMessageW(self.transcript, WM_VSCROLL, SB_BOTTOM as WPARAM, 0);
            InvalidateRect(self.transcript, std::ptr::null(), 0);
            self.place_thumb();
        }
    }

    /// What a session out of sight says: its state is kept for when it is opened. Whether its turn has ended, and whether it has nothing left to wait for and goes.
    unsafe fn absorb(&mut self, at: usize, event: Event) -> (bool, bool) {
        let event = hidden_shown(event);
        let parked = &mut self.parked[at];
        match event {
            Event::SessionFile(path) => {
                parked.path = Some(path);
                self.refresh_sessions();
            }
            Event::Content(text) => parked.partial.push_str(&text),
            Event::AssistantEnded => parked.partial.clear(),
            Event::ToolStarted { id, tool, summary } => {
                if let Some(place) = parked.writing.iter().position(|(_, writing, _)| *writing == tool) {
                    parked.writing.remove(place);
                }
                parked.tools_running.insert(id, (tool, summary.chars().take(160).collect()));
            }
            Event::RequestStarted => {
                parked.writing.clear();
                parked.compacting = false;
            }
            Event::Compacting => parked.compacting = true,
            Event::ToolWriting { index, tool, piece } => match parked.writing.iter_mut().find(|(at, _, _)| *at == index) {
                Some((_, _, raw)) => raw.push_str(&piece),
                None => parked.writing.push((index, tool, piece)),
            },
            Event::ToolEnded { id, .. } => {
                parked.tools_running.remove(&id);
            }
            Event::TurnStarted => parked.running = true,
            Event::Woken(_) => {
                parked.running = true;
                parked.woken = true;
                InvalidateRect(self.side, std::ptr::null(), 0);
            }
            Event::Waiting(waiting) => {
                parked.waiting = waiting;
                parked.waiting_taken = std::time::Instant::now();
                InvalidateRect(self.side, std::ptr::null(), 0);
                // (nothing at work and nothing left to wake it: there is nothing to keep it for)
                if !parked.running && parked.waiting.is_empty() {
                    return (false, true);
                }
            }
            Event::Approval(approval) => {
                let (tool, woken) = (approval.tool.clone(), parked.woken);
                parked.pending = Some(approval);
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.flash_for_attention(woken);
                self.tray_note(t("Dogmind is waiting for you"), &tf("It asks to use {0}.", &[&t(tool_name(&tool))]));
            }
            Event::Usage { used, window, usage } => {
                parked.used = used;
                parked.window_tokens = window;
                if usage.tokens_per_second.is_some() {
                    parked.rate = usage.tokens_per_second;
                }
                if usage.prompt > 0 {
                    parked.totals = (parked.totals.0 + usage.prompt, parked.totals.1 + usage.cached, parked.totals.2 + usage.completion, parked.totals.3 + 1);
                }
            }
            Event::Interjected(text) => {
                if let Some(place) = parked.interjected.iter().position(|known| *known == text) {
                    parked.interjected.remove(place);
                }
            }
            Event::Notice(text) => parked.notices.push((lang::notice(&text), Style::Warn)),
            Event::Failed(text) => {
                parked.failed = true;
                let embedded = self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded);
                let (words, said) = lang::failure_parts(&text, embedded);
                parked.notices.push((words, Style::Warn));
                parked.notices.extend(said.map(|said| (said, Style::Said)));
                keep_server_line(&self.folder, &self.leaving, &text);
            }
            Event::Todos(items) => parked.todos = items,
            Event::WebSeen(on) => parked.web_seen = on,
            Event::Question(question) => {
                let words: String = question.question.lines().next().unwrap_or("").chars().take(120).collect();
                let woken = parked.woken;
                parked.question = Some(question);
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.flash_for_attention(woken);
                self.tray_note(t("Dogmind is waiting for you"), &words);
            }
            Event::PlanReview(review) => {
                let woken = parked.woken;
                parked.plan_review = Some(review);
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.flash_for_attention(woken);
                self.tray_note(t("Dogmind is waiting for you"), t("It has a plan for you."));
            }
            Event::TurnEnded { .. } => {
                parked.pending = None;
                parked.question = None;
                parked.plan_review = None;
                parked.interjected.clear();
                parked.running = false;
                // (what was said too late for this turn is for the next)
                let late = std::mem::take(&mut *parked.agent.interject.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
                parked.queued.extend(late.into_iter().map(Outgoing::words));
                if parked.queued.is_empty() {
                    // (it goes when nothing waits for it: one that has a background command or a wait stays, to be woken)
                    return (true, parked.waiting.is_empty());
                }
                let next = parked.queued.remove(0);
                parked.running = true;
                let _ = parked.agent.commands.send(send_command(next));
            }
            _ => {}
        }
        (false, false)
    }

    unsafe fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }
        let mut at = 0;
        while at < self.parked.len() {
            let (mut ended, mut gone) = (false, false);
            while let Ok(event) = self.parked[at].events.try_recv() {
                let (turn_ended, nothing_left) = self.absorb(at, event);
                (ended, gone) = (ended || turn_ended, gone || nothing_left);
            }
            if ended {
                // (once, as the turn ends: a turn nobody sent says so as one that was sent does)
                let parked = &mut self.parked[at];
                let (title, failed, woken) = (parked.title.clone(), std::mem::take(&mut parked.failed), std::mem::take(&mut parked.woken));
                let (path, notices) = (parked.path.clone(), std::mem::take(&mut parked.notices));
                let words = if title.is_empty() { t("The work is done.").to_string() } else { title };
                self.tray_note(t(if failed { "Dogmind stopped with an error" } else { "Dogmind has finished" }), &words);
                if let (Some(path), true) = (&path, failed) {
                    self.failed_sessions.insert(path.clone());
                }
                if let (Some(path), false) = (path, notices.is_empty()) {
                    self.held_notices.insert(path, notices);
                }
                self.refresh_sessions();
                InvalidateRect(self.side, std::ptr::null(), 0);
                if GetForegroundWindow() != self.window {
                    self.flash_for_attention(woken);
                }
            }
            if gone {
                let parked = self.parked.remove(at);
                let _ = parked.agent.commands.send(Command::Quit);
                self.refresh_sessions();
                InvalidateRect(self.side, std::ptr::null(), 0);
            } else {
                at += 1;
            }
        }
        while let Ok(note) = self.notes.try_recv() {
            self.on_note(note);
        }
    }

    /// What waits to be shown goes in now: something else is about to follow it.
    unsafe fn flush_reveal(&mut self) {
        if !self.reveal.is_empty() {
            let text = std::mem::take(&mut self.reveal);
            self.append_answer(&text);
        }
    }

    /// A frame: some more of the answer if it is time, and the view some way nearer its end. All of it is on screen when this returns, in time for
    /// the composition this frame was waiting for.
    unsafe fn reveal_tick(&mut self) {
        let mut trace = crate::frametrace::frame();
        let mut went_in = 0;
        if !self.reveal.is_empty() && self.revealed_at.elapsed().as_millis() >= REVEAL_EVERY_MS {
            // (a window held up for long does not rush to make up for it)
            let seconds = self.revealed_at.elapsed().as_secs_f32().min(0.1);
            self.revealed_at = std::time::Instant::now();
            let waiting = self.reveal.chars().count();
            let (count, rate, carried) = reveal_step(waiting, self.reveal_rate, self.reveal_carried, seconds);
            (self.reveal_rate, self.reveal_carried) = (rate, carried);
            let at = self.reveal.char_indices().nth(count).map_or(self.reveal.len(), |(at, _)| at);
            let chunk: String = self.reveal.drain(..at).collect();
            went_in = chunk.len();
            self.revealing = true;
            self.append_answer(&chunk);
            self.revealing = false;
        }
        if let Some(trace) = &mut trace {
            trace.part(0);
        }
        let mut view = (POINT { x: 0, y: 0 }, POINT { x: 0, y: 0 });
        if trace.is_some() {
            SendMessageW(self.transcript, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(view.0) as LPARAM);
        }
        let settled = self.scroll_toward_end();
        if let Some(trace) = &mut trace {
            SendMessageW(self.transcript, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(view.1) as LPARAM);
            trace.part(1);
        }
        UpdateWindow(self.transcript);
        if let Some(mut trace) = trace {
            if crate::frametrace::prints_too() {
                let mut client: RECT = std::mem::zeroed();
                GetClientRect(self.transcript, &mut client);
                let window_device = GetDC(self.transcript);
                let device = CreateCompatibleDC(window_device);
                let bitmap = CreateCompatibleBitmap(window_device, client.right, client.bottom);
                let previous = SelectObject(device, bitmap);
                SendMessageW(self.transcript, WM_PRINTCLIENT, device as WPARAM, PRF_CLIENT as LPARAM);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
                ReleaseDC(self.transcript, window_device);
            }
            trace.part(2);
            trace.end(view.1.y - view.0.y, went_in, self.reveal.len());
        }
        if self.reveal.is_empty() && settled {
            self.frames.wanted.store(false, Ordering::Relaxed);
        }
    }

    /// True once the view is at the end, or does not follow it.
    unsafe fn scroll_toward_end(&mut self) -> bool {
        // (a window held up for long does not leap for it: the spring is told of a frame no longer than this)
        let seconds = self.frame_at.elapsed().as_secs_f32().min(0.05);
        self.frame_at = std::time::Instant::now();
        if !self.follow_end || self.panel != Panel::Chat {
            self.follow_speed = 0.0;
            self.follow_carried = 0.0;
            return true;
        }
        let edit = self.transcript;
        // Where the last line really is. The scrollbar's range is the control's estimate of a text it has not all laid out, and it jumps by
        // hundreds of pixels as the estimate is corrected: a view that followed it leapt a page at a time.
        let last = self.end_point();
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(edit, &mut client);
        let mut position = POINT { x: 0, y: 0 };
        SendMessageW(edit, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        let behind = last.y + scale(self.dpi, LAST_LINE) - client.bottom;
        let (step, speed, carried) = follow_step(behind, self.follow_speed, self.follow_carried, seconds, scale(self.dpi, FOLLOW_LEAST) as f32);
        self.follow_speed = speed;
        self.follow_carried = carried;
        if behind <= 0 {
            return true;
        }
        if step == 0 {
            return false;
        }
        let was = position.y;
        position.y += step;
        SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        SendMessageW(edit, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        if position.y == was {
            // The control's range ends here: the room kept for the last line is more than it has.
            self.follow_speed = 0.0;
            self.follow_carried = 0.0;
            return true;
        }
        self.place_thumb();
        false
    }

    unsafe fn on_event(&mut self, event: Event) {
        let event = hidden_shown(event);
        let live_report = matches!(&event, Event::Usage { usage, .. } if usage.prompt == 0 && usage.completion == 0);
        if !matches!(event, Event::Content(_) | Event::RequestStarted) && !live_report {
            self.flush_reveal();
        }
        match event {
            Event::SessionStarted { path, workspace, transcript } => {
                let notices = self.held_notices.remove(&path).unwrap_or_default();
                self.workspace = workspace;
                self.preview_follows_workspace();
                self.session_title = transcript.iter().find(|message| message.role == "user").and_then(|message| message.content.as_deref())
                    .map(|text| typed_text(text).lines().next().unwrap_or("").chars().take(70).collect()).unwrap_or_default();
                if let Some(summary) = self.all_sessions.iter().find(|summary| self.current_session.as_ref() == Some(&summary.path)) {
                    self.session_title = summary.title.clone();
                }
                self.look_at_workspace();
                self.changes.clear();
                // (what each answer changed is the session's own: the answers of the one before are not marked in the tree)
                self.turn_changes.clear();
                self.files_marks();
                self.refresh_changes();
                self.show_transcript(&transcript);
                // (what each answer of it changed: the cards come when the agent has said)
                if !transcript.is_empty() && self.current_session.is_some() {
                    let _ = self.agent.commands.send(Command::TurnsForCards);
                }
                for (notice, style) in notices {
                    self.append(&format!("{notice}\n"), style);
                }
                if self.find_after_open.is_some() {
                    self.find_opened();
                } else if self.find_open {
                    self.find_update();
                }
                if !self.panel_log.is_empty() && self.panel == Panel::Chat {
                    let notice = std::mem::take(&mut self.panel_log);
                    self.append(&format!("{notice}\n"), Style::Warn);
                }
                self.update_status();
                self.refresh_sessions();
            }
            Event::SessionFile(path) => {
                self.current_session = Some(path);
                self.refresh_sessions();
            }
            Event::TurnStarted => {
                // (a turn clears the flag as it begins: Stop was pressed before it did, while the engine warmed up)
                if self.stop_asked {
                    self.agent.cancel.store(true, Ordering::Relaxed);
                }
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.thinking.clear();
                self.answer.clear();
                self.thinking_written = false;
                self.set_activity(t("Thinking…").into());
            }
            Event::Reasoning(text) => {
                if self.settings.show_thinking {
                    self.append(&text, Style::Thinking);
                    self.line_start = text.ends_with('\n');
                    self.thinking_written = true;
                }
                self.thinking.push_str(&text);
                self.set_activity(t("Thinking…").into());
            }
            Event::Content(text) => {
                if self.answer.is_empty() {
                    // (the words coming say so: the ring alone says it is not done)
                    self.set_activity(" ".into());
                    // The thinking that went in ends here, shown still or hidden since: the answer is not written on after it.
                    if std::mem::take(&mut self.thinking_written) {
                        self.end_line();
                    }
                }
                self.answer.push_str(&text);
                // (what is shown of it has its keys hidden: the reply stays as the model wrote it)
                let shown = self.stream_mask.push(&text);
                self.reveal.push_str(&shown);
                self.want_frames();
            }
            Event::AssistantEnded => {
                self.request_live = None;
                self.end_of_answer();
            }
            // (what a long session is waiting for, which is not the model thinking: it is asked to sum the session up, and nothing of that shows)
            Event::Compacting => self.set_activity(t("Compacting the session…").into()),
            // (the request is kept for a server that is restarting: what it says of its loading, as the connection screen says it)
            Event::ServerRestarting(progress) => {
                let waiting = if progress.is_null() { t("Waiting for the server to restart…").to_string() } else { tf("Waiting for the server: {0}", &[&engine::progress_text(&progress)]) };
                self.set_activity(waiting);
            }
            Event::RequestStarted => {
                if self.activity == t("Compacting the session…") || self.activity == t("Waiting for the server to restart…") || self.activity.starts_with(&tf("Waiting for the server: {0}", &[&""])) {
                    self.set_activity(t("Thinking…").into());
                }
                // (the next answer does not start at the speed the last one ended)
                (self.reveal_rate, self.reveal_carried) = (0.0, 0.0);
                self.request_live = Some(std::time::Instant::now());
                self.plain_writing = None;
                self.run_carry.retain(|entry| entry.writing.is_none());
                if let Some(open) = self.run_open {
                    self.runs[open].entries.retain(|entry| entry.writing.is_none());
                    self.runs[open].live_rate = None;
                    if self.runs[open].entries.is_empty() {
                        self.close_run();
                    }
                }
            }
            // (a question or a plan is a card of its own: the call that makes it has no row among the calls)
            Event::ToolWriting { ref tool, .. } if cards::is_asking(tool) => {}
            Event::ToolWriting { index, tool, piece } => {
                if self.settings.compact_tools {
                    self.run_writes(index, tool, &piece);
                } else {
                    if !self.plain_writing.as_ref().is_some_and(|entry| entry.writing.as_ref().is_some_and(|writing| writing.index == index)) {
                        self.end_of_answer();
                        self.plain_writing = Some(RunEntry::being_written(index, tool));
                    }
                    if let Some(entry) = &mut self.plain_writing {
                        entry.more_written(&piece);
                        let line = toolrun::writing_line(entry, &self.workspace);
                        self.set_activity(line);
                    }
                }
            }
            Event::ToolStarted { id, tool, summary } if cards::is_asking(&tool) => {
                self.tools_running.insert(id, (tool, summary));
            }
            Event::ToolStarted { id, tool, summary } => {
                let summary: String = summary.chars().take(160).collect();
                if self.settings.compact_tools {
                    self.tools_running.insert(id.clone(), (tool.clone(), summary.clone()));
                    self.run_started(id, tool, summary);
                } else {
                    self.set_activity(format!("{}  {summary}", t(tool_name(&tool))));
                    self.tools_running.insert(id, (tool, summary));
                }
            }
            // (a look by a helper takes minutes: what it is reading now says it is getting somewhere)
            Event::ToolProgress { tool, summary, .. } => {
                let summary: String = summary.chars().take(120).collect();
                self.set_activity(format!("{} — {}  {summary}", t(tool_name("explore")), t(tool_name(&tool))));
            }
            Event::ToolEnded { id, .. } if self.tools_running.get(&id).is_some_and(|(tool, _)| cards::is_asking(tool)) => {
                self.tools_running.remove(&id);
                if self.tools_running.is_empty() && !self.waits_for_person() {
                    self.set_activity(t("Thinking…").into());
                }
            }
            Event::ToolEnded { id, ok, declined, elapsed, output, images } => {
                self.end_line();
                let (tool, summary) = self.tools_running.remove(&id).unwrap_or_default();
                if self.tools_running.is_empty() || !self.settings.compact_tools {
                    self.set_activity(t("Thinking…").into());
                }
                if matches!(tool.as_str(), "create_file" | "apply_patch" | "execute_command") {
                    // Several in a row are looked at once.
                    SetTimer(self.window, TIMER_CHANGES, 500, None);
                }
                let pictures = self.pictures_of(&images);
                if self.settings.compact_tools {
                    self.run_ended(&id, tool, summary, ok, declined, elapsed.as_secs_f64(), &output);
                    self.para_next = Some(Para::AfterBlock);
                    if !pictures.is_empty() {
                        // (under the run that opened them; what the model does next is a run of its own, below them)
                        self.close_run();
                        self.show_tool_pictures(&pictures);
                    }
                    return;
                }
                let first = output.lines().next().unwrap_or("").chars().take(240).collect::<String>();
                let what = if summary.is_empty() { t(tool_name(&tool)).to_string() } else { format!("{}  {summary}", t(tool_name(&tool))) };
                if declined {
                    self.append(&format!("✗  {what}  —  {}\n", t("Rejected")), Style::Warn);
                } else if ok {
                    self.append(&format!("✓  {what}  ·  {:.1}s\n", elapsed.as_secs_f64()), Style::Tool);
                    self.show_tool_output(&tool, &output);
                    self.show_tool_pictures(&pictures);
                } else {
                    self.append(&format!("✗  {what}  —  {first}\n", ), Style::Bad);
                }
                // (what comes next stands clear of a block of output)
                self.para_next = Some(Para::AfterBlock);
            }
            Event::Interjected(text) => {
                // Between two steps: its card comes here, in its place in the conversation.
                if let Some(place) = self.interjected.iter().position(|known| *known == text) {
                    self.interjected.remove(place);
                }
                self.show_user(&text, &[], &clock_now());
                self.layout();
            }
            Event::Approval(approval) => {
                self.run_asks();
                let reason = approval.reason.as_ref().map_or(String::new(), |reason| lang::reason(reason));
                // What Always means here, for whoever is about to use it, with the pointer or the key. That it covers this very call needs no line of its own.
                let always = match (&approval.rule, approval.always_allowed) {
                    (_, true) if matches!(approval.scope, Some(AlwaysScope::File(_))) => tf("Always allows this file, for this session: {0}", &[&always_places(&approval.scope)]),
                    (_, true) if matches!(approval.scope, Some(AlwaysScope::Folders(_))) => tf("Always allows this folder, for this session: {0}", &[&always_places(&approval.scope)]),
                    (_, true) if matches!(approval.scope, Some(AlwaysScope::Network(_))) => tf("Always allows this on the network, for this session: {0}", &[&always_places(&approval.scope)]),
                    (Some(rule), true) => tf("Always saves: {0}", &[&t(&rule.label())]),
                    _ => String::new(),
                };
                // (the rule is made of the command: what hides in it is shown as its mark here too, or the line under the buttons reads in another order than the command above)
                let always = markdown::show_hidden(&always).into_owned();
                self.tip(self.always, if always.is_empty() { t("Always allows this exact call, for this session.") } else { &always });
                self.always_line = always;
                // The sentences (why it asks, what the model says it is for) are in the body face above; what is run or written is in the control, in a monospace face with room between its lines.
                let (purpose, code) = approval_parts(&approval.tool, &approval.summary, &approval.details);
                self.approval_head = markdown::show_hidden(&[reason, purpose].iter().filter(|part| !part.is_empty()).cloned().collect::<Vec<String>>().join("\n")).into_owned();
                // (a line at its end with nothing on it would have it scroll for nothing)
                // (a command is read from its first character: what hides in it is shown as its mark)
                self.show_command(&markdown::show_hidden(&code).trim_end().replace('\n', "\r\n"));
                // From its first word: what is being approved is read from the top, whatever its length.
                SendMessageW(self.note, EM_SETSEL, 0, 0);
                SendMessageW(self.note, EM_SCROLLCARET, 0, 0);
                SendMessageW(self.note, WM_VSCROLL, SB_TOP as WPARAM, 0);
                EnableWindow(self.always, i32::from(approval.always_allowed));
                self.tray_note(t("Dogmind is waiting for you"), &tf("It asks to use {0}.", &[&t(tool_name(&approval.tool))]));
                self.pending = Some(approval);
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.layout();
                let in_a_field = [self.composer, self.find_field, self.side_search].contains(&GetFocus());
                if approval_takes_focus(in_a_field, GetWindowTextLengthW(self.composer) == 0, self.composer_typed_at.map(|at| at.elapsed())) {
                    if !self.modal_open {
                        SetFocus(self.window);
                    }
                }
                self.flash_for_attention(self.woken_turn);
            }
            Event::Usage { used, window, usage } => {
                // A report from a request under way comes a couple of times a second: only what it changes is drawn. The panel's words change with each thousand.
                let panel_changed = used / 1000 != self.used / 1000 || window != self.window_tokens;
                self.used = used;
                self.window_tokens = window;
                if usage.tokens_per_second.is_some() {
                    self.rate = usage.tokens_per_second;
                }
                if live_report && self.request_live.is_some() {
                    self.request_live = Some(std::time::Instant::now());
                }
                if let Some(open) = self.run_open.filter(|_| usage.tokens_per_second.is_some()) {
                    self.runs[open].live_rate = usage.tokens_per_second;
                }
                if usage.prompt > 0 {
                    self.totals = (self.totals.0 + usage.prompt, self.totals.1 + usage.cached, self.totals.2 + usage.completion, self.totals.3 + 1);
                }
                if live_report {
                    InvalidateRect(self.window, &self.status_area, 0);
                } else {
                    self.update_status();
                }
                if self.right_width > 0 && (!live_report || panel_changed) {
                    self.layout();
                }
            }
            Event::WebSeen(on) => {
                self.web_seen = on;
                self.layout();
            }
            Event::Todos(items) => {
                let lines: Vec<String> = items.iter().map(|item| format!("    {}  {}", task_mark(item["status"].as_str().unwrap_or("")), item["text"].as_str().unwrap_or(""))).collect();
                self.todos = items;
                self.layout();
                // Where the panel on the right is not there to show it.
                if self.right_width == 0 && !lines.is_empty() {
                    self.append(&format!("{}\n", lines.join("\n")), Style::Tool);
                }
            }
            // (one of another session, asked for before that one was left: not for the answers shown now)
            Event::TurnChanges { session, .. } if session.is_some() && self.current_session.is_some() && session != self.current_session => {}
            Event::CardTurns { session, .. } if session.is_some() && self.current_session.is_some() && session != self.current_session => {}
            Event::CardTurns { turns, .. } => self.turns_for_cards(&turns),
            Event::TurnChanges { turn, files, .. } => {
                if let Some(slot) = self.turn_cards_waiting.remove(&turn) {
                    // (a session opened again: the card goes where the answer ended, as soon as its reply has come, whatever came before and whatever does not)
                    if !files.is_empty() {
                        self.put_turn_card(slot, turn, &files);
                    }
                    self.turn_changes.insert(turn, files.clone());
                } else {
                    // (a card under the answer: the files, and their lines added and removed)
                    if !files.is_empty() && !self.turn_changes.contains_key(&turn) {
                        self.write_card(&Card::files(turn, &files));
                    }
                    self.turn_changes.insert(turn, files.clone());
                }
                // (the tree of the folder marks them, and reads again the folders they are in)
                self.files_turn_changed(&files);
            }
            Event::Question(question) => {
                self.card_next += 1;
                let id = self.card_next;
                self.write_card(&Card::Question { id, question: question.question.clone(), options: question.options.clone(), state: AnswerState::Waiting });
                let words: String = question.question.lines().next().unwrap_or("").chars().take(120).collect();
                self.question = Some(AskedQuestion { answer: question.answer, question: question.question, options: question.options, card: id });
                self.set_activity(String::new());
                self.update_buttons();
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.tray_note(t("Dogmind is waiting for you"), &words);
                self.flash_for_attention(self.woken_turn);
                // (the card's field is the composer: what is written in it is the answer)
                set_text(self.composer_cue, t("Write your own answer…"));
                self.update_composer_cue();
                self.focus_composer();
            }
            Event::Goal(goal) => {
                // (the bar under the title shows it, while there is one: set, or that of a session opened)
                if goal != self.goal {
                    self.goal = goal;
                    self.layout();
                }
            }
            Event::PlanReview(review) => {
                self.card_next += 1;
                let id = self.card_next;
                self.write_card(&Card::Plan { id, plan: review.plan.clone(), state: PlanState::Waiting });
                self.plan_review = Some(AskedPlan { answer: review.answer, plan: review.plan, card: id, changing: false });
                self.set_activity(String::new());
                self.update_buttons();
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.tray_note(t("Dogmind is waiting for you"), t("It has a plan for you."));
                self.flash_for_attention(self.woken_turn);
                self.focus_composer();
            }
            Event::ModeChanged(mode) => {
                // (a plan approved: plan mode is over, as the agent says)
                self.settings.mode = mode.name().to_string();
                self.save();
                self.update_status();
            }
            Event::Turns(turns) => {
                self.turns = turns;
                if self.turns.is_empty() {
                    self.say(t("This session has nothing to go back to yet."));
                } else {
                    self.set_panel(Panel::Rewind);
                    if !self.rewind_rows.is_empty() {
                        SendMessageW(self.list, LB_SETCURSEL, 0, 0);
                    }
                }
            }
            Event::Rewound { restored, prompt } => {
                self.end_line();
                let count = restored.written.len() + restored.removed.len();
                if count > 0 {
                    self.append(&if count == 1 { t("Went back: 1 file restored.\n").to_string() } else { tf("Went back: {0} files restored.\n", &[&count]) }, Style::Tool);
                    for path in restored.written.iter().chain(&restored.removed) {
                        self.append(&format!("    {}\n", path.display()), Style::Output);
                    }
                }
                for failure in &restored.failed {
                    self.append(&format!("{failure}\n"), Style::Bad);
                }
                self.line_start = true;
                if let Some(typed) = prompt {
                    set_text(self.composer, &typed);
                    SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
                    self.layout();
                    self.focus_composer();
                }
                self.look_at_workspace();
                self.refresh_changes();
            }
            Event::Notice(text) => {
                self.end_line();
                self.append(&format!("{}\n", lang::notice(&text)), Style::Warn);
            }
            Event::Failed(text) => {
                self.turn_failed = true;
                let embedded = self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded);
                let stopped = embedded && llm::is_gone(&text);
                // The engine may take seconds to end after a fatal error: what is written in them waits for a new one and is not sent to this one.
                if stopped && self.connected {
                    self.connected = false;
                    self.engine_waits = true;
                    self.connection = "starts when you write".into();
                    self.update_status();
                }
                if !(stopped && self.engine_stop_told) {
                    self.end_line();
                    // The app's sentence, and under it, smaller, what the engine or server said.
                    let (words, said) = lang::failure_parts(&text, embedded);
                    self.append(&format!("{words}\n"), Style::Bad);
                    if let Some(said) = said {
                        self.append(&format!("{said}\n"), Style::Said);
                    }
                }
                keep_server_line(&self.folder, &self.leaving, &text);
                self.engine_stop_told |= stopped;
                if llm::sentence_of(&text) == llm::NO_VISION {
                    self.vision = Some(false);
                    self.tell_about_vision();
                }
            }
            // The session woke itself, with nobody sending anything: what woke it is a row of its own, and the turn is at work as any is.
            Event::Woken(wakes) => {
                self.running = true;
                self.woken_turn = true;
                self.turn_failed = false;
                self.write_card(&Card::Notice { wakes, at: clock_now() });
                self.update_buttons();
                InvalidateRect(self.side, std::ptr::null(), 0);
            }
            // What it has going or waiting in the background: listed in the panel on the right, and marked in the list of sessions.
            Event::Waiting(waiting) => {
                let changed = !self.waiting.same_as(&waiting);
                self.waiting = waiting;
                self.waiting_taken = std::time::Instant::now();
                if changed {
                    InvalidateRect(self.side, std::ptr::null(), 0);
                    self.layout();
                }
            }
            Event::TurnEnded { cancelled } => {
                let woken = std::mem::take(&mut self.woken_turn);
                // (a question or a plan the turn left unanswered goes with it, and its card says so)
                self.question = None;
                self.plan_review = None;
                self.settle_waiting_cards();
                self.reset_composer_cue();
                // (what was said too late for this turn is for the next)
                let late = std::mem::take(&mut *self.agent.interject.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
                self.queued.extend(late.into_iter().map(Outgoing::words));
                self.interjected.clear();
                self.stop_asked = false;
                let failed = std::mem::take(&mut self.turn_failed);
                if !cancelled {
                    let title = if self.session_title.is_empty() { t("The work is done.").to_string() } else { self.session_title.clone() };
                    self.tray_note(t(if failed { "Dogmind stopped with an error" } else { "Dogmind has finished" }), &title);
                }
                self.running = false;
                self.request_live = None;
                self.plain_writing = None;
                InvalidateRect(self.side, std::ptr::null(), 0);
                SetTimer(self.window, TIMER_CHANGES, 200, None);
                self.finish_answer();
                self.close_run();
                self.run_carry.clear();
                self.asked_calls.clear();
                if cancelled {
                    self.append(t("Stopped\n"), Style::Tool);
                }
                if self.pending.take().is_some() {
                    self.layout();
                }
                self.set_activity(String::new());
                self.update_buttons();
                self.refresh_sessions();
                if GetForegroundWindow() != self.window {
                    self.flash_for_attention(woken);
                }
                self.flush_queue();
            }
        }
    }

    unsafe fn flush_queue(&mut self) {
        if !self.running && self.connected && !self.queued.is_empty() {
            let next = self.queued.remove(0);
            self.dispatch(next);
        } else if !self.running && (self.engine_waits || self.connect_failed) && !self.queued.is_empty() {
            // Written while the engine was going down, and said to be sent when this work ended: writing is what starts it.
            self.connect();
            self.set_activity(t(self.waiting_words()).into());
            self.update_buttons();
        }
        self.layout();
    }

    /// How the connection that has just ended went, for the trajectory: all of it, and the time it spent waiting for the server or the engine to be ready.
    fn note_connection(&mut self, outcome: &str) {
        let at = dogmind_agent::trajectory::now();
        if let Some(began) = self.connect_began.take() {
            let _ = self.agent.commands.send(Command::Connection { at: at.clone(), what: "connect".into(), ms: began.elapsed().as_millis() as u64, outcome: outcome.into() });
        }
        if let Some(waiting) = self.connect_waiting_since.take() {
            let _ = self.agent.commands.send(Command::Connection { at, what: "loading_wait".into(), ms: waiting.elapsed().as_millis() as u64, outcome: outcome.into() });
        }
    }

    unsafe fn on_note(&mut self, note: Note) {
        let current = self.generation.load(Ordering::SeqCst);
        match note {
            Note::Status(generation, text) if generation == current => {
                self.connect_waiting_since.get_or_insert_with(std::time::Instant::now);
                self.connection = text;
                self.update_status();
            }
            Note::Connected { generation, provider, model, window, straydog, vision } if generation == current => {
                self.note_connection("ok");
                self.connected = true;
                self.connect_retrying = false;
                self.last_connect_error.clear();
                self.vision = vision;
                self.connection = if straydog { "ready · Straydog".into() } else { "ready".into() };
                self.model = model.clone();
                self.window_tokens = window;
                self.straydog = straydog;
                for parked in &self.parked {
                    let _ = parked.agent.commands.send(Command::Provider(provider.clone(), model.clone(), window));
                }
                self.provider = Some((provider.clone(), model.clone(), window));
                let _ = self.agent.commands.send(Command::Provider(provider, model, window));
                self.update_status();
                self.send_effort();
                if !self.attachments.is_empty() || self.queued.iter().any(|item| !item.images.is_empty()) {
                    self.tell_about_vision();
                }
                self.engine_failed_at = None;
                let local = self.settings.providers.iter().any(|provider| provider.name == self.settings.active && provider.kind == Kind::Embedded);
                // (the agent looks for itself whether its session has begun: words on the page here may be only an earlier failure)
                if warms_on_connect(local, straydog, self.queued.is_empty()) {
                    let _ = self.agent.commands.send(Command::WarmPrefix);
                }
                self.flush_queue();
            }
            Note::Models(generation, models) if generation == current => self.provider_models = models,
            Note::ConnectFailed(generation, error) if generation == current => {
                self.note_connection(&format!("error:{}", dogmind_agent::agent::failure_kind(&error)));
                self.connection = "not connected".into();
                // (writing tries again)
                self.engine_waits = self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded);
                self.connect_failed = !self.engine_waits;
                if self.engine_waits {
                    self.engine_failed_at = Some(std::time::Instant::now());
                }
                // (an engine that ended while it loaded has now been spoken of: the timer need not say so again)
                self.engine_stop_told = self.engine_waits;
                let again = std::mem::take(&mut self.connect_retrying) && error == self.last_connect_error;
                // (nor when these very words are the last thing on the page: a second try with the same key has nothing new to say)
                let words = format!("{}\n", connect_failure(&error));
                // (what the engine or the server said under the sentence, if it said anything, smaller)
                let said = llm::line_under(&error).map(|line| format!("{}\n", lang::said_by(self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded), line)));
                let said_last = match &said {
                    Some(said) => matches!(self.shown.last(), Some(Shown::Text(held, Style::Said, _)) if held.ends_with(said)),
                    None => matches!(self.shown.last(), Some(Shown::Text(held, Style::Bad, _)) if held.ends_with(&words)),
                };
                if !again && !said_last {
                    self.last_connect_error = error.clone();
                    self.end_line();
                    self.append(&words, Style::Bad);
                    if let Some(said) = &said {
                        self.append(said, Style::Said);
                    }
                }
                keep_server_line(&self.folder, &self.leaving, &error);
                // (a key refused or an address that is not one will not put itself right: what was written goes back, to be sent when the settings have been seen to)
                if self.connect_failed && !self.queued.is_empty() && llm::sentence_of(&error) != llm::KEY_REFUSED && llm::sentence_of(&error) != llm::NOT_FOUND {
                    // Said to be sent when connected, and it is: a server comes back by itself, and the square stops this.
                    self.set_activity(t(self.waiting_words()).into());
                    SetTimer(self.window, TIMER_RECONNECT, RECONNECT_MS, None);
                } else {
                    // An engine that did not start is not started again unasked: what was written waits in the composer.
                    self.queued_back_to_composer();
                    self.set_activity(String::new());
                }
                self.update_buttons();
                self.update_status();
            }
            Note::EngineStarted(generation, started) if generation == current => {
                // (the port asked for was taken by something else: this one holds until that changes)
                if started.port != self.settings.port {
                    self.settings.port = started.port;
                    self.save();
                }
                // (an engine that is not held by a job stays when this app is killed, with its card's memory: the trajectory says that it was started so)
                if let Some(why) = &started.not_held {
                    let _ = self.agent.commands.send(Command::Connection { at: dogmind_agent::trajectory::now(), what: "engine_job".into(), ms: 0, outcome: format!("not held by a job, so it does not end with the app if the app is killed: {why}") });
                }
                self.engine = Some(started);
            }
            // Dropped, it stops: the person moved on while it started.
            Note::EngineStarted(..) | Note::Status(..) | Note::Models(..) | Note::Connected { .. } | Note::ConnectFailed(..) => {}
            Note::Attached { path, result } => self.file_attached(path, result),
            Note::Thumb { path, room, decoded } => self.thumb_came(path, room, decoded),
            Note::Sessions(mut sessions) => {
                // (the reason a conversation cannot be opened is its name in the list: said in the person's language)
                for summary in sessions.iter_mut().filter(|summary| summary.unreadable.is_some()) {
                    summary.title = t(&summary.title).to_string();
                }
                self.all_sessions = sessions;
                if let Some(summary) = self.all_sessions.iter().find(|summary| self.current_session.as_ref() == Some(&summary.path)) {
                    if self.session_title != summary.title {
                        self.session_title = summary.title.clone();
                        self.layout();
                    }
                }
                self.fill_side();
            }
            Note::Workspace { path, files, branch, skills } => {
                if path == self.workspace {
                    self.skills = skills;
                    self.files = files;
                    self.branch = branch;
                    // (what is written for another app's features is off, which is known now that they have been found)
                    self.send_skills_off();
                    if self.panel == Panel::Skills {
                        // (another folder's skills: a switch for each, set as the settings say)
                        self.set_panel(Panel::Skills);
                    }
                    self.layout();
                }
            }
            Note::Found(generation, hits) => {
                if generation == self.search_generation && !self.side_query.is_empty() {
                    self.side_hits = Some(hits);
                    self.fill_side();
                }
            }
            Note::Git(path, info) => {
                if path == self.workspace {
                    self.git = info;
                    self.layout();
                }
            }
            Note::FolderRead { root, path, result } => self.files_read_came(root, path, result),
            Note::WorkspaceChanged { root, changed } => self.files_changed_on_disk(&root, changed),
            Note::Changes(path, found) => {
                if path != self.workspace {
                    return;
                }
                if self.panel == Panel::Diff && self.trajectory.is_some() {
                    return;
                }
                // (opened from the card under an answer: that answer's files, with their diffs, and nothing the workspace has besides)
                let found = match (&self.turn_view, self.panel == Panel::Diff) {
                    (Some(files), true) => Ok(files.clone()),
                    _ => found,
                };
                if self.panel != Panel::Diff {
                    self.changes = found.unwrap_or_default();
                    self.layout();
                    return;
                }
                match found {
                    Ok(changes) => {
                        let (added, removed) = changes.iter().fold((0, 0), |(added, removed), change| (added + change.added, removed + change.removed));
                        self.panel_title = changes_heading(added, removed);
                        self.list_rows = changes.iter().map(|change| ListRow { title: change.path.clone(), detail: format!("+{}  −{}", change.added, change.removed), active: false }).collect();
                        self.changes = changes;
                        self.fill_list();
                        if self.changes.is_empty() {
                            self.diff_words = t("No changes.").to_string();
                        } else {
                            let first = self.change_wanted.take().filter(|at| *at < self.changes.len()).unwrap_or(0);
                            SendMessageW(self.list, LB_SETCURSEL, first, 0);
                            self.show_change(first);
                        }
                    }
                    Err(error) => self.diff_words = error,
                }
                self.layout();
            }
            Note::ReadFolderProgress(count) => {
                if let Some(work) = &mut self.read_folder_work {
                    work.count = count;
                }
                if self.panel == Panel::Permissions {
                    self.layout();
                }
            }
            Note::CryptProgress(done, total) => {
                if let Some(work) = &mut self.crypt.work {
                    work.done = done;
                    work.total = total;
                }
                if self.panel == Panel::CryptoWork {
                    self.layout();
                }
            }
            Note::CryptDone(outcome) => self.crypt_finished(outcome),
            Note::ReadFolderDone(words, problem) => {
                self.read_folder_work = None;
                self.read_folder_said = (words, problem);
                if self.panel == Panel::Permissions {
                    self.set_panel(Panel::Permissions);
                }
            }
            Note::Catalog(result) if self.panel == Panel::Setup => {
                // The first screen's first row, and it is not offered first if it cannot run.
                let said = match &result {
                    Ok(catalog) => catalog.refusal.as_ref().map(|refusal| refusal.short().to_string()),
                    Err(_) => Some(t("Straydog cannot start on this PC").to_string()),
                };
                if let (Some(said), Some(row)) = (said, self.list_rows.first_mut()) {
                    row.detail = said;
                    if self.list_selection() == Some(0) {
                        SendMessageW(self.list, LB_SETCURSEL, 1, 0);
                    }
                    InvalidateRect(self.list, std::ptr::null(), 1);
                }
            }
            Note::Catalog(result) => {
                if self.panel != Panel::Models {
                    return;
                }
                match result {
                    Ok(catalog) => {
                        self.catalog = catalog.entries;
                        self.machine_line = catalog.machine;
                        self.refusal = catalog.refusal;
                        self.show_setup_button();
                        self.list_rows = self.catalog.iter().map(|entry| ListRow {
                            title: if entry.origin.is_empty() { entry.name.clone() } else { format!("{}  ·  {}", entry.name, entry.origin) },
                            detail: entry.detail.clone(),
                            active: false,
                        }).collect();
                        self.fill_list();
                        SendMessageW(self.list, LB_SETCURSEL, 0, 0);
                        self.offer_context(Some(0));
                        self.update_licence_link();
                        self.update_vision_chip();
                        self.update_drafter_chip();
                    }
                    Err(error) => {
                        self.catalog.clear();
                        self.list_rows.clear();
                        self.fill_list();
                        self.refusal = Some(Refusal::DoesNotStart(error));
                        self.show_setup_button();
                    }
                }
                if let Some(words) = self.refusal.as_ref().map(Refusal::words) {
                    self.problem_in_panel(&words);
                } else {
                    self.layout();
                }
                RedrawWindow(self.window, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
            }
            Note::EngineVersion(said, made) => {
                self.engine_version = Some(engine_words(said.as_deref(), made));
                if self.panel == Panel::About {
                    self.layout();
                }
            }
            Note::ReleaseLooked { on_its_own, outcome } => {
                self.release_checking = false;
                // (on an install, a newer release with its setup is fetched in the background: the app's own look while the switch is on, the person's own look always)
                let to_fetch = updates::setup_to_fetch(&outcome, on_its_own, self.settings.check_for_updates, self.settings.dismissed_update.as_deref(), self.install_folder.is_some());
                // Only a look that learned something counts against the day: after a refusal, an error or a page that is no release, the hourly timer asks again.
                if outcome.learned_something() {
                    self.settings.last_update_check = Some(updates::now());
                    self.save();
                }
                // (a look that the person has turned off since it began tells nothing on its own)
                if let Some(release) = updates::release_to_tell(&outcome, on_its_own, self.settings.check_for_updates, self.settings.dismissed_update.as_deref()) {
                    self.tell_the_tray_of(&release.version, false);
                    self.release_offer = Some(release);
                }
                // (the About page says what its own button was asked for, and what the app learned on its own; that the app could not reach GitHub when no one asked is not news there)
                if !on_its_own || !matches!(outcome, updates::Outcome::Failed(_)) {
                    self.release_outcome = Some(outcome);
                }
                if self.panel == Panel::About {
                    self.show_release_controls();
                }
                self.layout();
                if let Some(release) = to_fetch {
                    self.fetch_release_setup(release, on_its_own);
                }
            }
            Note::ReleaseProgress { done, total } => {
                if let ReleaseUpdate::Fetching { done: so_far, total: of, .. } = &mut self.release_update {
                    (*so_far, *of) = (done, total);
                    if self.panel == Panel::About {
                        self.layout();
                    } else if self.notice_is_up() {
                        // (on the main screen the notice's button says how far it has come: its words change, and nothing else is laid out again)
                        if let Some(shown @ notice::Notice::Fetching { .. }) = self.notice() {
                            set_text_if_changed(self.update_offer_button, &notice_text(&shown).1);
                        }
                    }
                }
            }
            Note::ReleaseFetched(Ok(ready)) => {
                let again = matches!(&self.release_update, ReleaseUpdate::Ready(known) if known.version == ready.version);
                let version_ready = ready.version.clone();
                self.release_update = ReleaseUpdate::Ready(ready);
                self.release_update_said = None;
                if !again {
                    self.release_ready_closed = false;
                    self.tell_the_tray_of(&version_ready, true);
                }
                self.release_update_changed();
            }
            // (what is said of a try that did not come to an end; a setup that is already here stays, and one that was checked before and is still on disk, which the fetch left, is offered again)
            Note::ReleaseFetched(Err(failed)) => {
                if matches!(self.release_update, ReleaseUpdate::Fetching { .. }) {
                    self.release_update = ReleaseUpdate::None;
                    self.release_update_said = Some(fetch_words(&failed));
                    self.release_update_changed();
                    self.offer_the_setup_kept();
                }
            }
            Note::SetupChecked(ready, held) => self.setup_checked(ready, held),
            Note::Report(result) => match result {
                Ok(path) => {
                    let words = crate::crash::report_words(&shown_path(&path));
                    if self.panel == Panel::About { self.say_in_panel(&words) } else { self.say(&words) }
                }
                Err(error) => {
                    if self.panel == Panel::About { self.problem_in_panel(&error) } else { self.say(&error) }
                }
            },
            Note::Removed(words) => {
                self.leaving = Leaving::Gone(words);
                set_text(self.buttons[0], t("Close"));
                self.set_button_kinds(&[ButtonKind::Primary]);
                self.layout();
                RedrawWindow(self.window, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
            }
            Note::Setup(line) => match line {
                SetupLine::Progress { stage, done, total } => {
                    self.setup_tally.hear(&SetupLine::Progress { stage: stage.clone(), done, total });
                    // (a file alone has a bar of its own: not one nearly full with the model that was there)
                    let (place, words) = if self.fetching_alone() { engine::setup_progress(&stage, done, total, &Default::default()) } else { engine::setup_progress(&stage, done, total, &self.setup_tally) };
                    self.progress = Some(place);
                    self.show_setup_status(words);
                }
                SetupLine::Text(text) => {
                    self.setup_tally.hear(&SetupLine::Text(text.clone()));
                    if let Some((here, whole)) = self.setup_tally.overall().filter(|_| !self.fetching_alone()) {
                        self.progress = Some(here / whole);
                    }
                    if !text.is_empty() {
                        self.show_setup_status(text);
                    }
                }
            },
            Note::VisionSize(size) => match size {
                Some(bytes) => {
                    self.vision_offer = Some(VisionOffer::Download(bytes));
                    self.layout();
                }
                None => self.warn(t("This model can't read pictures.")),
            },
            Note::KeyNotKept(words) => self.say(&words),
            Note::VisionSwitched(result) => {
                match result {
                    Err(error) => {
                        if self.panel == Panel::Chat { self.say(&error) } else { self.problem_in_panel(&error) }
                    }
                    // (the file had gone, or been moved by hand, since the button was drawn: nothing was done, and the person is told so rather than that it was)
                    Ok(false) => {
                        let words = t("There was no vision file to turn on or off any more: nothing was changed.");
                        if self.panel == Panel::Chat { self.say(words) } else { self.problem_in_panel(words) }
                    }
                    Ok(true) => {}
                }
                self.vision_offer = None;
                self.vision_told = false;
                if self.panel == Panel::Models {
                    self.update_vision_chip();
                    self.layout();
                }
                if self.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded) {
                    self.connect();
                }
            }
            Note::SetupDone(result) => {
                let cancelled = self.setup_cancel.swap(false, Ordering::Relaxed);
                self.setting_up = false;
                let was_vision = std::mem::take(&mut self.vision_fetch);
                let drafter_folder = self.drafter_fetch.take();
                let was_drafter = drafter_folder.is_some();
                if was_vision && result.is_ok() {
                    self.vision_offer = None;
                    self.vision_told = false;
                }
                // The drafter that has been built goes into the settings, as setup would have put it, before the engine starts again.
                let result = match (result, drafter_folder) {
                    (Ok(()), Some(folder)) => drafter::turn_on(&engine::config_path(&self.folder), &folder).map(|_| ()),
                    (result, _) => result,
                };
                if self.panel == Panel::Models {
                    self.update_vision_chip();
                    self.update_drafter_chip();
                }
                if self.panel == Panel::Chat {
                    self.layout();
                }
                self.progress = None;
                self.setup_status.clear();
                if was_drafter {
                    // (the status line showed how far the fetch had come)
                    self.update_status();
                }
                if self.panel == Panel::Models {
                    self.show_setup_button();
                }
                let told = |app: &mut App, text: &str| {
                    if app.panel == Panel::Chat {
                        app.say(text);
                    } else {
                        app.problem_in_panel(text);
                    }
                };
                match result {
                    Ok(()) => {
                        if self.onboarding && self.panel == Panel::Models {
                            self.set_panel(Panel::Start);
                        }
                        if was_drafter {
                            // (the engine was not stopped for this: it is not started again over the one that runs)
                            self.restart_for_drafter();
                        } else {
                            self.connect();
                        }
                    }
                    // (the drafter's fetch left the engine as it was, and so does its end: there is nothing to start again)
                    Err(_) if cancelled => {
                        told(self, t("Download stopped. What has arrived stays in the folder, and downloading again goes on from there."));
                        // The engine was stopped for this, and a model it had is still there.
                        if !was_drafter && engine::is_set_up(&self.folder) && !self.onboarding {
                            self.connect();
                        }
                    }
                    Err(error) => {
                        told(self, &engine::setup_failure_words(&error));
                        if !was_drafter && engine::is_set_up(&self.folder) && !self.onboarding {
                            self.connect();
                        }
                    }
                }
            }
        }
    }

    unsafe fn store_engine_fields(&mut self) -> Result<(), String> {
        let (vram, context) = (get_text(self.fields[1]), get_text(self.fields[2]));
        let context = context.trim();
        self.settings.vram = settings::parse_vram(&vram)?;
        self.settings.context = if context.is_empty() { None } else { Some(context.parse().map_err(|_| t("The longest conversation is a number of tokens, or leave it empty.").to_string())?) };
        let folder = get_text(self.fields[0]);
        self.settings.models_folder = (!folder.trim().is_empty()).then(|| PathBuf::from(folder.trim()));
        self.save();
        Ok(())
    }

    unsafe fn start_setup(&mut self) {
        if self.setting_up {
            return;
        }
        if let Err(error) = self.store_engine_fields() {
            self.problem_in_panel(&error);
            return;
        }
        let Some(entry) = self.list_selection().and_then(|at| self.catalog.get(at)).cloned() else {
            self.problem_in_panel(t("Choose a model from the list."));
            return;
        };
        let Some(folder) = self.settings.models_folder.clone() else {
            self.problem_in_panel(t("Choose a folder for the model."));
            return;
        };
        let Some(straydog) = engine::find(&self.settings) else {
            self.problem_in_panel(engine::not_found_words());
            return;
        };
        // What cannot work there is said now, and not after 75 GB.
        if let Some(why) = crate::machine::model_folder_problem(&folder) {
            self.problem_in_panel(&why);
            return;
        }
        // A drive that plainly will not do is said now, in the person's words; what has already arrived Straydog takes into account, and says itself.
        if let (Some(needed), Some(free)) = (entry.disk_bytes, crate::machine::free_bytes(&folder)) {
            let nothing_there_yet = std::fs::read_dir(&folder).map_or(true, |mut inside| inside.next().is_none());
            if needed > free && nothing_there_yet {
                self.problem_in_panel(&tf("This model takes {0} GB on its drive, and that drive has {1} GB free. Choose a folder on another drive, or make room.", &[&(needed / 1_000_000_000), &(free / 1_000_000_000)]));
                return;
            }
        }
        let vision = if self.vision_for_installed().is_some() { self.vision_is_on() } else { self.vision_wanted };
        let mut args = engine::setup_arguments(entry.bundle.as_ref(), &folder, &engine::config_path(&self.folder), self.drafter_is_on(), vision);
        // One short of graphics memory is set up only when chosen, and by a Straydog that is told so: it refuses otherwise.
        if !entry.runs {
            if !engine::setup_can_force(&straydog) {
                self.problem_in_panel(t("This Straydog does not set up a model that it finds too big for this PC. A newer Straydog does."));
                return;
            }
            args.push("--force".into());
        }
        self.run_setup(straydog, args, tf("Setting up {0}...", &[&entry.name]));
    }

    /// `straydog setup` on another thread, what it says in the panel or the status line, and a dropped connection gone on from.
    unsafe fn run_setup(&mut self, straydog: PathBuf, args: Vec<String>, first_words: String) {
        self.setting_up = true;
        self.setup_cancel.store(false, Ordering::Relaxed);
        self.setup_tally = Default::default();
        self.progress = Some(0.0);
        self.show_setup_button();
        // A verify needs the files to itself, and its VRAM probe needs the card. (The drafter's fetch needs neither.)
        if self.setup_holds_off_the_engine() {
            self.engine = None;
        }
        self.show_setup_status(first_words);
        let (poster, cancel) = (self.poster.clone(), self.setup_cancel.clone());
        std::thread::spawn(move || {
            let (lines, seen) = channel();
            let forward = poster.clone();
            let relay = std::thread::spawn(move || {
                while let Ok(line) = seen.recv() {
                    forward.send(Note::Setup(line));
                }
            });
            // A PC left alone for a download of hours does not sleep through it. It ends with this thread.
            windows_sys::Win32::System::Power::SetThreadExecutionState(windows_sys::Win32::System::Power::ES_CONTINUOUS | windows_sys::Win32::System::Power::ES_SYSTEM_REQUIRED);
            let waker = poster.clone();
            // (a Straydog older than the app does not know every option: asked of its help, here and not on the window's thread)
            let args = engine::for_this_straydog(&straydog, args);
            let mut tries = 0;
            let result = loop {
                let waker = waker.clone();
                let result = engine::run_setup(&straydog, &args, &lines, &move || waker.wake(), &cancel);
                // A network that drops goes on by itself: what has arrived stays, and a person may be asleep.
                match &result {
                    Err(error) if tries < SETUP_TRIES && engine::setup_may_recover(error) && !cancel.load(Ordering::Relaxed) => {
                        tries += 1;
                        let _ = lines.send(SetupLine::Text(tf("The connection dropped. Trying again in {0} seconds, from where it stopped ({1} of {2})...", &[&SETUP_RETRY_SECONDS, &tries, &SETUP_TRIES])));
                        poster.wake();
                        let began = std::time::Instant::now();
                        while began.elapsed().as_secs() < SETUP_RETRY_SECONDS && !cancel.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        if cancel.load(Ordering::Relaxed) {
                            break Err("cancelled".to_string());
                        }
                    }
                    _ => break result,
                }
            };
            drop(lines);
            let _ = relay.join();
            poster.send(Note::SetupDone(result));
        });
    }

    /// Opens a folder to commands, off this thread: a large one takes minutes. Nothing else of this kind starts until it has ended.
    unsafe fn add_read_folder(&mut self, folder: PathBuf) {
        if self.read_folder_work.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.read_folder_work = Some(ReadFolderWork { adding: true, folder: folder.clone(), count: 0, cancel: cancel.clone(), can_stop: true, stopping: false });
        self.read_folder_said = (String::new(), false);
        self.layout();
        let (file, poster) = (read_folders::file_in(&self.folder), self.poster.clone());
        let mut secrets = key_files(&self.settings);
        secrets.push(self.folder.clone());
        std::thread::spawn(move || {
            let mut told = ReadFolderTeller::new(&poster);
            let result = read_folders::add(&file, &folder, &secrets, &mut |count| told.tell(count), &cancel);
            let (words, problem) = read_folder_added_words(&folder, &result);
            poster.send(Note::ReadFolderDone(words, problem));
        });
    }

    /// Takes back what a folder was given, off this thread.
    unsafe fn remove_read_folder(&mut self, folder: PathBuf) {
        if self.read_folder_work.is_some() {
            return;
        }
        self.read_folder_work = Some(ReadFolderWork { adding: false, folder: folder.clone(), count: 0, cancel: Arc::new(AtomicBool::new(false)), can_stop: false, stopping: false });
        self.read_folder_said = (String::new(), false);
        self.layout();
        let (file, poster) = (read_folders::file_in(&self.folder), self.poster.clone());
        std::thread::spawn(move || {
            let mut told = ReadFolderTeller::new(&poster);
            let result = read_folders::remove(&file, &folder, &mut |count| told.tell(count));
            let (words, problem) = match result {
                Ok(read_folders::Removed::From(place)) if place == folder => (tf("Commands can no longer read {0}. A file moved out of it while it was on this list keeps that permission, so a command could read it where it can reach it: resetting that file's permissions ends that.", &[&read_folder_name(&folder)]), false),
                Ok(read_folders::Removed::From(place)) => (tf("Commands can no longer read {0}, which had moved to {1}. A file moved out of it while it was on this list keeps that permission, so a command could read it where it can reach it: resetting that file's permissions ends that.", &[&read_folder_name(&folder), &shown_path(&place)]), false),
                Ok(read_folders::Removed::NotFound) => (tf("{0} is no longer on its drive, so nothing there was left to take back. If it was copied somewhere with its permissions, that copy can still be read.", &[&read_folder_name(&folder)]), false),
                Err(error) => (tf("{0} could not all be taken back, and the app will try again when it next starts: {1}", &[&read_folder_name(&folder), &lang::notice(&error)]), true),
            };
            poster.send(Note::ReadFolderDone(words, problem));
        });
    }

    unsafe fn browse_folder(&mut self) -> Option<PathBuf> {
        preview::choose_folder(self.window as isize, t("Choose a folder"))
    }

    unsafe fn copy_to_clipboard(&self, text: &str) {
        let units: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
        if OpenClipboard(self.window) == 0 {
            return;
        }
        EmptyClipboard();
        let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
        if !memory.is_null() {
            let target = GlobalLock(memory) as *mut u16;
            std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            GlobalUnlock(memory);
            SetClipboardData(13, memory); // CF_UNICODETEXT
        }
        CloseClipboard();
    }

    unsafe fn on_button(&mut self, at: usize) {
        match (self.panel, at) {
            (Panel::Models, 0) if self.setting_up => {
                self.setup_cancel.store(true, Ordering::Relaxed);
                self.show_setup_button();
            }
            (Panel::Models, 0) => self.start_setup(),
            (Panel::Models, 1) => {
                if let Some(folder) = self.browse_folder() {
                    set_text(self.fields[0], &folder.display().to_string());
                }
            }
            (Panel::Models, 2) if self.modal_open => {
                if let Err(error) = self.store_engine_fields() {
                    self.problem_in_panel(&error);
                    return;
                }
                self.connect();
                self.say_in_panel(t("Connecting again."));
            }
            (Panel::Models, 3) => self.on_menu(M_LOG),
            (Panel::Models, 4) => match self.dashboard_url() {
                Some(url) => open_in_browser(&url),
                None => self.problem_in_panel(t("Straydog is not what is connected, so it has no dashboard to open.")),
            },
            (Panel::Models, 2) => {
                if let Err(error) = self.store_engine_fields() {
                    self.problem_in_panel(&error);
                    return;
                }
                // (a download goes on behind what comes next)
                let next = if self.setting_up { Panel::Start } else { Panel::Setup };
                self.set_panel(next);
            }
            (Panel::Providers, 0) => {
                if self.save_provider() {
                    self.settings.active = get_text(self.fields[0]).trim().to_string();
                    self.save();
                    self.fill_providers();
                    self.connect();
                    self.say_in_panel(t("Connecting."));
                }
            }
            (Panel::Providers, 1) => {
                self.save_provider();
            }
            (Panel::Providers, 2) => {
                self.selected_provider = None;
                SendMessageW(self.list, LB_SETCURSEL, usize::MAX, 0);
                for field in self.fields {
                    set_text(field, "");
                }
                self.kind = Kind::Server;
                set_text(self.kind_chip, t(Kind::Server.label()));
                let blank = ProviderConfig { name: String::new(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: String::new() };
                self.show_provider_fields(&blank);
                SetFocus(self.fields[0]);
            }
            (Panel::Providers, 3) => {
                if let Some(at) = self.selected_provider.filter(|&at| at < self.settings.providers.len() && self.settings.providers.len() > 1) {
                    let gone = self.settings.providers.remove(at);
                    let left_behind = settings::remove_key(crate::keys::store(), &gone.api_key);
                    let was_active = gone.name == self.settings.active;
                    if was_active {
                        self.settings.active = self.settings.providers[0].name.clone();
                    }
                    self.selected_provider = None;
                    self.save();
                    self.fill_providers();
                    self.layout();
                    if was_active {
                        self.connect();
                    }
                    // (the connection is gone either way: what could not go with it is told, where a person looks)
                    if let Some(words) = left_behind {
                        if self.panel == Panel::Chat { self.say(&words) } else { self.problem_in_panel(&words) }
                    }
                }
            }
            (Panel::Welcome, 0) => self.set_panel(Panel::Setup),
            (Panel::Setup, 0) => self.setup_choose(),
            // As it is: the app says what is missing when it is used, and Settings has it all.
            (Panel::Setup, 1) => self.finish_onboarding(),
            (Panel::SetupDetails, 0) => self.setup_details_done(),
            (Panel::SetupDetails, 1) => self.set_panel(Panel::Setup),
            (Panel::ConfirmRemove, 0) if matches!(self.leaving, Leaving::Gone(_)) => {
                let data = self.tray_data();
                Shell_NotifyIconW(NIM_DELETE, &data);
                if let Some(uninstaller) = uninstaller_beside() {
                    let _ = std::process::Command::new(uninstaller).args(["--after-remove", "--wait-pid", &std::process::id().to_string()]).spawn();
                }
                std::process::exit(0);
            }
            (Panel::ConfirmRemove, 0) => self.leave_this_pc(false),
            (Panel::ConfirmRemove, 1) if self.model_in_the_data_folder().is_some() => self.leave_this_pc(true),
            (Panel::ConfirmRemove, _) => self.set_panel(Panel::General),
            (Panel::About, 1) => self.start_release_look(false),
            (Panel::About, 3) => match &self.release_update {
                ReleaseUpdate::Fetching { stop, .. } => stop.store(true, Ordering::Relaxed),
                ReleaseUpdate::Ready(_) => {
                    self.update_offer_pressed();
                    self.show_release_controls();
                }
                ReleaseUpdate::None => {}
            },
            (Panel::About, 2) => {
                if let Some((version, page)) = self.release_to_read() {
                    self.open_release(&version, &page);
                    self.layout();
                }
            }
            (Panel::About, 0) => {
                let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|folder| folder.join("LICENSE.txt")));
                match beside.filter(|file| file.is_file()) {
                    Some(file) => open_in_browser(&file.display().to_string()),
                    None => self.problem_in_panel(t("LICENSE.txt is not beside this program.")),
                }
            }
            (Panel::General, 0) => open_in_browser(&self.folder.display().to_string()),
            (Panel::Instructions, 0) => {
                if self.leave_page() {
                    self.say_in_panel(t("Saved."));
                }
            }
            (Panel::Rename, 1) | (Panel::Sessions, 1) | (Panel::Rewind, 3) => self.set_panel(Panel::Chat),
            (Panel::Rewind, 0..=2) => {
                let Some(id) = self.list_selection().and_then(|at| self.rewind_rows.get(at)).copied() else {
                    self.problem_in_panel(t("Choose a turn."));
                    return;
                };
                let chat_gone = self.turns.iter().find(|turn| turn.id == id).is_none_or(|turn| turn.message.is_none());
                if chat_gone && at != 1 {
                    self.problem_in_panel(t("That part of the conversation was compacted or rewound past: only its files can go back."));
                    return;
                }
                let _ = self.agent.commands.send(Command::Rewind { turn: id, files: at != 2, conversation: at != 1 });
                self.set_panel(Panel::Chat);
            }
            (Panel::Permissions, 0) => {
                let Some(rule) = self.list_selection().and_then(|at| self.shown_rules.get(at)).cloned() else {
                    self.problem_in_panel(t("Choose a rule."));
                    return;
                };
                let removed = rules::remove(&self.folder.join("permissions.json"), &rule);
                self.set_panel(Panel::Permissions);
                if let Err(error) = removed {
                    self.problem_in_panel(&error);
                }
            }
            (Panel::ConfirmDelete, 1) => {
                self.delete_target = None;
                self.set_panel(Panel::Chat);
            }
            (Panel::ConfirmDelete, 0) => {
                self.set_panel(Panel::Chat);
                if let Some((path, _)) = self.delete_target.take() {
                    let current = self.current_session.as_ref() == Some(&path);
                    let _ = std::fs::remove_file(&path);
                    let _ = dogmind_agent::checkpoint::remove_for(&path);
                    if current {
                        let workspace = self.workspace.clone();
                        self.start_session(workspace);
                    }
                    self.refresh_sessions();
                }
            }
            (Panel::Privacy, 0) => self.set_panel(if self.crypt.keystore.is_some() { Panel::CryptoPassphrase } else { Panel::CryptoOn }),
            (Panel::Privacy, 1) if self.crypt.keystore.is_some() => self.set_panel(Panel::CryptoCode),
            (Panel::Privacy, 2) if self.crypt.keystore.is_some() => self.set_panel(Panel::CryptoOff),
            (Panel::CryptoOn | Panel::CryptoPassphrase | Panel::CryptoCode | Panel::CryptoOff, 1) => self.set_panel(Panel::Privacy),
            (Panel::CryptoOn, 0) => self.crypt_turn_on(),
            (Panel::CryptoPassphrase, 0) => self.crypt_change_passphrase(),
            (Panel::CryptoCode, 0) => self.crypt_new_code(),
            (Panel::CryptoOff, 0) => self.crypt_turn_off(),
            (Panel::CryptoWork, 0) => self.crypt_done(),
            (Panel::Unlock, 0) => self.crypt_unlock(),
            (Panel::Unlock, 1) => {
                self.crypt.by_code = !self.crypt.by_code;
                self.set_panel(Panel::Unlock);
            }
            (Panel::Rename, 0) => {
                let name = get_text(self.fields[0]);
                let name = name.trim().to_string();
                if let (Some(path), false) = (self.rename_target.take(), name.is_empty()) {
                    let _ = session::rename(&path, &name);
                    if self.current_session.as_ref() == Some(&path) {
                        self.session_title = name;
                    }
                    self.refresh_sessions();
                }
                self.set_panel(Panel::Chat);
            }
            (Panel::Sessions, 0) => {
                if let Some(path) = self.list_selection().and_then(|at| self.resume_paths.get(at)).cloned() {
                    self.open_session(path);
                }
            }
            (Panel::Start, 1) if self.start_folders.is_empty() => self.start_back(),
            (Panel::Start, 0) if self.start_folders.is_empty() => {
                if let Some(folder) = self.browse_folder() {
                    self.start_session(folder);
                }
            }
            (Panel::Start, 0) => {
                if let Some(folder) = self.list_selection().and_then(|at| self.start_folders.get(at)).cloned() {
                    let _ = std::fs::create_dir_all(&folder);
                    self.start_session(folder);
                } else {
                    self.problem_in_panel(t("Choose a folder from the list, or another one."));
                }
            }
            (Panel::Start, 1) => {
                if let Some(folder) = self.browse_folder() {
                    self.start_session(folder);
                }
            }
            (Panel::Start, 2) => self.start_back(),
            _ => {}
        }
    }

    unsafe fn start_back(&mut self) {
        if self.onboarding {
            self.set_panel(Panel::Setup);
        } else if self.settings.workspace.is_some() {
            self.set_panel(Panel::Chat);
        } else {
            self.problem_in_panel(t("A session needs a folder to work in: choose one."));
        }
    }

    /// A menu under a control: dark where Windows allows, and its answer comes as a command.
    unsafe fn popup(&mut self, anchor: HWND, items: &[(usize, String, bool)]) {
        let menu = CreatePopupMenu();
        for (id, text, checked) in items {
            if *id == 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            } else {
                let text = wide(text);
                AppendMenuW(menu, MF_STRING | if *checked { MF_CHECKED } else { 0 }, *id, text.as_ptr());
            }
        }
        let mut area: RECT = std::mem::zeroed();
        GetWindowRect(anchor, &mut area);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_LEFTALIGN, area.left, area.top - 4, 0, self.window, std::ptr::null());
        DestroyMenu(menu);
        if chosen > 0 {
            self.on_menu(chosen as usize);
        }
    }

    unsafe fn popup_at(&mut self, x: i32, y: i32, align: u32, items: &[(usize, String, bool)]) {
        let menu = CreatePopupMenu();
        for (id, text, checked) in items {
            if *id == 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            } else {
                let text = wide(text);
                AppendMenuW(menu, MF_STRING | if *checked { MF_CHECKED } else { 0 }, *id, text.as_ptr());
            }
        }
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | align | TPM_LEFTALIGN, x, y, 0, self.window, std::ptr::null());
        DestroyMenu(menu);
        if chosen > 0 {
            self.on_menu(chosen as usize);
        }
    }

    /// What this setting can be, under its chip.
    unsafe fn choices(&mut self, anchor: HWND, base: usize, names: &[&str], chosen: usize) {
        let items: Vec<(usize, String, bool)> = names.iter().enumerate().map(|(at, name)| (base + at, t(name).to_string(), at == chosen)).collect();
        let mut area: RECT = std::mem::zeroed();
        GetWindowRect(anchor, &mut area);
        self.popup_at(area.left, area.bottom + 2, TPM_TOPALIGN, &items);
    }

    /// The display language has changed, or the app has just started: what was written once is written again, and a panel
    /// that is open is opened again. What is already in a conversation stays in the words it was said in.
    unsafe fn apply_language(&mut self) {
        lang::set(&self.settings.display_language);
        for (window, words) in [
            (self.new_session, "plus|New session"),
            (self.settings_button, "settings|Settings"),
            (self.approve, "Yes (Y)"),
            (self.always, "Always (A)"),
            (self.reject, "No (N)"),
            (self.composer_cue, "Message Dogmind"),
        ] {
            set_text(window, t(words));
        }
        for (edit, words) in [(self.feedback, "Reason (optional)"), (self.find_field, "Find in this conversation"), (self.side_search, "Search sessions")] {
            let cue = wide(t(words));
            SendMessageW(edit, EM_SETCUEBANNER, 0, cue.as_ptr() as LPARAM);
        }
        // What has only an icon says what it is when the pointer rests on it.
        for (window, words) in [
            (self.help_button, t("Help")),
            (self.goal_clear, t("Clear the goal")),
            (self.right_toggle, t("Show or hide the panel on the right")),
            (self.attach_button, t("Attach a picture")),
            (self.preview_reload, t("Reload")),
            (self.preview_browser, t("Open in browser")),
            (self.preview_close, t("Close the preview")),
            (self.find_previous, t("Previous")),
            (self.find_next, t("Next")),
            (self.find_close, t("Close")),
            (self.provider_up, t("Move up")),
            (self.provider_down, t("Move down")),
        ] {
            self.tip(window, words);
        }
        self.tip(self.modal_close, t("Close"));
        self.tip(self.diff_previous, t("Previous change (P)"));
        self.tip(self.diff_next, t("Next change (N)"));
        self.tip(self.diff_close, t("Close"));
        self.tip(self.provider_up, t("Move up"));
        self.tip(self.provider_down, t("Move down"));
        self.terminal_words();
        self.update_chips();
        self.update_toggles();
        self.update_drafter_chip();
        self.update_speed_chip();
        // (with an empty box from the start, not lit until the first thing is typed and taken back)
        self.update_send_enabled();
        self.refresh_sessions();
        if matches!(self.panel, Panel::General | Panel::Setup | Panel::Welcome) || self.modal_open {
            self.set_panel(self.panel);
        }
        self.layout();
    }

    unsafe fn on_menu(&mut self, id: usize) {
        match id {
            _ if (M_DISPLAY..M_DISPLAY + DISPLAY_LANGUAGES.len()).contains(&id) => {
                self.settings.display_language = DISPLAY_LANGUAGES[id - M_DISPLAY].0.to_string();
                self.save();
                self.apply_language();
            }
            _ if (M_THEME..M_THEME + THEMES.len()).contains(&id) => {
                self.settings.theme = THEMES[id - M_THEME].0.to_string();
                self.save();
                self.apply_theme();
                self.update_toggles();
            }
            _ if (M_ANSWERS..M_ANSWERS + ANSWER_LANGUAGES.len()).contains(&id) => {
                self.settings.language = ANSWER_LANGUAGES[id - M_ANSWERS].0.to_string();
                self.save();
                let language = self.settings.language.clone();
                let _ = self.agent.commands.send(Command::SetLanguage(language.clone()));
                for parked in &self.parked {
                    let _ = parked.agent.commands.send(Command::SetLanguage(language.clone()));
                }
                self.update_toggles();
            }
            _ if (M_WEB_CHOICE..M_WEB_CHOICE + 2).contains(&id) => {
                self.settings.web_tools = id == M_WEB_CHOICE;
                self.save();
                self.apply_web();
                self.update_toggles();
            }
            M_FOLDER_CHOOSE => {
                if let Some(folder) = self.browse_folder() {
                    self.go_to_folder(folder);
                }
            }
            _ if (M_FOLDER_RECENT..M_FOLDER_RECENT + FOLDERS_SHOWN).contains(&id) => {
                let folder = self.settings.workspaces.iter().filter(|folder| folder.is_dir()).nth(id - M_FOLDER_RECENT).cloned();
                if let Some(folder) = folder {
                    self.go_to_folder(folder);
                }
            }
            M_OPEN_SESSION | M_PIN_SESSION | M_RENAME_SESSION | M_DELETE_SESSION | M_NEW_THERE => self.on_session_menu(id),
            M_COMPACT => {
                if !self.connected {
                    self.say(t("The engine is not ready yet: ask again once it is."));
                } else if !self.running {
                    self.running = true;
                    self.update_buttons();
                    let _ = self.agent.commands.send(Command::Compact);
                }
            }
            M_COPY => self.copy_to_clipboard(&self.last_answer.clone()),
            M_MODELS => self.set_panel(Panel::Models),
            M_PROVIDERS => self.set_panel(Panel::Providers),
            M_RESTART => self.connect(),
            M_LOG => {
                let (open, path) = (wide("open"), wide(&engine::log_path(&self.folder).display().to_string()));
                ShellExecuteW(self.window, open.as_ptr(), path.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL as i32);
            }
            M_SAFE => {
                self.settings.safe_commands = !self.settings.safe_commands;
                let _ = self.agent.commands.send(Command::SetSafeCommands(self.settings.safe_commands));
                self.save();
                self.update_toggles();
            }
            M_THINKING => {
                self.settings.show_thinking = !self.settings.show_thinking;
                self.save();
                self.update_toggles();
            }
            _ if (M_MODE..M_MODE + MODES.len()).contains(&id) => {
                let (mode, _) = MODES[id - M_MODE];
                self.settings.mode = mode.name().to_string();
                let _ = self.agent.commands.send(Command::SetMode(mode));
                self.every_agent(|live| live.mode = mode);
                self.save();
                self.update_status();
            }
            _ if (M_EFFORT..M_EFFORT + EFFORTS.len()).contains(&id) => {
                self.settings.effort = EFFORTS[id - M_EFFORT].0.map(str::to_string);
                self.send_effort();
                self.save();
                self.update_status();
            }
            _ if (M_KIND..M_KIND + Kind::ALL.len()).contains(&id) => {
                self.kind = Kind::ALL[id - M_KIND];
                set_text(self.kind_chip, t(self.kind.label()));
                let shown = ProviderConfig { name: get_text(self.fields[0]), kind: self.kind, base_url: String::new(), model: String::new(), api_key: String::new() };
                self.show_provider_fields(&shown);
            }
            _ if id >= M_PROVIDER && id - M_PROVIDER < self.settings.providers.len() => {
                self.settings.active = self.settings.providers[id - M_PROVIDER].name.clone();
                self.save();
                self.connect();
            }
            // Kept with the provider, so that going back to it comes back to this.
            _ if id >= M_MODEL_CHOICE && id - M_MODEL_CHOICE < self.provider_models.len() => {
                let chosen = self.provider_models[id - M_MODEL_CHOICE].clone();
                let active = self.settings.active.clone();
                if let Some(provider) = self.settings.providers.iter_mut().find(|provider| provider.name == active) {
                    provider.model = chosen;
                }
                self.save();
                self.connect();
            }
            _ => {}
        }
    }

    unsafe fn on_chip(&mut self, id: isize) {
        match id {
            ID_MODEL_CHIP => {
                let items = model_chip_items(&self.settings.providers, &self.settings.active, &self.provider_models, &self.model);
                self.popup(self.model_chip, &items);
            }
            ID_MODE_CHIP => {
                let mode = self.current_mode();
                let mut items: Vec<(usize, String, bool)> = MODES.iter().enumerate().map(|(at, (known, name))| (M_MODE + at, t(name).to_string(), *known == mode)).collect();
                items.push((0, String::new(), false));
                items.push((M_SAFE, t("Allow safe commands without asking").into(), self.settings.safe_commands));
                self.popup(self.mode_chip, &items);
            }
            ID_EFFORT_CHIP => {
                let mut items: Vec<(usize, String, bool)> = self.efforts().iter().enumerate().map(|(at, (known, name))| (M_EFFORT + at, t(name).to_string(), *known == self.settings.effort.as_deref())).collect();
                items.push((0, String::new(), false));
                items.push((M_THINKING, t("Show the thinking").into(), self.settings.show_thinking));
                self.popup(self.effort_chip, &items);
            }
            ID_KIND_CHIP => {
                let names = Kind::ALL.map(|kind| kind.label());
                let chosen = Kind::ALL.iter().position(|kind| *kind == self.kind).unwrap_or(0);
                self.choices(self.kind_chip, M_KIND, &names, chosen);
            }
            _ => {}
        }
    }
}

unsafe fn app<'a>() -> Option<&'a mut App> {
    APP.load(Ordering::Relaxed).as_mut()
}

/// The transcript: when it moves, the copy buttons on it move too.
/// The hint lies over the composer's first line, and what an IME is composing is not yet its text: the hint goes when composing begins.
unsafe extern "system" fn composer_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let base = std::mem::transmute::<isize, WNDPROC>(app.composer_procedure);
    if message == WM_IME_STARTCOMPOSITION_ || message == WM_IME_COMPOSITION_ {
        app.composing = true;
        app.update_composer_cue();
    }
    // (what is being composed changes nothing in the box's eyes until it is settled, and a first sentence in Japanese can take a while)
    if message == WM_IME_STARTCOMPOSITION_ {
        app.message_begun();
    }
    if message == WM_RBUTTONUP {
        app.edit_menu(window, lparam, true);
        return 0;
    }
    let result = with_thin_bar(app, window, base, message, wparam, lparam);
    if message == WM_IME_ENDCOMPOSITION_ {
        app.composing = false;
        app.update_composer_cue();
    }
    result
}

unsafe extern "system" fn doc_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    if message == WM_RBUTTONUP {
        let can_change = GetWindowLongPtrW(window, GWL_STYLE) as u32 & ES_READONLY as u32 == 0;
        app.edit_menu(window, lparam, can_change);
        return 0;
    }
    with_thin_bar(app, window, std::mem::transmute::<isize, WNDPROC>(app.doc_procedure), message, wparam, lparam)
}

/// The words of an approval.
unsafe extern "system" fn note_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    with_thin_bar(app, window, std::mem::transmute::<isize, WNDPROC>(app.note_procedure), message, wparam, lparam)
}

/// Where a control's thin scrollbar has its thumb now. `None`: all of it is in view.
unsafe fn thin_thumb(window: HWND, dpi: u32) -> Option<RECT> {
    // (with all of it in view a control hides or disables its bar, and what it says of the range is left from when there was more)
    let mut state: SCROLLBARINFO = std::mem::zeroed();
    state.cbSize = std::mem::size_of::<SCROLLBARINFO>() as u32;
    if GetScrollBarInfo(window, -5, &mut state) == 0 || state.rgstate[0] & (1 | 0x8000) != 0 { // OBJID_VSCROLL, STATE_SYSTEM_UNAVAILABLE | STATE_SYSTEM_INVISIBLE
        return None;
    }
    let mut bar: SCROLLINFO = std::mem::zeroed();
    bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
    bar.fMask = SIF_ALL;
    if GetScrollInfo(window, SB_VERT, &mut bar) == 0 {
        return None;
    }
    let (range, page) = (bar.nMax - bar.nMin + 1, bar.nPage as i32);
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);
    let inset = scale(dpi, ds::space::XS);
    let track = client.bottom - 2 * inset;
    if page <= 0 || range <= page || track <= 0 {
        return None;
    }
    let height = thumb_height(track, page, range, scale(dpi, ds::space::XL));
    let top = inset + thumb_offset(track - height, bar.nPos - bar.nMin, range - page);
    let right = client.right - scale(dpi, ds::space::HAIR);
    Some(RECT { left: right - scale(dpi, ds::size::THUMB), top, right, bottom: top + height })
}

/// A control's own scrollbar is square and in its own colours whatever the app's are: a region takes it off, and a thumb like the transcript's stands at the end of what is left.
/// All a control with one needs of it goes through here, in place of calling the control.
unsafe fn with_thin_bar(app: &mut App, window: HWND, base: WNDPROC, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
    let grabbed = app.bar_grab.filter(|(held, _)| *held == window).map(|(_, grab)| grab);
    match message {
        WM_LBUTTONDOWN => {
            let x = (lparam & 0xFFFF) as i16 as i32;
            if let Some(thumb) = thin_thumb(window, app.dpi).filter(|thumb| x >= thumb.left - scale(app.dpi, ds::space::S) && y >= thumb.top && y < thumb.bottom) {
                app.bar_grab = Some((window, y - thumb.top));
                SetCapture(window);
                return 0;
            }
        }
        WM_MOUSEMOVE if grabbed.is_some() => {
            let mut bar: SCROLLINFO = std::mem::zeroed();
            bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
            bar.fMask = SIF_ALL;
            GetScrollInfo(window, SB_VERT, &mut bar);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            if let Some(thumb) = thin_thumb(window, app.dpi) {
                let inset = scale(app.dpi, ds::space::XS);
                let room = (client.bottom - 2 * inset - (thumb.bottom - thumb.top)).max(1);
                let (range, page) = (bar.nMax - bar.nMin + 1, bar.nPage as i32);
                let at = (y - grabbed.unwrap_or(0) - inset).clamp(0, room);
                let position = bar.nMin + (i64::from(at) * i64::from(range - page) / i64::from(room)) as i32;
                CallWindowProcW(base, window, WM_VSCROLL, (SB_THUMBPOSITION as usize) | ((position as usize & 0xFFFF) << 16), 0);
                InvalidateRect(window, std::ptr::null(), 1);
            }
            return 0;
        }
        WM_LBUTTONUP if grabbed.is_some() => {
            app.bar_grab = None;
            ReleaseCapture();
            return 0;
        }
        WM_CAPTURECHANGED if grabbed.is_some() => app.bar_grab = None,
        _ => {}
    }
    let was = GetScrollPos(window, SB_VERT);
    let result = CallWindowProcW(base, window, message, wparam, lparam);
    // (what scrolled took the old thumb with it)
    if GetScrollPos(window, SB_VERT) != was {
        InvalidateRect(window, std::ptr::null(), 1);
    }
    match message {
        WM_SIZE => {
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            SetWindowRgn(window, CreateRectRgn(0, 0, client.right, client.bottom), 1);
        }
        WM_PAINT | WM_PRINT | WM_PRINTCLIENT => {
            if let Some(thumb) = thin_thumb(window, app.dpi) {
                let device = if message == WM_PAINT { GetDC(window) } else { wparam as HDC };
                rounded(device, thumb, (thumb.right - thumb.left) / 2, Some(theme::line()), None);
                if message == WM_PAINT {
                    ReleaseDC(window, device);
                }
            }
        }
        _ => {}
    }
    result
}

unsafe extern "system" fn transcript_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let base = std::mem::transmute::<isize, WNDPROC>(app.transcript_procedure);
    if message == WM_RBUTTONUP {
        app.edit_menu(window, lparam, false);
        return 0;
    }
    if message == WM_MOUSEMOVE && !(app.links.is_empty() && app.runs.is_empty()) {
        app.track_link(lparam);
    }
    // The icons over the text are drawn in its paint: the pointer is theirs when it is over one, and a press on one is not a press on the text.
    if message == WM_MOUSEMOVE {
        let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        app.overlay_pointer(x, y);
    }
    if matches!(message, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
        let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        if let Some(key) = app.overlay_at(x, y) {
            app.overlay_press = Some(key);
            SetCapture(window);
            app.invalidate_overlay(key);
            return 0;
        }
    }
    if message == WM_LBUTTONUP && app.overlay_press.is_some() {
        let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        let pressed = app.overlay_press.take();
        ReleaseCapture();
        if let Some(key) = pressed {
            app.invalidate_overlay(key);
            if app.overlay_at(x, y) == Some(key) {
                app.overlay_act(key);
            }
        }
        return 0;
    }
    if message == WM_MOUSEMOVE && !app.pointer_tracked && !test_pointer_stays() {
        app.pointer_tracked = true;
        let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: window, dwHoverTime: 0 };
        TrackMouseEvent(&mut track);
    }
    if message == WM_MOUSELEAVE {
        app.pointer_tracked = false;
        app.run_hover = None;
        // (a test that has no pointer of its own: the leave the system sends when the capture changes is not the pointer leaving)
        if let Some(key) = if test_pointer_stays() { None } else { app.overlay_hover.take() } {
            app.invalidate_overlay(key);
            app.keep_overlay_tip();
        }
        if app.link_hover.take().is_some() {
            InvalidateRect(app.window, &app.status_area, 0);
        }
    }
    if message == WM_SETCURSOR && app.overlay_hover.is_some() {
        SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
        return 1;
    }
    // (a row that has nothing more to show only says its whole path)
    if message == WM_SETCURSOR && app.link_hover.is_some() && app.run_hover.map_or(true, |run| app.runs[run].expandable()) {
        SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
        return 1;
    }
    // Over italic words (the thinking) RichEdit slants its I-beam, and keeps it slanted wherever the pointer goes next: the plain one, over all of the text.
    if message == WM_SETCURSOR && wparam as HWND == window && (lparam & 0xFFFF) as u32 == HTCLIENT {
        SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_IBEAM));
        return 1;
    }
    if message == WM_LBUTTONUP {
        let mut range = CharRange { first: 0, last: 0 };
        SendMessageW(window, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
        // (a drag that ended on a link was to select)
        if range.first == range.last {
            if let Some(run) = app.run_under(lparam) {
                let result = CallWindowProcW(base, window, message, wparam, lparam);
                app.toggle_run(run);
                return result;
            }
            if let Some(url) = app.link_hover.clone() {
                let result = CallWindowProcW(base, window, message, wparam, lparam);
                app.open_link(&url);
                return result;
            }
        }
    }
    // The person scrolling up takes the view; coming back to the end gives it back (at_end).
    let wheel_up = message == WM_MOUSEWHEEL && ((wparam >> 16) & 0xFFFF) as u16 as i16 > 0;
    let bar_up = message == WM_VSCROLL && matches!((wparam & 0xFFFF) as i32, SB_LINEUP | SB_PAGEUP | SB_THUMBTRACK | SB_THUMBPOSITION);
    let keys_up = message == WM_KEYDOWN && matches!(wparam as u16, VK_UP | VK_PRIOR | VK_HOME);
    if wheel_up || bar_up || keys_up {
        app.follow_end = false;
        app.scrolled_up_at = Some(std::time::Instant::now());
    }
    if message == WM_SIZE {
        // Its rows go to another width: the cards are looked for again.
        app.user_cards.iter_mut().for_each(|card| card.shape = None);
        app.transcript_resized = true;
    }
    let mut about_to_paint: RECT = std::mem::zeroed();
    if message == WM_PAINT {
        GetUpdateRect(window, &mut about_to_paint, 0);
    }
    let result = if message == WM_MOUSEWHEEL && wparam & 0x0008 == 0 {
        // RichEdit turns a notch into lines, and its lines here are of every height: a card, a table's row, the room of a picture, a gap of
        // six points. So a notch went far or hardly at all, and differently at another width of the window. It goes the same distance now,
        // in pixels, with what a fine wheel sends kept until it adds up. (With Ctrl it is the control's zoom, left to it.)
        let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32;
        app.wheel_rest -= delta * scale(app.dpi, 100);
        let pixels = app.wheel_rest / 120;
        app.wheel_rest -= pixels * 120;
        if pixels != 0 {
            let mut position = POINT { x: 0, y: 0 };
            SendMessageW(window, EM_GETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
            // (no further than the end at the foot of the view: RichEdit takes a place past it, with nothing but the page under the last answer)
            let mut bar: SCROLLINFO = std::mem::zeroed();
            bar.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
            bar.fMask = SIF_RANGE | SIF_PAGE;
            GetScrollInfo(window, SB_VERT, &mut bar);
            let last = (bar.nMax - bar.nPage as i32 + 1).max(0);
            position.y = (position.y + pixels).clamp(0, last);
            SendMessageW(window, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        }
        0
    } else {
        CallWindowProcW(base, window, message, wparam, lparam)
    };
    if matches!(message, WM_VSCROLL | WM_MOUSEWHEEL | WM_KEYDOWN | WM_LBUTTONUP | WM_SIZE) {
        app.place_thumb();
        // (what moved under the pointer is what it is on now)
        if matches!(message, WM_VSCROLL | WM_MOUSEWHEEL | WM_KEYDOWN) {
            app.overlay_pointer_again();
        }
    }
    if matches!(message, WM_PAINT | WM_TIMER) && app.panel == Panel::Chat {
        app.follow_the_thumb();
    }
    if matches!(message, WM_PAINT | WM_PRINT | WM_PRINTCLIENT) {
        if message != WM_PAINT {
            GetClientRect(window, &mut about_to_paint);
        }
        app.round_user_cards(wparam as HDC, about_to_paint);
    }
    result
}

/// A run's lines into the transcript: over those it has at `place`, or at the caret, at the end. Returns where they begin, where the first line's words end
/// and where the last line ends. Its last line's end is left in place when they are written over, as whatever comes next (a table's row) begins right behind it.
unsafe fn put_run_lines(edit: HWND, place: Option<(i32, i32)>, lines: &[(String, LineKind)]) -> (i32, i32, i32) {
    let joined = lines.iter().map(|(line, _)| line.as_str()).collect::<Vec<_>>().join("\r");
    let first = match place {
        Some((first, last)) => {
            let range = CharRange { first, last: last - 1 };
            SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
            first
        }
        None => {
            let mut range = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
            range.first
        }
    };
    let text = wide(&if place.is_some() { joined } else { joined + "\r" });
    SendMessageW(edit, EM_REPLACESEL, 0, text.as_ptr() as LPARAM);
    let (ui, mono) = (UI_FACE, "Consolas");
    let mut at = first;
    let mut head_end = first;
    for (index, (line, kind)) in lines.iter().enumerate() {
        let (format, paragraph) = match kind {
            LineKind::Head(Tone::Done) => (char_format(theme::muted(), 0, ui, 9, None), para_format(9, 0, 60, 40)),
            LineKind::Head(Tone::Bad | Tone::Warn) => (char_format(theme::soft(), 0, ui, 9, None), para_format(9, 0, 60, 40)),
            LineKind::Entry => (char_format(theme::muted(), 0, ui, 9, None), para_format(9, 240, 0, 20)),
            LineKind::Output => (char_format(theme::code(), 0, mono, 9, None), para_format(9, 480, 0, 0)),
            LineKind::Added => (char_format(theme::good(), 0, mono, 9, None), para_format(9, 480, 0, 0)),
            LineKind::Removed => (char_format(theme::bad(), 0, mono, 9, None), para_format(9, 480, 0, 0)),
            LineKind::Hunk => (char_format(theme::accent_hover(), 0, mono, 9, None), para_format(9, 480, 0, 0)),
        };
        let units = line.encode_utf16().count() as i32;
        let words = CharRange { first: at, last: at + units };
        SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(words) as LPARAM);
        SendMessageW(edit, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
        let whole = CharRange { first: at, last: at + units + 1 };
        SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(whole) as LPARAM);
        SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
        if let LineKind::Head(tone @ (Tone::Bad | Tone::Warn)) = kind {
            let mark = CharRange { first: at, last: at + 1 };
            let format = char_format(if *tone == Tone::Bad { theme::bad() } else { theme::warn() }, 0, ui, 9, None);
            SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(mark) as LPARAM);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
        }
        if index == 0 {
            head_end = at + units;
        }
        at += units + 1;
    }
    SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
    (first, head_end, at)
}

/// Where a cell's text begins, after what the control keeps at the start of a table's row.
unsafe fn text_start(edit: HWND, first: i32) -> i32 {
    #[repr(C)]
    struct TextRange {
        range: CharRange,
        text: *mut u16,
    }
    let mut units = [0u16; 9];
    let mut range = TextRange { range: CharRange { first, last: first + 8 }, text: units.as_mut_ptr() };
    let got = SendMessageW(edit, WM_USER + 75, 0, std::ptr::addr_of_mut!(range) as LPARAM).clamp(0, 8) as usize;
    first + units[..got].iter().position(|unit| !matches!(unit, 0xFFF9 | 0xFFFA | 0xFFFB | 0x0D | 0x0A | 0x07)).unwrap_or(0) as i32
}

/// A card of what the person wrote: where its text begins, and once it has been seen whole, where its edges are from there.
struct UserCard {
    start: i32,
    kind: CardKind,
    shape: Option<CardShape>,
    /// When it was last looked for and not seen whole: a card taller than the view is never seen whole, and all the view's pixels are not read for each of its paints.
    looked_at: Option<std::time::Instant>,
}

/// What a card is, which says its colour: the theme's of now, as a card outlives a change of theme.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CardKind {
    /// What the person wrote.
    Person,
    /// A block of code or of a command's output.
    Block,
}

impl CardKind {
    fn back(self) -> u32 {
        match self {
            CardKind::Person => theme::user_back(),
            CardKind::Block => theme::code_back(),
        }
    }
}

/// Whether a card whose foot was found at some row goes on below it now: the row's pixel and the one under it are both the card's colour.
fn card_has_grown(inside: u32, below: u32, back: u32) -> bool {
    inside == back && below == back
}

/// The squares at the two lower corners of a card, which are drawn over to round it.
fn foot_corners(card: &CardArea, radius: i32) -> [RECT; 2] {
    [
        RECT { left: card.left, top: card.bottom + 1 - radius, right: card.left + radius, bottom: card.bottom + 1 },
        RECT { left: card.right + 1 - radius, top: card.bottom + 1 - radius, right: card.right + 1, bottom: card.bottom + 1 },
    ]
}

/// Whether the rows of a block are each drawn as they come: a debug build with `DOGMIND_TEST_ROWPAINT` set to a number of milliseconds draws the transcript after each and waits that long,
/// as it is drawn when a command's output comes in over some time while the window is being painted for other reasons.
fn test_rows_painted() -> Option<u64> {
    static ON: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    if !cfg!(debug_assertions) {
        return None;
    }
    *ON.get_or_init(|| std::env::var("DOGMIND_TEST_ROWPAINT").ok().map(|text| text.parse().unwrap_or(0)))
}

/// A debug build with `DOGMIND_TEST_POINTER` (or `DOGMIND_TEST_HOVER`) set keeps what the pointer is on as it is: a test sends the transcript's window the pointer's messages, the real pointer
/// being elsewhere, and the system's own word that it has left (it comes when the capture changes) is not the pointer leaving.
fn test_pointer_stays() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cfg!(debug_assertions) && *ON.get_or_init(|| std::env::var_os("DOGMIND_TEST_POINTER").is_some()) || forced_hover()
}

/// A debug build with `DOGMIND_TEST_NOFADE` set draws no fade at the edges of the text: a test that reads colours of the cards near them sees them as they are.
fn test_no_fade() -> bool {
    cfg!(debug_assertions) && std::env::var_os("DOGMIND_TEST_NOFADE").is_some()
}

/// A debug build with `DOGMIND_TEST_STALE_SHAPES` set keeps a card's shape as it was first taken, which is how the corners were drawn before they followed the card: to see what that was.
fn test_stale_shapes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cfg!(debug_assertions) && *ON.get_or_init(|| std::env::var_os("DOGMIND_TEST_STALE_SHAPES").is_some())
}

/// A card's edges from the start of its text, and the colour of the page around it.
#[derive(Clone, Copy)]
struct CardShape {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
    page: u32,
}

impl CardShape {
    fn around(card: &CardArea, text: POINT, page: u32) -> CardShape {
        CardShape { left: card.left - text.x, right: card.right - text.x, top: card.top - text.y, bottom: card.bottom - text.y, page }
    }

    /// Where it is with its text here. An end above or below the view is only clipped.
    fn at(&self, text: POINT) -> CardArea {
        CardArea { left: text.x + self.left, right: text.x + self.right, top: text.y + self.top, bottom: text.y + self.bottom, top_open: false, bottom_open: false }
    }
}

/// What a window shows, read at once: for looking at many of its pixels.
struct ViewPixels {
    pixels: Vec<u32>,
    width: i32,
    height: i32,
}

impl ViewPixels {
    unsafe fn of(device: HDC, client: RECT) -> ViewPixels {
        let (width, height) = (client.right.max(0), client.bottom.max(0));
        let mut pixels = vec![0u32; width as usize * height as usize];
        let copy = CreateCompatibleDC(device);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height; // from the top down
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(copy, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if !bitmap.is_null() && !bits.is_null() {
            let previous = SelectObject(copy, bitmap);
            BitBlt(copy, 0, 0, width, height, device, 0, 0, SRCCOPY);
            GdiFlush();
            std::ptr::copy_nonoverlapping(bits as *const u32, pixels.as_mut_ptr(), pixels.len());
            SelectObject(copy, previous);
            DeleteObject(bitmap);
        }
        DeleteDC(copy);
        ViewPixels { pixels, width, height }
    }

    /// As GetPixel gives it.
    fn at(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return u32::MAX;
        }
        let pixel = self.pixels[y as usize * self.width as usize + x as usize];
        ((pixel & 0xFF) << 16) | (pixel & 0xFF00) | ((pixel >> 16) & 0xFF)
    }
}

/// A card's edges, inclusive, and which of its ends are out of view.
struct CardArea {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
    top_open: bool,
    bottom_open: bool,
}

/// The card around a point just before its text begins. `None`: it is not in view, or what is there is not one card's rectangle.
/// `seam`: the most pixels from one that is the card's to the next that is when the rows of a card are walked across (3 design pixels, in this window's scale: the seam between rows is a
/// fraction of a row's height and grows with the type, as the gap between two cards, which is more than that, does too).
fn find_card(client: RECT, text: POINT, radius: i32, seam: i32, is_card: impl Fn(i32, i32) -> bool) -> Option<CardArea> {
    let x = text.x - 2;
    if x <= client.left || x >= client.right || text.y >= client.bottom {
        return None;
    }
    let from = text.y.max(client.top);
    if !is_card(x, from) {
        return None;
    }
    // A card that is rows of a table has a seam between two of them, a line or two of another colour, where the control draws their edges at fractions of a pixel: it is walked over.
    let reach = |from: i32, step: i32, limit: i32| -> i32 {
        let mut y = from;
        loop {
            let found = (1..=seam.max(3)).map(|gap| y + step * gap).take_while(|at| if step > 0 { *at < limit } else { *at > limit }).find(|at| is_card(x, *at));
            match found {
                Some(at) => y = at,
                None => return y,
            }
        }
    };
    let (top, bottom) = (reach(from, -1, client.top - 1), reach(from, 1, client.bottom));
    let (top_open, bottom_open) = (top == client.top, bottom + 1 == client.bottom);
    if top_open && bottom_open || bottom - top < 2 * radius {
        return None;
    }
    // Past the corners, drawn or not. Left of the text there is none of it to stop at, and the right edge is walked to from
    // outside, as it would stop at the text from inside.
    let row = from.clamp(top + radius, bottom - radius);
    let mut left = x;
    while left > client.left && is_card(left - 1, row) {
        left -= 1;
    }
    let mut right = client.right - 1;
    while right > x && !is_card(right, row) {
        right -= 1;
    }
    let height = bottom - top;
    let whole = left > client.left && right - left >= 2 * radius && height >= 2 * radius
        && (top + radius..=bottom - radius).all(|y| !is_card(x, y) || (is_card(left, y) && is_card(right, y) && !is_card(left - 1, y) && (right + 1 == client.right || !is_card(right + 1, y))));
    whole.then_some(CardArea { left, right, top, bottom, top_open, bottom_open })
}

/// How much of a pixel in a corner, counted from the corner, the rounded card covers.
fn corner_cover(radius: i32, i: i32, j: i32) -> f32 {
    let (dx, dy) = (radius as f32 - (i as f32 + 0.5), radius as f32 - (j as f32 + 0.5));
    (radius as f32 - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0)
}

fn mix(under: u32, over: u32, cover: f32) -> u32 {
    let channel = |shift: u32| {
        let (from, to) = (((under >> shift) & 0xFF) as f32, ((over >> shift) & 0xFF) as f32);
        ((from + (to - from) * cover).round() as u32) << shift
    };
    channel(0) | channel(8) | channel(16)
}

/// The list boxes, with hover and, in the sidebar, a click that knows where it fell.
unsafe extern "system" fn list_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let base = std::mem::transmute::<isize, WNDPROC>(app.list_procedure);
    let is_side = window == app.side;
    let is_tree = window == app.tree_list;
    match message {
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            // (a test that has no pointer of its own: the leave the system sends is not the pointer leaving)
            if message == WM_MOUSELEAVE && is_tree && test_pointer_stays() {
                return 0;
            }
            let found = if message == WM_MOUSEMOVE { SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam) } else { 0x10000 };
            let hover = ((found >> 16) & 0xFFFF == 0).then_some((found & 0xFFFF) as usize);
            let slot = if is_side { &mut app.side_hover } else if is_tree { &mut app.tree_hover } else if window == app.suggest { &mut app.suggest_hover } else { &mut app.list_hover };
            if *slot != hover {
                for index in [*slot, hover].into_iter().flatten() {
                    let mut area: RECT = std::mem::zeroed();
                    if SendMessageW(window, LB_GETITEMRECT, index, std::ptr::addr_of_mut!(area) as LPARAM) != -1 {
                        InvalidateRect(window, &area, 0);
                    }
                }
                *slot = hover;
                let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: window, dwHoverTime: 0 };
                TrackMouseEvent(&mut track);
            }
            if message == WM_MOUSELEAVE {
                return 0;
            }
            with_thin_bar(app, window, base, message, wparam, lparam)
        }
        // The files of the folder: a press on a row's own button, a double click, the keys and the menu.
        WM_RBUTTONUP if is_tree => {
            let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
            if (found >> 16) & 0xFFFF == 0 {
                app.files_menu((found & 0xFFFF) as usize, ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32));
            }
            0
        }
        WM_CONTEXTMENU if is_tree => {
            app.files_menu_from_key();
            0
        }
        WM_LBUTTONUP if is_tree && app.bar_grab.is_none() => {
            let result = CallWindowProcW(base, window, message, wparam, lparam);
            let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
            if (found >> 16) & 0xFFFF == 0 {
                app.files_row_clicked((found & 0xFFFF) as usize, (lparam & 0xFFFF) as i16 as i32);
            }
            result
        }
        WM_LBUTTONDBLCLK if is_tree => {
            let result = CallWindowProcW(base, window, message, wparam, lparam);
            let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
            if (found >> 16) & 0xFFFF == 0 {
                app.files_row_double_clicked((found & 0xFFFF) as usize);
            }
            result
        }
        WM_KEYDOWN if is_tree && app.files_key(wparam as u16) => 0,
        WM_RBUTTONUP if is_side => {
            let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
            if (found >> 16) & 0xFFFF == 0 {
                app.on_side_menu((found & 0xFFFF) as usize);
            }
            0
        }
        WM_LBUTTONUP if is_side && app.bar_grab.is_none() => {
            let result = CallWindowProcW(base, window, message, wparam, lparam);
            let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
            if (found >> 16) & 0xFFFF == 0 {
                let x = (lparam & 0xFFFF) as i16 as i32;
                let index = (found & 0xFFFF) as usize;
                // It may rebuild this very list: after the control has finished with the click.
                app.on_side_click(index, x);
            }
            result
        }
        _ => with_thin_bar(app, window, base, message, wparam, lparam),
    }
}

/// The diff: the wheel, a drag of its thumb or across its rows.
unsafe extern "system" fn diff_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);
    let row = scale(app.dpi, ds::size::DIFF_ROW);
    let on_strip = x >= client.right - scale(app.dpi, ds::space::M);
    let row_at = |app: &App, y: i32| ((y + app.diff_scroll).max(0) / row) as usize;
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            if client.right > 0 && client.bottom > 0 {
                let device = CreateCompatibleDC(target);
                let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
                let previous = SelectObject(device, bitmap);
                app.paint_diff(device, client);
                BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            EndPaint(window, &paint);
            0
        }
        WM_MOUSEWHEEL => {
            app.diff_wheel(wparam);
            0
        }
        WM_LBUTTONDOWN => {
            SetCapture(window);
            if on_strip {
                let total = app.diff_len() as i32 * row;
                if total > client.bottom {
                    let tall = thumb_height(client.bottom, client.bottom, total, scale(app.dpi, ds::space::XXL));
                    let at = thumb_offset(client.bottom - tall, app.diff_scroll, total - client.bottom);
                    // (on the thumb it goes with the pointer; off it, the thumb comes to the pointer first)
                    let grab = if y >= at && y < at + tall { y - at } else { tall / 2 };
                    app.diff_thumb_grab = Some(grab);
                    app.diff_to((i64::from(y - grab) * i64::from(total - client.bottom) / i64::from((client.bottom - tall).max(1))) as i32);
                }
            } else {
                let at = row_at(app, y).min(app.diff_len().saturating_sub(1));
                // (on the trajectory page a press on a turn's row opens it or shuts it)
                if app.trajectory.is_some() {
                    // (not the header, which stays over the rows that scroll under it)
                    let hit = if y < row { None } else { app.trajectory.as_ref().and_then(|page| page.rows.get(row_at(app, y)).map(|line| line.kind)) };
                    if let Some(crate::trajectory::RowKind::Turn { turn, .. }) = hit {
                        app.toggle_trajectory_turn(turn);
                    }
                    return 0;
                }
                app.diff_chosen = Some((at, at));
                InvalidateRect(window, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSEMOVE if wparam & 1 != 0 => { // MK_LBUTTON
            if let Some(grab) = app.diff_thumb_grab {
                let total = app.diff_len() as i32 * row;
                let tall = thumb_height(client.bottom, client.bottom, total, scale(app.dpi, ds::space::XXL));
                app.diff_to((i64::from(y - grab) * i64::from((total - client.bottom).max(0)) / i64::from((client.bottom - tall).max(1))) as i32);
            } else if let Some((from, _)) = app.diff_chosen {
                let at = row_at(app, y).min(app.diff_len().saturating_sub(1));
                app.diff_chosen = Some((from, at));
                // (past an edge, the view goes along)
                if y < 0 {
                    app.diff_to(app.diff_scroll - row);
                } else if y > client.bottom {
                    app.diff_to(app.diff_scroll + row);
                } else {
                    InvalidateRect(window, std::ptr::null(), 0);
                }
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            if app.diff_thumb_grab.take().is_some() {
                InvalidateRect(window, std::ptr::null(), 0);
            } else if matches!(app.diff_chosen, Some((from, to)) if from == to) {
                // (a click chooses nothing)
                app.diff_chosen = None;
                InvalidateRect(window, std::ptr::null(), 0);
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// A dialog's backdrop and its pane. What their controls say goes to the main window, as it does from any control; they draw what `layout_modal` has set out.
unsafe extern "system" fn modal_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let within_card = |lparam: LPARAM| {
        let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        let card = app.modal_card;
        x >= card.left && x < card.right && y >= card.top && y < card.bottom
    };
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            if client.right > 0 && client.bottom > 0 {
                let device = CreateCompatibleDC(target);
                let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
                let previous = SelectObject(device, bitmap);
                app.paint_modal(window, device, client);
                BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            EndPaint(window, &paint);
            0
        }
        WM_COMMAND | WM_CTLCOLORLISTBOX | WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_MEASUREITEM | WM_DRAWITEM | WM_MOUSEWHEEL => procedure(app.window, message, wparam, lparam),
        WM_LBUTTONDOWN if window == app.modal => {
            app.modal_pressed_outside = !within_card(lparam);
            0
        }
        // The page's thumb: held, it goes with the pointer; pressed beside it, it comes to the pointer first. Its strip is a little wider than it, to be found.
        WM_LBUTTONDOWN if window == app.pane && app.pane_thumb.is_some_and(|(thumb, _, _)| (lparam & 0xFFFF) as i16 as i32 >= thumb.left - scale(app.dpi, 8)) => {
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
            if let Some((thumb, _, _)) = app.pane_thumb {
                let tall = thumb.bottom - thumb.top;
                let grab = if y >= thumb.top && y < thumb.bottom { y - thumb.top } else { tall / 2 };
                app.pane_thumb_grab = Some(grab);
                SetCapture(window);
                app.drag_pane_thumb(y);
            }
            0
        }
        WM_MOUSEMOVE if window == app.pane && app.pane_thumb_grab.is_some() => {
            app.drag_pane_thumb(((lparam >> 16) & 0xFFFF) as i16 as i32);
            0
        }
        WM_LBUTTONUP if window == app.pane && app.pane_thumb_grab.is_some() => {
            ReleaseCapture();
            0
        }
        WM_CAPTURECHANGED if window == app.pane && app.pane_thumb_grab.is_some() => {
            app.pane_thumb_grab = None;
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(app.window, &mut client);
            app.layout_modal(client.right, client.bottom);
            0
        }
        WM_LBUTTONUP if window == app.modal => {
            if std::mem::take(&mut app.modal_pressed_outside) && !within_card(lparam) {
                app.set_panel(Panel::Chat);
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    match message {
        WM_WAKE => {
            app.drain();
            0
        }
        WM_TERM => {
            app.terminal_wake(wparam as u64, lparam == 1);
            0
        }
        // (a debug build told to: the commands of a test, which has no keyboard, pointer or focus to give the terminals)
        #[cfg(debug_assertions)]
        WM_COPYDATA if term_panel::hooks_on() => {
            let data = &*(lparam as *const COPYDATASTRUCT);
            if data.dwData == term_panel::HOOK {
                let bytes = std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize);
                return app.terminal_hook(&String::from_utf8_lossy(bytes));
            }
            0
        }
        WM_SIZE => {
            app.layout();
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            // A bitmap of what has to be drawn and no more, with the window's coordinates on it.
            let dirty = paint.rcPaint;
            let (width, height) = (dirty.right - dirty.left, dirty.bottom - dirty.top);
            if width > 0 && height > 0 {
                let device = CreateCompatibleDC(target);
                let bitmap = CreateCompatibleBitmap(target, width, height);
                let previous = SelectObject(device, bitmap);
                SetViewportOrgEx(device, -dirty.left, -dirty.top, std::ptr::null_mut());
                app.paint(device, client, dirty);
                BitBlt(target, dirty.left, dirty.top, width, height, device, dirty.left, dirty.top, SRCCOPY);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            EndPaint(window, &paint);
            0
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: scale(app.dpi, ds::size::WINDOW_LEAST.0), y: scale(app.dpi, ds::size::WINDOW_LEAST.1) };
            0
        }
        WM_SETTINGCHANGE => {
            // Windows's light or dark, changed while this is open.
            if lparam != 0 && app.settings.theme == "system" {
                let name: Vec<u16> = (0..64).map(|at| *(lparam as *const u16).add(at)).take_while(|&unit| unit != 0).collect();
                if String::from_utf16_lossy(&name) == "ImmersiveColorSet" {
                    app.apply_theme();
                }
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_DPICHANGED => {
            app.dpi = (wparam & 0xffff) as u32;
            app.fonts = Fonts::new(app.dpi);
            set_button_fonts(app.fonts);
            for edit in app.fields.into_iter().chain([app.feedback, app.composer_cue, app.find_field, app.side_search, app.term.rename_field]) {
                SendMessageW(edit, WM_SETFONT, app.fonts.field as WPARAM, 1);
            }
            // (the command's own size follows the screen's: it is set again when it is next shown, and this one keeps what it has)
            let rect = &*(lparam as *const RECT);
            SetWindowPos(window, std::ptr::null_mut(), rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        WM_PREVIEW_MADE => {
            app.preview_made();
            0
        }
        WM_MOVE => {
            if let Some(shown) = &app.preview {
                shown.moved();
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_TIMER if wparam == TIMER_ENGINE => {
            app.watch_preview();
            app.waiting_tick();
            if let Some(reason) = app.engine.as_mut().and_then(Engine::ended) {
                app.engine = None;
                // (one that ended before it was ready failed to start; one that had been working may be tried again at once)
                let never_ready = !app.connected;
                if never_ready {
                    app.engine_failed_at = Some(std::time::Instant::now());
                }
                app.connected = false;
                // The next word written starts it again: the conversation is all still here.
                app.engine_waits = true;
                app.connection = "starts when you write".into();
                // (what still waits for it to answer waits no longer)
                app.generation.fetch_add(1, Ordering::SeqCst);
                if !app.running {
                    app.queued_back_to_composer();
                    app.set_activity(String::new());
                    app.update_buttons();
                }
                // (the engine's last word before it ended, as its own line under the app's sentence)
                let told = llm::gone_with_line(&reason);
                if !app.engine_stop_told {
                    app.engine_stop_told = true;
                    app.end_line();
                    // (never ready, what it said is all a person has to go on: a driver, a folder it could not open)
                    let (words, said) = if never_ready && llm::gone_words(&reason) == llm::SERVER_GONE { (tf("the engine did not start: {0}", &[&engine::in_plain_words(&reason)]), None) } else { lang::failure_parts(&told, true) };
                    app.append(&format!("{words}\n"), Style::Bad);
                    if let Some(said) = said {
                        app.append(&format!("{said}\n"), Style::Said);
                    }
                }
                keep_server_line(&app.folder, &app.leaving, &told);
                app.update_status();
            }
            if app.drafter_restart_waits {
                app.restart_for_drafter();
            }
            0
        }
        WM_FRAME => {
            app.frames.posted.store(false, Ordering::Relaxed);
            app.reveal_tick();
            0
        }
        WM_TIMER if wparam == TIMER_COPIED => {
            KillTimer(window, TIMER_COPIED);
            if let Some(key) = app.overlay_copied.take() {
                app.invalidate_overlay(key);
            }
            0
        }
        WM_TIMER if wparam == TIMER_PROXIES => {
            KillTimer(window, TIMER_PROXIES);
            app.sync_proxies();
            0
        }
        WM_PROXY_FOCUS => {
            InvalidateRect(app.transcript, std::ptr::null(), 0);
            0
        }
        WM_TIMER if wparam == TIMER_UPDATE => {
            app.look_for_update();
            0
        }
        WM_TIMER if wparam == TIMER_RELEASES => {
            // (the first look came a little after the start; from here the timer asks once an hour whether six hours have gone by)
            SetTimer(window, TIMER_RELEASES, RELEASE_LOOK_MS, None);
            app.look_for_release();
            0
        }
        WM_TIMER if wparam == TIMER_RECONNECT => {
            KillTimer(window, TIMER_RECONNECT);
            if !app.connected && app.connect_failed && !app.queued.is_empty() {
                app.connect_retrying = true;
                app.connect();
            }
            0
        }
        WM_DROPFILES => {
            let dropped = dropped_files(wparam as HDROP);
            DragFinish(wparam as HDROP);
            app.files_came(dropped);
            0
        }
        WM_TIMER if wparam == TIMER_REFLOW => {
            KillTimer(window, TIMER_REFLOW);
            app.reflow();
            0
        }
        WM_TIMER if wparam == TIMER_CHANGES => {
            KillTimer(window, TIMER_CHANGES);
            if app.panel != Panel::Diff {
                app.refresh_changes();
            }
            0
        }
        WM_LBUTTONDOWN if app.panel == Panel::Chat && app.has_messages => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
            let strip = app.thumb_strip();
            if x >= strip.left && x < strip.right && y >= strip.top && y < strip.bottom {
                if let Some(thumb) = app.thumb {
                    if y >= thumb.top && y < thumb.bottom {
                        app.thumb_grab = Some(y - thumb.top);
                        SetCapture(window);
                        InvalidateRect(window, &strip, 0);
                    } else {
                        app.follow_end = false;
                        SendMessageW(app.transcript, WM_VSCROLL, if y < thumb.top { SB_PAGEUP } else { SB_PAGEDOWN } as WPARAM, 0);
                        app.place_thumb();
                    }
                }
            }
            0
        }
        WM_MOUSEMOVE if app.thumb_grab.is_some() => {
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
            if let Some(grab) = app.thumb_grab {
                app.drag_thumb(y, grab);
            }
            0
        }
        WM_LBUTTONUP if app.thumb_grab.is_some() => {
            app.thumb_grab = None;
            ReleaseCapture();
            let strip = app.thumb_strip();
            InvalidateRect(window, &strip, 0);
            0
        }
        WM_CAPTURECHANGED if app.thumb_grab.is_some() => {
            app.thumb_grab = None;
            let strip = app.thumb_strip();
            InvalidateRect(window, &strip, 0);
            0
        }
        WM_MOUSEWHEEL if app.panel != Panel::Chat => {
            // (a notch is 120: about three lines of a page, as a browser goes)
            let notches = ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32;
            app.scroll_panel(-notches * scale(app.dpi, 120) / 120);
            0
        }
        WM_MOUSEMOVE if app.panel == Panel::Chat => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
            let within = |area: &RECT| x >= area.left && x < area.right && y >= area.top && y < area.bottom;
            let found = app.preview_hits.iter().map(|(area, _)| *area).chain(app.right_hits.iter().map(|(area, _)| *area)).chain(app.right_tab_hits.iter().map(|(area, _)| *area)).chain(app.queue_hits.iter().map(|(area, _, _)| *area)).chain(app.waiting_hits.iter().map(|(area, _)| *area)).chain(app.dashboard_hit.iter().map(|(area, _)| *area)).find(|area| within(area));
            app.hover_on(found);
            app.rail_hover_at(x, y);
            0
        }
        WM_MOUSELEAVE => {
            app.hover_on(None);
            app.rail_hover_at(-1, -1);
            0
        }
        WM_SETCURSOR if wparam as HWND == window && (app.hover_row.is_some() || app.rail_hover.is_some()) && (lparam & 0xFFFF) as u32 == HTCLIENT => {
            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
            1
        }
        WM_MOUSEWHEEL if app.panel == Panel::Chat && app.has_messages => {
            // Over the strip the wheel reaches this window: the control would have taken it.
            SendMessageW(app.transcript, message, wparam, lparam)
        }
        WM_LBUTTONUP if app.panel == Panel::Chat => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
            let within = |area: &RECT| x >= area.left && x < area.right && y >= area.top && y < area.bottom;
            if within(&app.title_area) {
                let twice = app.title_clicked.is_some_and(|at| at.elapsed().as_millis() <= u128::from(GetDoubleClickTime()));
                app.title_clicked = (!twice).then(std::time::Instant::now);
                if twice {
                    if let Some(path) = app.current_session.clone() {
                        app.rename_target = Some(path);
                        app.set_panel(Panel::Rename);
                    }
                }
                return 0;
            }
            if app.vision_offer_close.is_some_and(|close| within(&close)) && !app.setting_up {
                app.vision_offer = None;
                app.layout();
                return 0;
            }
            if let Some(view) = app.right_tab_at(x, y) {
                app.set_right_view(view);
            } else if let Some((at, act)) = app.queue_hits.iter().find(|(area, _, _)| within(area)).map(|(_, at, act)| (*at, *act)) {
                app.queue_act(at, act);
            } else if let Some(at) = app.rail_hits.iter().find(|(area, _)| within(area)).map(|(_, at)| *at) {
                app.jump_to_turn(at);
            } else if let Some(file) = app.preview_hits.iter().find(|(area, _)| within(area)).map(|(_, file)| file.display().to_string()) {
                app.open_preview(&file);
            } else if let Some(id) = app.waiting_hits.iter().find(|(area, _)| within(area)).map(|(_, id)| id.clone()) {
                app.stop_waiting(&id);
            } else if let Some(url) = app.dashboard_hit.as_ref().filter(|(area, _)| within(area)).map(|(_, url)| url.clone()) {
                open_in_browser(&url);
            } else if let Some((_, at)) = app.right_hits.iter().find(|(area, _)| within(area)).copied() {
                app.change_wanted = Some(at);
                app.set_panel(Panel::Diff);
            }
            0
        }
        WM_TIMER if wparam == TIMER_SEARCH => {
            app.search_now();
            0
        }
        WM_TIMER if wparam == TIMER_DOTS => {
            app.rate_tick();
            app.run_tick();
            app.spinner_tick();
            0
        }
        WM_CTLCOLORLISTBOX => {
            let side = lparam as HWND == app.side || lparam as HWND == app.tree_list || app.panel == Panel::Diff;
            SetBkColor(wparam as HDC, if side { theme::sidebar() } else { theme::surface() });
            app.brushes[if side { 1 } else { 2 }] as LRESULT
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
            let device = wparam as HDC;
            let raised = lparam as HWND == app.feedback;
            SetTextColor(device, if lparam as HWND == app.composer_cue { theme::faint() } else { theme::text() });
            if lparam as HWND == app.side_search {
                SetBkColor(device, theme::sidebar());
                return app.brushes[1] as LRESULT;
            }
            SetBkColor(device, if raised { theme::raised() } else { theme::surface() });
            app.brushes[if raised { 3 } else { 2 }] as LRESULT
        }
        WM_MEASUREITEM => {
            let item = &mut *(lparam as *mut MEASUREITEMSTRUCT);
            item.itemHeight = if item.CtlID as isize == ID_SUGGEST {
                scale(app.dpi, ds::size::MENU_ROW) as u32
            } else if item.CtlID as isize == ID_FILES {
                scale(app.dpi, ds::size::TEXT_ROW) as u32
            } else if item.CtlID as isize == ID_FILES_MENU {
                scale(app.dpi, files_panel::MENU_ROW) as u32
            } else if item.CtlID as isize == ID_SIDE {
                match app.side_rows.get(item.itemID as usize) {
                    Some(SideRow::Session { .. }) if app.side_hits.is_some() => scale(app.dpi, ds::size::SIDE_ROW_TALL) as u32,
                    Some(SideRow::Session { .. }) => scale(app.dpi, ds::size::SIDE_ROW) as u32,
                    _ => scale(app.dpi, ds::size::SIDE_GROUP) as u32,
                }
            } else if app.panel == Panel::Diff {
                scale(app.dpi, ds::size::SIDE_ROW) as u32
            } else {
                scale(app.dpi, ds::size::LIST_ROW) as u32
            };
            1
        }
        WM_DRAWITEM => {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            if item.hwndItem == app.composer_cue {
                FillRect(item.hDC, &item.rcItem, app.brushes[2]);
                draw_text(item.hDC, &get_text(app.composer_cue), item.rcItem, app.fonts.body, theme::faint(), DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
                return 1;
            }
            if item.itemID != u32::MAX {
                if item.hwndItem == app.side {
                    app.draw_side_row(item);
                } else if item.hwndItem == app.tree_list {
                    app.draw_file_row(item);
                } else if item.hwndItem == app.tree_menu {
                    app.draw_menu_row(item);
                } else if item.hwndItem == app.suggest {
                    app.draw_suggestion(item);
                } else if item.hwndItem == app.list {
                    app.draw_list_row(item);
                }
            }
            1
        }
        WM_COMMAND => {
            let (id, code) = (wparam & 0xffff, (wparam >> 16) & 0xffff);
            if lparam == 0 {
                app.on_menu(id);
                return 0;
            }
            if lparam as HWND == app.composer {
                if code == EN_CHANGE as usize {
                    app.update_suggestions();
                    app.composer_grow();
                    app.update_send_enabled();
                    // (a command is no message, and has no use for the GPU)
                    if app.engine_waits && !app.restoring_draft && !get_text(app.composer).trim().is_empty() && !get_text(app.composer).trim_start().starts_with('/') {
                        app.message_begun();
                    }
                }
                return 0;
            }
            if app.fields.contains(&(lparam as HWND)) {
                if code == EN_SETFOCUS as usize || code == EN_KILLFOCUS as usize {
                    app.layout();
                }
                return 0;
            }
            if lparam as HWND == app.find_field {
                if code == EN_CHANGE as usize {
                    app.find_update();
                }
                return 0;
            }
            if lparam as HWND == app.term.rename_field {
                if code == EN_KILLFOCUS as usize {
                    app.terminal_rename_end(true);
                }
                return 0;
            }
            if lparam as HWND == app.side_search {
                if code == EN_CHANGE as usize {
                    // Each file of each session is read: after the typing pauses.
                    SetTimer(window, TIMER_SEARCH, 200, None);
                }
                return 0;
            }
            let id = id as isize;
            match id {
                ID_SEND => {
                    if app.running || app.waiting_to_connect() {
                        app.stop_work();
                    } else {
                        app.send_message();
                    }
                }
                ID_ATTACH => app.choose_pictures(),
                _ if (ID_ATTACHED..ID_ATTACHED + attach::MOST_IMAGES as isize).contains(&id) => {
                    let at = (id - ID_ATTACHED) as usize;
                    if code == THUMB_REMOVE {
                        app.remove_attachment(at);
                    } else if let Some(path) = app.attachments.get(at).map(|attached| attached.path.clone()) {
                        app.open_picture(&path);
                    }
                }
                ID_PANEL_TOGGLE => app.toggle_right_panel(),
                ID_PREVIEW_RELOAD => {
                    if let Some(shown) = &app.preview {
                        shown.reload();
                    }
                }
                ID_PREVIEW_BROWSER => {
                    if let Some(file) = app.preview_file.clone() {
                        open_in_browser(&file.display().to_string());
                    }
                }
                ID_PREVIEW_CLOSE => app.close_preview(),
                ID_FIND_PREVIOUS => app.find_go(false),
                ID_FIND_NEXT => app.find_go(true),
                ID_FIND_CLOSE => app.close_find(),
                ID_APPROVE => app.answer_approval(Decision::Approve),
                ID_ALWAYS => app.answer_approval(Decision::Always),
                ID_REJECT => app.answer_approval(Decision::Reject),
                ID_NEW_SESSION => app.new_session(),
                ID_TERM_NEW | ID_TERM_START | ID_TERM_AGAIN => {
                    app.terminal_new(None);
                }
                ID_TERM_SHELLS => app.terminal_shells_menu(app.term.shells_button),
                ID_TERM_START_SHELLS => app.terminal_shells_menu(app.term.start_shells),
                _ if (ID_TERM_TAB..ID_TERM_TAB + crate::term::MOST_TERMINALS as isize).contains(&id) => app.terminal_tab_pressed((id - ID_TERM_TAB) as usize),
                _ if (ID_TERM_CLOSE..ID_TERM_CLOSE + crate::term::MOST_TERMINALS as isize).contains(&id) => {
                    if let Some(terminal) = app.term.ids().get((id - ID_TERM_CLOSE) as usize).copied() {
                        app.terminal_close(terminal);
                    }
                }
                ID_DRAFTER => app.drafter_clicked(),
                ID_SPEED_FIRST => {
                    app.settings.speed_first = !app.settings.speed_first;
                    app.save();
                    app.update_speed_chip();
                    app.tip(app.speed_chip, t("Takes effect when the engine starts again"));
                    app.layout();
                }
                ID_VISION => app.vision_clicked(),
                ID_VISION_DELETE => {
                    if let Some(crate::vision::State::Off(path)) = app.vision_for_installed().map(|installed| crate::vision::state(&installed)) {
                        match std::fs::remove_file(&path) {
                            Ok(()) => app.layout(),
                            Err(error) => app.problem_in_panel(&format!("{}: {error}", path.display())),
                        }
                    }
                }
                ID_UPDATE_OFFER => app.update_offer_pressed(),
                ID_NOTICE_LATER => app.notice_sent_away(),
                ID_RELEASE_OFFER => {
                    if let Some(release) = app.release_offer.clone() {
                        app.open_release(&release.version, &release.page);
                        app.layout();
                    }
                }
                ID_TOGGLE_UPDATES => app.updates_switched(),
                ID_TOGGLE_REMEMBER => app.crypt_remember_switched(),
                ID_VISION_OFFER => match app.vision_offer {
                    _ if app.setting_up && app.vision_fetch => app.setup_cancel.store(true, Ordering::Relaxed),
                    Some(VisionOffer::Download(_)) => app.start_vision_setup(),
                    Some(VisionOffer::TurnOn) => app.vision_switch(true),
                    None => {}
                },
                ID_MODEL_CHIP | ID_MODE_CHIP | ID_EFFORT_CHIP | ID_KIND_CHIP => app.on_chip(id),
                ID_SETTINGS => app.set_panel(Panel::General),
                ID_MODAL_CLOSE => app.set_panel(Panel::Chat),
                _ if (ID_NAV..ID_NAV + NAV_POOL as isize).contains(&id) => {
                    let at = (id - ID_NAV) as usize;
                    if app.panel == Panel::Help {
                        app.help_section = at;
                        app.show_help_section();
                        app.layout();
                        SetFocus(app.nav_buttons[at]);
                    } else if let Some((panel, _, _)) = SETTINGS_PLACES.get(at) {
                        if *panel != app.panel {
                            app.set_panel(*panel);
                        }
                    }
                }
                _ if (ID_TEXT_SIZE..ID_TEXT_SIZE + TEXT_SIZES.len() as isize).contains(&id) => {
                    app.settings.text_size = TEXT_SIZES[(id - ID_TEXT_SIZE) as usize].0;
                    app.save();
                    app.apply_text_size();
                    app.update_toggles();
                }
                _ if (ID_MEMORY_REMOVE..ID_MEMORY_REMOVE + app.memory_remove.len() as isize).contains(&id) => {
                    if let Some((scope, note)) = app.memory_notes.get((id - ID_MEMORY_REMOVE) as usize).cloned() {
                        match dogmind_agent::memory::forget_one(&app.folder.join("memory"), &app.workspace, scope, &note, &app.policy) {
                            Ok(_) => app.set_panel(Panel::Memory),
                            Err(error) => app.problem_in_panel(&error),
                        }
                    }
                }
                _ if (ID_SKILL_TOGGLE..ID_SKILL_TOGGLE + app.skill_toggles.len() as isize).contains(&id) => {
                    if let Some(name) = app.skills.get((id - ID_SKILL_TOGGLE) as usize).map(|skill| skill.name.clone()) {
                        let on = app.skill_enabled(&name);
                        app.settings.set_skill(&name, !on);
                        app.save();
                        app.send_skills_off();
                        app.update_toggles();
                        // (the words beside the switch say which it is now)
                        app.layout();
                    }
                }
                _ if (ID_THEME_SEGMENT..ID_THEME_SEGMENT + 3).contains(&id) => {
                    app.on_menu(M_THEME + (id - ID_THEME_SEGMENT) as usize);
                    app.update_toggles();
                }
                ID_MODE_SELECT => {
                    let mode = app.current_mode();
                    let chosen = MODES.iter().position(|(known, _)| *known == mode).unwrap_or(0);
                    let names = MODES.map(|(_, name)| name);
                    app.choices(app.mode_select, M_MODE, &names, chosen);
                    app.update_toggles();
                }
                ID_KEY_REPLACE => {
                    app.key_editing = true;
                    set_text(app.fields[KEY_FIELD], "");
                    app.layout();
                    SetFocus(app.fields[KEY_FIELD]);
                }
                ID_KEY_REMOVE => app.forget_key(),
                ID_ADVANCED => {
                    app.advanced_open = !app.advanced_open;
                    set_text(app.advanced_button, if app.advanced_open { t("chevron-down|Other ways to give the key") } else { t("chevron-right|Other ways to give the key") });
                    app.layout();
                }
                ID_VARIABLE_TOGGLE => {
                    app.variable_on = !app.variable_on;
                    set_button_style(app.variable_toggle, if app.variable_on { ButtonKind::ToggleOn } else { ButtonKind::Toggle }, theme::background());
                    app.layout();
                    if app.variable_on {
                        SetFocus(app.fields[VARIABLE_FIELD]);
                    }
                }
                ID_MCP_IMPORT => app.import_mcp_servers(),
                ID_READ_FOLDER_ADD => {
                    // (a debug build told to, for tests: the picker is a dialog on the person's screen, which nothing may put there)
                    let forced = if cfg!(debug_assertions) { std::env::var_os("DOGMIND_TEST_READ_FOLDER").map(PathBuf::from) } else { None };
                    if let Some(folder) = forced.or_else(|| app.browse_folder()) {
                        app.add_read_folder(folder);
                    }
                }
                ID_READ_FOLDER_STOP => {
                    if let Some(work) = &mut app.read_folder_work {
                        work.cancel.store(true, Ordering::Relaxed);
                        work.can_stop = false;
                        work.stopping = true;
                    }
                    app.layout();
                }
                _ if (ID_READ_FOLDER_REMOVE..ID_READ_FOLDER_REMOVE + app.read_folder_remove.len() as isize).contains(&id) => {
                    if let Some(folder) = app.read_folders.get((id - ID_READ_FOLDER_REMOVE) as usize).map(|folder| folder.path.clone()) {
                        app.remove_read_folder(folder);
                    }
                }
                _ if (ID_MCP_TRUST..ID_MCP_TRUST + app.mcp_trust.len() as isize).contains(&id) => {
                    if let Some((server, on)) = app.mcp.trust().get((id - ID_MCP_TRUST) as usize).cloned() {
                        match app.mcp.set_trusted(&server, !on) {
                            Ok(()) => app.update_toggles(),
                            Err(error) => app.problem_in_panel(&error),
                        }
                    }
                }
                ID_SETUP_AGAIN => {
                    app.onboarding = true;
                    app.set_panel(Panel::Setup);
                }
                ID_REMOVE_ALL => app.set_panel(Panel::ConfirmRemove),
                ID_MODEL_LICENCE => {
                    if let Some(licence) = app.list_selection().and_then(|at| app.catalog.get(at)).and_then(|entry| entry.licence.clone()) {
                        open_in_browser(&licence.terms);
                    }
                }
                ID_HELP => app.set_panel(Panel::Help),
                ID_GOAL_CLEAR => {
                    let _ = app.agent.commands.send(Command::SetGoal(None));
                }
                _ if (ID_PROXY_POOL..ID_PROXY_POOL + PROXY_POOL as isize).contains(&id) => {
                    if let Some(key) = app.proxy_keys[(id - ID_PROXY_POOL) as usize] {
                        app.overlay_act(key);
                    }
                }
                ID_TOGGLE_THINKING => app.on_menu(M_THINKING),
                ID_TOGGLE_SAFE => app.on_menu(M_SAFE),
                ID_TOGGLE_LANGUAGE => {
                    let chosen = ANSWER_LANGUAGES.iter().position(|(known, _)| *known == app.settings.language).unwrap_or(0);
                    let names = ANSWER_LANGUAGES.map(|(known, name)| if known == "auto" { auto_answer_words() } else { name });
                    app.choices(app.toggle_language, M_ANSWERS, &names, chosen);
                }
                ID_TOGGLE_DISPLAY => {
                    let chosen = DISPLAY_LANGUAGES.iter().position(|(known, _)| *known == app.settings.display_language).unwrap_or(0);
                    let names = DISPLAY_LANGUAGES.map(|(_, name)| name);
                    app.choices(app.toggle_display, M_DISPLAY, &names, chosen);
                }
                ID_FOLDER_CHIP => app.folder_menu(),
                ID_PROVIDER_UP => app.move_provider(-1),
                ID_PROVIDER_DOWN => app.move_provider(1),
                ID_TOGGLE_COMPACT => app.on_menu(M_COMPACT_CHOICE + usize::from(app.settings.compact_tools)),
                ID_TOGGLE_WEB => app.on_menu(M_WEB_CHOICE + usize::from(app.settings.web_tools)),
                ID_FILES => {
                    if code == LBN_SELCHANGE as usize {
                        app.files_selection_changed();
                    }
                }
                ID_SUGGEST => {
                    if code == LBN_SELCHANGE as usize {
                        let at = SendMessageW(app.suggest, LB_GETCURSEL, 0, 0);
                        if at >= 0 && GetFocus() == app.suggest {
                            app.accept_suggestion(at as usize);
                        }
                    }
                }
                ID_LIST if code == LBN_SELCHANGE as usize && app.panel == Panel::Models => {
                    let selected = app.list_selection();
                    app.offer_context(selected);
                    app.update_licence_link();
                    app.update_vision_chip();
                    app.update_drafter_chip();
                    app.layout();
                }
                ID_LIST if code == LBN_DBLCLK as usize && app.panel == Panel::Sessions => app.on_button(0),
                ID_DIFF_PREVIOUS => app.diff_jump(false),
                ID_DIFF_NEXT => app.diff_jump(true),
                ID_DIFF_CLOSE => app.set_panel(Panel::Chat),
                ID_LIST if code == LBN_SELCHANGE as usize && app.panel == Panel::Diff => {
                    if let Some(at) = app.list_selection() {
                        app.show_change(at);
                    }
                }
                ID_LIST if code == LBN_SELCHANGE as usize && app.panel == Panel::Providers => {
                    let selected = app.list_selection();
                    app.select_provider(selected);
                }
                ID_LIST if code == LBN_DBLCLK as usize && matches!(app.panel, Panel::Start | Panel::Setup) => app.on_button(0),
                _ if (ID_BUTTONS..ID_BUTTONS + 5).contains(&id) => app.on_button((id - ID_BUTTONS) as usize),
                _ => {}
            }
            0
        }
        // Closing the window leaves the app, its engine loaded, in the tray. It ends from there, or with /quit.
        WM_CLOSE => {
            app.remember_place();
            app.save();
            ShowWindow(window, SW_HIDE);
            if !app.waiting.is_empty() || app.parked.iter().any(|parked| !parked.waiting.is_empty()) {
                // (what waits in the background goes on while the window is hidden, and says here when it has something for the person)
                app.tray_note(t("Dogmind is still here"), t("It goes on with what waits in the background, and says here when it has something for you. Quit it from the tray icon's menu to end all of it."));
            } else if !app.settings.tray_told {
                app.settings.tray_told = true;
                app.save();
                app.tray_note(t("Dogmind is still here"), t("It keeps running in the tray, with the engine loaded. Quit it from the tray icon's menu."));
            }
            0
        }
        WM_TRAY => {
            match lparam as u32 {
                WM_LBUTTONUP | TRAY_BALLOON_CLICK => app.tray_open(),
                WM_RBUTTONUP => app.tray_menu(),
                _ => {}
            }
            0
        }
        // Windows is ending: as if it had been quit.
        WM_ENDSESSION if wparam != 0 => {
            DestroyWindow(window);
            0
        }
        _ if message == app.please_quit => {
            app.asked_to_quit();
            0
        }
        _ if message == app.taskbar_created => {
            app.tray_add();
            0
        }
        WM_DESTROY => {
            let data = app.tray_data();
            Shell_NotifyIconW(NIM_DELETE, &data);
            app.remember_place();
            app.save();
            app.agent.cancel.store(true, Ordering::Relaxed);
            // (a setup on its way is let go of, and its partial file is taken out at the next start)
            if let ReleaseUpdate::Fetching { stop, .. } = &app.release_update {
                stop.store(true, Ordering::Relaxed);
            }
            dogmind_agent::workspace_grants::app_is_closing();
            // (every shell, and all that it started, ends here: its job is closed)
            app.terminals_end();
            let _ = app.agent.commands.send(Command::Quit);
            for parked in &app.parked {
                parked.agent.cancel.store(true, Ordering::Relaxed);
                let _ = parked.agent.commands.send(Command::Quit);
            }
            app.engine = None;
            // (before the controls go, after this: the document holds the text services, and they hold the control's host, which goes with
            // its window whether anything holds it or not)
            release_document(std::mem::replace(&mut app.transcript_document, std::ptr::null_mut()));
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Keys that belong to the window whichever control has the focus. `true`: taken.
unsafe fn take_key(app: &mut App, message: &MSG) -> bool {
    // With the keys in a terminal, every key is the shell's but a few that stay the window's (see `terminal_key`).
    if matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN) && message.hwnd == app.term.window && !app.modal_open {
        match app.terminal_key(message) {
            term_panel::TermKey::Taken => return true,
            term_panel::TermKey::Dispatch => return false,
            term_panel::TermKey::Pass => {}
        }
    }
    // A tab being renamed: Enter keeps the name and Esc leaves it.
    if message.message == WM_KEYDOWN && message.hwnd == app.term.rename_field && matches!(message.wParam as u16, VK_RETURN | VK_ESCAPE) {
        app.terminal_rename_end(message.wParam as u16 == VK_RETURN);
        return true;
    }
    if message.message == WM_SYSKEYDOWN && app.panel == Panel::Providers && matches!(message.wParam as u16, VK_UP | VK_DOWN) {
        app.move_provider(if message.wParam as u16 == VK_UP { -1 } else { 1 });
        return true;
    }
    if message.message == WM_LBUTTONDOWN {
        keys_in_use(false);
    }
    if message.message == WM_MOUSEWHEEL && app.panel == Panel::Diff {
        let (x, y) = ((message.lParam & 0xFFFF) as i16 as i32, ((message.lParam >> 16) & 0xFFFF) as i16 as i32);
        if WindowFromPoint(POINT { x, y }) == app.diff_view {
            app.diff_wheel(message.wParam);
            return true;
        }
    }
    if message.message != WM_KEYDOWN {
        return false;
    }
    let key = message.wParam as u16;
    if matches!(key, VK_TAB | VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT) {
        keys_in_use(true);
    }
    // Esc on the little menu of a file: it is put away, and nothing else (below, Esc stops the work or leaves a page).
    if key == VK_ESCAPE && message.hwnd == app.tree_menu && app.panel_menu.whose != files_panel::MenuFor::Nothing {
        app.files_menu_close(true);
        return true;
    }
    let shift = GetKeyState(VK_SHIFT as i32) < 0;
    let control = GetKeyState(VK_CONTROL as i32) < 0;
    // In a dialog, only what the dialog does: what is behind it is out of reach.
    if app.modal_open {
        let chosen = app.nav_rows();
        let count = chosen.0.len();
        match key {
            // (what is asked before anything is opened, and a change of every conversation under way, have no way out but their own)
            VK_ESCAPE if matches!(app.panel, Panel::Unlock | Panel::CryptoWork) => {}
            VK_ESCAPE => app.set_panel(Panel::Chat),
            VK_TAB if !control => {
                let mut stops: Vec<HWND> = chosen.0.get(chosen.1).map(|_| app.nav_buttons[chosen.1]).into_iter().collect();
                stops.extend(tab_stops(app.pane, i32::MIN));
                if IsWindowVisible(app.modal_close) != 0 {
                    stops.push(app.modal_close);
                }
                if let Some(next) = next_stop(&stops, GetFocus(), shift) {
                    SetFocus(next);
                    app.scroll_to(next);
                    if app.fields.contains(&next) {
                        SendMessageW(next, EM_SETSEL, 0, -1);
                    }
                }
            }
            VK_UP | VK_DOWN if count > 0 && app.nav_buttons.contains(&message.hwnd) => {
                let next = if key == VK_UP { (chosen.1 + count - 1) % count } else { (chosen.1 + 1) % count };
                SendMessageW(app.window, WM_COMMAND, (ID_NAV + next as isize) as usize, app.nav_buttons[next] as LPARAM);
                if let Some(button) = app.nav_buttons.get(app.nav_rows().1) {
                    SetFocus(*button);
                }
            }
            VK_RETURN if !shift && app.panel == Panel::Providers && app.fields.contains(&message.hwnd) => app.on_button(1),
            VK_RETURN if !shift && app.panel == Panel::Rename && app.fields.contains(&message.hwnd) => app.on_button(0),
            VK_RETURN if !shift && matches!(app.panel, Panel::Unlock | Panel::CryptoOn | Panel::CryptoPassphrase) && app.fields.contains(&message.hwnd) => app.on_button(0),
            VK_RETURN if !shift && matches!(app.panel, Panel::Sessions | Panel::Rewind) && message.hwnd == app.list => app.on_button(0),
            _ => return false,
        }
        return true;
    }
    if message.hwnd == app.composer {
        app.composer_typed_at = Some(std::time::Instant::now());
    }
    if message.hwnd == app.composer && ((control && key == 0x56) || (shift && key == VK_INSERT)) && app.paste_pictures() {
        return true;
    }
    if message.hwnd == app.composer && key == VK_PROCESSKEY {
        // A key an IME took: composing is about to begin, whichever way this IME says so.
        show(app.composer_cue, false);
    }
    if message.hwnd == app.find_field {
        match key {
            VK_RETURN => {
                app.find_go(!shift);
                return true;
            }
            VK_ESCAPE => {
                app.close_find();
                return true;
            }
            _ => {}
        }
    }
    if message.hwnd == app.side_search {
        match key {
            VK_RETURN => {
                let first = app.side_rows.iter().position(|row| matches!(row, SideRow::Session { .. }));
                if let (Some(first), true) = (first, app.side_hits.is_some()) {
                    app.on_side_click(first, 0);
                }
                return true;
            }
            VK_ESCAPE => {
                set_text(app.side_search, "");
                app.search_now();
                SetFocus(app.composer);
                return true;
            }
            _ => {}
        }
    }
    if key == VK_F2 {
        // (on a tab of the terminals: that tab's name; elsewhere, the session's)
        if let Some(index) = app.term.tab_of(message.hwnd) {
            app.terminal_rename_tab_at(index);
            return true;
        }
    }
    if key == VK_F2 && app.panel == Panel::Chat {
        // The session on screen, by name; /delete for the rest of what the row's menu has.
        if let Some(path) = app.current_session.clone() {
            app.rename_target = Some(path);
            app.set_panel(Panel::Rename);
        }
        return true;
    }
    if app.panel == Panel::Diff {
        let page = (child_area(app.diff_view, app.window).3 - scale(app.dpi, ds::size::DIFF_ROW)).max(0);
        match key {
            // N and P, as the buttons' words say; F8 as other programs have it.
            0x4E | 0x4A if !control => app.diff_jump(true),
            0x50 | 0x4B if !control => app.diff_jump(false),
            VK_F8 => app.diff_jump(!shift),
            VK_NEXT => app.diff_to(app.diff_scroll + page),
            VK_PRIOR => app.diff_to(app.diff_scroll - page),
            VK_HOME => app.diff_to(0),
            VK_END => app.diff_to(i32::MAX),
            VK_LEFT => app.diff_sideways(-1),
            VK_RIGHT => app.diff_sideways(1),
            0x43 if control => app.copy_from_diff(),
            _ if key == VK_ESCAPE => {
                app.set_panel(Panel::Chat);
            }
            _ => return false,
        }
        return true;
    }
    if control && key == VK_OEM_2 {
        // Ctrl+/: the keys.
        app.set_panel(Panel::Help);
        app.help_section = 1;
        app.show_help_section();
        app.layout();
        return true;
    }
    if control && !shift && (0x31..=0x39).contains(&key) {
        // Ctrl+1 to 9: that session, as the sidebar has them.
        let wanted = app.side_rows.iter().filter_map(|row| if let SideRow::Session { path, .. } = row { Some(path.clone()) } else { None }).nth(usize::from(key - 0x31));
        if let Some(path) = wanted {
            app.open_session(path);
        }
        return true;
    }
    if control && key == VK_TAB {
        // Ctrl+Tab: the next session of the sidebar, and with Shift the one before.
        let sessions: Vec<PathBuf> = app.side_rows.iter().filter_map(|row| if let SideRow::Session { path, .. } = row { Some(path.clone()) } else { None }).collect();
        if !sessions.is_empty() {
            let at = sessions.iter().position(|path| Some(path) == app.current_session.as_ref());
            let next = match at {
                Some(at) if shift => (at + sessions.len() - 1) % sessions.len(),
                Some(at) => (at + 1) % sessions.len(),
                None => 0,
            };
            app.open_session(sessions[next].clone());
        }
        return true;
    }
    if control && key == 0x4B {
        // Ctrl+K: everything there is to do, from anywhere, in the list `/` opens.
        if app.panel != Panel::Chat {
            app.set_panel(Panel::Chat);
        }
        SetFocus(app.composer);
        if GetWindowTextLengthW(app.composer) == 0 {
            set_text(app.composer, "/");
        }
        let end = GetWindowTextLengthW(app.composer) as usize;
        SendMessageW(app.composer, EM_SETSEL, end, end as LPARAM);
        return true;
    }
    if control && key == 0x42 {
        // Ctrl+B
        app.toggle_right_panel();
        return true;
    }
    if term_panel::opens_a_terminal(key, crate::term::keymap::Mods { shift, ctrl: control, alt: GetKeyState(VK_MENU as i32) < 0 }) {
        // Ctrl+`: a terminal, in the panel on the right
        app.terminal_command_new();
        return true;
    }
    if control && key == 0x46 {
        // Ctrl+F, and with Shift all sessions
        if shift {
            SetFocus(app.side_search);
            SendMessageW(app.side_search, EM_SETSEL, 0, -1);
        } else {
            app.open_find();
        }
        return true;
    }
    if !app.suggestions.is_empty() && message.hwnd == app.composer {
        let selected = SendMessageW(app.suggest, LB_GETCURSEL, 0, 0).max(0) as usize;
        match key {
            VK_UP | VK_DOWN => {
                let count = app.suggestions.len();
                let next = if key == VK_UP { (selected + count - 1) % count } else { (selected + 1) % count };
                SendMessageW(app.suggest, LB_SETCURSEL, next, 0);
                return true;
            }
            VK_TAB => {
                app.accept_suggestion(selected);
                return true;
            }
            VK_RETURN if !shift => {
                let command = app.suggestions.get(selected).is_some_and(|row| row.title.starts_with('/'));
                app.accept_suggestion(selected);
                if command {
                    app.send_message();
                }
                return true;
            }
            VK_ESCAPE => {
                app.suggestions.clear();
                SendMessageW(app.suggest, LB_RESETCONTENT, 0, 0);
                app.layout();
                return true;
            }
            _ => {}
        }
    }
    if control {
        match key {
            0x4E => {
                // Ctrl+N
                app.new_session();
                return true;
            }
            0xBC => {
                // Ctrl+,
                app.set_panel(Panel::General);
                return true;
            }
            _ => {}
        }
    }
    if control && shift && key == 0x43 {
        // Ctrl+Shift+C
        app.on_menu(M_COPY);
        return true;
    }
    if key == VK_TAB && !control {
        if let Some(next) = next_stop(&tab_stops(app.window, app.sidebar_width), GetFocus(), shift) {
            SetFocus(next);
            app.scroll_to(next);
            // What is in a field is taken whole, to be typed over; not the composer's draft, not what the chat shows.
            if next != app.composer && next != app.transcript {
                SendMessageW(next, EM_SETSEL, 0, -1);
            }
        }
        return true;
    }
    if key == VK_ESCAPE {
        let back_in_setup = app.panel == Panel::SetupDetails || (app.panel == Panel::Models && app.onboarding);
        match escape_does(app.panel != Panel::Chat, back_in_setup || app.settings.workspace.is_some(), app.pending.is_some(), app.running || app.waiting_to_connect()) {
            EscapeDoes::ClosePanel if back_in_setup => app.set_panel(Panel::Setup),
            EscapeDoes::ClosePanel => {
                app.onboarding = false;
                app.set_panel(Panel::Chat);
            }
            EscapeDoes::Reject => app.answer_approval(Decision::Reject),
            EscapeDoes::Stop => app.stop_work(),
            EscapeDoes::Nothing => {}
        }
        return true;
    }
    if key == VK_RETURN && message.hwnd == app.composer && !shift {
        app.send_typed(!control);
        return true;
    }
    if matches!(key, VK_PRIOR | VK_NEXT) && message.hwnd == app.composer && app.panel == Panel::Chat && app.has_messages {
        // Reading back without leaving what is being written.
        SendMessageW(app.transcript, WM_VSCROLL, if key == VK_PRIOR { SB_PAGEUP } else { SB_PAGEDOWN } as WPARAM, 0);
        return true;
    }
    if key == VK_RETURN && !shift && app.panel != Panel::Chat {
        let focus = message.hwnd;
        let button = match app.panel {
            Panel::SetupDetails if app.fields.contains(&focus) => Some(0),
            Panel::Setup | Panel::Start if focus == app.list => Some(0),
            Panel::Welcome => Some(0),
            _ => None,
        };
        if let Some(button) = button {
            app.on_button(button);
            return true;
        }
    }
    if app.pending.is_some() && ![app.feedback, app.composer, app.find_field, app.side_search].contains(&GetFocus()) {
        match key {
            0x59 => app.answer_approval(Decision::Approve), // Y
            0x41 => {
                if app.pending.as_ref().is_some_and(|pending| pending.always_allowed) {
                    app.answer_approval(Decision::Always);
                }
            }
            0x4E => app.answer_approval(Decision::Reject), // N
            _ => return false,
        }
        return true;
    }
    false
}

/// The title bar and the window's border, in the theme.
unsafe fn title_bar(window: HWND) {
    let dark = i32::from(!theme::is_light());
    DwmSetWindowAttribute(window, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, std::ptr::addr_of!(dark).cast(), 4);
    let caption = theme::sidebar();
    DwmSetWindowAttribute(window, DWMWA_CAPTION_COLOR as u32, std::ptr::addr_of!(caption).cast(), 4);
    let border = theme::sidebar();
    DwmSetWindowAttribute(window, DWMWA_BORDER_COLOR as u32, std::ptr::addr_of!(border).cast(), 4);
}

/// What a call is, as a person reads it: what it is for, then what it will do, in full, with no JSON around it. Nothing of a
/// command is left out, as it is being approved; a file's content may be long, and its first part tells what it is.
/// What a call says of itself, and what it brought back, is drawn as it is written: a command with a right-to-left override in it showed its end backwards, as `echo safe live world`
/// for what ran as `echo safe <U+202E> dlrow evil`. What acts on text and shows nothing is shown as its mark before any of it is drawn.
/// So are keys and tokens a tool brought back or a call names: hidden but their beginning (mask::hide_keys), where they are shown.
fn hidden_shown(event: Event) -> Event {
    let event = keys_hidden(event);
    match event {
        Event::ToolStarted { id, tool, summary } if markdown::has_marks(&summary) => Event::ToolStarted { id, tool, summary: markdown::show_hidden(&summary).into_owned() },
        Event::ToolProgress { id, tool, summary } if markdown::has_marks(&summary) => Event::ToolProgress { id, tool, summary: markdown::show_hidden(&summary).into_owned() },
        Event::ToolEnded { id, ok, declined, elapsed, output, images } if markdown::has_marks(&output) => Event::ToolEnded { id, ok, declined, elapsed, output: markdown::show_hidden(&output).into_owned(), images },
        other => other,
    }
}

fn keys_hidden(event: Event) -> Event {
    let hide = |text: String| match crate::mask::hide_keys(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(hidden) => hidden,
    };
    match event {
        Event::ToolStarted { id, tool, summary } => Event::ToolStarted { id, tool, summary: hide(summary) },
        Event::ToolProgress { id, tool, summary } => Event::ToolProgress { id, tool, summary: hide(summary) },
        Event::ToolEnded { id, ok, declined, elapsed, output, images } => Event::ToolEnded { id, ok, declined, elapsed, output: hide(output), images },
        other => other,
    }
}

/// How tall a line of the command on an approval card is, in twips: exactly 1.45 times the size of the type, where a block of code in the conversation has 1.6 times a line of text.
fn command_line_twips() -> i32 {
    ds::points::CODE * 29 * text_scale() / 100
}

/// What a card says of a call in two parts: the model's purpose (sentences, which belong with the reason), and what is run or written (which is code, or a path and the text of a file).
fn approval_parts(tool: &str, summary: &str, details: &str) -> (String, String) {
    let purpose = serde_json::from_str::<serde_json::Value>(details).ok().and_then(|arguments| arguments.get("purpose").and_then(serde_json::Value::as_str).map(|purpose| purpose.trim().to_string())).unwrap_or_default();
    let body = approval_body(tool, summary, details);
    let code = match body.strip_prefix(&format!("{purpose}\n\n")) {
        Some(rest) if !purpose.is_empty() => rest.to_string(),
        _ => body,
    };
    (purpose, code)
}

fn approval_body(tool: &str, summary: &str, details: &str) -> String {
    const SHOWN: usize = 4000;
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(details) else { return format!("{summary}\n{details}") };
    let text = |name: &str| arguments.get(name).and_then(serde_json::Value::as_str).unwrap_or("");
    let cut = |content: &str| -> String {
        let count = content.chars().count();
        if count <= SHOWN {
            content.to_string()
        } else {
            tf("{0}\n... and {1} more characters", &[&content.chars().take(SHOWN).collect::<String>(), &(count - SHOWN)])
        }
    };
    let purpose = match text("purpose") {
        "" => String::new(),
        purpose => format!("{purpose}\n\n"),
    };
    match tool {
        "execute_command" => {
            let place = match text("cwd") {
                "" | "." => String::new(),
                cwd => tf("\n\nin {0}", &[&cwd]),
            };
            // (what runs is shown first: where the sandbox changes a line, the line the model wrote comes after it)
            let written = match text("written_as") {
                "" => String::new(),
                written => format!("\n\n{}", tf("As written: {0}", &[&written])),
            };
            format!("{purpose}{}{written}{place}", text("command"))
        }
        "create_file" => format!("{purpose}{}\n\n{}", text("path"), cut(text("content"))),
        "apply_patch" => {
            let edits: Vec<String> = arguments.get("edits").and_then(serde_json::Value::as_array).into_iter().flatten().map(|edit| {
                let of = |name: &str| edit.get(name).and_then(serde_json::Value::as_str).unwrap_or("");
                format!("{}\n--- from\n{}\n+++ to\n{}", of("path"), of("old_string"), of("new_string"))
            }).collect();
            if edits.is_empty() {
                format!("{summary}\n{}", cut(details))
            } else {
                format!("{purpose}{}", cut(&edits.join("\n\n")))
            }
        }
        "web_read" => format!("{purpose}{}", text("url")),
        "web_search" => {
            let queries: Vec<&str> = arguments.get("queries").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(serde_json::Value::as_str).collect();
            format!("{purpose}{}", queries.join("\n"))
        }
        // Whatever else: its fields as lines, not JSON.
        _ => match arguments.as_object() {
            Some(fields) => {
                let lines: Vec<String> = fields.iter().filter(|(name, _)| name.as_str() != "purpose").map(|(name, value)| match value {
                    serde_json::Value::String(words) => format!("{name}: {words}"),
                    other => format!("{name}: {other}"),
                }).collect();
                format!("{purpose}{}", cut(&lines.join("\n")))
            }
            None => format!("{purpose}{summary}\n\n{}", cut(details)),
        },
    }
}

fn same_rect(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// Whether a place kept in the settings is a window at all.
fn usable_place(place: &[i32; 4], least_width: i32, least_height: i32) -> bool {
    place[2] - place[0] >= least_width && place[3] - place[1] >= least_height
}

/// Things side by side, as far as the room goes and then on the next row: for each its row, and how far along it.
fn wrap_row(widths: &[i32], gap: i32, room: i32) -> Vec<(i32, i32)> {
    let (mut row, mut x) = (0, 0);
    widths.iter().map(|width| {
        if x > 0 && x + width > room {
            row += 1;
            x = 0;
        }
        let place = (row, x);
        x += width + gap;
        place
    }).collect()
}

/// So that a layout does not repaint a label that has not changed.
unsafe fn set_text_if_changed(window: HWND, text: &str) {
    let mut buffer = [0u16; 512];
    let length = GetWindowTextW(window, buffer.as_mut_ptr(), 512).max(0) as usize;
    if String::from_utf16_lossy(&buffer[..length]) != text {
        set_text(window, text);
    }
}

/// A tool as a person calls it. One from an MCP server keeps its own name.
pub(crate) fn tool_name(tool: &str) -> &str {
    match tool {
        "read_file" => "Read",
        "list_files" => "List",
        "find_files" => "Find",
        "search_files" => "Search",
        "show_diff" => "Diff",
        "apply_patch" => "Edit",
        "create_file" => "Write",
        "execute_command" => "Run",
        "command_status" => "Check a command",
        "command_stop" => "Stop a command",
        "wake_at" => "Wake later",
        "todo" => "Tasks",
        "skill" => "Skill",
        "remember" => "Remember",
        "forget" => "Forget",
        "web_search" => "Web search",
        "web_read" => "Read the web",
        "check_page" => "Check a page",
        "explore" => "Look into",
        other => other,
    }
}

/// A line of `/status`.
fn status_line(label: &str, value: impl std::fmt::Display) -> String {
    format!("{}:  {value}", t(label))
}

/// How many tasks and changes the panel on the right shows, of what there is, in a room for this many rows. Each keeps a row for its "and more".
fn share_rows(room: i32, tasks: usize, changes: usize) -> (usize, usize) {
    let room = room.max(4) as usize;
    if tasks + changes <= room {
        return (tasks, changes);
    }
    // A half each, and what one does not need goes to the other. What is cut has a row for saying so.
    let half = room / 2;
    if tasks <= half {
        (tasks, (room - tasks - 1).min(changes))
    } else if changes <= half {
        ((room - changes - 1).min(tasks), changes)
    } else {
        (half - 1, room - half - 1)
    }
}

/// The link at this character, if any: they are in order.
fn link_at(links: &[(i32, i32, String)], at: i32) -> Option<&str> {
    let after = links.partition_point(|(first, _, _)| *first <= at);
    links[..after].last().filter(|(_, last, _)| at < *last).map(|(_, _, url)| url.as_str())
}

/// How far along a task is, for where the panel on the right is not there to show it.
fn task_mark(status: &str) -> &'static str {
    match status {
        "done" | "completed" => "✓",
        "doing" | "in_progress" => "▸",
        _ => "○",
    }
}

/// A list's marker at the start of a line: what to show for it, and how long it is with the space after it.
fn list_marker(line: &str) -> Option<(String, usize)> {
    if line.starts_with("- [ ] ") || line.starts_with("* [ ] ") {
        return Some(("☐".to_string(), 6));
    }
    if line.starts_with("- [x] ") || line.starts_with("* [x] ") || line.starts_with("- [X] ") {
        return Some(("☑".to_string(), 6));
    }
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return Some(("•".to_string(), 2));
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && digits < 4 && line[digits..].starts_with(". ")).then(|| (line[..digits + 1].to_string(), digits + 2))
}

/// How a message goes to the agent.
fn send_command(out: Outgoing) -> Command {
    if out.images.is_empty() {
        Command::Send(out.text)
    } else {
        Command::SendWith(out.text, out.images.into_iter().map(|attached| attached.picture).collect())
    }
}

/// The files of a drop, or of what the Explorer put on the clipboard.
unsafe fn dropped_files(drop: HDROP) -> Vec<PathBuf> {
    let count = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
    (0..count).filter_map(|at| {
        let length = DragQueryFileW(drop, at, std::ptr::null_mut(), 0) as usize;
        let mut name = vec![0u16; length + 1];
        let read = DragQueryFileW(drop, at, name.as_mut_ptr(), name.len() as u32) as usize;
        (read > 0).then(|| PathBuf::from(String::from_utf16_lossy(&name[..read])))
    }).collect()
}

/// A small copy as a bitmap to draw with.
unsafe fn bitmap_of(decoded: &attach::Decoded) -> Option<HBITMAP> {
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = decoded.width;
    info.bmiHeader.biHeight = -decoded.height; // from the top down
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(std::ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bitmap.is_null() || bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(decoded.pixels.as_ptr(), bits as *mut u8, decoded.pixels.len());
    Some(bitmap)
}

/// The installer's own program beside this one, when it was installed and not unzipped.
fn uninstaller_beside() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("dogmind-uninstall.exe"))).filter(|file| file.is_file())
}

/// The class of the app's window, and the property on it that says which data folder its app has.
const WINDOW_CLASS: &str = "DogmindApp";
const FOLDER_PROPERTY: &str = "DogmindApp.Folder";

/// What stands for a data folder, in its name's lower case: it names the mutex of the app that has the folder, and is on that app's window.
fn folder_key(folder: &Path) -> u64 {
    folder.display().to_string().to_lowercase().bytes().fold(0xcbf29ce484222325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3))
}

/// The mutex an app holds for as long as it runs on a data folder.
fn mutex_name(key: u64) -> String {
    format!("Local\\DogmindApp-{key:016x}")
}

/// Says on this window which data folder its app has, for a start on the same folder to find it by.
unsafe fn mark_window_of_folder(window: HWND, key: u64) {
    SetPropW(window, wide(FOLDER_PROPERTY).as_ptr(), key as usize as HANDLE);
}

/// The window of the app that has the data folder with this key, among the windows of this class that there are: a copy on another data folder (a test's, a development build, the person's
/// own on theirs) is not it, and nor is a window that says nothing of a folder. None when there is none.
unsafe fn window_of_folder(class: &str, key: u64) -> Option<HWND> {
    let (class, property) = (wide(class), wide(FOLDER_PROPERTY));
    let mut window: HWND = std::ptr::null_mut();
    loop {
        window = FindWindowExW(std::ptr::null_mut(), window, class.as_ptr(), std::ptr::null());
        if window.is_null() {
            return None;
        }
        let marked = GetPropW(window, property.as_ptr());
        if !marked.is_null() && marked as usize as u64 == key {
            return Some(window);
        }
    }
}

/// Whether this data folder has its app already, whose window then comes forward: two would write over each other's
/// settings and start two engines. The mutex lasts as long as this process. Only the window of the app that has this folder comes
/// forward; where there is none to be found, no window is touched.
unsafe fn already_running(folder: &Path) -> bool {
    let key = folder_key(folder);
    let name = wide(&mutex_name(key));
    let mutex = windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, name.as_ptr());
    if mutex.is_null() || windows_sys::Win32::Foundation::GetLastError() != windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS {
        return false;
    }
    if let Some(window) = window_of_folder(WINDOW_CLASS, key) {
        ShowWindow(window, SW_SHOW);
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window);
    }
    true
}

/// How a copy started by an older one, which saw this build replace it, carries on where that one was.
#[derive(Debug, Default)]
pub struct Restart {
    /// The older copy: this one waits for it to end, as only one may run on a data folder.
    pub after: Option<u32>,
    /// The session it had open.
    pub resume: Option<PathBuf>,
    /// It was in the tray, out of sight, and this one stays there too.
    pub hidden: bool,
}

/// Waits, at most this long, for a process to end. One that is gone already, or cannot be looked at, is not waited for.
unsafe fn wait_for_end(pid: u32, longest: std::time::Duration) {
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
    if !process.is_null() {
        WaitForSingleObject(process, longest.as_millis() as u32);
        CloseHandle(process);
    }
}

/// When a file was last written, and how long it is: what a new build of it changes.
fn stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let data = std::fs::metadata(path).ok()?;
    Some((data.modified().ok()?, data.len())).filter(|(_, length)| *length > 0)
}

pub fn run(folder_argument: Option<PathBuf>, remove_first: bool, restart: Restart) -> ExitCode {
    // SAFETY: the Win32 calls below run on this one thread, on handles they created; `APP` is set once before any message
    // reaches the procedure and is only used on this thread.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if crate::machine::windows_build().is_some_and(|build| !crate::machine::is_supported(build)) {
            lang::set("system");
            let (words, title) = (wide(t("Dogmind needs Windows 11.")), wide("Dogmind"));
            MessageBoxW(std::ptr::null_mut(), words.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
            return ExitCode::FAILURE;
        }
        crate::crash::install(&settings::data_folder());
        if let Some(pid) = restart.after {
            wait_for_end(pid, std::time::Duration::from_secs(30));
        }
        if already_running(&settings::data_folder()) {
            return ExitCode::SUCCESS;
        }
        // (walking the screens: that what goes wrong is written down, and said at the next start)
        #[cfg(debug_assertions)]
        match std::env::var("DOGMIND_TEST_CRASH").as_deref() {
            Ok("panic") => panic!("a panic asked for by a test"),
            Ok("fault") => std::ptr::null_mut::<u8>().write_volatile(1),
            _ => {}
        }
        theme::set_light(Settings::load(&settings::data_folder().join("settings.json")).is_light());
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        prefer_theme(theme::is_light());
        start_gdiplus();
        register_button_class();
        let library = wide("Msftedit.dll");
        LoadLibraryW(library.as_ptr());

        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(WINDOW_CLASS);
        let background = CreateSolidBrush(theme::background());
        let icon = window_icon(64);
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            hbrBackground: background,
            hIcon: icon,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);
        let title = wide("Dogmind");
        let window = CreateWindowExW(0, class_name.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, 1100, 800, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
        title_bar(window);
        // (so that a start on this data folder finds this window, and no other app's)
        mark_window_of_folder(window, folder_key(&settings::data_folder()));

        let folder = settings::data_folder();
        // (what was pasted in an earlier run: a session keeps its own copy of what it sent)
        let _ = std::fs::remove_dir_all(folder.join("attachments"));
        let mut saved = Settings::load(&folder.join("settings.json"));
        let moved = saved.move_keys_to(crate::keys::store());
        if moved.to_save() {
            let _ = saved.save(&folder.join("settings.json"));
        }
        let keys_not_kept = moved.words();
        lang::set(&saved.display_language);
        // (before the transcript and the composer are made, whose first format takes it)
        TEXT_SCALE.store(saved.text_percent(), Ordering::Relaxed);
        let chosen = folder_argument.filter(|path| path.is_dir()).or_else(|| saved.workspace.clone().filter(|path| path.is_dir()));
        let needs_folder = chosen.is_none();
        // Until a folder is chosen the start panel stays over the chat, and nothing can be sent.
        let workspace = chosen.unwrap_or_else(|| Path::new(".").to_path_buf());

        let servers = dogmind_agent::mcp::Hub::start(&folder.join("mcp.json"));
        let (event_sender, events) = channel();
        let (notes_sender, notes) = channel();
        let poster = Poster { notes: notes_sender, window: window as isize };
        // (a key that Credential Manager would not keep: said once, when the window has begun, and never again, as the file no longer has it)
        for words in keys_not_kept {
            poster.send(Note::KeyNotKept(words));
        }
        // What a crash left half done on a folder commands can read: put right before anything else asks.
        {
            let (file, poster) = (read_folders::file_in(&folder), poster.clone());
            std::thread::spawn(move || {
                let failures = read_folders::recover(&file, &mut |_| {});
                if !failures.is_empty() {
                    poster.send(Note::ReadFolderDone(failures.join("\n"), true));
                }
            });
        }
        let waker = poster.clone();
        let mode = match saved.mode.as_str() {
            "accept_edits" => Mode::AcceptEdits,
            "auto" => Mode::Auto,
            "plan" => Mode::Plan,
            _ => Mode::Default,
        };
        let (crypt, policy) = Crypt::open(&folder);
        let handle = agent::start(
            agent_config(&saved, &folder, workspace.clone(), mode, servers.clone(), policy.clone()),
            event_sender,
            Box::new(move || waker.wake()),
        );
        let _ = handle.commands.send(Command::SetSafeCommands(saved.safe_commands));
        // What earlier runs left writable, now that what this one uses is counted. (With the key to ask for, the conversations' workspaces are not known yet: after it.)
        if !crypt.sweep_due {
            let (data, mut named, poster) = (folder.clone(), saved.workspaces.clone(), poster.clone());
            std::thread::spawn(move || {
                named.extend(session::list(&data.join("sessions")).into_iter().map(|summary| summary.workspace));
                named.sort();
                named.dedup();
                // (a folder whose take-back left something behind stays on the list and is tried again at each start; the person is told, as they are of a folder they named for commands)
                let left: Vec<String> = dogmind_agent::workspace_grants::sweep(&data, &named).iter().map(|folder| format!("{}: {}", folder.display(), t(dogmind_agent::workspace_grants::NOT_TAKEN_BACK))).collect();
                if !left.is_empty() {
                    poster.send(Note::ReadFolderDone(left.join("\n"), true));
                }
            });
        }

        let dpi = GetDpiForWindow(window);
        #[cfg(debug_assertions)]
        let dpi = std::env::var("DOGMIND_TEST_DPI").ok().and_then(|given| given.parse().ok()).unwrap_or(dpi);
        // Where it was left, if that is still on a screen: a monitor unplugged since would leave it out of reach.
        let left_at = saved.window.filter(|place| usable_place(place, scale(dpi, 780), scale(dpi, 520)) && !MonitorFromRect(&RECT { left: place[0], top: place[1], right: place[2], bottom: place[3] }, MONITOR_DEFAULTTONULL).is_null());
        match left_at {
            Some([left, top, right, bottom]) => {
                SetWindowPos(window, std::ptr::null_mut(), left, top, right - left, bottom - top, SWP_NOZORDER);
            }
            None => {
                SetWindowPos(window, std::ptr::null_mut(), 0, 0, scale(dpi, 1200), scale(dpi, 800), SWP_NOMOVE | SWP_NOZORDER);
            }
        }
        // Each at the size it is shown, not one scaled down.
        SendMessageW(window, WM_SETICON, ICON_SMALL as WPARAM, window_icon(scale(dpi, 16)) as LPARAM);
        SendMessageW(window, WM_SETICON, ICON_BIG as WPARAM, window_icon(scale(dpi, 32)) as LPARAM);
        let fonts = Fonts::new(dpi);
        set_button_fonts(fonts);
        let diff_class = wide("DogDiff");
        let class = WNDCLASSW { lpfnWndProc: Some(diff_procedure), hInstance: instance, lpszClassName: diff_class.as_ptr(), hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW), ..std::mem::zeroed() };
        RegisterClassW(&class);
        let diff_view = CreateWindowExW(0, diff_class.as_ptr(), std::ptr::null(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 0, 0, window, std::ptr::null_mut(), instance, std::ptr::null());
        let (modal_class, pane_class) = (wide("DogModal"), wide("DogPane"));
        for name in [&modal_class, &pane_class] {
            let class = WNDCLASSW { lpfnWndProc: Some(modal_procedure), hInstance: instance, lpszClassName: name.as_ptr(), hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW), ..std::mem::zeroed() };
            RegisterClassW(&class);
        }
        let nothing = wide("");
        let modal = CreateWindowExW(0, modal_class.as_ptr(), nothing.as_ptr(), WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS, 0, 0, 0, 0, window, std::ptr::null_mut(), instance, std::ptr::null());
        let pane = CreateWindowExW(0, pane_class.as_ptr(), nothing.as_ptr(), WS_CHILD | WS_CLIPCHILDREN | WS_VISIBLE, 0, 0, 0, 0, modal, std::ptr::null_mut(), instance, std::ptr::null());
        let list_style = WS_VSCROLL | WS_CLIPSIBLINGS | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | LBS_OWNERDRAWVARIABLE) as u32;
        let edit_style = WS_TABSTOP | ES_AUTOHSCROLL as u32;
        // What is found stays selected while the focus is in the find field.
        crate::textview::register(instance);
        let transcript_class = if crate::textview::wanted() { crate::textview::CLASS } else { "RICHEDIT50W" };
        let transcript = rich_edit_of(transcript_class, window, WS_VSCROLL | WS_CLIPSIBLINGS | ES_DISABLENOSCROLL | ES_READONLY as u32 | ES_NOHIDESEL as u32 | WS_TABSTOP, theme::background(), 11);
        SendMessageW(transcript, EM_SETEDITSTYLE, SES_HIDEGRIDLINES, SES_HIDEGRIDLINES as LPARAM);
        // (it is written only by this program: an undo list would only grow with every format set)
        SendMessageW(transcript, WM_USER + 82, 0, 0); // EM_SETUNDOLIMIT
        let composer = rich_edit_of(transcript_class, window, WS_VSCROLL | WS_TABSTOP | WS_CLIPSIBLINGS | ES_WANTRETURN as u32, theme::surface(), 11);
        SendMessageW(composer, EM_SETTEXTMODE, (TM_PLAINTEXT | TM_MULTILEVELUNDO | TM_MULTICODEPAGE) as WPARAM, 0);
        crate::textview::set_name(composer, "Message Dogmind");
        let doc = rich_edit_of(transcript_class, window, WS_VSCROLL | ES_READONLY as u32, theme::background(), 10);
        crate::textview::set_name(doc, "Document");
        // (a line an edge of the view cuts in half fades out there, as it does in a browser's chat, and does not read as letters gone wrong)
        for reading in [transcript, doc] {
            crate::textview::fade_edges(reading, if test_no_fade() { 0 } else { ds::size::TEXT_FADE });
        }
        let side = control(window, "LISTBOX", "", list_style, 0, ID_SIDE);
        let list = control(window, "LISTBOX", "", list_style, 0, ID_LIST);
        let chip = |id: isize, backdrop: u32| make_button(window, id, "", ButtonKind::Chip, backdrop);
        let app = Box::new(App {
            window,
            folder,
            settings: saved,
            agent: handle,
            events,
            notes,
            poster,
            engine: None,
            generation: Arc::new(AtomicU64::new(0)),
            panel: Panel::Chat,
            dpi,
            fonts,
            brushes: [background, CreateSolidBrush(theme::sidebar()), CreateSolidBrush(theme::surface()), CreateSolidBrush(theme::raised())],
            shapes: Vec::new(),
            new_session: make_button(window, ID_NEW_SESSION, t("plus|New session"), ButtonKind::Nav, theme::sidebar()),
            settings_button: make_button(window, ID_SETTINGS, "settings|Settings", ButtonKind::Nav, theme::sidebar()),
            side,
            side_rows: Vec::new(),
            side_hover: None,
            tree: files::Tree::new(),
            tree_list: control(window, "LISTBOX", "", list_style | LBS_HASSTRINGS as u32 | WS_TABSTOP, 0, ID_FILES),
            tree_rows: Vec::new(),
            tree_selected: None,
            tree_hover: None,
            tree_for: None,
            tree_watcher: None,
            tree_stale: false,
            tree_dpi: 0,
            tree_version: None,
            tree_menu: control(window, "LISTBOX", "", list_style & !WS_VSCROLL | LBS_HASSTRINGS as u32, 0, ID_FILES_MENU),
            tree_menu_row: None,
            tree_menu_hover: None,
            panel_menu: files_panel::PanelMenu { items: Vec::new(), whose: files_panel::MenuFor::Nothing },
            term: term_panel::TermView::new(window, theme::sidebar()),
            right_tab_hits: Vec::new(),
            collapsed: HashSet::new(),
            all_sessions: Vec::new(),
            current_session: None,
            transcript,
            composer,
            // (drawn by the app, as what is typed is: its Latin and its Japanese each in its face, which a static control's one font cannot be)
            composer_cue: control(window, "STATIC", t("Message Dogmind"), SS_NOPREFIX | 0x0D, 0, 0), // SS_OWNERDRAW
            send: make_button(window, ID_SEND, "arrow-up", ButtonKind::Round, theme::surface()),
            model_chip: chip(ID_MODEL_CHIP, theme::surface()),
            mode_chip: chip(ID_MODE_CHIP, theme::surface()),
            effort_chip: chip(ID_EFFORT_CHIP, theme::surface()),
            help_button: make_button(window, ID_HELP, "circle-question-mark", ButtonKind::Icon, theme::sidebar()),
            doc,
            overlay_hits: Vec::new(),
            overlay_hover: None,
            overlay_press: None,
            overlay_copied: None,
            overlay_tip_for: None,
            proxies: (0..PROXY_POOL).map(|at| make_button(window, ID_PROXY_POOL + at as isize, "", ButtonKind::Hidden, theme::background())).collect(),
            proxy_keys: vec![None; PROXY_POOL],
            answers: Vec::new(),
            user_cards: Vec::new(),
            rail_turns: Vec::new(),
            turn_words_due: false,
            rail: None,
            rail_reading: 0,
            rail_hover: None,
            rail_hits: Vec::new(),
            rail_tips: Vec::new(),
            column_right: 0,
            transcript_area: rect(0, 0, 0, 0),
            thumb: None,
            thumb_grab: None,
            shown: Vec::new(),
            replaying: false,
            transcript_rect: (0, 0, 0),
            text_twips: 9000,
            code_language: String::new(),
            composing: false,
            composer_procedure: 0,
            straydog: false,
            modal,
            pane,
            modal_close: make_button(modal, ID_MODAL_CLOSE, "x", ButtonKind::Icon, theme::background()),
            nav_buttons: (0..NAV_POOL).map(|at| make_button(modal, ID_NAV + at as isize, "", ButtonKind::Nav, theme::sidebar())).collect(),
            modal_open: false,
            modal_backdrop: std::ptr::null_mut(),
            modal_backdrop_size: (0, 0),
            modal_return: std::ptr::null_mut(),
            modal_shapes: Vec::new(),
            pane_shapes: Vec::new(),
            modal_card: rect(0, 0, 0, 0),
            modal_pressed_outside: false,
            help_section: 0,
            theme_segments: std::array::from_fn(|at| make_button(pane, ID_THEME_SEGMENT + at as isize, "", ButtonKind::Segment, theme::surface())),
            mode_select: make_button(pane, ID_MODE_SELECT, "", ButtonKind::Select, theme::background()),
            key_replace: make_button(pane, ID_KEY_REPLACE, "", ButtonKind::Secondary, theme::background()),
            key_remove: make_button(pane, ID_KEY_REMOVE, "", ButtonKind::Secondary, theme::background()),
            advanced_button: make_button(pane, ID_ADVANCED, "", ButtonKind::Ghost, theme::background()),
            variable_toggle: make_button(pane, ID_VARIABLE_TOGGLE, "", ButtonKind::Toggle, theme::background()),
            mcp_import: make_button(pane, ID_MCP_IMPORT, "", ButtonKind::Secondary, theme::background()),
            mcp_trust: Vec::new(),
            size_segments: std::array::from_fn(|at| make_button(pane, ID_TEXT_SIZE + at as isize, "", ButtonKind::Segment, theme::surface())),
            skill_toggles: Vec::new(),
            memory_notes: Vec::new(),
            memory_remove: Vec::new(),
            builds: Vec::new(),
            update_seen: None,
            update_offer: false,
            update_offer_button: make_button(window, ID_UPDATE_OFFER, "", ButtonKind::Secondary, theme::surface()),
            notice_later: make_button(window, ID_NOTICE_LATER, "", ButtonKind::Link, theme::surface()),
            release_offer: None,
            release_offer_button: make_button(window, ID_RELEASE_OFFER, "", ButtonKind::Secondary, theme::surface()),
            tray_told_release: None,
            release_checking: false,
            release_outcome: None,
            install_folder: updates::install_folder(),
            release_update: ReleaseUpdate::None,
            release_update_said: None,
            release_ready_closed: false,
            toggle_updates: make_button(pane, ID_TOGGLE_UPDATES, "", ButtonKind::Toggle, theme::background()),
            toggle_remember: make_button(pane, ID_TOGGLE_REMEMBER, "", ButtonKind::Toggle, theme::background()),
            update_declined: None,
            restart_when_done: false,
            checking_setup: false,
            read_folder_add: make_button(pane, ID_READ_FOLDER_ADD, "", ButtonKind::Secondary, theme::background()),
            read_folder_stop: make_button(pane, ID_READ_FOLDER_STOP, "", ButtonKind::Secondary, theme::background()),
            read_folder_remove: Vec::new(),
            read_folders: Vec::new(),
            read_folder_places: Vec::new(),
            read_folder_work: None,
            read_folder_said: (String::new(), false),
            setup_again: make_button(pane, ID_SETUP_AGAIN, "", ButtonKind::Secondary, theme::background()),
            key_editing: false,
            advanced_open: false,
            variable_on: false,
            greeting: next_greeting(0, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.subsec_nanos() as u64)),
            toggle_language: make_button(pane, ID_TOGGLE_LANGUAGE, "", ButtonKind::Select, theme::background()),
            toggle_display: make_button(window, ID_TOGGLE_DISPLAY, "", ButtonKind::Select, theme::background()),
            folder_chip: make_button(window, ID_FOLDER_CHIP, "", ButtonKind::Chip, theme::background()),
            composer_lines: 1,
            provider_up: make_button(window, ID_PROVIDER_UP, "chevron-up", ButtonKind::Icon, theme::background()),
            provider_down: make_button(window, ID_PROVIDER_DOWN, "chevron-down", ButtonKind::Icon, theme::background()),
            find_open: false,
            find_field: control(window, "EDIT", "", edit_style, 0, 0),
            find_previous: make_button(window, ID_FIND_PREVIOUS, "chevron-up", ButtonKind::Icon, theme::surface()),
            find_next: make_button(window, ID_FIND_NEXT, "chevron-down", ButtonKind::Icon, theme::surface()),
            find_close: make_button(window, ID_FIND_CLOSE, "x", ButtonKind::Icon, theme::surface()),
            find_matches: Vec::new(),
            find_at: 0,
            find_after_open: None,
            side_search: control(window, "EDIT", "", edit_style, 0, 0),
            side_query: String::new(),
            side_hits: None,
            search_generation: 0,
            suggest: control(window, "LISTBOX", "", list_style, 0, ID_SUGGEST),
            suggestions: Vec::new(),
            suggest_hover: None,
            files: Vec::new(),
            skills: Vec::new(),
            mcp: servers.clone(),
            policy: policy.clone(),
            crypt,
            branch: None,
            session_title: String::new(),
            totals: (0, 0, 0, 0),
            side_target: None,
            rename_target: None,
            rename_seed: None,
            resume_paths: Vec::new(),
            parked: Vec::new(),
            waiting: Waiting::default(),
            waiting_taken: std::time::Instant::now(),
            woken_turn: false,
            waiting_hits: Vec::new(),
            waiting_times: Vec::new(),
            provider: None,
            held_notices: HashMap::new(),
            failed_sessions: HashSet::new(),
            turn_failed: false,
            turns: Vec::new(),
            turn_changes: std::collections::BTreeMap::new(),
            rewind_rows: Vec::new(),
            shown_rules: Vec::new(),
            changes: Vec::new(),
            change_wanted: None,
            turn_view: None,
            turn_slots: Vec::new(),
            turn_end_slot: 0,
            turn_cards_waiting: HashMap::new(),
            turn_cards_came: Vec::new(),
            goal_clear: make_button(window, ID_GOAL_CLEAR, "x", ButtonKind::Icon, theme::surface()),
            right_width: 0,
            right_toggle: make_button(window, ID_PANEL_TOGGLE, "panel-right", ButtonKind::Icon, theme::background()),
            right_hits: Vec::new(),
            please_quit: {
                let name = wide("DogmindApp.PleaseQuit");
                RegisterWindowMessageW(name.as_ptr())
            },
            taskbar_created: {
                let name = wide("TaskbarCreated");
                RegisterWindowMessageW(name.as_ptr())
            },
            delete_target: None,
            preview: None,
            preview_file: None,
            preview_lines: None,
            preview_written: None,
            preview_loaded: 0,
            preview_gone: false,
            preview_making: false,
            preview_blocked: 0,
            preview_hits: Vec::new(),
            dashboard_hit: None,
            preview_reload: make_button(window, ID_PREVIEW_RELOAD, "rotate-cw", ButtonKind::Icon, theme::sidebar()),
            preview_browser: make_button(window, ID_PREVIEW_BROWSER, "external-link", ButtonKind::Icon, theme::sidebar()),
            preview_close: make_button(window, ID_PREVIEW_CLOSE, "x", ButtonKind::Icon, theme::sidebar()),
            todos: Vec::new(),
            git: None,
            web_seen: false,
            toggle_web: make_button(pane, ID_TOGGLE_WEB, "", ButtonKind::Toggle, theme::background()),
            toggle_compact: make_button(pane, ID_TOGGLE_COMPACT, "", ButtonKind::Toggle, theme::background()),
            context_auto: None,
            onboarding: false,
            setup_kind: Kind::Embedded,
            sidebar_width: 0,
            diff_view,
            trajectory: None,
            diff_previous: make_button(window, ID_DIFF_PREVIOUS, "chevron-up", ButtonKind::Icon, theme::background()),
            diff_next: make_button(window, ID_DIFF_NEXT, "chevron-down", ButtonKind::Icon, theme::background()),
            diff_close: make_button(window, ID_DIFF_CLOSE, "x", ButtonKind::Icon, theme::background()),
            diff_rows: Vec::new(),
            diff_starts: Vec::new(),
            diff_scroll: 0,
            diff_across: 0,
            diff_chosen: None,
            diff_thumb_grab: None,
            diff_words: String::new(),
            diff_file: None,
            note: rich_edit_of("RICHEDIT50W", window, ES_READONLY as u32 | WS_VSCROLL, theme::surface(), ds::points::CODE),
            approve: make_button(window, ID_APPROVE, t("Yes (Y)"), ButtonKind::Primary, theme::surface()),
            always: make_button(window, ID_ALWAYS, t("Always (A)"), ButtonKind::Secondary, theme::surface()),
            reject: make_button(window, ID_REJECT, t("No (N)"), ButtonKind::Secondary, theme::surface()),
            feedback: control(window, "EDIT", "", edit_style, 0, 0),
            activity_area: rect(0, 0, 0, 0),
            status_area: rect(0, 0, 0, 0),
            list,
            list_rows: Vec::new(),
            list_hover: None,
            fields: std::array::from_fn(|_| control(window, "EDIT", "", edit_style, 0, 0)),
            labels: std::array::from_fn(|_| String::new()),
            buttons: std::array::from_fn(|at| make_button(window, ID_BUTTONS + at as isize, "", ButtonKind::Secondary, theme::background())),
            kind_chip: make_button(window, ID_KIND_CHIP, "", ButtonKind::Select, theme::background()),
            drafter_chip: make_button(window, ID_DRAFTER, "", ButtonKind::Chip, theme::background()),
            speed_chip: make_button(window, ID_SPEED_FIRST, "", ButtonKind::Chip, theme::background()),
            vision_chip: make_button(window, ID_VISION, "", ButtonKind::Chip, theme::background()),
            vision_delete: make_button(pane, ID_VISION_DELETE, "", ButtonKind::Secondary, theme::background()),
            vision_offer_button: make_button(window, ID_VISION_OFFER, "", ButtonKind::Secondary, theme::surface()),
            vision_wanted: true,
            vision_fetch: false,
            vision_offer: None,
            vision_offer_close: None,
            toggle_thinking: make_button(pane, ID_TOGGLE_THINKING, "", ButtonKind::Toggle, theme::background()),
            toggle_safe: make_button(pane, ID_TOGGLE_SAFE, "", ButtonKind::Toggle, theme::background()),
            panel_title: String::new(),
            panel_log: String::new(),
            progress: None,
            kind: Kind::Server,
            drafter: true,
            drafter_fetch: None,
            drafter_restart_waits: false,
            pending: None,
            tools_running: HashMap::new(),
            running: false,
            has_messages: false,
            queued: Vec::new(),
            thumb_at: 0,
            transcript_region: ((0, 0), None),
            interjected: Vec::new(),
            question: None,
            card_slots: Vec::new(),
            card_next: 0,
            plan_review: None,
            goal: None,
            queue_hits: Vec::new(),
            queue_tips: Vec::new(),
            blocks: Vec::new(),
            runs: Vec::new(),
            run_open: None,
            run_writing: false,
            run_carry: Vec::new(),
            asked_calls: HashSet::new(),
            run_hover: None,
            plain_writing: None,
            request_live: None,
            open_block: None,
            block_card_due: false,
            last_inserted: (0, 0),
            links: Vec::new(),
            link_hover: None,
            hover_row: None,
            panel_scroll: 0,
            panel_extent: 0,
            pane_thumb: None,
            pane_thumb_grab: None,
            panel_problem: false,
            stop_asked: false,
            connect_failed: false,
            connect_retrying: false,
            connect_began: None,
            connect_waiting_since: None,
            last_connect_error: String::new(),
            restoring_draft: false,
            composer_typed_at: None,
            attach_button: make_button(window, ID_ATTACH, "paperclip", ButtonKind::Icon, theme::surface()),
            attachments: Vec::new(),
            attached_windows: (0..attach::MOST_IMAGES).map(|at| make_button(window, ID_ATTACHED + at as isize, "", ButtonKind::Attached, theme::surface())).collect(),
            thumb_wanted: std::collections::HashSet::new(),
            picture_blocks: Vec::new(),
            thumbs: HashMap::new(),
            thumb_clock: 0,
            vision: None,
            vision_told: false,
            provider_models: Vec::new(),
            preview_root: None,
            tooltip: {
                let common = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
                InitCommonControlsEx(&common);
                CreateWindowExW(0, TOOLTIPS_CLASSW, std::ptr::null(), WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX, 0, 0, 0, 0, window, std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null())
            },
            workspace,
            answer: String::new(),
            last_answer: String::new(),
            pending_text: String::new(),
            reveal: String::new(),
            stream_mask: crate::mask::StreamMask::default(),
            revealed_at: std::time::Instant::now(),
            revealing: false,
            unkept: false,
            transcript_resized: false,
            transcript_document: text_document(transcript),
            opening: false,
            frames: Arc::new(FrameClock { wanted: AtomicBool::new(false), posted: AtomicBool::new(false) }),
            frame_thread: None,
            frame_at: std::time::Instant::now(),
            follow_speed: 0.0,
            follow_carried: 0.0,
            follow_end: true,
            wheel_rest: 0,
            scrolled_up_at: None,
            line_start: true,
            thinking_written: false,
            in_code: false,
            heading: 0,
            italic: false,
            struck: false,
            quote: false,
            inline_before: None,
            para: Para::Normal,
            para_next: None,
            code_comment: false,
            table: Vec::new(),
            bold: false,
            inline_code: false,
            thinking: String::new(),
            activity: String::new(),
            activity_since: None,
            activity_second: 0,
            connected: false,
            connection: String::new(),
            engine_waits: false,
            engine_failed_at: None,
            engine_stop_told: false,
            model: String::new(),
            used: 0,
            window_tokens: 0,
            rate: None,
            catalog: Vec::new(),
            machine_line: String::new(),
            refusal: None,
            leaving: Leaving::No,
            remove_all: make_button(pane, ID_REMOVE_ALL, "", ButtonKind::Secondary, theme::background()),
            start_folders: Vec::new(),
            setting_up: false,
            setup_tally: Default::default(),
            licence_link: make_button(window, ID_MODEL_LICENCE, "", ButtonKind::Link, theme::background()),
            setup_cancel: Arc::new(AtomicBool::new(false)),
            setup_status: String::new(),
            selected_provider: None,
            list_procedure: 0,
            transcript_procedure: 0,
            pointer_tracked: false,
            engine_version: None,
            title_area: rect(0, 0, 0, 0),
            title_clicked: None,
            folder_told: PathBuf::new(),
            always_line: String::new(),
            approval_head: String::new(),
            cues_for: None,
            doc_procedure: 0,
            note_procedure: 0,
            bar_grab: None,
            reveal_rate: 0.0,
            reveal_carried: 0.0,
        });
        APP.store(Box::into_raw(app), Ordering::Relaxed);
        let app = &mut *APP.load(Ordering::Relaxed);
        SendMessageW(app.composer, EM_SETEVENTMASK, 0, ENM_CHANGE as LPARAM);
        SendMessageW(app.note, EM_SETMARGINS, 3, 0); // EC_LEFTMARGIN | EC_RIGHTMARGIN
        for edit in app.fields.into_iter().chain([app.feedback, app.composer_cue, app.find_field, app.side_search]) {
            SendMessageW(edit, WM_SETFONT, app.fonts.field as WPARAM, 1);
        }
        app.list_procedure = SetWindowLongPtrW(app.side, GWLP_WNDPROC, list_subclass as *const () as isize);
        SetWindowLongPtrW(app.list, GWLP_WNDPROC, list_subclass as *const () as isize);
        app.composer_procedure = SetWindowLongPtrW(app.composer, GWLP_WNDPROC, composer_subclass as *const () as isize);
        SetWindowLongPtrW(app.suggest, GWLP_WNDPROC, list_subclass as *const () as isize);
        SetWindowLongPtrW(app.tree_list, GWLP_WNDPROC, list_subclass as *const () as isize);
        SetWindowLongPtrW(app.tree_menu, GWLP_WNDPROC, files_panel::menu_subclass as *const () as isize);
        // (what a reader says of the list: its own name)
        set_text(app.tree_list, t("Files"));
        set_text(app.tree_menu, t("Files"));
        SendMessageW(app.term.rename_field, WM_SETFONT, app.fonts.field as WPARAM, 1);
        app.transcript_procedure = SetWindowLongPtrW(app.transcript, GWLP_WNDPROC, transcript_subclass as *const () as isize);
        app.doc_procedure = SetWindowLongPtrW(app.doc, GWLP_WNDPROC, doc_subclass as *const () as isize);
        app.note_procedure = SetWindowLongPtrW(app.note, GWLP_WNDPROC, note_subclass as *const () as isize);
        app.theme_controls();
        app.tray_add();
        DragAcceptFiles(window, 1);
        SendMessageW(app.tooltip, TTM_SETMAXTIPWIDTH, 0, scale(dpi, 420) as LPARAM);
        app.apply_language();
        // (a copy that took over from one in the tray stays there, out of the person's way)
        if !restart.hidden {
            ShowWindow(window, if app.settings.maximized { SW_SHOWMAXIMIZED } else { SW_SHOW });
        }
        SetTimer(window, TIMER_ENGINE, 1000, None);
        // What this copy runs, to see a new build come in over it.
        app.builds = std::env::current_exe().ok().into_iter().chain(engine::find(&app.settings)).map(|path| { let now = stamp(&path); (path, now) }).collect();
        SetTimer(window, TIMER_UPDATE, 5000, None);
        SetTimer(window, TIMER_RELEASES, FIRST_RELEASE_LOOK_MS, None);
        app.tidy_updates();
        app.update_status();
        app.refresh_sessions();
        if !needs_folder {
            app.look_at_workspace();
            app.refresh_changes();
        }
        SetFocus(app.composer);
        // Someone who has used the app before setups came in has been through it, in effect.
        if too_long_for_commands(&app.workspace) {
            app.say(t(PATH_TOO_LONG));
        }
        if let Some(crash) = crate::crash::untold(&app.folder) {
            app.say(&tf("This app ended unexpectedly last time. What happened is in {0}. /report writes a report you can send.", &[&shown_path(&crash)]));
        }
        let first_run = !app.settings.set_up && app.settings.workspace.is_none() && !engine::is_set_up(&app.folder);
        if remove_first {
            app.set_panel(Panel::ConfirmRemove);
        } else if first_run {
            app.set_panel(Panel::Welcome);
        } else {
            if app.settings.active_provider().is_some_and(|provider| provider.kind == Kind::Embedded) {
                app.engine_waits = true;
                app.connection = "starts when you write".into();
                app.update_status();
            } else {
                app.connect();
            }
            // No folder to come back to: it begins in the usual one, and the header changes it.
            if needs_folder {
                app.finish_onboarding();
            }
            // The session the copy before this one had open, as the person left it.
            let resume = restart.resume.filter(|session| session.is_file());
            if app.crypt.is_locked() {
                app.crypt.resume = resume;
            } else if let Some(session) = resume {
                app.open_session(session);
            }
        }
        // The key, or the question for it, before anything is opened: with no way past it but the key, the passphrase or the recovery code.
        if app.crypt.is_locked() {
            app.set_panel(Panel::Unlock);
        } else if let Some(pending) = app.crypt.keystore.as_ref().and_then(Keystore::pending) {
            // (a change of every conversation that was stopped before it was done, finished now)
            app.start_crypt(if pending == Pending::Encrypt { CryptJob::FinishOn } else { CryptJob::FinishOff }, String::new(), false);
        }

        crate::frametrace::start();
        // A window that answers is little work, and the engine loses none of its speed to it: it should not have to wait for a core behind threads that would take them all.
        windows_sys::Win32::System::Threading::SetThreadPriority(windows_sys::Win32::System::Threading::GetCurrentThread(), windows_sys::Win32::System::Threading::THREAD_PRIORITY_ABOVE_NORMAL);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            if take_key(&mut *APP.load(Ordering::Relaxed), &message) {
                continue;
            }
            let trace = crate::frametrace::begin();
            TranslateMessage(&message);
            DispatchMessageW(&message);
            if trace.is_some() {
                let shown = &*APP.load(Ordering::Relaxed);
                crate::frametrace::end(trace, message.message, if message.hwnd == shown.transcript { "transcript" } else if message.hwnd == shown.window { "window" } else { "other" });
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod read_folder_words_tests {
    use super::*;

    fn folder(state: read_folders::State, not_changed: u64) -> read_folders::Folder {
        let mut text = serde_json::json!({"path": "C:\\data", "state": state, "not_changed": not_changed});
        text["held"] = serde_json::json!([]);
        serde_json::from_value(text).unwrap()
    }

    #[test]
    fn a_folder_with_nothing_to_say_says_nothing() {
        assert_eq!(read_folder_detail(&folder(read_folders::State::Open, 0)), None);
    }

    #[test]
    fn what_is_not_finished_and_what_could_not_be_changed_are_said() {
        assert!(read_folder_detail(&folder(read_folders::State::Removing, 0)).unwrap().contains("next starts"));
        assert!(read_folder_detail(&folder(read_folders::State::Open, 3)).unwrap().contains('3'));
    }

    #[test]
    fn adding_says_how_it_went_and_keeps_to_what_is_promised_of_keys() {
        let place = Path::new("C:\\data");
        let (words, problem) = read_folder_added_words(place, &Ok(read_folders::Added::Open { objects: 9, held_out: 2, not_changed: 0 }));
        assert!(words.contains('2') && words.contains("made later") && !problem, "{words}");
        assert!(!read_folder_added_words(place, &Ok(read_folders::Added::Open { objects: 9, held_out: 0, not_changed: 0 })).1);
        assert!(!read_folder_added_words(place, &Ok(read_folders::Added::Stopped)).1);
        assert!(read_folder_added_words(place, &Err(read_folders::TOO_WIDE.to_string())).1);
    }

    #[test]
    fn a_long_path_keeps_its_start_and_its_end() {
        let long = PathBuf::from(format!("C:\\Users\\me\\{}\\the-project", "very-deep\\".repeat(20)));
        let shown = shown_path(&long);
        assert!(shown.chars().count() < 90 && shown.starts_with("C:\\Users") && shown.ends_with("the-project") && shown.contains("..."), "{shown}");
        assert_eq!(shown_path(Path::new("C:\\data")), "C:\\data");
        assert_eq!(read_folder_name(Path::new("C:\\work\\named")), "named");
    }

    #[test]
    fn work_going_on_says_which_way_and_how_far() {
        let mut work = ReadFolderWork { adding: true, folder: PathBuf::from("C:\\data"), count: 12, cancel: Arc::new(AtomicBool::new(false)), can_stop: true, stopping: false };
        assert!(work.words().contains("12"));
        work.stopping = true;
        assert!(work.words().contains("undoing"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_names_the_version_and_its_one_button_says_what_a_press_does() {
        use notice::Notice;
        let text = |shown: Notice| notice_text(&shown);
        let said = |version: &str| tf("Dogmind {0} is available.", &[&version]);
        assert_eq!(text(Notice::Page { version: "0.2.0".into() }), (said("0.2.0"), t("Open the release page").to_string()));
        // (while the setup comes, the button is how far it has come)
        assert_eq!(text(Notice::Fetching { version: "0.2.0".into(), percent: 37 }), (said("0.2.0"), tf("Downloading… {0}%", &[&37])));
        // (once it is here and checked, one press updates, and the button says that Dogmind restarts)
        let ready = text(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: false, checking: false });
        assert_eq!(ready, (tf("Dogmind {0} is ready to install.", &[&"0.2.0"]), t("Update now (Dogmind restarts)").to_string()));
        assert_eq!(text(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: false, checking: true }).1, t("Checking…"));
        // (work under way is waited for, and the same button takes that back)
        assert_eq!(text(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: true, checking: false }), (t("It restarts into the new version when this work is done.").to_string(), t("Not now").to_string()));
        assert_eq!(text(Notice::UpdateNow { version: None, waiting: false, checking: false }), (t("A new version is here.").to_string(), t("Update now (Dogmind restarts)").to_string()));
        // (what the person reads in English is the words the test names)
        if !lang::is_japanese() {
            assert_eq!(ready.1, "Update now (Dogmind restarts)");
        }
    }

    #[test]
    fn the_notice_is_laid_out_on_the_chat_and_the_changes_and_nowhere_else() {
        let on = |panel: Panel, onboarding: bool| notice::is_on_page(panel == Panel::Chat, panel == Panel::Diff, onboarding);
        assert!(on(Panel::Chat, false) && on(Panel::Diff, false));
        for panel in [Panel::About, Panel::General, Panel::Models, Panel::Help, Panel::Sessions, Panel::Rename] {
            assert!(!on(panel, false), "a dialog stands over the page that has it");
        }
        assert!(!on(Panel::Chat, true), "the first screens ask nothing of the kind");
    }

    #[test]
    fn a_note_of_progress_changes_the_percent_and_leaves_the_update_button_as_it_was() {
        let fetching = |done: u64| ReleaseUpdate::Fetching { version: "0.2.0".into(), done, total: 1000, stop: Arc::new(AtomicBool::new(false)) };
        for (checking, restart_when_done) in [(false, false), (false, true), (true, false)] {
            let words = update_button_words_of(&fetching(0), false, checking, restart_when_done);
            assert_eq!(words, Some(t("Cancel")), "a way to stop it, from the first note to the last");
            for done in [1, 17, 500, 999, 1000] {
                assert_eq!(update_button_words_of(&fetching(done), false, checking, restart_when_done), words, "at {done} of 1000");
                assert_eq!(update_button_kind_of(&fetching(done), restart_when_done), update_button_kind_of(&fetching(0), restart_when_done));
            }
        }
        assert_eq!(update_button_kind_of(&fetching(3), false), ButtonKind::Link);
        // (the buttons of an update that is not being fetched are as they were: none when there is none)
        assert_eq!(update_button_words_of(&ReleaseUpdate::None, false, false, false), None);
    }

    #[test]
    fn what_is_japanese_in_a_rich_edit_stays_in_its_own_face() {
        // SAFETY: a hidden control of this test's own, on this thread, and the structures are as long as they say.
        unsafe {
            let (library, class, title) = (wide("Msftedit.dll"), wide("RICHEDIT50W"), wide(""));
            LoadLibraryW(library.as_ptr());
            let edit = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_POPUP | ES_MULTILINE as u32, 0, 0, 400, 200, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
            assert!(!edit.is_null());
            let format = char_format(theme::text(), 0, UI_FACE, 11, None);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            associate_japanese(edit, 11);
            let words = wide("abc \u{65e5}\u{672c}\u{8a9e} def");
            SendMessageW(edit, EM_REPLACESEL, 0, words.as_ptr() as LPARAM);
            let face_of = |first: i32, last: i32| {
                let range = CharRange { first, last };
                SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
                let mut found = char_format(0, 0, "", 11, None);
                SendMessageW(edit, 0x043A, SCF_SELECTION as WPARAM, std::ptr::addr_of_mut!(found) as LPARAM); // EM_GETCHARFORMAT
                let length = found.face.iter().position(|&unit| unit == 0).unwrap_or(32);
                String::from_utf16_lossy(&found.face[..length])
            };
            // (a Japanese Windows names the face in Japanese)
            let japanese = |face: String| face == japanese_face() || (japanese_face() == crate::paint::BUNDLED_FACE && face == "BIZ UDゴシック");
            assert_eq!(face_of(0, 3), UI_FACE);
            assert!(japanese(face_of(4, 7)), "{}", face_of(4, 7));
            // A format over all of it, as a line of the transcript gets, leaves that as it was.
            let range = CharRange { first: 0, last: -1 };
            SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            assert!(japanese(face_of(4, 7)), "{}", face_of(4, 7));
            DestroyWindow(edit);
        }
    }

    #[test]
    fn the_model_chip_offers_what_the_provider_lists_and_marks_what_is_in_use() {
        let provider = |name: &str| ProviderConfig { name: name.into(), kind: Kind::Codex, base_url: String::new(), model: String::new(), api_key: String::new() };
        let providers = [provider("This PC"), provider("Codex")];
        let models = ["gpt-a".to_string(), "gpt-b".to_string()];
        let items = model_chip_items(&providers, "Codex", &models, "gpt-b");
        let named: Vec<(usize, &str, bool)> = items.iter().map(|(id, text, on)| (*id, text.as_str(), *on)).collect();
        assert_eq!(named[..5], [(M_PROVIDER, "This PC", false), (M_PROVIDER + 1, "Codex", true), (0, "", false), (M_MODEL_CHOICE, "gpt-a", false), (M_MODEL_CHOICE + 1, "gpt-b", true)]);
        assert_eq!(items.last().unwrap().0, M_PROVIDERS);
        assert_eq!(model_chip_items(&providers, "This PC", &[], "").len(), 4, "a provider with no list: the providers alone");
    }

    #[test]
    fn the_highest_effort_is_max_for_a_deepseek_model_and_extra_high_for_another() {
        let names = |model: &str| efforts_for(model).map(|(_, name)| name);
        for model in ["DeepSeek-V4.1", "deepseek-v4.1-iq2_xs", "unsloth/DEEPSEEK-V4", "my-deepseek"] {
            assert_eq!(names(model), ["Default", "None", "Low", "Medium", "High", "Max"], "{model}");
        }
        for model in ["", "gpt-5", "Qwen3.8-35B", "claude-sonnet"] {
            assert_eq!(names(model), ["Default", "None", "Low", "Medium", "High", "Extra high"], "{model}");
        }
        // What is sent does not change with the name.
        assert_eq!(efforts_for("deepseek-v4.1").map(|(known, _)| known), EFFORTS.map(|(known, _)| known));
        assert_eq!(efforts_for("deepseek-v4.1")[5].0, Some("xhigh"));
    }

    #[test]
    fn the_efforts_are_named_for_the_model_the_provider_is_set_to_or_else_the_one_connected() {
        let provider = |name: &str, model: &str| ProviderConfig { name: name.into(), kind: Kind::Server, base_url: String::new(), model: model.into(), api_key: String::new() };
        let mut settings = Settings::default();
        settings.providers = vec![provider("a", "deepseek-v4.1"), provider("b", ""), provider("c", "gpt-5")];
        settings.active = "a".into();
        assert_eq!(model_of_effort(&settings, "gpt-5"), "deepseek-v4.1", "what the provider is set to");
        settings.active = "b".into();
        assert_eq!(model_of_effort(&settings, "DeepSeek-V4.1"), "DeepSeek-V4.1", "set to none: what the connection settled on");
        assert_eq!(efforts_for(model_of_effort(&settings, "DeepSeek-V4.1"))[5].1, "Max");
        settings.active = "c".into();
        assert_eq!(efforts_for(model_of_effort(&settings, "DeepSeek-V4.1"))[5].1, "Extra high");
    }

    #[test]
    fn max_asks_for_xhigh_for_any_model() {
        let xhigh = EFFORTS.iter().position(|(known, _)| *known == Some("xhigh"));
        assert!(xhigh.is_some());
        for word in ["max", "Max", "MAX", " max ", "最大", "xhigh", "extra high", "Extra High"] {
            assert_eq!(effort_asked_for(word), xhigh, "{word}");
        }
        assert_eq!(effort_asked_for("default"), Some(0));
        assert_eq!(effort_asked_for("low"), Some(2));
        assert_eq!(effort_asked_for("maximum"), None);
    }

    #[test]
    fn the_files_that_hold_keys_are_named_and_a_variable_is_not_a_file() {
        let mut settings = Settings::default();
        settings.providers = vec![ProviderConfig { name: "a".into(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: "D:\\keys\\a.txt".into() }, ProviderConfig { name: "b".into(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: "env:KEY".into() }];
        settings.search_key = " D:\\keys\\brave.txt ".into();
        assert_eq!(key_files(&settings), [PathBuf::from("D:\\keys\\a.txt"), PathBuf::from("D:\\keys\\brave.txt")]);
    }

    #[test]
    fn a_path_windows_cannot_start_a_program_in_is_told_from_one_it_can() {
        assert!(!too_long_for_commands(Path::new("C:\\Users\\me\\work")));
        assert!(too_long_for_commands(&Path::new("C:\\").join("長".repeat(300))));
    }

    #[test]
    fn every_constant_word_has_its_japanese() {
        let mut words: Vec<&str> = COMMANDS.iter().map(|(_, about)| *about).collect();
        words.extend(["read_file", "list_files", "find_files", "search_files", "show_diff", "apply_patch", "create_file", "execute_command", "command_status", "command_stop", "wake_at", "todo", "skill", "remember", "forget", "web_search", "web_read", "check_page", "explore"].map(tool_name));
        words.extend(EFFORTS.iter().map(|(_, name)| *name));
        words.extend(efforts_for("DeepSeek-V4.1").iter().map(|(_, name)| *name));
        words.extend(MODES.iter().map(|(_, name)| *name));
        words.extend(THEMES.iter().map(|(_, name)| *name));
        words.extend(TEXT_SIZES.iter().map(|(_, name)| *name));
        words.extend(ANSWER_LANGUAGES.iter().map(|(_, name)| *name));
        words.extend([CODEX_MISSING, PATH_TOO_LONG, "As you write"]);
        for word in words {
            assert!(lang::has_japanese(word), "{word}");
        }
    }

    #[test]
    fn the_rail_has_every_turn_while_they_fit_and_keeps_the_one_read_in_its_middle_when_not() {
        // (three turns at a pitch of 10 with 6 over and under: 42 high)
        assert_eq!(rail_marks(3, 0, 42, 10, 6), [(0, 11), (1, 21), (2, 31)]);
        let long = rail_marks(100, 50, 100, 10, 6);
        assert!(long.iter().any(|&(at, y)| at == 50 && (y - 50).abs() <= 5), "{long:?}");
        assert!(long.iter().all(|&(_, y)| (5..=95).contains(&y)) && long.len() <= 10);
        // (at either end of a long conversation the ladder stops at its own end)
        assert_eq!(rail_marks(100, 0, 100, 10, 6)[0], (0, 11));
        assert_eq!(rail_marks(100, 99, 100, 10, 6).last(), Some(&(99, 89)));
        assert_eq!(rail_words("## 見出し\n**太字** の  `code`", 50), "見出し 太字 の code");
        assert_eq!(rail_words("abcdef", 3), "abc…");
    }

    #[test]
    fn a_message_waiting_for_the_work_to_end_can_go_into_it() {
        assert_eq!(queue_acts(true), [QueueAct::Now, QueueAct::Edit, QueueAct::Drop]);
        assert_eq!(queue_acts(false), [QueueAct::Edit, QueueAct::Drop]);
        assert!([QueueAct::Now, QueueAct::Edit, QueueAct::Drop].iter().all(|act| lang::has_japanese(act.tip())));
    }

    #[test]
    fn a_waiting_message_reads_as_one_line() {
        assert_eq!(waiting_words("a\nb", 0), "a");
        assert_eq!(waiting_words("a", 2), format!("a  ·  {}", tf("{0} pictures", &[&2])));
        assert_eq!(waiting_words("", 1), tf("{0} pictures", &[&1]));
    }

    #[test]
    fn a_message_goes_as_plain_words_unless_it_has_a_picture() {
        assert!(matches!(send_command(Outgoing::words("a".into())), Command::Send(text) if text == "a"));
        let picture = dogmind_agent::images::Attachment { name: "p.png".into(), bytes: vec![1], media_type: "image/png".into(), width: 1, height: 1 };
        let attached = Attached { path: PathBuf::from("p.png"), picture: picture.clone() };
        assert!(matches!(send_command(Outgoing { text: String::new(), images: vec![attached], card: String::new() }), Command::SendWith(text, pictures) if text.is_empty() && pictures == [picture]));
    }

    /// One request from a scripted server, and its answer: a word, as a stream.
    fn serve_one(listener: std::net::TcpListener, seen: Sender<String>) {
        // (not one and done: a listener that closes takes with it a connection still waiting in its queue, as a reset)
        for stream in listener.incoming() {
            let seen = seen.clone();
            std::thread::spawn(move || serve_a_request(stream.unwrap(), seen));
        }
    }

    fn serve_a_request(mut stream: std::net::TcpStream, seen: Sender<String>) {
        use std::io::{Read, Write};
        let mut bytes = Vec::new();
        let mut piece = [0u8; 65536];
        let body_at = loop {
            let count = stream.read(&mut piece).unwrap_or(0);
            if count == 0 {
                return;
            }
            bytes.extend_from_slice(&piece[..count]);
            if let Some(at) = bytes.windows(4).position(|four| four == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = String::from_utf8_lossy(&bytes[..body_at]).to_lowercase();
        let length: usize = head.split("content-length:").nth(1).and_then(|rest| rest.lines().next()).and_then(|number| number.trim().parse().ok()).unwrap();
        while bytes.len() < body_at + length {
            let count = stream.read(&mut piece).unwrap();
            bytes.extend_from_slice(&piece[..count]);
        }
        if length > 0 {
            let _ = seen.send(String::from_utf8_lossy(&bytes[body_at..]).into_owned());
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        for data in ["data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n", "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n", "data: [DONE]\n\n"] {
            stream.write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes()).unwrap();
        }
        stream.write_all(b"0\r\n\r\n").unwrap();
        // Closed at once, with the client still on its way, Windows may reset the connection and take back what it has not read yet.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        while stream.read(&mut piece).is_ok_and(|count| count > 0) {}
    }

    /// A connection that failed is told by the app's sentence alone; what the engine said under it is shown apart, in smaller text.
    #[test]
    fn a_connection_that_failed_is_told_by_the_sentence_and_the_engines_line_is_apart() {
        let with_line = llm::with_line(llm::START_OUT_OF_MEMORY, "Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory)");
        assert_eq!(connect_failure(&with_line), connect_failure(llm::START_OUT_OF_MEMORY));
        assert!(!connect_failure(&with_line).contains("Straydog"), "{}", connect_failure(&with_line));
        assert_eq!(llm::line_under(&with_line), Some("Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory)"));
        let refused = llm::with_line(llm::KEY_REFUSED, "Incorrect API key provided");
        assert_eq!(connect_failure(&refused), connect_failure(llm::KEY_REFUSED), "a refused key keeps its short words");
    }

    /// The line goes to the report, once, and not while the app is taking itself off this PC.
    #[test]
    fn the_line_a_server_said_goes_to_the_report_and_nothing_is_written_while_the_app_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let text = llm::with_line(llm::SERVER_GONE, "GPU fault");
        keep_server_line(dir.path(), &Leaving::Going, &text);
        keep_server_line(dir.path(), &Leaving::Gone("done".into()), &text);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing written while leaving");
        keep_server_line(dir.path(), &Leaving::No, llm::SERVER_GONE);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "no line, nothing to keep");
        keep_server_line(dir.path(), &Leaving::No, &text);
        let report = std::fs::read_to_string(crate::crash::write_report(dir.path(), &Settings::default(), "RTX").unwrap()).unwrap();
        assert!(report.contains("--- the last engine error ---\n") && report.contains("  GPU fault\n"), "{report}");
    }

    #[test]
    fn a_new_chat_has_a_line_of_its_own_and_it_stays() {
        for last in 0..GREETINGS.len() {
            for seed in 0..40 {
                let next = next_greeting(last, seed);
                assert!(next < GREETINGS.len());
                assert_ne!(next, last, "a new chat looks new");
            }
        }
        // In English whatever the language: nothing to translate.
        assert!(GREETINGS.iter().chain([&WELCOME]).all(|line| line.is_ascii() && line.len() <= 40));
    }

    #[test]
    fn the_engine_says_what_it_is_once() {
        assert_eq!(engine_words(Some("straydog 0.3.1\nbuilt with..."), None), "Straydog  0.3.1");
        assert_eq!(engine_words(Some("straydog 0.3.1"), Some(0)), "Straydog  0.3.1  (1970-01-01)");
        assert_eq!(engine_words(Some("0.3.1 (abc)"), None), "Straydog  0.3.1 (abc)");
        assert_eq!(engine_words(Some("  \n"), Some(0)), t("Straydog: no version given"));
        assert_eq!(engine_words(None, None), t("Straydog: no version given"));
    }

    #[test]
    fn a_session_says_when_as_shortly_as_it_can() {
        assert_eq!(short_when("2026-11-02 20:21:33 +09:00", "2026-11-02"), "20:21");
        assert_eq!(short_when("2026-10-08 07:00:00 +09:00", "2026-11-02"), "10/8");
        assert_eq!(short_when("2025-10-08 07:00:00 +09:00", "2026-11-02"), "2025");
        assert_eq!(short_when("", "2026-11-02"), "");
    }

    #[test]
    fn a_build_says_which_commit_and_what_day() {
        assert_eq!(readable_build("056bbaaa1a with changes, built at 1793616313"), "056bbaaa1a (2026-11-02)");
        assert_eq!(readable_build("abc built at 0"), "abc (1970-01-01)");
        assert_eq!(readable_build("abc"), "abc");
        assert_eq!(readable_build("abc, built at 0"), "abc (1970-01-01)");
    }

    #[test]
    fn the_first_screen_offers_what_it_found() {
        assert_eq!(suggested_way(true, true, true, true), 0, "a model ready on this PC");
        assert_eq!(suggested_way(true, false, true, false), 2, "Codex, ready, before a long download");
        assert_eq!(suggested_way(false, false, false, true), 3);
        assert_eq!(suggested_way(true, false, false, false), 0, "what can be set up here");
        assert_eq!(suggested_way(false, false, false, false), 1, "no engine and nothing found: an address");
    }

    #[test]
    fn a_key_setting_follows_what_was_done_with_its_fields() {
        // A key pasted goes to the store, and the setting says only that.
        assert_eq!(key_setting("", false, "", "sk-secret", "Claude"), "credential:Claude");
        assert!(!key_setting("env:OLD", false, "", "sk-secret", "Claude").contains("sk-secret"));
        // Nothing typed: as it was.
        assert_eq!(key_setting("credential:Claude", false, "", "", "Claude"), "credential:Claude");
        assert_eq!(key_setting("C:\\keys\\a.txt", false, "", "", "x"), "C:\\keys\\a.txt", "a file set long ago keeps working");
        // A variable: what the switch says.
        assert_eq!(key_setting("", true, "MY_KEY", "", "x"), "env:MY_KEY");
        assert_eq!(key_setting("env:MY_KEY", true, "MY_KEY", "", "x"), "env:MY_KEY", "as a setting from before reads");
        assert_eq!(key_setting("env:MY_KEY", false, "MY_KEY", "", "x"), "", "turned off");
        assert_eq!(key_setting("credential:x", true, "", "", "x"), "");
    }

    #[test]
    fn the_settings_have_a_place_for_each_page_and_the_help_fits_its_list() {
        assert!(SETTINGS_PLACES.len() <= NAV_POOL);
        for (panel, icon, _) in SETTINGS_PLACES {
            assert!(is_settings_place(panel));
            assert!(crate::icon::exists(icon), "{icon}");
        }
        assert!(!is_settings_place(Panel::Chat) && !is_settings_place(Panel::Rename));
        let sections = help_sections();
        assert!(!sections.is_empty() && sections.len() <= NAV_POOL, "{}", sections.len());
        assert!(sections.iter().all(|(name, body)| !name.is_empty() && !body.trim().is_empty()));
        assert!(sections.iter().any(|(_, body)| body.contains("/rewind")), "the commands are in it");
    }

    #[test]
    fn a_key_given_to_the_agent_reaches_the_server_and_nothing_written() {
        const KEY: &str = "sk-never-written-0123456789";
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (told, seen) = channel();
        std::thread::spawn(move || serve_one(listener, told));
        let store = crate::keys::MemoryKeys::default();
        crate::keys::KeyStore::write(&store, "t", KEY).unwrap();
        let key = KeySource::of("credential:t").read("t", &store).unwrap();
        let client = Client::new(&format!("http://127.0.0.1:{port}/v1"), key).unwrap();
        let (events_to, events) = channel();
        let config = agent_config(&Settings::default(), dir.path(), dir.path().to_path_buf(), Mode::Default, dogmind_agent::mcp::Hub::start(&dir.path().join("mcp.json")), Policy::off());
        let handle = agent::start(config, events_to, Box::new(|| {}));
        handle.commands.send(Command::Provider(Arc::new(client), "m".into(), 100_000)).unwrap();
        handle.commands.send(Command::Send("hello".into())).unwrap();
        let _ = seen.recv_timeout(std::time::Duration::from_secs(30)).expect("the server was asked");
        loop {
            if let Event::TurnEnded { .. } = events.recv_timeout(std::time::Duration::from_secs(30)).expect("the turn ends") {
                break;
            }
        }
        let _ = handle.commands.send(Command::Quit);
        fn holds(folder: &Path, key: &str) -> bool {
            std::fs::read_dir(folder).into_iter().flatten().flatten().any(|entry| {
                let path = entry.path();
                if path.is_dir() { holds(&path, key) } else { std::fs::read(&path).is_ok_and(|bytes| bytes.windows(key.len()).any(|window| window == key.as_bytes())) }
            })
        }
        assert!(!holds(dir.path(), KEY), "not in a session, nor in anything else the agent wrote");
    }

    #[test]
    fn the_engine_starts_when_a_message_begins_and_does_not_keep_trying() {
        let second = std::time::Duration::from_secs(1);
        assert!(starts_the_engine(true, false, None), "waiting, and asked");
        assert!(!starts_the_engine(false, false, None), "already started, or not an engine that waits");
        assert!(!starts_the_engine(true, true, None), "the setup is working on its files");
        assert!(!starts_the_engine(true, false, Some(second)), "it has just failed: another key is no reason to load it again");
        assert!(starts_the_engine(true, false, Some(ENGINE_RETRY_AFTER + second)), "later, it may have been put right");
    }

    #[test]
    fn only_the_drafter_s_fetch_leaves_the_engine_running_and_startable() {
        assert!(setup_holds_off_the_engine(true, false), "the model's own setup and the vision file's: the files, and the card's probe");
        assert!(!setup_holds_off_the_engine(true, true), "the fetch reads no shard and takes nothing of the card");
        assert!(!setup_holds_off_the_engine(false, false), "no setup, nothing held off");
        // the engine that is asked for during the fetch starts, as it does with no setup
        assert!(starts_the_engine(true, setup_holds_off_the_engine(true, true), None));
        assert!(!starts_the_engine(true, setup_holds_off_the_engine(true, false), None));
    }

    #[test]
    fn the_engine_starts_again_for_the_drafter_only_when_no_work_is_under_way() {
        assert_eq!(drafter_restart(true, false), DrafterRestart::Now, "an engine that began, and nothing at work");
        assert_eq!(drafter_restart(true, true), DrafterRestart::WhenIdle, "an answer, a tool run or a session in the background is not cut short");
        assert_eq!(drafter_restart(false, false), DrafterRestart::Not, "no engine: its next start reads the settings");
        assert_eq!(drafter_restart(false, true), DrafterRestart::Not, "and work under way on a server of another kind needs no engine");
    }

    #[test]
    fn the_prompt_is_read_ahead_only_when_no_message_would_read_it_anyway() {
        assert!(warms_on_connect(true, true, true));
        assert!(!warms_on_connect(true, true, false), "the message's own request reads it, once");
        assert!(!warms_on_connect(false, true, true), "a server someone else runs");
        assert!(!warms_on_connect(true, false, true));
    }

    #[test]
    fn a_picture_reaches_the_server_as_a_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (told, seen) = channel();
        std::thread::spawn(move || serve_one(listener, told));
        let (events_to, events) = channel();
        let config = agent_config(&Settings::default(), dir.path(), dir.path().to_path_buf(), Mode::Default, dogmind_agent::mcp::Hub::start(&dir.path().join("mcp.json")), Policy::off());
        let handle = agent::start(config, events_to, Box::new(|| {}));
        let client = Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap();
        handle.commands.send(Command::Provider(Arc::new(client), "m".into(), 100_000)).unwrap();
        // As a paste comes: a bitmap's pixels, then a PNG the agent has looked over.
        let png = attach::png_of(2, 2, &[0x80, 0x40, 0x20, 0, 0x80, 0x40, 0x20, 0, 0x80, 0x40, 0x20, 0, 0x80, 0x40, 0x20, 0]).unwrap();
        let picture = dogmind_agent::images::attach_bytes("pasted.png", png).unwrap();
        handle.commands.send(send_command(Outgoing { text: "見て".into(), images: vec![Attached { path: PathBuf::from("pasted.png"), picture }], card: "見て".into() })).unwrap();
        let body = seen.recv_timeout(std::time::Duration::from_secs(30)).expect("the server was asked");
        let request: serde_json::Value = serde_json::from_str(&body).unwrap();
        let last = request["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "user");
        let parts = last["content"].as_array().expect("pictures and words, in parts");
        assert!(parts[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR"), "{}", parts[0]);
        assert!(parts.last().unwrap()["text"].as_str().unwrap().contains("見て"));
        loop {
            match events.recv_timeout(std::time::Duration::from_secs(30)).expect("the turn ends") {
                Event::TurnEnded { .. } => break,
                Event::Failed(words) => panic!("{words}"),
                _ => {}
            }
        }
        let _ = handle.commands.send(Command::Quit);
    }

    #[test]
    fn esc_goes_to_the_panel_first_then_the_card_then_the_work() {
        assert_eq!(escape_does(true, true, true, true), EscapeDoes::ClosePanel, "Settings open during an answer: it closes them and the answer goes on");
        assert_eq!(escape_does(true, false, false, true), EscapeDoes::Nothing);
        assert_eq!(escape_does(false, true, true, true), EscapeDoes::Reject);
        assert_eq!(escape_does(false, true, false, true), EscapeDoes::Stop);
        assert_eq!(escape_does(false, true, false, false), EscapeDoes::Nothing);
    }

    /// A question or a plan that waits is the person's to answer, as an approval is: the sidebar says Waiting and not Working, and the send button is not a stop button as if the model were writing.
    #[test]
    fn a_session_that_waits_for_the_person_says_waiting_and_is_not_stopped_by_its_send_button() {
        // the one on screen: waiting for an approval, an answer or a decision, with the turn still running
        assert_eq!(side_state(None, true, true, true, false, false), Some(SideState::Waiting));
        assert_eq!(side_state(None, true, false, true, false, false), Some(SideState::Working));
        assert_eq!(side_state(None, true, false, false, false, false), None);
        // one that is not on screen: parked at work, and whether it waits
        assert_eq!(side_state(Some((true, true)), false, false, false, false, false), Some(SideState::Waiting));
        assert_eq!(side_state(Some((false, true)), false, false, false, false, false), Some(SideState::Working));
        // a session that is not the one shown, and failed
        assert_eq!(side_state(None, false, true, true, false, true), Some(SideState::Error));
        assert_eq!(side_state(None, false, true, true, false, false), None, "what the window waits for is the shown session's");
        // something waits in the background, and no turn is at work: the list marks the session, the one on screen and one out of sight
        assert_eq!(side_state(None, true, false, false, true, false), Some(SideState::StandingBy));
        assert_eq!(side_state(Some((false, false)), false, false, false, false, false), Some(SideState::StandingBy));
        assert_eq!(side_state(None, true, false, true, true, false), Some(SideState::Working), "a turn at work is said first");
        assert_eq!(side_state(None, false, false, false, true, false), None, "what stands by on screen is the shown session's");
        assert_eq!(side_state(Some((true, false)), false, false, false, false, false), Some(SideState::Waiting), "a woken turn that asks says so even where it has ended its work");
        // the button
        assert!(is_stoppable(true, false, false), "the model is writing");
        assert!(!is_stoppable(true, true, false), "the model waits for an answer: the button sends it");
        assert!(is_stoppable(false, false, true), "the connection is being waited for");
        assert!(!is_stoppable(false, false, false));
    }

    #[test]
    fn a_card_does_not_take_the_keys_of_someone_writing() {
        use std::time::Duration;
        assert!(approval_takes_focus(false, false, None), "the focus is not in a field");
        assert!(approval_takes_focus(true, true, None), "an empty composer, not touched");
        assert!(approval_takes_focus(true, true, Some(Duration::from_secs(60))));
        assert!(!approval_takes_focus(true, false, Some(Duration::from_secs(60))), "a draft in it: the next key belongs to it");
        assert!(!approval_takes_focus(true, true, Some(Duration::from_millis(300))), "typing now, its first word about to come");
    }

    #[test]
    fn a_path_is_a_message_and_a_slash_word_is_a_command() {
        for command in ["/help", "/find 語句", "/model Claude", "/my-skill do it"] {
            assert!(reads_as_command(command), "{command}");
        }
        for message in ["", "hello", "/usr/bin/env は何？", "/ は何", "/日本語", "/etc/hosts"] {
            assert!(!reads_as_command(message), "{message}");
        }
    }

    #[test]
    fn a_command_the_sandbox_changed_is_shown_as_it_runs_with_the_line_as_written_after_it() {
        let runs = "(powershell -NoProfile -NonInteractive -Command \"[IO.File]::Delete('a.txt')\")";
        let changed = serde_json::json!({"command": runs, "written_as": "del a.txt", "purpose": "clear it", "cwd": "sub"}).to_string();
        assert_eq!(approval_body("execute_command", "s", &changed), format!("clear it\n\n{runs}\n\nAs written: del a.txt\n\nin sub"));
        let plain = serde_json::json!({"command": runs}).to_string();
        assert_eq!(approval_body("execute_command", "s", &plain), runs);
        let empty = serde_json::json!({"command": runs, "written_as": ""}).to_string();
        assert_eq!(approval_body("execute_command", "s", &empty), runs, "nothing is said of a line that was not changed");
        // (the words are in the other language too)
        assert!(crate::lang::has_japanese("As written: {0}"));
    }

    #[test]
    fn a_call_is_shown_as_it_reads_with_all_of_a_command() {
        let long = "x".repeat(9000);
        let command = serde_json::json!({"command": format!("python -c \"{long}\""), "purpose": "復号する", "risk_level": "safe", "cwd": "."}).to_string();
        let body = approval_body("execute_command", "python", &command);
        assert!(body.starts_with("復号する\n\npython -c"), "what it is for, then the command from its first word");
        assert!(body.contains(&long) && !body.contains("risk_level") && !body.contains('{'), "all of it, and no JSON");
        let there = serde_json::json!({"command": "dir", "cwd": "sub"}).to_string();
        assert_eq!(approval_body("execute_command", "dir", &there), "dir\n\nin sub");
        let file = serde_json::json!({"path": "a.txt", "content": "one\ntwo"}).to_string();
        assert_eq!(approval_body("create_file", "a.txt", &file), "a.txt\n\none\ntwo");
        let big = serde_json::json!({"path": "a.txt", "content": long}).to_string();
        assert!(approval_body("create_file", "a.txt", &big).ends_with("and 5000 more characters"));
        let patch = serde_json::json!({"edits": [{"path": "a.rs", "old_string": "x", "new_string": "y"}]}).to_string();
        assert_eq!(approval_body("apply_patch", "a.rs", &patch), "a.rs\n--- from\nx\n+++ to\ny");
        assert_eq!(approval_body("web_read", "u", "not json"), "u\nnot json");
        assert_eq!(approval_body("web_read", "u", r#"{"url": "https://a.example/b", "start_char": 0}"#), "https://a.example/b");
        assert_eq!(approval_body("web_search", "q", r#"{"queries": ["one", "two"], "max_results": 5}"#), "one\ntwo");
        assert_eq!(approval_body("mcp__x__y", "s", r#"{"purpose": "why", "a": "b", "n": 1}"#), "why\n\na: b\nn: 1");
    }

    #[test]
    fn a_paragraph_sets_its_own_hanging_indent() {
        let plain = para_format(11, 0, 0, 120);
        assert!(plain.mask & PFM_OFFSET != 0 && plain.offset == 0);
    }

    /// The steps a following view makes of `lines` arriving `every` seconds apart, in frames of `frame` seconds.
    fn glide(lines: usize, height: i32, every: f32, frame: f32) -> Vec<i32> {
        let (mut target, mut position, mut speed, mut carried, mut next) = (0, 0, 0.0, 0.0, 0.0);
        let mut steps = Vec::new();
        let mut time = 0.0;
        while time < lines as f32 * every + 3.0 {
            if time >= next && target < lines as i32 * height {
                target += height;
                next += every;
            }
            let (step, on, rest) = follow_step(target - position, speed, carried, frame, 60.0);
            (speed, carried) = (on, rest);
            position += step;
            steps.push(step);
            time += frame;
        }
        assert_eq!(position, target, "it arrives");
        steps
    }

    #[test]
    fn a_following_view_glides_and_arrives_at_any_frame_rate() {
        assert_eq!(follow_step(0, 50.0, 0.5, 0.016, 60.0), (0, 0.0, 0.0));
        assert_eq!(follow_step(-3, 50.0, 0.5, 0.016, 60.0), (0, 0.0, 0.0));
        for frame in [1.0 / 120.0, 1.0 / 60.0, 1.0 / 30.0, 0.05] {
            let steps = glide(40, 41, 0.35, frame);
            // A line of 41 pixels every 0.35 s is 117 pixels a second, or 12 in a tenth of a second: the view stays near that, as lines come and go.
            let tenth = ((0.1 / frame) as usize).max(1);
            let fastest = steps.windows(tenth).map(|some| some.iter().sum::<i32>()).max().unwrap();
            assert!(fastest <= 20, "{fastest} pixels in a tenth of a second, in frames of {frame}");
            assert!(steps.iter().all(|step| *step >= 0), "it never goes back");
        }
        // Once, and all of a page at the end of a burst: it closes on it within half a second.
        let (mut behind, mut speed, mut carried, mut frames) = (1200, 0.0, 0.0, 0);
        while behind > 0 && frames < 60 {
            let (step, on, rest) = follow_step(behind, speed, carried, 1.0 / 60.0, 60.0);
            (speed, carried) = (on, rest);
            behind -= step;
            frames += 1;
        }
        assert_eq!(behind, 0, "it gets there in a second");
        // A window held up: no leap past the end.
        assert!(follow_step(10, 500.0, 0.9, 0.05, 60.0).0 <= 10);
    }

    #[test]
    fn a_shape_is_drawn_when_any_of_it_can_show() {
        let icon = Shape::Icon { name: "x", area: rect(100, 100, 10, 10), size: 30, color: 0 };
        assert!(overlaps(&icon.bounds(), &rect(92, 92, 3, 3)), "an icon stands out of an area smaller than it");
        assert!(!overlaps(&icon.bounds(), &rect(0, 0, 50, 50)));
        let bubble = Shape::Bubble { area: rect(0, 0, 50, 50), radius: 8, tip: (40, 80), fill: 0, line: 0, thickness: 3.0 };
        assert!(overlaps(&bubble.bounds(), &rect(35, 78, 4, 4)), "a bubble's tail is outside its area");
    }

    #[test]
    fn a_kept_card_is_where_its_text_is() {
        let card = CardArea { left: 300, right: 480, top: 100, bottom: 140, top_open: false, bottom_open: false };
        let shape = CardShape::around(&card, POINT { x: 322, y: 110 }, 7);
        let moved = shape.at(POINT { x: 322, y: 60 });
        assert_eq!((moved.left, moved.right, moved.top, moved.bottom), (300, 480, 50, 90));
        assert!(!moved.top_open && !moved.bottom_open);
    }

    #[test]
    fn a_thumb_shows_how_much_is_in_view() {
        assert_eq!(thumb_height(1000, 500, 1000, 32), 500);
        assert_eq!(thumb_height(1000, 500, 100000, 32), 32);
        assert_eq!(thumb_height(20, 500, 100000, 32), 20);
        assert_eq!(thumb_offset(500, 0, 2000), 0);
        assert_eq!(thumb_offset(500, 1000, 2000), 250);
        assert_eq!(thumb_offset(500, 2000, 2000), 500);
        assert_eq!(thumb_offset(500, 9000, 2000), 500);
    }

    #[test]
    fn the_words_flow_evenly_from_tokens_that_come_in_bursts() {
        // A burst of 30 characters each second, an insertion each 30 ms.
        let (mut waiting, mut rate, mut carried) = (0usize, 0.0f32, 0.0f32);
        let (mut shown, mut most, mut stalled, mut longest) = (Vec::new(), 0usize, 0, 0);
        for frame in 0..400 {
            if frame % 33 == 0 {
                waiting += 30;
            }
            let (count, next, left) = reveal_step(waiting, rate, carried, 0.03);
            (rate, carried) = (next, left);
            waiting -= count;
            if frame > 66 {
                shown.push(count);
                most = most.max(count);
                stalled = if count == 0 { stalled + 1 } else { 0 };
                longest = longest.max(stalled);
            }
        }
        assert!(most <= 4, "no leap after a burst: {most} at once");
        assert!(longest * 30 <= 300, "no long stop between them: {} ms", longest * 30);
        // Over any second, what comes.
        assert!(waiting <= 30, "it keeps up: {waiting} behind");
    }

    #[test]
    fn a_flood_goes_in_a_piece_at_a_time_and_all_of_it_arrives() {
        assert_eq!(reveal_step(0, 0.0, 0.0, 0.03).0, 0);
        let (count, _, _) = reveal_step(100_000, 250_000.0, 0.0, 0.03);
        assert_eq!(count, 400);
        // What is left does not creep for ever.
        let (mut waiting, mut rate, mut carried, mut frames) = (3usize, 0.0f32, 0.0f32, 0);
        while waiting > 0 {
            let (count, next, left) = reveal_step(waiting, rate, carried, 0.03);
            (rate, carried) = (next, left);
            waiting -= count;
            frames += 1;
            assert!(frames < 60, "it stopped with {waiting} to go");
        }
    }

    #[test]
    fn writing_to_a_hidden_transcript_does_not_show_it() {
        unsafe {
            let class = wide("STATIC");
            let window = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_POPUP, 0, 0, 10, 10, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
            assert!(!window.is_null());
            let visible = || GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_VISIBLE != 0;
            // What this guards against.
            SendMessageW(window, WM_SETREDRAW, 1, 0);
            assert!(visible(), "WM_SETREDRAW no longer shows a hidden window");
            ShowWindow(window, SW_HIDE);
            redraw(window, false);
            redraw(window, true);
            assert!(!visible());
            ShowWindow(window, SW_SHOWNA);
            redraw(window, false);
            redraw(window, true);
            assert!(visible());
            DestroyWindow(window);
        }
    }

    fn card_at(left: i32, right: i32, top: i32, bottom: i32) -> impl Fn(i32, i32) -> bool {
        move |x, y| (left..=right).contains(&x) && (top..=bottom).contains(&y)
    }

    #[test]
    fn the_pointer_is_over_the_icon_drawn_under_it_and_tab_goes_down_the_page() {
        let hit = |left: i32, top: i32, key: OverlayKey| OverlayHit { area: RECT { left, top, right: left + 28, bottom: top + 28 }, key };
        let hits = vec![hit(60, 300, OverlayKey::Fork(1)), hit(30, 300, OverlayKey::Copy(1)), hit(30, 100, OverlayKey::Copy(0)), hit(500, 120, OverlayKey::Block(0))];
        assert_eq!(icon_under(&hits, 40, 310), Some(OverlayKey::Copy(1)));
        assert_eq!(icon_under(&hits, 70, 310), Some(OverlayKey::Fork(1)));
        assert_eq!(icon_under(&hits, 58, 310), None, "between the two icons of a row: the first is 28 wide, and the second begins at 60");
        assert_eq!(icon_under(&hits, 40, 128), None, "the icon's edge is not in it");
        assert_eq!(icon_under(&hits, 10, 10), None);
        // top to bottom, and left to right on a row
        assert_eq!(proxy_order(&hits), vec![OverlayKey::Copy(0), OverlayKey::Block(0), OverlayKey::Copy(1), OverlayKey::Fork(1)]);
        let many: Vec<OverlayHit> = (0..PROXY_POOL as i32 + 5).map(|at| hit(0, at * 30, OverlayKey::Run(at as usize))).collect();
        assert_eq!(proxy_order(&many).len(), PROXY_POOL, "no more than there are controls");
        assert_eq!(proxy_order(&[]), Vec::<OverlayKey>::new());
    }

    #[test]
    fn a_pictures_place_is_its_rooms_place_in_the_frame_and_moves_with_it_by_as_much() {
        let dpi = 168;
        let sizes = [(640, 480); 5];
        let tiles = attach::Tiles::of(&sizes, scale(dpi, 400), dpi);
        assert!(tiles.per_row >= 2 && tiles.rows >= 2, "five tiles are in more than one row");
        for index in 0..sizes.len() {
            let (x, y) = tiles.at(index);
            let here = picture_area(POINT { x: 30, y: 200 }, (x, y), (tiles.width, tiles.height), dpi);
            // (the card's padding is above the row's first line)
            assert_eq!((here.left, here.top), (30 + x, 200 + y + scale(dpi, 7)));
            assert_eq!((here.right - here.left, here.bottom - here.top), (tiles.width, tiles.height));
            // The same room a step further down the page, as a scroll or a line that came in above it makes it: the tile is a step further down, and no other way.
            let moved = picture_area(POINT { x: 30, y: 200 + 57 }, (x, y), (tiles.width, tiles.height), dpi);
            assert_eq!((moved.left - here.left, moved.top - here.top, moved.right - here.right, moved.bottom - here.bottom), (0, 57, 0, 57));
        }
        // the tiles of a row stand side by side with the gap between them, the rows one under another
        let (first, second, below) = (tiles.at(0), tiles.at(1), tiles.at(tiles.per_row as usize));
        assert_eq!((second.0 - first.0, second.1), (tiles.width + tiles.gap, first.1));
        assert_eq!((below.0, below.1 - first.1), (first.0, tiles.height + tiles.gap));
    }

    #[test]
    fn a_tile_is_in_view_when_any_of_it_is_and_not_at_the_edge() {
        let tile = |top: i32, bottom: i32| RECT { left: 0, top, right: 50, bottom };
        assert!(tile_is_in_view(&tile(100, 200), 500));
        assert!(tile_is_in_view(&tile(-90, 10), 500), "its foot is in view");
        assert!(tile_is_in_view(&tile(490, 600), 500), "its head is in view");
        assert!(!tile_is_in_view(&tile(-100, 0), 500), "it ends where the view begins");
        assert!(!tile_is_in_view(&tile(500, 600), 500), "it begins where the view ends");
    }

    #[test]
    fn the_pictures_of_a_message_are_named_by_their_files_and_take_their_turn_among_the_icons() {
        assert_eq!(file_name_of(Path::new("C:/work/shots/chart.png")), "chart.png");
        assert_eq!(file_name_of(Path::new("chart.png")), "chart.png");
        let tile = |left: i32, top: i32, key: OverlayKey| OverlayHit { area: RECT { left, top, right: left + 100, bottom: top + 80 }, key };
        let hits = vec![tile(120, 300, OverlayKey::Picture(0, 1)), tile(10, 300, OverlayKey::Picture(0, 0)), tile(10, 100, OverlayKey::Copy(0))];
        // (the pointer is on the tile it is over, and the hidden controls are in the page's order)
        assert_eq!(icon_under(&hits, 20, 310), Some(OverlayKey::Picture(0, 0)));
        assert_eq!(icon_under(&hits, 200, 370), Some(OverlayKey::Picture(0, 1)));
        assert_eq!(icon_under(&hits, 112, 310), None, "in the gap between two tiles");
        assert_eq!(proxy_order(&hits), vec![OverlayKey::Copy(0), OverlayKey::Picture(0, 0), OverlayKey::Picture(0, 1)]);
    }

    #[test]
    fn a_reader_is_told_what_a_control_of_a_card_is_by_its_words() {
        let part = |text: String, kind: ButtonKind, tip: &str| CardPart { text, kind, act: CardAct::SendOwn, width: 0, tip: tip.to_string() };
        assert_eq!(card_part_words(&part(choice_text(2, "", "notes.txt", ""), ButtonKind::Choice, "")), "2. notes.txt");
        assert_eq!(card_part_words(&part(choice_text(1, "Suggested", "notes.md", "the usual"), ButtonKind::Choice, "")), "1. notes.md: the usual");
        assert_eq!(card_part_words(&part(file_row_text(11, 0, 8, 0, "src/app.py"), ButtonKind::FileRow, "")), "src/app.py: +11 \u{2212}0");
        assert_eq!(card_part_words(&part("external-link".to_string(), ButtonKind::Icon, "Open in its app")), "Open in its app");
        assert_eq!(card_part_words(&part("Send".to_string(), ButtonKind::Primary, "")), "Send");
        assert_eq!(card_part_words(&part("check|Carry it out".to_string(), ButtonKind::Primary, "")), "Carry it out");
    }

    #[test]
    fn the_mode_chip_has_its_words_whatever_the_room_and_the_models_name_gives_way() {
        // (a wide window: all three have their words, the model's up to its most)
        assert_eq!(chip_widths(900, 96, 130, 260, 300, 60), (96, 130, 260));
        assert_eq!(chip_widths(900, 96, 130, 500, 300, 60), (96, 130, 300));
        // (a narrow one: the share of the room a chip was held to cut "Plan first"; now it does not, and the model's name takes what is left)
        assert_eq!(chip_widths(320, 96, 130, 260, 300, 60), (96, 130, 94));
        // (narrower: the model's name is down to its least, and the effort's words give way next; the mode's stay)
        assert_eq!(chip_widths(200, 96, 130, 260, 300, 60), (96, 44, 60));
        assert_eq!(chip_widths(200, 96, 130, 40, 300, 60), (96, 64, 40));
        assert_eq!(chip_widths(100, 96, 130, 260, 300, 60), (96, 0, 60));
    }

    #[test]
    fn a_card_that_goes_on_below_its_foot_has_grown_and_its_old_corners_are_its_own() {
        let (back, page) = (0x0034_3738, 0x001e_2021);
        // (the card's last row, and the page under it: as it was when the shape was taken)
        assert!(!card_has_grown(back, page, back));
        // (more of it under the foot: rows came after)
        assert!(card_has_grown(back, back, back));
        // (a pixel that is not the card's, text selected in it, or the card gone from there: not grown)
        assert!(!card_has_grown(page, back, back));
        assert!(!card_has_grown(page, page, back));
        let card = CardArea { left: 100, right: 300, top: 50, bottom: 120, top_open: false, bottom_open: false };
        let [left, right] = foot_corners(&card, 10);
        assert_eq!((left.left, left.top, left.right, left.bottom), (100, 111, 110, 121));
        assert_eq!((right.left, right.top, right.right, right.bottom), (291, 111, 301, 121));
    }

    #[test]
    fn a_seam_of_a_line_or_two_between_the_rows_of_a_card_does_not_end_it() {
        let client = RECT { left: 0, top: 0, right: 500, bottom: 300 };
        let text = POINT { x: 322, y: 110 };
        let seamed = |seam: std::ops::RangeInclusive<i32>| {
            let card = card_at(300, 480, 100, 160);
            move |x: i32, y: i32| card(x, y) && !seam.contains(&y)
        };
        // (one line, or two, at the foot of the last row: the card is all of it)
        for seam in [150..=150, 150..=151, 120..=121] {
            let found = find_card(client, text, 6, 3, seamed(seam.clone())).unwrap_or_else(|| panic!("{seam:?}"));
            assert_eq!((found.top, found.bottom), (100, 160), "{seam:?}");
        }
        // (three lines are a gap between two cards)
        let found = find_card(client, text, 6, 3, seamed(150..=152)).unwrap();
        assert_eq!(found.bottom, 149);
        // (at a larger scale of the window the seam between rows is larger, and the gap between cards with it: a seam of four lines, at 175%, is walked across and a gap of five is not)
        let reach = scale(168, 3);
        assert_eq!(reach, 5);
        let found = find_card(client, text, 6, reach, seamed(150..=153)).unwrap();
        assert_eq!(found.bottom, 160);
        let found = find_card(client, text, 6, reach, seamed(150..=154)).unwrap();
        assert_eq!(found.bottom, 149);
    }

    #[test]
    fn a_card_is_found_by_its_edges_and_its_ends_out_of_view_stay_square() {
        let client = RECT { left: 0, top: 0, right: 500, bottom: 300 };
        let text = POINT { x: 322, y: 110 };
        let whole = find_card(client, text, 6, 3, card_at(300, 480, 100, 140)).unwrap();
        assert_eq!((whole.left, whole.right, whole.top, whole.bottom), (300, 480, 100, 140));
        assert!(!whole.top_open && !whole.bottom_open);
        // Its text has scrolled above the view; the rest of it is still there.
        let above = find_card(client, POINT { x: 322, y: -30 }, 6, 3, card_at(300, 480, -40, 20)).unwrap();
        assert!(above.top_open && !above.bottom_open);
        assert_eq!((above.left, above.right, above.bottom), (300, 480, 20));
        let below = find_card(client, POINT { x: 322, y: 290 }, 6, 3, card_at(300, 480, 280, 400)).unwrap();
        assert!(!below.top_open && below.bottom_open);
    }

    /// The card says the model's purpose with the reason, in the body face, and the command alone is in the monospace one; what has no purpose has the whole of it in the control.
    #[test]
    fn the_purpose_of_a_call_is_apart_from_what_it_runs() {
        let run = serde_json::json!({"command": "curl -s https://example.com | wc -c", "purpose": "count the items on the second page"}).to_string();
        assert_eq!(approval_parts("execute_command", "s", &run), ("count the items on the second page".to_string(), "curl -s https://example.com | wc -c".to_string()));
        let bare = serde_json::json!({"command": "dir"}).to_string();
        assert_eq!(approval_parts("execute_command", "s", &bare), (String::new(), "dir".to_string()));
        let file = serde_json::json!({"path": "a.txt", "content": "one\ntwo", "purpose": "make the note"}).to_string();
        assert_eq!(approval_parts("create_file", "a.txt", &file), ("make the note".to_string(), "a.txt\n\none\ntwo".to_string()));
        // (a purpose that the body does not begin with, as in a call whose arguments are not JSON, is not lost and not said twice)
        assert_eq!(approval_parts("other", "summary", "not json"), (String::new(), "summary\nnot json".to_string()));
        assert!(command_line_twips() > ds::points::CODE * 20, "more than a line of the type, which is 20 twips to a point");
    }

    #[test]
    fn what_is_not_a_card_is_left_alone() {
        let client = RECT { left: 0, top: 0, right: 500, bottom: 300 };
        let text = POINT { x: 322, y: 110 };
        assert!(find_card(client, text, 6, 3, |_, _| false).is_none(), "another colour");
        assert!(find_card(client, POINT { x: 322, y: 400 }, 6, 3, |_, _| true).is_none(), "below the view");
        assert!(find_card(client, text, 6, 3, card_at(300, 480, 0, 299)).is_none(), "taller than the view: nothing to round");
        // Text on the row walked across does not stop it, but an edge that is not straight is no card.
        let lettered = |x: i32, y: i32| card_at(300, 480, 100, 140)(x, y) && !(x == 400 && y == 110);
        assert_eq!(find_card(client, text, 6, 3, lettered).map(|card| card.right), Some(480));
        let stepped = |x: i32, y: i32| card_at(300, 480, 100, 140)(x, y) && !(x > 460 && y > 120);
        assert!(find_card(client, text, 6, 3, stepped).is_none());
    }

    #[test]
    fn a_corner_is_cut_on_the_curve() {
        assert_eq!(corner_cover(6, 0, 0), 0.0);
        assert_eq!(corner_cover(6, 5, 5), 1.0);
        let edge = corner_cover(6, 1, 1);
        assert!(edge > 0.0 && edge < 1.0, "{edge}");
        assert_eq!(corner_cover(6, 1, 3), corner_cover(6, 3, 1));
        assert_eq!(mix(0x000000, 0x204060, 1.0), 0x204060);
        assert_eq!(mix(0x102030, 0x204060, 0.0), 0x102030);
        assert_eq!(mix(0x000000, 0x204060, 0.5), 0x102030);
    }

    #[test]
    fn tasks_and_changes_share_the_panel_and_stay_in_it() {
        assert_eq!(share_rows(10, 3, 3), (3, 3));
        assert_eq!(share_rows(10, 0, 40).0, 0);
        assert!(share_rows(10, 0, 40).1 >= 8, "the other's room is not kept empty");
        assert!(share_rows(10, 40, 1).0 >= 7);
        assert_eq!(share_rows(10, 40, 1).1, 1);
        for (room, tasks, changes) in [(10, 20, 20), (4, 20, 20), (2, 20, 20), (10, 40, 1), (10, 1, 40), (6, 6, 6)] {
            let (shown_tasks, shown_changes) = share_rows(room, tasks, changes);
            let more = usize::from(shown_tasks < tasks) + usize::from(shown_changes < changes);
            assert!(shown_tasks + shown_changes + more <= (room as usize).max(4), "{room} {tasks} {changes}: {shown_tasks} {shown_changes}");
            assert!(shown_tasks >= 1 && shown_changes >= 1);
        }
    }

    #[test]
    fn a_kept_place_has_to_be_a_window() {
        assert!(usable_place(&[10, 10, 1210, 810], 780, 520));
        assert!(!usable_place(&[10, 10, 100, 810], 780, 520));
        assert!(!usable_place(&[0, 0, 0, 0], 780, 520));
    }

    #[test]
    fn buttons_go_to_a_new_row_rather_than_off_the_window() {
        assert_eq!(wrap_row(&[100, 100, 100], 10, 1000), [(0, 0), (0, 110), (0, 220)]);
        assert_eq!(wrap_row(&[100, 100, 100], 10, 215), [(0, 0), (0, 110), (1, 0)]);
        assert_eq!(wrap_row(&[300], 10, 100), [(0, 0)], "one too wide stays on its row");
        assert!(wrap_row(&[], 10, 100).is_empty());
    }

    #[test]
    fn a_click_finds_its_link() {
        let links = [(10, 15, "a".to_string()), (20, 21, "b".to_string())];
        assert_eq!(link_at(&links, 9), None);
        assert_eq!(link_at(&links, 10), Some("a"));
        assert_eq!(link_at(&links, 14), Some("a"));
        assert_eq!(link_at(&links, 15), None);
        assert_eq!(link_at(&links, 20), Some("b"));
        assert_eq!(link_at(&links, 21), None);
        assert_eq!(link_at(&[], 3), None);
    }

    #[test]
    fn a_task_says_how_far_it_is_without_its_status_word() {
        assert_eq!(task_mark("done"), "✓");
        assert_eq!(task_mark("in_progress"), "▸");
        assert_eq!(task_mark("pending"), "○");
    }

    #[test]
    fn a_list_is_told_from_a_line_that_only_looks_like_one() {
        assert_eq!(list_marker("- [ ] a"), Some(("☐".to_string(), 6)));
        assert_eq!(list_marker("- [x] a"), Some(("☑".to_string(), 6)));
        assert_eq!(list_marker("- a"), Some(("•".to_string(), 2)));
        assert_eq!(list_marker("* a"), Some(("•".to_string(), 2)));
        assert_eq!(list_marker("12. a"), Some(("12.".to_string(), 4)));
        for not in ["-a", "**bold**", "3.14 is pi", "2026. was", "text", ""] {
            assert_eq!(list_marker(not), None, "{not}");
        }
    }

    /// A window of this class that is never shown, on this thread, with the key of a data folder on it if it is to say which it has.
    unsafe fn unshown_window(class: &str, key: Option<u64>) -> HWND {
        let (name, title, instance) = (wide(class), wide("a test's window"), GetModuleHandleW(std::ptr::null()));
        // (a class that is there from an earlier call of this is no matter: registering it again says no)
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: instance, lpszClassName: name.as_ptr(), ..std::mem::zeroed() });
        let window = CreateWindowExW(0, name.as_ptr(), title.as_ptr(), WS_OVERLAPPED, 0, 0, 100, 100, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
        assert!(!window.is_null());
        assert_eq!(IsWindowVisible(window), 0, "never shown");
        if let Some(key) = key {
            mark_window_of_folder(window, key);
        }
        window
    }

    #[test]
    fn a_data_folder_is_known_by_its_name_in_lower_case() {
        let (one, two) = (folder_key(Path::new("C:/scratch/one")), folder_key(Path::new("C:/scratch/two")));
        assert_ne!(one, two);
        assert_eq!(folder_key(Path::new("C:/Scratch/ONE")), one);
        // (the mutex it names is the one the app has always had, so that a copy of an older build and one of this still keep out of each other's folder)
        assert_eq!(mutex_name(folder_key(Path::new(""))), "Local\\DogmindApp-cbf29ce484222325");
        assert_ne!(mutex_name(one), mutex_name(two));
    }

    /// What is found of the app of a data folder is the window that says it has it, and no other of its class: a copy on another folder (a test's, a development build, the person's own
    /// on theirs) and a window that says nothing are not it, and where there is none, none is found and none is taken instead. Only windows of this test's own, never shown.
    #[test]
    fn the_window_that_comes_forward_is_the_one_of_the_app_that_has_the_data_folder() {
        let (one, two, three, four) = (folder_key(Path::new("C:/scratch/one")), folder_key(Path::new("C:/scratch/two")), folder_key(Path::new("C:/scratch/three")), folder_key(Path::new("C:/scratch/four")));
        // SAFETY: windows of this test's own, on this thread, made here and destroyed here; none is shown.
        unsafe {
            let (first, second, bare) = (unshown_window(WINDOW_CLASS, Some(one)), unshown_window(WINDOW_CLASS, Some(two)), unshown_window(WINDOW_CLASS, None));
            let of_another_class = unshown_window("DogmindTestOtherClass", Some(three));
            assert_eq!(window_of_folder(WINDOW_CLASS, one), Some(first));
            assert_eq!(window_of_folder(WINDOW_CLASS, two), Some(second));
            assert_eq!(window_of_folder(WINDOW_CLASS, three), None, "a window of another class is no app's");
            assert_eq!(window_of_folder(WINDOW_CLASS, four), None, "no app has it: no window is found, whichever others there are");
            assert_eq!(window_of_folder(WINDOW_CLASS, 0), None, "a window that says nothing is not the app of any folder");
            // (the same folder in other letters is the same folder)
            assert_eq!(window_of_folder(WINDOW_CLASS, folder_key(Path::new("c:/SCRATCH/Two"))), Some(second));
            for window in [first, second, bare, of_another_class] {
                assert_eq!(IsWindowVisible(window), 0, "nothing was shown");
                DestroyWindow(window);
            }
            assert_eq!(window_of_folder(WINDOW_CLASS, one), None, "a window that is gone is not found");
        }
    }

    #[test]
    fn what_a_call_says_and_what_it_brings_back_is_drawn_with_its_hidden_characters_as_marks() {
        let bad = char::from_u32(0x202e).unwrap();
        let summary = format!("echo safe {bad} dlrow evil");
        let Event::ToolStarted { summary: shown, .. } = hidden_shown(Event::ToolStarted { id: "a".into(), tool: "execute_command".into(), summary }) else { panic!("it is the same event") };
        assert_eq!(shown, "echo safe <U+202E> dlrow evil");
        let Event::ToolEnded { output, .. } = hidden_shown(Event::ToolEnded { id: "a".into(), ok: true, declined: false, elapsed: std::time::Duration::ZERO, output: format!("out {bad}x"), images: Vec::new() }) else { panic!("it is the same event") };
        assert_eq!(output, "out <U+202E>x");
        // what has nothing hidden in it, Japanese and an emoji included, is the very same text
        let Event::ToolStarted { summary: same, .. } = hidden_shown(Event::ToolStarted { id: "b".into(), tool: "read_file".into(), summary: "日本語.md 👍🏽".into() }) else { panic!("it is the same event") };
        assert_eq!(same, "日本語.md 👍🏽");
        assert_eq!(markdown::show_hidden(&format!("{bad}ls")).as_ref(), "<U+202E>ls", "an approval card's command reads in order");
    }
}

#[cfg(test)]
mod crypt_tests {
    use super::*;

    /// What the start makes of the data folder: no key file is conversations as they are; a key file is a question, unless this Windows user has the key; one that cannot be read is no way in.
    #[test]
    fn the_start_asks_for_the_key_exactly_when_the_folder_says_it_must() {
        let data = tempfile::tempdir().unwrap();
        let (crypt, policy) = Crypt::open(data.path());
        assert!(!crypt.is_locked() && crypt.keystore.is_none() && policy.writer().unwrap().is_none());

        // a key file that is not kept on this PC: locked, and nothing may be written until it is unlocked
        let (_, cipher, _) = Keystore::create(data.path(), "the passphrase of the test", false, None).unwrap();
        let (crypt, policy) = Crypt::open(data.path());
        assert!(crypt.is_locked() && crypt.sweep_due && crypt.cipher.is_none());
        assert_eq!(policy.writer().unwrap_err(), conversation_key::LOCKED);
        assert_eq!(crypt.keystore.as_ref().unwrap().unlock("the passphrase of the test").unwrap().fingerprint(), cipher.fingerprint());

        // one that is: the key is here at once, and nothing is asked
        std::fs::remove_file(conversation_key::path_in(data.path())).unwrap();
        let (mut keystore, cipher, _) = Keystore::create(data.path(), "the passphrase of the test", true, Some(Pending::Encrypt)).unwrap();
        let (crypt, policy) = Crypt::open(data.path());
        assert!(!crypt.is_locked() && crypt.remember && !crypt.sweep_due);
        assert_eq!(policy.writer().unwrap().unwrap().fingerprint(), cipher.fingerprint());
        assert_eq!(crypt.keystore.as_ref().unwrap().pending(), Some(Pending::Encrypt), "the change that was stopped is still to be finished");
        // the box cleared: the same key file asks again
        keystore.set_remembered(&cipher, false).unwrap();
        assert!(Crypt::open(data.path()).0.is_locked());

        // a file that cannot be read is locked, and says why
        std::fs::write(conversation_key::path_in(data.path()), "{not a key file").unwrap();
        let (crypt, policy) = Crypt::open(data.path());
        assert!(crypt.is_locked() && crypt.damaged.is_some() && crypt.keystore.is_none());
        assert!(policy.writer().is_err());
    }

    /// Remove takes the key file with the conversations it was for, and the page's words are in both languages.
    #[test]
    fn the_key_file_goes_with_what_remove_takes() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("settings.json"), r#"{"providers": []}"#).unwrap();
        std::fs::create_dir_all(data.path().join("sessions")).unwrap();
        Keystore::create(data.path(), "the passphrase of the test", false, None).unwrap();
        let mut taken: Vec<String> = settings::what_remove_takes(data.path(), None, None).unwrap().into_iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        taken.sort();
        assert_eq!(taken, ["conversation-key.json", "sessions", "settings.json"]);
        assert!(settings::what_remove_leaves(data.path(), None).is_empty());
    }
}
