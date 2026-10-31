//! The window. The app draws its own surface (`paint`) and keeps two system controls where they are worth having: RichEdit
//! for the transcript and the composer, so that IME input, selection and copy are the system's own. No GPU memory goes to
//! drawing. Everything slow happens on other threads, which post a wake message; the UI thread sleeps otherwise.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use dogmind_agent::agent::{self, Approval, Command, Decision, Event, Handle};
use dogmind_agent::anthropic::{self, Anthropic};
use dogmind_agent::codex::{self, Codex};
use dogmind_agent::llm::{Client, Health, Provider};
use dogmind_agent::message::Message;
use dogmind_agent::prompt::typed_text;
use dogmind_agent::rules;
use dogmind_agent::safety::Mode;
use dogmind_agent::session;
use dogmind_agent::tools::{FileChange, GitInfo};
use dogmind_agent::web::{WebSettings, WARNING_MARK};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::engine::{self, CatalogEntry, Engine, SetupLine};
use crate::icon;
use crate::paint::*;
use crate::rtf;
use crate::settings::{self, Kind, ProviderConfig, Settings};

const WM_WAKE: u32 = WM_APP + 1;
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
const CFM_BOLD: u32 = 0x0000_0001;
const CFM_ITALIC: u32 = 0x0000_0002;
const CFM_BACKCOLOR: u32 = 0x0400_0000;
const CFM_FACE: u32 = 0x2000_0000;
const CFM_COLOR: u32 = 0x4000_0000;
const CFM_SIZE: u32 = 0x8000_0000;
const CFE_BOLD: u32 = 1;
const CFE_ITALIC: u32 = 2;
const CFE_AUTOBACKCOLOR: u32 = 0x0400_0000;
const PFM_STARTINDENT: u32 = 0x0000_0001;
const PFM_TABSTOPS: u32 = 0x0000_0010;
const PFM_SPACEBEFORE: u32 = 0x0000_0040;
const PFM_SPACEAFTER: u32 = 0x0000_0080;
const PFM_LINESPACING: u32 = 0x0000_0100;
const TM_PLAINTEXT: u32 = 1;
const TM_MULTILEVELUNDO: u32 = 8;
const TM_MULTICODEPAGE: u32 = 32;
const ES_NOOLEDRAGDROP: u32 = 8;
const SS_NOPREFIX: u32 = 0x80;
const EM_SETEVENTMASK: u32 = WM_USER + 69;
const EM_POSFROMCHAR_RICH: u32 = 0x00D6;
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
const ID_TAB_PROVIDERS: isize = 25;
const ID_TAB_MODELS: isize = 26;
const ID_DRAFTER: isize = 27;
const ID_LIST: isize = 30;
const ID_SIDE: isize = 31;
const ID_HELP: isize = 35;
const ID_ABOUT: isize = 36;
const ID_TAB_GENERAL: isize = 37;
const ID_SUGGEST: isize = 38;
const ID_TOGGLE_THINKING: isize = 39;
const ID_TOGGLE_SAFE: isize = 40;
const ID_FIND_PREVIOUS: isize = 41;
const ID_FIND_NEXT: isize = 42;
const ID_FIND_CLOSE: isize = 43;
const ID_PANEL_TOGGLE: isize = 44;
const ID_TOGGLE_WEB: isize = 45;
const ID_TOGGLE_LANGUAGE: isize = 46;
const ID_EDIT_INSTRUCTIONS: isize = 47;
const ID_COPY_POOL: isize = 300; // ..300 + COPY_POOL
const COPY_POOL: usize = 24;
const TIMER_COPIED: usize = 3;
const TIMER_SEARCH: usize = 4;
const TIMER_CHANGES: usize = 5;
const TIMER_REFLOW: usize = 6;

// Menu ids.
const M_PROVIDERS: usize = 110;
const M_MODELS: usize = 111;
const M_RESTART: usize = 112;
const M_LOG: usize = 113;
const M_COMPACT: usize = 114;
const M_COPY: usize = 115;
const M_THINKING: usize = 116;
const M_SAFE: usize = 117;
const M_MODE: usize = 120; // ..=122
const M_EFFORT: usize = 130; // ..=135
const M_KIND: usize = 140; // ..=143
const M_OPEN_SESSION: usize = 150;
const M_RENAME_SESSION: usize = 151;
const M_DELETE_SESSION: usize = 152;
const M_NEW_THERE: usize = 153;
const M_PROVIDER: usize = 200; // and on

/// What `/` offers, in this order.
const COMMANDS: [(&str, &str); 32] = [
    ("/setup", "Go through the first setup again"),
    ("/help", "How to use this app"),
    ("/status", "Version, model, workspace and connection"),
    ("/new", "A new session in this workspace"),
    ("/clear", "The same as /new"),
    ("/resume", "Open another session of this workspace"),
    ("/find", "Find in this conversation: /find words"),
    ("/search", "Search all sessions: /search words"),
    ("/rename", "Name this session"),
    ("/compact", "Sum up the conversation to free the context"),
    ("/retry", "Ask the last request again"),
    ("/rewind", "Go back to an earlier turn: its files, the conversation, or both"),
    ("/copy", "Copy the last answer"),
    ("/diff", "What changed in the workspace, file by file"),
    ("/usage", "Tokens and speed of this session"),
    ("/init", "Have the model write this project's DOGMIND.md"),
    ("/model", "Change the provider: /model Claude"),
    ("/effort", "How hard to think: none low medium high xhigh default"),
    ("/mode", "When to ask: ask edits auto"),
    ("/permissions", "What may always run in this workspace"),
    ("/think", "Show or hide the thinking"),
    ("/tools", "Let sandboxed commands use tools like Node.js, with one Windows prompt"),
    ("/instructions", "How the agent should talk and work, for every new session"),
    ("/dashboard", "Open Straydog's dashboard in the browser"),
    ("/panel", "Show or hide the panel on the right"),
    ("/safe", "Allow safe commands without asking, or not"),
    ("/workspace", "Start a session in another folder"),
    ("/providers", "Settings: providers"),
    ("/models", "Settings: models on this PC"),
    ("/settings", "Settings: general"),
    ("/log", "Open the engine's log"),
    ("/quit", "Close the app"),
];
const HELP: &str = "Type what you want done, and Enter sends it. The model reads and changes files in this session's folder with your approval.

KEYS
Enter\tsend
Shift+Enter\ta new line
Esc\tstop the work, reject an approval, or go back
Ctrl+N\ta new session
Ctrl+,\tsettings
Ctrl+Shift+C\tcopy the last answer
Ctrl+B\tshow or hide the panel on the right
Ctrl+F\tfind in this conversation; Enter and Shift+Enter go through what it finds
Ctrl+Shift+F\tsearch all sessions, what was said in them and their names
Y / A / N\tapprove, always, reject, while a Paw Note is open

IN THE COMPOSER
/\tcommands
@\ta file of this workspace

COMMANDS
{commands}

APPROVALS
Ask first asks before each change and command. Accept edits changes files without asking. Auto also runs commands, but never what is high risk, outside the workspace or wants the network.
Always saves a rule for this workspace: edits, or a command by its first words. What deletes or runs code it is handed is saved to the letter. No rule reaches what is high risk, outside the workspace or wants the network. /permissions lists the rules.

GOING BACK
/rewind goes back to the beginning of an earlier turn, in its files, in the conversation, or both. Only the agent's own edits come back: not what a command changed, and not what you changed. What a rewind overwrites is saved first, as a turn you can go back to.

SEVERAL SESSIONS
A session goes on working when you open another. The sidebar says which are working, and which wait for your approval. With Straydog on this PC they take turns on the one engine, and each turn may read its conversation again.

THE WEB
The model can search the web and read pages; a query goes to a search index, DuckDuckGo or Bing, or Brave with a key in Settings. What comes back is marked for the model as data, and text a reader would not see is removed. A page can still try to talk a model into something, so once web content is in a conversation, every command asks, whatever the mode and the rules, and an address that neither you, a search nor a page gave asks too.

COMMANDS RUN IN A SANDBOX
A command can write only in the session's folder, and has no network unless you approve it for that command. The first command in a folder grants access to it, and may take a while on a large tree.

MODELS
Settings > Providers: Straydog on this PC, any OpenAI-compatible server or service, Codex through its CLI, or Claude with an API key. A key is never stored in the settings: they name a file or a variable that holds it.

HOW THE AGENT TALKS
/instructions holds what goes for every session, such as its tone. A DOGMIND.md from the git root down to the folder holds what goes for one project, and /init has the model write one. A session reads both when it starts, so a change reaches new sessions. The language of answers is in Settings > General.
";
const INIT_PROMPT: &str = "Look through this project and write a DOGMIND.md at its root for agents who work here: what the project is, how to build and test it, its layout and its conventions. Keep it short, and only what you have checked.";

const EFFORTS: [(Option<&str>, &str); 6] = [(None, "Default"), (Some("none"), "None"), (Some("low"), "Low"), (Some("medium"), "Medium"), (Some("high"), "High"), (Some("xhigh"), "Extra high")];
const MODES: [(Mode, &str); 3] = [(Mode::Default, "Ask first"), (Mode::AcceptEdits, "Accept edits"), (Mode::Auto, "Auto")];

/// What other threads have to tell the window, besides the agent's events.
enum Note {
    Status(u64, String),
    Connected { generation: u64, provider: Arc<dyn Provider>, model: String, window: u64, straydog: bool },
    ConnectFailed(u64, String),
    Catalog(Result<Vec<CatalogEntry>, String>),
    Sessions(Vec<session::Summary>),
    Workspace { path: PathBuf, files: Vec<String>, branch: Option<String> },
    /// For this workspace: there may be another by the time it comes.
    Changes(PathBuf, Result<Vec<FileChange>, String>),
    Found(u64, Vec<session::Hit>),
    Git(PathBuf, Option<GitInfo>),
    Setup(SetupLine),
    SetupDone(Result<(), String>),
}

/// A session at work that is not on screen: its agent goes on, and what it says waits here.
struct Parked {
    path: Option<PathBuf>,
    workspace: PathBuf,
    title: String,
    agent: Handle,
    events: Receiver<Event>,
    pending: Option<Approval>,
    /// What has streamed since the last whole message, which the session file holds.
    partial: String,
    tools_running: HashMap<String, (String, String)>,
    queued: Vec<String>,
    used: u64,
    window_tokens: u64,
    rate: Option<f64>,
    totals: (u64, u64, u64, u32),
    /// What it said that no file keeps, for when it is opened.
    notices: Vec<String>,
    todos: Vec<serde_json::Value>,
    web_seen: bool,
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
    /// The first run: how to reach a model.
    Setup,
    /// The first run: the details of a server, of Codex or of Claude.
    SetupDetails,
}

/// What the transcript holds, in order: a block's width is fixed when it goes in, so a new width means writing it all again.
enum Shown {
    Text(String, Style),
    /// `width`: none for the column's.
    Row { lines: Vec<String>, face: String, points: i32, color: u32, back: u32, width: Option<i32>, right: bool, padding: (i32, i32) },
    Answer(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Body,
    Strong,
    Heading,
    Code,
    InlineCode,
    Thinking,
    Tool,
    CopyRow,
    Output,
    Warn,
    Bad,
}

/// What the parent window paints itself, recomputed by `layout`.
enum Shape {
    Round { area: RECT, radius: i32, fill: Option<u32>, border: Option<u32> },
    Text { area: RECT, text: String, font: FontKind, color: u32, flags: u32 },
    Logo { x: i32, y: i32, size: i32 },
    Icon { name: &'static str, area: RECT, size: i32, color: u32 },
}

#[derive(Clone, Copy)]
enum FontKind {
    Body,
    Small,
    Strong,
    Title,
}

#[derive(Clone)]
enum SideRow {
    Workspace { path: PathBuf, name: String, collapsed: bool, count: usize },
    Session { path: PathBuf, title: String, when: String },
    Hint(String),
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
    brushes: [HBRUSH; 4], // BACKGROUND, SIDEBAR, SURFACE, RAISED
    shapes: Vec<Shape>,
    // The sidebar.
    new_session: HWND,
    settings_button: HWND,
    side: HWND,
    side_rows: Vec<SideRow>,
    side_hover: Option<usize>,
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
    about_button: HWND,
    doc: HWND,
    copy_buttons: Vec<HWND>,
    /// Where each answer ends in the transcript, and its text.
    answers: Vec<(i32, String)>,
    /// Which answer each visible copy button stands for.
    copy_targets: Vec<Option<usize>>,
    transcript_area: RECT,
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
    folder_hit: RECT,
    straydog: bool,
    toggle_language: HWND,
    edit_instructions: HWND,
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
    branch: Option<String>,
    session_title: String,
    totals: (u64, u64, u64, u32), // prompt, cached, completion, requests
    side_target: Option<usize>,
    rename_target: Option<PathBuf>,
    rename_seed: Option<String>,
    resume_paths: Vec<PathBuf>,
    parked: Vec<Parked>,
    /// What a session opened beside a working one will connect with.
    provider: Option<(Arc<dyn Provider>, String, u64)>,
    /// What sessions said after they stopped being on screen, until they are opened.
    held_notices: HashMap<PathBuf, Vec<String>>,
    /// The session's turns, the rows of the rewind panel, and the rows of the rules panel.
    turns: Vec<dogmind_agent::checkpoint::Turn>,
    rewind_rows: Vec<u64>,
    shown_rules: Vec<rules::Rule>,
    changes: Vec<FileChange>,
    /// The file to show first when the changes come, chosen in the panel on the right.
    change_wanted: Option<usize>,
    // The panel on the right.
    right_width: i32,
    right_toggle: HWND,
    right_hits: Vec<(RECT, usize)>,
    todos: Vec<serde_json::Value>,
    git: Option<GitInfo>,
    /// Web content is in this conversation, so commands ask.
    web_seen: bool,
    toggle_web: HWND,
    context_auto: Option<u64>,
    /// Going through the first setup: a model folder or a provider leads on to the workspace.
    onboarding: bool,
    setup_kind: Kind,
    sidebar_width: i32,
    diff_top: i32,
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
    fields: [HWND; 4],
    labels: [String; 4],
    buttons: [HWND; 5],
    kind_chip: HWND,
    drafter_chip: HWND,
    tab_providers: HWND,
    tab_models: HWND,
    tab_general: HWND,
    toggle_thinking: HWND,
    toggle_safe: HWND,
    panel_title: String,
    panel_log: String,
    progress: Option<f64>,
    kind: Kind,
    drafter: bool,
    // State.
    pending: Option<Approval>,
    tools_running: HashMap<String, (String, String)>,
    running: bool,
    has_messages: bool,
    queued: Vec<String>,
    workspace: PathBuf,
    answer: String,
    last_answer: String,
    pending_text: String,
    line_start: bool,
    in_code: bool,
    heading: bool,
    bold: bool,
    inline_code: bool,
    thinking: String,
    activity: String,
    dots: usize,
    connected: bool,
    connection: String,
    model: String,
    used: u64,
    window_tokens: u64,
    rate: Option<f64>,
    catalog: Vec<CatalogEntry>,
    start_folders: Vec<PathBuf>,
    setting_up: bool,
    /// Ends the setup that is running.
    setup_cancel: Arc<AtomicBool>,
    /// Its last word, for when the panel is opened again.
    setup_status: String,
    selected_provider: Option<usize>,
    list_procedure: isize,
    transcript_procedure: isize,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

/// The row of the panel on the right that opens the dashboard, among those that open a changed file.
const DASHBOARD_ROW: usize = usize::MAX;
const CODE_BACK: u32 = rgb(46, 45, 42);
const USER_BACK: u32 = rgb(58, 54, 49);

/// The default browser, for an address this app made.
unsafe fn open_in_browser(url: &str) {
    let (verb, target) = (wide("open"), wide(url));
    ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
}

fn agent_config(settings: &Settings, folder: &Path, workspace: PathBuf, mode: Mode) -> agent::Config {
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
    }
}

/// What the agents read the web with. A key that cannot be read leaves the free indexes, and /status says so.
fn web_settings(settings: &Settings) -> WebSettings {
    let holder = ProviderConfig { name: "Search key".into(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: settings.search_key.trim().to_string() };
    WebSettings { enabled: settings.web_tools, search_key: holder.key().ok().flatten().filter(|key| !key.is_empty()), allow_local: false }
}

unsafe fn control(parent: HWND, class: &str, text: &str, style: u32, ex: u32, id: isize) -> HWND {
    let (class, text) = (wide(class), wide(text));
    CreateWindowExW(ex, class.as_ptr(), text.as_ptr(), WS_CHILD | style, 0, 0, 0, 0, parent, id as HMENU, GetModuleHandleW(std::ptr::null()), std::ptr::null())
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

unsafe fn show(window: HWND, visible: bool) {
    ShowWindow(window, if visible { SW_SHOWNA } else { SW_HIDE });
}

unsafe fn place(window: HWND, x: i32, y: i32, width: i32, height: i32) {
    MoveWindow(window, x, y, width, height, 1);
}

fn char_format(color: u32, effects: u32, face: &str, points: i32, back: Option<u32>) -> CharFormat {
    let mut names = [0u16; 32];
    for (slot, unit) in names.iter_mut().zip(face.encode_utf16()) {
        *slot = unit;
    }
    CharFormat {
        size: std::mem::size_of::<CharFormat>() as u32,
        mask: CFM_COLOR | CFM_BOLD | CFM_ITALIC | CFM_FACE | CFM_SIZE | CFM_BACKCOLOR,
        effects: effects | if back.is_none() { CFE_AUTOBACKCOLOR } else { 0 },
        height: points * 20,
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

fn para_format(indent: i32, before: i32, after: i32) -> ParaFormat {
    // SAFETY: all zeros is a valid value of this plain structure.
    let mut format: ParaFormat = unsafe { std::mem::zeroed() };
    format.size = std::mem::size_of::<ParaFormat>() as u32;
    format.mask = PFM_STARTINDENT | PFM_SPACEBEFORE | PFM_SPACEAFTER | PFM_LINESPACING;
    format.start_indent = indent;
    format.space_before = before;
    format.space_after = after;
    format.line_spacing = 27; // twentieths of a line: 1.35
    format.line_spacing_rule = 5;
    format
}

unsafe fn rich_edit(parent: HWND, style: u32, background: u32, points: i32) -> HWND {
    let edit = control(parent, "RICHEDIT50W", "", style | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_NOOLEDRAGDROP, 0, 0);
    SendMessageW(edit, EM_SETBKGNDCOLOR, 0, background as LPARAM);
    SendMessageW(edit, EM_EXLIMITTEXT, 0, 256 << 20);
    let format = char_format(TEXT, 0, "Yu Gothic UI", points, None);
    SendMessageW(edit, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
    edit
}

impl App {
    unsafe fn dark_controls(&self) {
        let theme = wide("DarkMode_Explorer");
        for window in [self.transcript, self.composer, self.list, self.side, self.note, self.suggest, self.doc] {
            allow_dark(window);
            SetWindowTheme(window, theme.as_ptr(), std::ptr::null());
        }
    }

    /// Text goes in at the end of the transcript, and the view stays where the person has it. To insert is to move the caret
    /// there, and that scrolls: nothing is drawn until the view is back, or the page jumps and flickers with every token.
    unsafe fn at_end(&mut self, insert: impl FnOnce(HWND)) {
        if !self.has_messages {
            self.has_messages = true;
            self.layout();
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
        if self.replaying {
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            insert(edit);
            return;
        }
        SendMessageW(edit, WM_SETREDRAW, 0, 0);
        SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
        insert(edit);
        SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(selection) as LPARAM);
        if at_bottom {
            SendMessageW(edit, WM_VSCROLL, SB_BOTTOM as WPARAM, 0);
        } else {
            SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of!(position) as LPARAM);
        }
        SendMessageW(edit, WM_SETREDRAW, 1, 0);
        // Far above the end, what is on screen has not changed: only the scrollbar has.
        if at_bottom || end_in_view {
            InvalidateRect(edit, std::ptr::null(), 0);
        } else {
            RedrawWindow(edit, std::ptr::null(), std::ptr::null_mut(), RDW_FRAME | RDW_INVALIDATE | RDW_NOERASE | RDW_NOCHILDREN | RDW_VALIDATE);
        }
        if at_bottom && !self.answers.is_empty() {
            self.place_copy_buttons();
        }
    }

    unsafe fn append(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if !self.replaying {
            match self.shown.last_mut() {
                Some(Shown::Text(held, kind)) if *kind == style => held.push_str(text),
                _ => self.shown.push(Shown::Text(text.to_string(), style)),
            }
        }
        let (ui, mono) = ("Yu Gothic UI", "Consolas");
        let format = match style {
            Style::Body => char_format(TEXT, 0, ui, 11, None),
            Style::Strong => char_format(TEXT, CFE_BOLD, ui, 11, None),
            Style::Heading => char_format(TEXT, CFE_BOLD, ui, 13, None),
            Style::Code => char_format(CODE, 0, mono, 10, None),
            Style::InlineCode => char_format(ACCENT_HOVER, 0, mono, 10, None),
            Style::Thinking => char_format(FAINT, CFE_ITALIC, ui, 10, None),
            Style::Tool => char_format(MUTED, 0, ui, 9, None),
            Style::CopyRow => char_format(BACKGROUND, 0, ui, 13, None),
            Style::Output => char_format(FAINT, 0, mono, 9, None),
            Style::Warn => char_format(WARN, 0, ui, 10, None),
            Style::Bad => char_format(BAD, 0, ui, 10, None),
        };
        let paragraph = match style {
            Style::Code | Style::Output => para_format(240, 0, 0),
            Style::Tool => para_format(0, 60, 40),
            Style::Heading => para_format(0, 200, 60),
            _ => para_format(0, 0, 80),
        };
        let text = wide(text);
        self.at_end(|edit| {
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            SendMessageW(edit, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
            SendMessageW(edit, EM_REPLACESEL, 0, text.as_ptr() as LPARAM);
        });
    }

    /// One row of a shaded block.
    unsafe fn append_row(&mut self, row: &rtf::Row) {
        if !self.replaying {
            self.shown.push(Shown::Row { lines: row.lines.iter().map(|line| line.to_string()).collect(), face: row.face.to_string(), points: row.points, color: row.color, back: row.back, width: row.right.then_some(row.width), right: row.right, padding: row.padding });
        }
        let bytes: Vec<u8> = rtf::row(row).into_bytes().into_iter().chain(std::iter::once(0)).collect();
        let how = SetTextEx { flags: ST_SELECTION, code_page: 65001 };
        let mut color = char_format(row.color, 0, "", 0, None);
        color.mask = CFM_COLOR;
        self.at_end(|edit| {
            let mut range = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(range) as LPARAM);
            SendMessageW(edit, EM_SETTEXTEX, std::ptr::addr_of!(how) as WPARAM, bytes.as_ptr() as LPARAM);
            // The control keeps a row's shading from its RTF and not the colour of its text: that is set on what went in.
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            let mut end = CharRange { first: 0, last: 0 };
            SendMessageW(edit, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(end) as LPARAM);
            range.last = end.last;
            SendMessageW(edit, EM_EXSETSEL, 0, std::ptr::addr_of!(range) as LPARAM);
            SendMessageW(edit, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(color) as LPARAM);
        });
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
        self.replaying = true;
        SendMessageW(edit, WM_SETREDRAW, 0, 0);
        set_text(edit, "");
        self.answers.clear();
        for item in &held {
            match item {
                Shown::Text(text, style) => self.append(text, *style),
                Shown::Row { lines, face, points, color, back, width, right, padding } => {
                    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
                    self.append_row(&rtf::Row { lines: &lines, face, points: *points, color: *color, back: *back, width: width.unwrap_or(self.text_twips), right: *right, padding: *padding });
                }
                Shown::Answer(text) => self.answer_row(text),
            }
        }
        self.replaying = false;
        self.shown = held;
        GetScrollInfo(edit, SB_VERT, &mut bar);
        if at_bottom {
            SendMessageW(edit, WM_VSCROLL, SB_BOTTOM as WPARAM, 0);
        } else {
            let mut position = POINT { x: 0, y: (share * f64::from(bar.nMax)) as i32 };
            SendMessageW(edit, EM_SETSCROLLPOS, 0, std::ptr::addr_of_mut!(position) as LPARAM);
        }
        SendMessageW(edit, WM_SETREDRAW, 1, 0);
        InvalidateRect(edit, std::ptr::null(), 0);
        self.place_copy_buttons();
    }

    /// A line of a block of code or of a command's output.
    unsafe fn block_line(&mut self, line: &str, color: u32, points: i32) {
        let shown = if line.is_empty() { " " } else { line };
        self.append_row(&rtf::Row { lines: &[shown], face: "Consolas", points, color, back: CODE_BACK, width: self.text_twips, right: false, padding: (160, 0) });
    }

    /// What opens and closes a block: a little space, and what the code is in if it says.
    unsafe fn block_edge(&mut self, label: &str) {
        let shown = if label.is_empty() { " " } else { label };
        let points = if label.is_empty() { 4 } else { 8 };
        self.append_row(&rtf::Row { lines: &[shown], face: "Yu Gothic UI", points, color: MUTED, back: CODE_BACK, width: self.text_twips, right: false, padding: (160, 20) });
    }

    /// A line of code in an answer; a diff in its colours.
    unsafe fn code_line(&mut self, line: &str) {
        let diff = matches!(self.code_language.as_str(), "diff" | "patch");
        let color = match line.chars().next() {
            _ if diff && (line.starts_with("+++") || line.starts_with("---")) => MUTED,
            Some('+') if diff => GOOD,
            Some('-') if diff => BAD,
            Some('@') if diff => ACCENT_HOVER,
            _ => CODE,
        };
        self.block_line(line, color, 10);
    }

    /// The caret stays out of the composer's way: its hint lies over the first line, and what an IME is composing goes there.
    unsafe fn update_composer_cue(&self) {
        let empty = GetWindowTextLengthW(self.composer) == 0;
        show(self.composer_cue, self.panel == Panel::Chat && empty && !self.composing);
    }

    /// An answer as it streams, with its Markdown rendered: what a marker means is known once the line has begun, so a
    /// line's first characters wait until they can be told.
    unsafe fn append_answer(&mut self, text: &str) {
        self.pending_text.push_str(text);
        loop {
            let pending = std::mem::take(&mut self.pending_text);
            if pending.is_empty() {
                return;
            }
            if self.line_start {
                if !self.in_code && pending.starts_with('\n') {
                    self.pending_text = pending[1..].to_string();
                    continue;
                }
                let newline = pending.find('\n');
                let head = newline.map_or(pending.as_str(), |at| &pending[..at]);
                let trimmed = head.trim_start();
                let undecided = newline.is_none() && trimmed.chars().count() < 4 && trimmed.chars().all(|c| "`#-* ".contains(c));
                if undecided {
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
                        let label = self.code_language.clone();
                        self.block_edge(&label);
                    } else {
                        self.block_edge("");
                    }
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
                self.heading = false;
                if !self.in_code {
                    let indent = pending.len() - pending.trim_start().len();
                    if trimmed.starts_with('#') {
                        self.heading = true;
                        self.pending_text = pending[indent..].trim_start_matches('#').trim_start_matches(' ').to_string();
                        continue;
                    }
                    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                        self.append(&format!("{}•  ", " ".repeat(indent)), Style::Body);
                        self.pending_text = pending[indent + 2..].to_string();
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

    /// Text inside a line, up to its end: `**bold**` and `` `code` `` lose their marks. What follows the line goes back to wait.
    unsafe fn emit_inline(&mut self, text: &str) {
        let mut run = String::new();
        let mut chars = text.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            if c == '\n' {
                run.push('\n');
                self.flush_run(&mut run);
                self.line_start = true;
                self.bold = false;
                self.inline_code = false;
                self.heading = false;
                self.pending_text = text[at + 1..].to_string();
                return;
            }
            if c == '`' {
                self.flush_run(&mut run);
                self.inline_code = !self.inline_code;
                continue;
            }
            if c == '*' && !self.inline_code {
                match chars.peek() {
                    Some((_, '*')) => {
                        chars.next();
                        self.flush_run(&mut run);
                        self.bold = !self.bold;
                        continue;
                    }
                    None => {
                        self.flush_run(&mut run);
                        self.pending_text = "*".to_string();
                        return;
                    }
                    _ => {}
                }
            }
            run.push(c);
        }
        self.flush_run(&mut run);
    }

    unsafe fn flush_run(&mut self, run: &mut String) {
        let style = if self.heading { Style::Heading } else if self.inline_code { Style::InlineCode } else if self.bold { Style::Strong } else { Style::Body };
        let text = std::mem::take(run);
        self.append(&text, style);
    }

    /// The end of an answer: what was waiting for more goes out as it is.
    unsafe fn finish_answer(&mut self) {
        let rest = std::mem::take(&mut self.pending_text);
        if self.in_code {
            // An answer that ended inside its block, or with the fence still on its last line.
            if !rest.is_empty() && !rest.trim_start().starts_with("```") {
                self.code_line(rest.trim_end_matches(['\r', '\n']));
            }
            self.block_edge("");
            self.line_start = true;
        } else if !rest.is_empty() {
            self.line_start = false;
            self.append(&rest, Style::Body);
        }
        self.end_line();
        self.in_code = false;
        self.bold = false;
        self.inline_code = false;
        self.heading = false;
    }

    /// Below an answer, a row for its copy button.
    unsafe fn mark_answer(&mut self, text: &str) {
        self.end_line();
        self.shown.push(Shown::Answer(text.to_string()));
        self.answer_row(text);
    }

    unsafe fn answer_row(&mut self, text: &str) {
        let was = std::mem::replace(&mut self.replaying, true);
        let mut end = CharRange { first: 0, last: 0 };
        SendMessageW(self.transcript, EM_SETSEL, usize::MAX, -1);
        SendMessageW(self.transcript, EM_EXGETSEL, 0, std::ptr::addr_of_mut!(end) as LPARAM);
        self.answers.push((end.first, text.to_string()));
        self.append("\n", Style::CopyRow);
        self.replaying = was;
        self.line_start = true;
        self.place_copy_buttons();
    }

    /// Each answer on screen gets a button of the pool at its row, wherever the transcript has scrolled to.
    unsafe fn place_copy_buttons(&mut self) {
        let dpi = self.dpi;
        let (size, area) = (scale(dpi, 28), self.transcript_area);
        let visible = self.panel == Panel::Chat && self.has_messages;
        let height = area.bottom - area.top;
        let mut used = 0;
        self.copy_targets.iter_mut().for_each(|target| *target = None);
        if visible {
            for at in 0..self.answers.len() {
                if used >= self.copy_buttons.len() {
                    break;
                }
                let mut point = POINT { x: 0, y: 0 };
                SendMessageW(self.transcript, EM_POSFROMCHAR_RICH, std::ptr::addr_of_mut!(point) as WPARAM, self.answers[at].0 as LPARAM);
                if point.y < -size || point.y > height - size {
                    continue;
                }
                let button = self.copy_buttons[used];
                SetWindowPos(button, HWND_TOP, area.left + point.x - scale(dpi, 2), area.top + point.y, size, size, SWP_NOACTIVATE);
                show(button, true);
                self.copy_targets[used] = Some(at);
                used += 1;
            }
        }
        for button in &self.copy_buttons[used..] {
            show(*button, false);
        }
    }

    unsafe fn copy_answer(&mut self, slot: usize) {
        let Some(at) = self.copy_targets.get(slot).copied().flatten() else { return };
        let text = self.answers[at].1.clone();
        self.copy_to_clipboard(&text);
        set_text(self.copy_buttons[slot], "check");
        SetTimer(self.window, TIMER_COPIED, 1400, None);
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
                self.append("This has text that reads like instructions to an AI. It went to the model marked as data, with word not to act on it.\n", Style::Warn);
            }
            "apply_patch" => self.show_diff(output, 40),
            "execute_command" => {
                let body = output.split_once("stdout:\n").map_or("", |(_, rest)| rest);
                let (out, rest) = body.split_once("\nstderr:\n").unwrap_or((body, ""));
                let err = rest.rsplit_once("\nexit_code:").map_or(rest, |(text, _)| text);
                let lines: Vec<&str> = out.lines().chain(err.lines()).filter(|line| !line.trim().is_empty()).collect();
                let from = lines.len().saturating_sub(6);
                if from > 0 {
                    self.block_line(&format!("… {from} lines above"), MUTED, 9);
                }
                for line in &lines[from..] {
                    let shown: String = line.chars().take(200).collect();
                    self.block_line(&shown, CODE, 9);
                }
                self.line_start = true;
            }
            _ => {}
        }
    }

    unsafe fn show_diff(&mut self, text: &str, limit: usize) {
        let lines: Vec<&str> = text.lines().filter(|line| !line.starts_with("--- applied_diff") && !line.starts_with("--- ") && !line.starts_with("+++ ")).collect();
        for line in lines.iter().take(limit) {
            let color = match line.chars().next() {
                Some('+') => GOOD,
                Some('-') => BAD,
                Some('@') => ACCENT_HOVER,
                _ => CODE,
            };
            self.block_line(line, color, 9);
        }
        if lines.len() > limit {
            self.block_line(&format!("… {} more lines", lines.len() - limit), MUTED, 9);
        }
        self.line_start = true;
    }

    /// What the person wrote: a card against the right edge, and what the model writes runs under it at full width.
    unsafe fn show_user(&mut self, text: &str) {
        self.end_line();
        let lines: Vec<&str> = text.trim().lines().collect();
        let device = GetDC(self.transcript);
        let previous = SelectObject(device, self.fonts.body);
        let widest = lines.iter().map(|line| {
            let units = wide(line);
            let mut size = SIZE { cx: 0, cy: 0 };
            GetTextExtentPoint32W(device, units.as_ptr(), units.len() as i32 - 1, &mut size);
            size.cx
        }).max().unwrap_or(0);
        SelectObject(device, previous);
        ReleaseDC(self.transcript, device);
        // The body font is drawn a size larger in the transcript than it was measured here.
        let padding = (200, 80);
        let wanted = i64::from(widest) * 1440 * 115 / 100 / i64::from(self.dpi) + i64::from(padding.0) * 2 + 120;
        let width = (wanted as i32).clamp(800, (self.text_twips * 4 / 5).max(800));
        self.append_row(&rtf::Row { lines: &lines, face: "Yu Gothic UI", points: 11, color: TEXT, back: USER_BACK, width, right: true, padding });
        self.append("\n", Style::Tool);
        self.line_start = true;
    }

    fn current_mode(&self) -> Mode {
        match self.settings.mode.as_str() {
            "accept_edits" => Mode::AcceptEdits,
            "auto" => Mode::Auto,
            _ => Mode::Default,
        }
    }

    /// The folder's files for `@`, and its branch: on another thread, as a large tree takes a while.
    unsafe fn look_at_workspace(&self) {
        let (poster, path) = (self.poster.clone(), self.workspace.clone());
        std::thread::spawn(move || {
            let files = dogmind_agent::tools::workspace_files(&path, 20000);
            let branch = dogmind_agent::tools::git_branch(&path);
            poster.send(Note::Workspace { path, files, branch });
        });
    }

    /// What is offered above the composer for the word being typed: commands after `/`, files after `@`.
    unsafe fn update_suggestions(&mut self) {
        let text = get_text(self.composer);
        let mut found = Vec::new();
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            for (name, about) in COMMANDS {
                if name.starts_with(&text) {
                    found.push(Suggestion { title: name.to_string(), detail: about.to_string(), insert: format!("{name} ") });
                }
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
        SetFocus(self.composer);
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
            "status" => {
                let straydog = engine::find(&self.settings);
                let engine_line = straydog.as_deref().and_then(engine::version).map_or("not found".to_string(), |version| format!("{version}  ({})", straydog.as_deref().map_or(String::new(), |path| path.display().to_string())));
                let provider = self.settings.active_provider().map_or(String::new(), |provider| format!("{}  ·  {}", provider.name, provider.kind.label()));
                let mode = self.current_mode();
                let mode = MODES.iter().find(|(known, _)| *known == mode).map_or("", |(_, name)| name);
                let effort = EFFORTS.iter().find(|(known, _)| *known == self.settings.effort.as_deref()).map_or("Default", |(_, name)| name);
                let lines = [
                    format!("Dogmind App  {}", env!("CARGO_PKG_VERSION")),
                    format!("Dogmind      {}", dogmind_agent::prompt::DOGMIND_COMMIT.trim().chars().take(10).collect::<String>()),
                    format!("Engine       {engine_line}"),
                    format!("Provider     {provider}"),
                    format!("Model        {}", self.model),
                    format!("Connection   {}", self.connection),
                    format!("Workspace    {}", self.workspace.display()),
                    format!("Branch       {}", self.branch.as_deref().unwrap_or("none")),
                    format!("Session      {}", self.current_session.as_ref().map_or("not saved yet".to_string(), |path| path.display().to_string())),
                    format!("Approvals    {mode}"),
                    format!("Web          {}", match (self.settings.web_tools, self.settings.search_key.trim().is_empty(), web_settings(&self.settings).search_key.is_some()) {
                        (false, _, _) => "off",
                        (true, true, _) => "on, with DuckDuckGo and Bing",
                        (true, false, true) => "on, with Brave",
                        (true, false, false) => "on, with DuckDuckGo and Bing: the search key could not be read",
                    }),
                    format!("Effort       {effort}"),
                    format!("Context      {} of {} tokens", self.used, self.window_tokens),
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
                    self.say("This session has no messages yet.");
                }
            }
            "compact" => self.on_menu(M_COMPACT),
            "rewind" => {
                if self.running {
                    self.say("Stop the work first.");
                } else {
                    let _ = self.agent.commands.send(Command::ListTurns);
                }
            }
            "permissions" => self.set_panel(Panel::Permissions),
            "retry" => {
                if !self.running && self.connected {
                    self.running = true;
                    self.update_buttons();
                    self.say("Asking again.");
                    let _ = self.agent.commands.send(Command::Retry);
                }
            }
            "copy" => {
                self.copy_to_clipboard(&self.last_answer.clone());
                self.say("Copied the last answer.");
            }
            "diff" => self.set_panel(Panel::Diff),
            "usage" => {
                let (prompt, cached, completion, requests) = self.totals;
                let rate = self.rate.map_or(String::new(), |rate| format!(", last answer at {rate:.1} tok/s"));
                self.say(&format!("{requests} requests: {prompt} prompt tokens, {cached} of them from the cache, {completion} written{rate}. The context holds {} of {}.", self.used, self.window_tokens));
            }
            "init" => {
                self.show_user("/init");
                if self.connected && !self.running {
                    self.dispatch(INIT_PROMPT.to_string());
                }
            }
            "model" => {
                let found = self.settings.providers.iter().position(|provider| provider.name.eq_ignore_ascii_case(argument));
                match found {
                    Some(at) => self.on_menu(M_PROVIDER + at),
                    None => self.on_chip(ID_MODEL_CHIP),
                }
            }
            "effort" => {
                let wanted = argument.to_lowercase();
                match EFFORTS.iter().position(|(known, name)| known.map_or(wanted == "default", |known| known == wanted) || name.eq_ignore_ascii_case(&wanted)) {
                    Some(at) => self.on_menu(M_EFFORT + at),
                    None => self.on_chip(ID_EFFORT_CHIP),
                }
            }
            "mode" => {
                let at = match argument.to_lowercase().as_str() {
                    "ask" | "default" => Some(0),
                    "edits" | "accept" => Some(1),
                    "auto" => Some(2),
                    _ => None,
                };
                match at {
                    Some(at) => self.on_menu(M_MODE + at),
                    None => self.on_chip(ID_MODE_CHIP),
                }
            }
            "panel" => self.toggle_right_panel(),
            "instructions" => self.set_panel(Panel::Instructions),
            "tools" => {
                let closed = dogmind_agent::sandbox::closed_tool_folders();
                if closed.is_empty() {
                    self.say("Every folder on PATH is open to sandboxed commands.");
                } else if let Some(command) = dogmind_agent::sandbox::open_folders_command(&closed) {
                    let names: Vec<String> = closed.iter().map(|folder| folder.display().to_string()).collect();
                    self.say(&format!("Windows will ask to let sandboxed commands read and run: {}. Nothing else changes.", names.join(", ")));
                    let (verb, program, arguments) = (wide("runas"), wide("cmd.exe"), wide(&format!("/c {command}")));
                    ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), program.as_ptr(), arguments.as_ptr(), std::ptr::null(), SW_HIDE);
                }
            }
            "dashboard" => match self.dashboard_url() {
                Some(url) => open_in_browser(&url),
                None => self.say("Straydog is not what this session is connected to, so it has no dashboard to open."),
            },
            "think" => {
                self.on_menu(M_THINKING);
                self.say(if self.settings.show_thinking { "The thinking is shown." } else { "The thinking is hidden." });
            }
            "safe" => {
                self.on_menu(M_SAFE);
                self.say(if self.settings.safe_commands { "Safe commands run without asking." } else { "Every command asks." });
            }
            "workspace" => self.set_panel(Panel::Start),
            "providers" => self.set_panel(Panel::Providers),
            "models" => self.set_panel(Panel::Models),
            "settings" => self.set_panel(Panel::General),
            "log" => self.on_menu(M_LOG),
            "quit" | "exit" => {
                DestroyWindow(self.window);
            }
            _ => return false,
        }
        true
    }

    unsafe fn update_chips(&self) {
        let provider = self.settings.active_provider().map_or(String::new(), |provider| provider.name.clone());
        let label = if self.model.is_empty() { provider } else { format!("{provider} · {}", self.model) };
        set_text(self.model_chip, &label);
        let mode = self.current_mode();
        let name = MODES.iter().find(|(known, _)| *known == mode).map_or("", |(_, name)| name);
        set_text(self.mode_chip, name);
        let effort = EFFORTS.iter().find(|(known, _)| *known == self.settings.effort.as_deref()).map_or("Default", |(_, name)| name);
        set_text(self.effort_chip, &format!("Effort: {effort}"));
    }

    unsafe fn update_status(&mut self) {
        self.update_chips();
        let title = if self.workspace == Path::new(".") { "Dogmind".to_string() } else { format!("Dogmind — {}", self.workspace.display()) };
        set_text(self.window, &title);
        InvalidateRect(self.window, &self.status_area, 0);
    }

    unsafe fn set_activity(&mut self, text: String) {
        if self.activity != text {
            self.activity = text;
            InvalidateRect(self.window, &self.activity_area, 0);
        }
    }

    unsafe fn save(&mut self) {
        if let Err(error) = self.settings.save(&self.folder.join("settings.json")) {
            self.append(&format!("The settings could not be saved: {error}\n"), Style::Bad);
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
        let sidebar = if bare { 0 } else { s(272) };
        self.sidebar_width = sidebar;
        let pad = s(14);
        place(self.new_session, pad, s(12), sidebar - 2 * pad, s(36));
        let search_top = s(56);
        if !bare {
            shapes.push(Shape::Round { area: rect(pad, search_top, sidebar - 2 * pad, s(32)), radius: s(10), fill: Some(SURFACE), border: Some(LINE) });
            shapes.push(Shape::Icon { name: "search", area: rect(pad + s(8), search_top, s(16), s(32)), size: s(14), color: FAINT });
        }
        place(self.side_search, pad + s(30), search_top + s(6), sidebar - 2 * pad - s(38), s(20));
        show(self.side_search, !bare);
        place(self.side, s(8), search_top + s(40), sidebar - s(16), height - search_top - s(40) - s(50));
        place(self.settings_button, s(8), height - s(44), sidebar - s(16) - s(72), s(34));
        place(self.help_button, sidebar - s(8) - s(68), height - s(41), s(30), s(30));
        place(self.about_button, sidebar - s(8) - s(34), height - s(41), s(30), s(30));
        show(self.new_session, !bare);
        show(self.side, !bare);
        show(self.settings_button, !bare);

        // The panel on the right, when there is room for it beside a conversation.
        let right = if chat && !bare && self.settings.right_panel && width - sidebar - s(280) >= s(560) { s(280) } else { 0 };
        self.right_width = right;
        self.right_hits.clear();
        if right > 0 {
            self.layout_right(&mut shapes, width - right + s(18), right - s(36), height);
        }
        show(self.right_toggle, chat && !bare);

        // The column the content keeps to, however wide the window.
        let main_left = sidebar;
        let main_width = width - main_left - right;
        let column = main_width.min(if bare { s(720) } else { s(860) }) - s(48);
        let left = main_left + (main_width - column) / 2;

        for window in [self.composer, self.send, self.model_chip, self.mode_chip, self.effort_chip] {
            show(window, chat);
        }
        show(self.help_button, !bare);
        show(self.about_button, !bare);
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
            let status_height = s(20);
            let status_top = height - status_height - s(10);
            self.status_area = rect(left, status_top, column, status_height);
            let card_height = s(118);
            let card_top = status_top - card_height - s(6);
            shapes.push(Shape::Round { area: rect(left, card_top, column, card_height), radius: s(16), fill: Some(SURFACE), border: Some(LINE) });
            let inner = s(14);
            place(self.composer, left + inner, card_top + s(12), column - 2 * inner, card_height - s(58));
            place(self.composer_cue, left + inner + s(2), card_top + s(13), column - 2 * inner - s(20), s(24));
            self.update_composer_cue();
            SetWindowPos(self.composer_cue, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let row_top = card_top + card_height - s(42);
            let chip = s(28);
            // They give way to the send button in a narrow window.
            let room = column - s(70);
            let model_width = s(230).min(room * 46 / 100);
            let mode_width = s(118).min(room * 25 / 100);
            let effort_width = s(132).min(room * 28 / 100);
            place(self.model_chip, left + s(8), row_top + s(3), model_width, chip);
            place(self.mode_chip, left + s(8) + model_width + s(2), row_top + s(3), mode_width, chip);
            place(self.effort_chip, left + s(8) + model_width + mode_width + s(4), row_top + s(3), effort_width, chip);
            let send = s(34);
            place(self.send, left + column - send - s(10), row_top, send, send);
            if !self.suggestions.is_empty() {
                let rows = self.suggestions.len().min(8) as i32;
                let list_height = rows * s(36) + s(4);
                let top = card_top - list_height - s(16);
                shapes.push(Shape::Round { area: rect(left, top, column, list_height + s(12)), radius: s(12), fill: Some(SURFACE), border: Some(LINE) });
                place(self.suggest, left + s(6), top + s(6), column - s(12), list_height);
                SetWindowPos(self.suggest, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            // The header: what this session is, and what can be done with it.
            let title = if self.session_title.is_empty() { "New session".to_string() } else { self.session_title.clone() };
            shapes.push(Shape::Text { area: rect(left, s(10), column - s(40), s(24)), text: title, font: FontKind::Strong, color: TEXT, flags: ONE_LINE });
            place(self.right_toggle, left + column - s(30), s(14), s(30), s(30));
            let name = self.workspace.file_name().map_or_else(|| self.workspace.display().to_string(), |name| name.to_string_lossy().into_owned());
            shapes.push(Shape::Icon { name: "folder", area: rect(left, s(32), s(14), s(20)), size: s(13), color: FAINT });
            self.folder_hit = rect(left, s(30), s(250), s(24));
            shapes.push(Shape::Text { area: rect(left + s(18), s(32), s(240), s(20)), text: name, font: FontKind::Small, color: FAINT, flags: ONE_LINE });
            if let Some(branch) = &self.branch {
                shapes.push(Shape::Icon { name: "git-branch", area: rect(left + s(260), s(32), s(14), s(20)), size: s(13), color: FAINT });
                shapes.push(Shape::Text { area: rect(left + s(278), s(32), s(300), s(20)), text: branch.clone(), font: FontKind::Small, color: FAINT, flags: ONE_LINE });
            }
            self.activity_area = rect(left, card_top - s(26), column, s(22));
            let mut bottom = card_top - s(30);
            if band {
                let lines = get_text(self.note).lines().count() as i32;
                let note_height = (lines * s(21) + s(6)).clamp(s(32), s(230));
                let buttons = s(44);
                let top = bottom - note_height - buttons - s(36);
                let area = rect(left, top, column, note_height + buttons + s(36));
                shapes.push(Shape::Round { area, radius: s(14), fill: Some(SURFACE), border: Some(ACCENT) });
                let title = self.pending.as_ref().map_or(String::new(), |pending| format!("Paw Note  ·  {} risk  ·  {}", pending.risk.name(), pending.tool));
                shapes.push(Shape::Text { area: rect(left + s(16), top + s(8), column - s(32), s(26)), text: title, font: FontKind::Strong, color: ACCENT, flags: ONE_LINE });
                place(self.note, left + s(16), top + s(36), column - s(32), note_height - s(4));
                let row = top + s(36) + note_height;
                let button = s(104);
                place(self.approve, left + s(14), row, button, s(32));
                place(self.always, left + s(14) + button + s(6), row, button, s(32));
                place(self.reject, left + s(14) + 2 * (button + s(6)), row, button, s(32));
                let after = left + s(14) + 3 * (button + s(6));
                shapes.push(Shape::Round { area: rect(after, row, left + column - s(14) - after, s(32)), radius: s(10), fill: Some(RAISED), border: None });
                place(self.feedback, after + s(10), row + s(7), left + column - s(34) - after, s(20));
                bottom = top - s(8);
            }
            let mut top = s(60);
            if finding {
                let bar = s(36);
                shapes.push(Shape::Round { area: rect(left, top, column, bar), radius: s(10), fill: Some(SURFACE), border: Some(LINE) });
                shapes.push(Shape::Icon { name: "search", area: rect(left + s(10), top, s(16), bar), size: s(14), color: FAINT });
                let button = s(26);
                let right = left + column - s(6);
                place(self.find_close, right - button, top + s(5), button, button);
                place(self.find_next, right - 2 * button - s(2), top + s(5), button, button);
                place(self.find_previous, right - 3 * button - s(4), top + s(5), button, button);
                let count = if self.find_matches.is_empty() {
                    if get_text(self.find_field).trim().is_empty() { String::new() } else { "None".to_string() }
                } else {
                    format!("{} / {}", self.find_at + 1, self.find_matches.len())
                };
                let count_right = right - 3 * button - s(10);
                shapes.push(Shape::Text { area: rect(count_right - s(70), top, s(70), bar), text: count, font: FontKind::Small, color: MUTED, flags: DT_RIGHT | ONE_LINE });
                place(self.find_field, left + s(34), top + s(8), count_right - s(70) - left - s(40), s(20));
                top += bar + s(6);
            }
            if self.has_messages {
                // As wide as the panel, with the text kept to the column: the scrollbar belongs at the panel's edge.
                self.transcript_area = rect(main_left, top, main_width, bottom - top);
                place(self.transcript, main_left, top, main_width, bottom - top);
                let twips = (i64::from(column) * 1440 / i64::from(self.dpi)) as i32;
                if twips != self.text_twips {
                    self.text_twips = twips;
                    // Once the window has stopped moving: it writes the whole conversation again.
                    SetTimer(self.window, TIMER_REFLOW, 150, None);
                }
                let area = RECT { left: left - main_left, top: 0, right: left - main_left + column, bottom: bottom - top };
                if self.transcript_rect != (area.left, area.right, area.bottom) {
                    self.transcript_rect = (area.left, area.right, area.bottom);
                    SendMessageW(self.transcript, EM_SETRECT, 0, std::ptr::addr_of!(area) as LPARAM);
                }
            } else {
                let name = self.workspace.file_name().map_or_else(|| self.workspace.display().to_string(), |name| name.to_string_lossy().into_owned());
                let middle = bottom / 2;
                shapes.push(Shape::Logo { x: left + column / 2 - s(24), y: middle - s(76), size: s(48) });
                shapes.push(Shape::Text { area: rect(left, middle - s(18), column, s(44)), text: "What shall we work on?".into(), font: FontKind::Title, color: TEXT, flags: DT_CENTER | ONE_LINE });
                shapes.push(Shape::Text { area: rect(left, middle + s(28), column, s(24)), text: format!("in {name}"), font: FontKind::Body, color: MUTED, flags: DT_CENTER | ONE_LINE });
            }
        }

        // The other panels.
        let panel = !chat;
        let has_list = match self.panel {
            Panel::Start => !self.start_folders.is_empty(),
            Panel::Diff => !self.list_rows.is_empty(),
            Panel::Providers | Panel::Models | Panel::Sessions | Panel::Setup | Panel::Rewind | Panel::Permissions => true,
            _ => false,
        };
        show(self.list, has_list);
        show(self.kind_chip, self.panel == Panel::Providers);
        show(self.drafter_chip, self.panel == Panel::Models);
        let settings_panel = !bare && matches!(self.panel, Panel::Providers | Panel::Models | Panel::General);
        show(self.tab_providers, settings_panel);
        show(self.tab_models, settings_panel);
        show(self.tab_general, settings_panel);
        show(self.toggle_thinking, self.panel == Panel::General);
        show(self.toggle_safe, self.panel == Panel::General);
        show(self.toggle_web, self.panel == Panel::General);
        show(self.toggle_language, self.panel == Panel::General);
        show(self.edit_instructions, self.panel == Panel::General);
        let field_count: usize = match self.panel {
            Panel::Models => 3,
            Panel::Providers if self.kind == Kind::Embedded => 3,
            Panel::Providers => 4,
            Panel::Rename | Panel::General => 1,
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
            Panel::Providers => 5,
            Panel::Start if self.start_folders.is_empty() => 2,
            Panel::General => 4,
            Panel::Start => 3,
            Panel::Rename | Panel::Sessions | Panel::Permissions | Panel::Instructions => 2,
            Panel::Rewind => 4,
            Panel::Help | Panel::About | Panel::Setup | Panel::Diff => 1,
            Panel::SetupDetails => 2,
            Panel::Chat => 0,
        };
        for (at, button) in self.buttons.iter().enumerate() {
            show(*button, at < button_count);
        }
        if panel {
            let mut top = s(28);
            if bare {
                shapes.push(Shape::Logo { x: left, y: top, size: s(44) });
                top += s(56);
            }
            shapes.push(Shape::Text { area: rect(left, top, column, s(44)), text: self.panel_title.clone(), font: FontKind::Title, color: TEXT, flags: ONE_LINE });
            top += s(52);
            if settings_panel {
                place(self.tab_providers, left, top, s(120), s(32));
                place(self.tab_models, left + s(126), top, s(170), s(32));
                place(self.tab_general, left + s(302), top, s(100), s(32));
                top += s(44);
            }
            if matches!(self.panel, Panel::Help | Panel::About | Panel::Instructions) {
                top = height - s(54);
            }
            let mut diff_top = top;
            if self.panel == Panel::General {
                shapes.push(Shape::Text { area: rect(left, top, s(200), s(38)), text: "Thinking".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.toggle_thinking, left + s(200), top + s(4), s(340), s(30));
                top += s(46);
                shapes.push(Shape::Text { area: rect(left, top, s(200), s(38)), text: "Safe commands".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.toggle_safe, left + s(200), top + s(4), s(340), s(30));
                top += s(46);
                shapes.push(Shape::Text { area: rect(left, top, s(200), s(38)), text: "Web tools".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.toggle_web, left + s(200), top + s(4), s(340), s(30));
                top += s(46);
                shapes.push(Shape::Text { area: rect(left, top, s(200), s(38)), text: "Language of answers".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.toggle_language, left + s(200), top + s(4), s(340), s(30));
                top += s(46);
                shapes.push(Shape::Text { area: rect(left, top, s(200), s(38)), text: "Your instructions".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.edit_instructions, left + s(200), top + s(4), s(340), s(30));
                top += s(46);
                shapes.push(Shape::Text { area: rect(left, top, s(200), s(38)), text: "Data folder".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                shapes.push(Shape::Text { area: rect(left + s(212), top, column - s(212), s(38)), text: self.folder.display().to_string(), font: FontKind::Small, color: MUTED, flags: ONE_LINE });
                top += s(56);
            }
            let row = s(38);
            let below = field_count as i32 * (row + s(8)) + if self.panel == Panel::Providers { row + s(8) } else { 0 } + if self.panel == Panel::Models { row + s(30) } else { 0 } + s(40) + s(70);
            if has_list {
                let shown_rows = self.list_rows.len().min(4) as i32;
                let list_height = if self.panel == Panel::Diff { shown_rows * s(44) + s(14) } else { (height - top - below - s(16)).max(s(90)) };
                shapes.push(Shape::Round { area: rect(left, top, column, list_height), radius: s(14), fill: Some(SURFACE), border: Some(LINE) });
                place(self.list, left + s(6), top + s(6), column - s(12), list_height - s(12));
                top += list_height + s(14);
            }
            if self.panel == Panel::Diff {
                diff_top = top;
                top = height - s(54);
            }
            self.diff_top = diff_top;
            let label_width = s(200);
            let field_left = left + label_width;
            let field_width = column - label_width;
            if self.panel == Panel::Providers {
                shapes.push(Shape::Text { area: rect(left, top, label_width, row), text: "Kind".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.kind_chip, field_left, top + s(4), s(300), s(30));
                top += row + s(8);
            }
            for at in 0..field_count {
                shapes.push(Shape::Text { area: rect(left, top, label_width, row), text: self.labels[at].clone(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                shapes.push(Shape::Round { area: rect(field_left, top, field_width, row), radius: s(10), fill: Some(SURFACE), border: Some(LINE) });
                place(self.fields[at], field_left + s(12), top + s(9), field_width - s(24), s(22));
                top += row + s(8);
            }
            if self.panel == Panel::Models {
                shapes.push(Shape::Text { area: rect(left, top, label_width, row), text: "MTP drafter".into(), font: FontKind::Body, color: MUTED, flags: ONE_LINE });
                place(self.drafter_chip, field_left, top + s(4), s(380), s(30));
                top += row + s(4);
                if let Some(done) = self.progress {
                    shapes.push(Shape::Round { area: rect(left, top + s(6), column, s(6)), radius: s(3), fill: Some(RAISED), border: None });
                    shapes.push(Shape::Round { area: rect(left, top + s(6), ((column as f64) * done.clamp(0.02, 1.0)) as i32, s(6)), radius: s(3), fill: Some(ACCENT), border: None });
                }
                top += s(22);
            }
            let widths = [s(160), s(160), s(100), s(100), s(90)];
            let mut x = left;
            for at in 0..button_count {
                place(self.buttons[at], x, top, widths[at], s(36));
                x += widths[at] + s(8);
            }
            top += s(44);
            shapes.push(Shape::Text { area: rect(left, top, column, (height - top - s(10)).max(s(20))), text: self.panel_log.clone(), font: FontKind::Small, color: MUTED, flags: DT_WORDBREAK });
        }
        // Help and About: a document, and the way back.
        let document = matches!(self.panel, Panel::Help | Panel::About | Panel::Diff | Panel::Instructions);
        show(self.doc, document);
        if document {
            let top = match self.panel {
                Panel::About => s(150),
                Panel::Instructions => s(120),
                Panel::Diff => self.diff_top,
                _ => s(84),
            };
            place(self.doc, main_left, top, main_width, height - top - s(66));
            let area = RECT { left: left - main_left, top: 0, right: left - main_left + column, bottom: height - top - s(66) };
            SendMessageW(self.doc, EM_SETRECT, 0, std::ptr::addr_of!(area) as LPARAM);
            if self.panel == Panel::Instructions {
                shapes.push(Shape::Text { area: rect(left, s(72), column, s(44)), text: "How the agent should talk and work, in your own words. Every new session reads this. What goes for one project belongs in its DOGMIND.md.".into(), font: FontKind::Small, color: MUTED, flags: DT_WORDBREAK });
            }
            if self.panel == Panel::About {
                shapes.push(Shape::Logo { x: left, y: s(82), size: s(48) });
                shapes.push(Shape::Text { area: rect(left + s(60), s(80), column, s(28)), text: "Dogmind App".into(), font: FontKind::Strong, color: TEXT, flags: ONE_LINE });
                shapes.push(Shape::Text { area: rect(left + s(60), s(106), column, s(24)), text: format!("Version {}", env!("CARGO_PKG_VERSION")), font: FontKind::Small, color: MUTED, flags: ONE_LINE });
            }
        }
        self.shapes = shapes;
        self.place_copy_buttons();
        InvalidateRect(self.window, std::ptr::null(), 0);
    }

    /// Tasks, what has changed and how full the context is, all painted by the window; a changed file can be clicked.
    unsafe fn layout_right(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, height: i32) {
        const MAX_TASKS: usize = 12;
        const MAX_CHANGES: usize = 14;
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let row = s(26);
        let mut y = s(18);
        let heading = |shapes: &mut Vec<Shape>, y: &mut i32, text: String| {
            shapes.push(Shape::Text { area: rect(x, *y, width, s(24)), text, font: FontKind::Strong, color: MUTED, flags: ONE_LINE });
            *y += s(30);
        };
        let plain = |shapes: &mut Vec<Shape>, y: &mut i32, text: String, color: u32| {
            shapes.push(Shape::Text { area: rect(x, *y, width, s(22)), text, font: FontKind::Small, color, flags: ONE_LINE });
            *y += s(24);
        };

        if let Some(git) = &self.git {
            heading(shapes, &mut y, "Git".to_string());
            shapes.push(Shape::Icon { name: "git-branch", area: rect(x, y, s(16), row), size: s(14), color: MUTED });
            let mut counts = String::new();
            if git.ahead > 0 {
                counts.push_str(&format!("↑{}", git.ahead));
            }
            if git.behind > 0 {
                counts.push_str(&format!(" ↓{}", git.behind));
            }
            let counts_width = if counts.is_empty() { 0 } else { s(64) };
            shapes.push(Shape::Text { area: rect(x + s(22), y, width - s(22) - counts_width, row), text: git.branch.clone(), font: FontKind::Small, color: TEXT, flags: ONE_LINE });
            if !counts.is_empty() {
                shapes.push(Shape::Text { area: rect(x + width - counts_width, y, counts_width, row), text: counts, font: FontKind::Small, color: ACCENT, flags: DT_RIGHT | ONE_LINE });
            }
            y += row;
            let mut state = Vec::new();
            if git.conflicted > 0 {
                state.push(format!("{} in conflict", git.conflicted));
            }
            if git.staged > 0 {
                state.push(format!("{} staged", git.staged));
            }
            if git.changed > 0 {
                state.push(format!("{} changed", git.changed));
            }
            if git.untracked > 0 {
                state.push(format!("{} new", git.untracked));
            }
            let clean = state.is_empty();
            plain(shapes, &mut y, if clean { "Clean".to_string() } else { state.join(", ") }, if git.conflicted > 0 { BAD } else if clean { FAINT } else { MUTED });
            if let Some(commit) = &git.last_commit {
                plain(shapes, &mut y, commit.clone(), FAINT);
            }
            y += s(18);
        }

        heading(shapes, &mut y, "Tasks".to_string());
        if self.todos.is_empty() {
            plain(shapes, &mut y, "None yet".to_string(), FAINT);
        }
        for item in self.todos.iter().take(MAX_TASKS) {
            let status = item["status"].as_str().unwrap_or("pending");
            let done = matches!(status, "done" | "completed");
            let doing = matches!(status, "doing" | "in_progress");
            if done {
                shapes.push(Shape::Icon { name: "check", area: rect(x, y, s(16), row), size: s(14), color: GOOD });
            } else {
                let dot = s(9);
                let area = rect(x + s(3), y + (row - dot) / 2, dot, dot);
                shapes.push(Shape::Round { area, radius: dot / 2, fill: doing.then_some(ACCENT), border: (!doing).then_some(FAINT) });
            }
            let color = if done { FAINT } else if doing { TEXT } else { MUTED };
            shapes.push(Shape::Text { area: rect(x + s(22), y, width - s(22), row), text: item["text"].as_str().unwrap_or("").to_string(), font: FontKind::Small, color, flags: ONE_LINE });
            y += row;
        }
        if self.todos.len() > MAX_TASKS {
            plain(shapes, &mut y, format!("and {} more", self.todos.len() - MAX_TASKS), FAINT);
        }
        y += s(18);

        let (added, removed) = self.changes.iter().fold((0, 0), |(added, removed), change| (added + change.added, removed + change.removed));
        heading(shapes, &mut y, if self.changes.is_empty() { "Changes".to_string() } else { format!("Changes  +{added} −{removed}") });
        if self.changes.is_empty() {
            plain(shapes, &mut y, "Nothing yet".to_string(), FAINT);
        }
        for (at, change) in self.changes.iter().enumerate().take(MAX_CHANGES) {
            let name = change.path.rsplit(['/', '\\']).next().unwrap_or(&change.path).to_string();
            let numbers = s(76);
            shapes.push(Shape::Text { area: rect(x, y, width - numbers, row), text: name, font: FontKind::Small, color: TEXT, flags: ONE_LINE });
            let half = numbers / 2;
            shapes.push(Shape::Text { area: rect(x + width - numbers, y, half, row), text: format!("+{}", change.added), font: FontKind::Small, color: GOOD, flags: DT_RIGHT | ONE_LINE });
            shapes.push(Shape::Text { area: rect(x + width - half, y, half, row), text: format!("−{}", change.removed), font: FontKind::Small, color: BAD, flags: DT_RIGHT | ONE_LINE });
            self.right_hits.push((rect(x - s(6), y, width + s(12), row), at));
            y += row;
        }
        if self.changes.len() > MAX_CHANGES {
            plain(shapes, &mut y, format!("and {} more: /diff", self.changes.len() - MAX_CHANGES), FAINT);
        }
        y += s(18);

        // What is left of the height goes to the context, from the bottom up, if it still fits.
        let dashboard = self.dashboard_url().is_some();
        let block = s(112) + if self.web_seen { s(44) } else { 0 } + if dashboard { s(34) } else { 0 };
        let top = (height - block - s(14)).max(y);
        if top + block <= height {
            let mut y = top;
            if self.web_seen {
                shapes.push(Shape::Icon { name: "info", area: rect(x, y, s(16), s(22)), size: s(14), color: WARN });
                shapes.push(Shape::Text { area: rect(x + s(22), y, width - s(22), s(22)), text: "Web content is here: commands ask".to_string(), font: FontKind::Small, color: WARN, flags: ONE_LINE });
                y += s(44);
            }
            heading(shapes, &mut y, "Context".to_string());
            let share = if self.window_tokens > 0 { (self.used as f64 / self.window_tokens as f64).clamp(0.0, 1.0) } else { 0.0 };
            let bar = s(6);
            shapes.push(Shape::Round { area: rect(x, y, width, bar), radius: bar / 2, fill: Some(RAISED), border: None });
            if share > 0.0 {
                let color = if share > 0.85 { BAD } else if share > 0.6 { WARN } else { ACCENT };
                shapes.push(Shape::Round { area: rect(x, y, ((width as f64 * share) as i32).max(bar), bar), radius: bar / 2, fill: Some(color), border: None });
            }
            y += s(12);
            plain(shapes, &mut y, format!("{}K of {}K tokens", self.used / 1000, self.window_tokens / 1000), MUTED);
            let (prompt, cached, _, requests) = self.totals;
            let reuse = if prompt > 0 { format!(", {}% from the cache", cached * 100 / prompt) } else { String::new() };
            plain(shapes, &mut y, format!("{requests} requests{reuse}"), FAINT);
            if dashboard {
                y += s(6);
                shapes.push(Shape::Icon { name: "external-link", area: rect(x, y, s(16), s(24)), size: s(14), color: ACCENT });
                shapes.push(Shape::Text { area: rect(x + s(22), y, width - s(22), s(24)), text: "Straydog dashboard".to_string(), font: FontKind::Small, color: ACCENT, flags: ONE_LINE });
                self.right_hits.push((rect(x - s(6), y, width + s(12), s(24)), DASHBOARD_ROW));
            }
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
                Err(_) => Err("This folder is no git repository, and the agent has changed nothing in this session.".to_string()),
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
        }
    }

    unsafe fn paint(&mut self, device: HDC, client: RECT) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        FillRect(device, &client, self.brushes[0]);
        let side = RECT { left: 0, top: 0, right: self.sidebar_width, bottom: client.bottom };
        FillRect(device, &side, self.brushes[1]);
        if self.right_width > 0 {
            let right = RECT { left: client.right - self.right_width, top: 0, right: client.right, bottom: client.bottom };
            FillRect(device, &right, self.brushes[1]);
        }
        for shape in &self.shapes {
            match shape {
                Shape::Round { area, radius, fill, border } => rounded(device, *area, *radius, *fill, *border),
                Shape::Text { area, text, font, color, flags } => draw_text(device, text, *area, self.font(*font), *color, *flags),
                Shape::Logo { x, y, size } => logo(device, *x, *y, *size),
                Shape::Icon { name, area, size, color } => icon::draw(device, name, *area, *size, *color),
            }
        }
        if self.panel == Panel::Chat {
            if !self.activity.is_empty() {
                let dots = ".".repeat(1 + self.dots % 3);
                let text = if self.activity.ends_with('…') { format!("{}{dots}", self.activity.trim_end_matches('…')) } else { self.activity.clone() };
                let area = self.activity_area;
                draw_text(device, &text, RECT { left: area.left + s(6), ..area }, self.fonts.small, MUTED, ONE_LINE);
            }
            self.paint_status(device);
        }
    }

    /// The line under the composer: the engine's state, how full the context is, how fast the last answer came.
    unsafe fn paint_status(&self, device: HDC) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let area = self.status_area;
        let mut x = area.left + s(8);
        let middle = (area.top + area.bottom) / 2;
        let dot = s(8);
        let color = if self.connected { GOOD } else { WARN };
        rounded(device, rect(x, middle - dot / 2, dot, dot), dot / 2, Some(color), None);
        x += dot + s(8);
        let connection = if self.running { "working" } else { self.connection.as_str() };
        draw_text(device, connection, RECT { left: x, right: (area.left + area.right) / 2, ..area }, self.fonts.small, MUTED, ONE_LINE);
        let mut right = area.right - s(8);
        if let Some(rate) = self.rate {
            let text = format!("{rate:.1} tok/s");
            let block = s(70);
            draw_text(device, &text, RECT { left: right - block, right, ..area }, self.fonts.small, MUTED, DT_RIGHT | ONE_LINE);
            right -= block + s(12);
        }
        if self.window_tokens > 0 {
            let text = format!("{}K / {}K", self.used / 1000, self.window_tokens / 1000);
            let block = s(92);
            draw_text(device, &text, RECT { left: right - block, right, ..area }, self.fonts.small, MUTED, DT_RIGHT | ONE_LINE);
            right -= block + s(8);
            let bar = s(70);
            let height = s(4);
            let top = middle - height / 2;
            rounded(device, rect(right - bar, top, bar, height), height / 2, Some(RAISED), None);
            let share = (self.used as f64 / self.window_tokens as f64).clamp(0.0, 1.0);
            if share > 0.0 {
                let color = if share > 0.85 { BAD } else if share > 0.6 { WARN } else { ACCENT };
                rounded(device, rect(right - bar, top, ((bar as f64 * share) as i32).max(height), height), height / 2, Some(color), None);
            }
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
        // Only its own panel writes in the document.
        SendMessageW(self.doc, EM_SETREADONLY, 1, 0);
        if self.panel == Panel::General && panel != Panel::General {
            let key = get_text(self.fields[0]).trim().to_string();
            if key != self.settings.search_key {
                self.settings.search_key = key;
                self.save();
                self.apply_web();
            }
        }
        self.panel = panel;
        self.panel_log.clear();
        self.list_rows.clear();
        self.progress = if self.setting_up { self.progress } else { None };
        let tab = |selected: bool| if selected { ButtonKind::TabSelected } else { ButtonKind::Tab };
        set_button_style(self.tab_providers, tab(panel == Panel::Providers), BACKGROUND);
        set_button_style(self.tab_models, tab(panel == Panel::Models), BACKGROUND);
        set_button_style(self.tab_general, tab(panel == Panel::General), BACKGROUND);
        match panel {
            Panel::Chat => {}
            Panel::Setup => {
                self.panel_title = "Welcome. How shall Dogmind reach a model?".into();
                self.onboarding = true;
                let straydog = engine::find(&self.settings);
                let codex = dogmind_agent::codex::find_executable();
                self.list_rows = vec![
                    ListRow { title: "On this PC, with Straydog".into(), detail: if straydog.is_some() { "Private, and it costs nothing to run. Needs a GPU, and a model of about 40 to 80 GB".into() } else { "straydog.exe is not beside this app".into() }, active: false },
                    ListRow { title: "Another machine, or a service".into(), detail: "Straydog on your network, or anything with an OpenAI-compatible API".into(), active: false },
                    ListRow { title: "Codex, with a ChatGPT plan".into(), detail: if codex.is_some() { "Uses the Codex CLI that is installed here".into() } else { "Needs the Codex CLI: it was not found".into() }, active: false },
                    ListRow { title: "Claude, with an API key".into(), detail: "Anthropic's API, billed to your key".into(), active: false },
                ];
                set_text(self.buttons[0], "Continue");
                self.set_button_kinds(&[ButtonKind::Primary]);
                self.panel_log = "You can add the others later, in Settings.".into();
            }
            Panel::SetupDetails => {
                match self.setup_kind {
                    Kind::Server => {
                        self.panel_title = "Where is the server?".into();
                        self.labels = ["URL".into(), "Model (empty: its first)".into(), "API key: a file, or env:NAME".into(), String::new()];
                        set_text(self.fields[0], "http://");
                        set_text(self.fields[1], "");
                        set_text(self.fields[2], "");
                        self.panel_log = "For example http://192.168.0.100:8080/v1 or https://api.example.com/v1. A key sent to an http:// server crosses the network as plain text.".into();
                    }
                    Kind::Anthropic => {
                        self.panel_title = "Where is your API key?".into();
                        self.labels = ["API key: a file, or env:NAME".into(), "Model (empty: the default)".into(), String::new(), String::new()];
                        set_text(self.fields[0], "env:ANTHROPIC_API_KEY");
                        set_text(self.fields[1], "");
                        self.panel_log = format!("The key itself is never stored in the settings, only where it is. The default model is {}. Requests are billed to your key.", anthropic::DEFAULT_MODEL);
                    }
                    _ => {
                        self.panel_title = "Codex".into();
                        self.panel_log = if dogmind_agent::codex::find_executable().is_some() {
                            "The Codex CLI is installed. If you have not signed in yet, run  codex login  in a terminal first.".into()
                        } else {
                            "The Codex CLI was not found. Install it (npm install -g @openai/codex), run  codex login , and start this app again; or go back and choose another way.".into()
                        };
                    }
                }
                set_text(self.buttons[0], "Continue");
                set_text(self.buttons[1], "Back");
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::Help => {
                self.panel_title = "Help".into();
                let commands: Vec<String> = COMMANDS.iter().map(|(name, about)| format!("{name}\t{about}")).collect();
                self.show_document(&HELP.replace("{commands}", &commands.join("\r\n")).replace('\n', "\r\n").replace("\r\r", "\r"));
                set_text(self.buttons[0], "Done");
                self.set_button_kinds(&[ButtonKind::Primary]);
            }
            Panel::About => {
                self.panel_title = "About Dogmind".into();
                self.show_document("A native Windows application running Dogmind's agent, with Straydog as its engine.\r\n\r\nThe licenses are in LICENSE.txt, beside this program.\r\n/status shows the versions and what is connected.");
                set_text(self.buttons[0], "Done");
                self.set_button_kinds(&[ButtonKind::Primary]);
            }
            Panel::General => {
                self.panel_title = "Settings".into();
                self.update_toggles();
                self.labels = ["Search key: a file, or env:NAME".into(), String::new(), String::new(), String::new()];
                set_text(self.fields[0], &self.settings.search_key);
                self.panel_log = "A Brave Search API key gives an index meant for programs. Without one, searches use DuckDuckGo and Bing, which may refuse a program, and then the model is told search is unavailable.".into();
                set_text(self.buttons[0], "Connect again");
                set_text(self.buttons[1], "Engine's log");
                set_text(self.buttons[2], "Done");
                set_text(self.buttons[3], "Dashboard");
                self.set_button_kinds(&[ButtonKind::Secondary, ButtonKind::Secondary, ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::Rename => {
                self.panel_title = "Name this session".into();
                self.labels = ["Name".into(), String::new(), String::new(), String::new()];
                let seed = self.rename_seed.take().unwrap_or_else(|| self.session_title.clone());
                set_text(self.fields[0], &seed);
                set_text(self.buttons[0], "Save");
                set_text(self.buttons[1], "Cancel");
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::Sessions => {
                self.panel_title = "Open a session".into();
                set_text(self.buttons[0], "Open");
                set_text(self.buttons[1], "Back");
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
                let here = self.workspace.to_string_lossy().to_lowercase();
                let found: Vec<session::Summary> = self.all_sessions.iter().filter(|summary| summary.workspace.to_string_lossy().to_lowercase() == here).cloned().collect();
                self.resume_paths = found.iter().map(|summary| summary.path.clone()).collect();
                self.list_rows = found.iter().map(|summary| ListRow { title: summary.title.clone(), detail: format!("{}  ·  {} messages", summary.created.get(..16).unwrap_or(""), summary.messages), active: self.current_session.as_ref() == Some(&summary.path) }).collect();
            }
            Panel::Rewind => {
                self.panel_title = "Go back to the beginning of".into();
                for (at, text) in ["Files and chat", "Files only", "Chat only", "Back"].iter().enumerate() {
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
                        0 => "no file changed since".to_string(),
                        1 => "1 file changed since".to_string(),
                        count => format!("{count} files changed since"),
                    };
                    let chat = if turn.message.is_some() { "" } else { "  ·  files only" };
                    self.list_rows.push(ListRow { title: turn.title.clone(), detail: format!("{}  ·  {files}{chat}", turn.at.get(..16).unwrap_or("")), active: false });
                }
                self.panel_log = "Only the agent's own edits come back: not what a command changed, and not what you changed. What this overwrites is saved first, as a turn you can go back to.".into();
            }
            Panel::Instructions => {
                self.panel_title = "Your instructions".into();
                let text = std::fs::read_to_string(self.folder.join("instructions.md")).unwrap_or_default();
                self.show_document(&text.replace('\n', "\r\n"));
                SendMessageW(self.doc, EM_SETREADONLY, 0, 0);
                set_text(self.buttons[0], "Save");
                set_text(self.buttons[1], "Cancel");
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
            }
            Panel::Diff => {
                self.panel_title = "Changes".into();
                set_text(self.buttons[0], "Done");
                self.set_button_kinds(&[ButtonKind::Primary]);
                self.show_document("Looking...");
                self.refresh_changes();
            }
            Panel::Permissions => {
                self.panel_title = "What may always run here".into();
                set_text(self.buttons[0], "Remove");
                set_text(self.buttons[1], "Done");
                self.set_button_kinds(&[ButtonKind::Secondary, ButtonKind::Primary]);
                let here = self.workspace.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
                self.shown_rules = rules::list(&self.folder.join("permissions.json")).into_iter()
                    .filter(|rule| rule.workspace.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase() == here).collect();
                self.list_rows = self.shown_rules.iter().map(|rule| ListRow { title: rule.label(), detail: if rule.exact { "This exact command".into() } else if rule.prefix.is_empty() { "Without asking".into() } else { "Any command that starts so".into() }, active: false }).collect();
                self.panel_log = if self.shown_rules.is_empty() {
                    "No rules yet: choose Always on a Paw Note to save one.".into()
                } else {
                    "No rule reaches what is high risk, outside the workspace, a place secrets are kept, or the network: those ask every time.".into()
                };
            }
            Panel::Models => {
                self.panel_title = "Settings".into();
                self.labels = ["Folder for models".into(), "VRAM in GiB, or auto".into(), "Context in tokens".into(), String::new()];
                let folder = self.settings.models_folder.clone().map_or(String::new(), |folder| folder.display().to_string());
                set_text(self.fields[0], &folder);
                self.context_auto = None;
                set_text(self.fields[1], &settings::show_vram(self.settings.vram));
                set_text(self.fields[2], &self.settings.context.map_or(String::new(), |context| context.to_string()));
                self.update_drafter_chip();
                set_text(self.buttons[1], "Browse...");
                set_text(self.buttons[2], if self.onboarding { "Back" } else { "Done" });
                self.show_setup_button();
                self.panel_log = if self.setting_up { self.setup_status.clone() } else { "A folder that already holds the model is verified, not downloaded again.".to_string() };
                if self.onboarding {
                    self.panel_title = "Which model?".into();
                }
                match engine::find(&self.settings) {
                    Some(straydog) => {
                        self.list_rows.push(ListRow { title: "Asking Straydog...".into(), detail: String::new(), active: false });
                        let poster = self.poster.clone();
                        std::thread::spawn(move || poster.send(Note::Catalog(engine::load_catalog(&straydog))));
                    }
                    None => self.panel_log = "straydog.exe was not found: put it beside dogmind-app.exe.".into(),
                }
            }
            Panel::Providers => {
                self.panel_title = "Settings".into();
                for (at, text) in ["Use this", "Save", "New", "Delete", "Done"].iter().enumerate() {
                    set_text(self.buttons[at], text);
                }
                self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary, ButtonKind::Secondary, ButtonKind::Secondary, ButtonKind::Secondary]);
                self.fill_providers();
            }
            Panel::Start => {
                self.panel_title = "Where shall we work?".into();
                self.start_folders = self.settings.workspaces.iter().filter(|folder| folder.is_dir()).cloned().collect();
                if self.start_folders.is_empty() {
                    set_text(self.buttons[0], "Choose a folder...");
                    set_text(self.buttons[1], "Back");
                    self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary]);
                } else {
                    set_text(self.buttons[0], "Start here");
                    set_text(self.buttons[1], "Choose a folder...");
                    set_text(self.buttons[2], "Back");
                    self.set_button_kinds(&[ButtonKind::Primary, ButtonKind::Secondary, ButtonKind::Secondary]);
                }
                self.list_rows = self.start_folders.iter().map(|folder| ListRow {
                    title: folder.file_name().map_or_else(|| folder.display().to_string(), |name| name.to_string_lossy().into_owned()),
                    detail: folder.display().to_string(),
                    active: false,
                }).collect();
                self.panel_log = "A session's commands can write in its folder, and nowhere else.".into();
            }
        }
        if panel != Panel::Providers {
            self.fill_list();
        }
        if (panel == Panel::Start && !self.start_folders.is_empty()) || (panel == Panel::Permissions && !self.shown_rules.is_empty()) {
            SendMessageW(self.list, LB_SETCURSEL, 0, 0);
        }
        self.layout();
        if panel == Panel::Chat {
            SetFocus(self.composer);
        }
    }

    /// Plain text in the panels' document, with one column for what a tab starts.
    unsafe fn show_document(&self, text: &str) {
        set_text(self.doc, "");
        let format = char_format(TEXT, 0, "Yu Gothic UI", 10, None);
        SendMessageW(self.doc, EM_SETCHARFORMAT, SCF_ALL as WPARAM, std::ptr::addr_of!(format) as LPARAM);
        set_text(self.doc, text);
        let mut paragraph = para_format(0, 0, 0);
        paragraph.mask |= PFM_TABSTOPS;
        paragraph.tab_count = 1;
        paragraph.tabs[0] = 1800;
        SendMessageW(self.doc, EM_SETSEL, 0, -1);
        SendMessageW(self.doc, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(paragraph) as LPARAM);
        SendMessageW(self.doc, EM_SETSEL, 0, 0);
    }

    /// One file's diff, coloured: runs of a kind of line go in together, so that a long diff takes no long.
    unsafe fn show_change(&self, at: usize) {
        const MAX_LINES: usize = 30000;
        let Some(change) = self.changes.get(at) else { return };
        let color = |line: &str| match line.chars().next() {
            _ if line.starts_with("+++") || line.starts_with("---") => MUTED,
            Some('+') => GOOD,
            Some('-') => BAD,
            Some('@') => ACCENT_HOVER,
            _ => CODE,
        };
        set_text(self.doc, "");
        let lines: Vec<&str> = change.diff.lines().collect();
        let mut at = 0;
        while at < lines.len().min(MAX_LINES) {
            let kind = color(lines[at]);
            let run = lines[at..].iter().take(MAX_LINES - at).take_while(|line| color(line) == kind).count();
            let text: String = lines[at..at + run].iter().flat_map(|line| [*line, "\r"]).collect();
            let format = char_format(kind, 0, "Consolas", 9, None);
            SendMessageW(self.doc, EM_SETSEL, usize::MAX, -1);
            SendMessageW(self.doc, EM_SETCHARFORMAT, SCF_SELECTION as WPARAM, std::ptr::addr_of!(format) as LPARAM);
            let text = wide(&text);
            SendMessageW(self.doc, EM_REPLACESEL, 0, text.as_ptr() as LPARAM);
            at += run;
        }
        let mut tight = para_format(0, 0, 0);
        tight.line_spacing = 20;
        SendMessageW(self.doc, EM_SETSEL, 0, -1);
        SendMessageW(self.doc, EM_SETPARAFORMAT, 0, std::ptr::addr_of!(tight) as LPARAM);
        SendMessageW(self.doc, EM_SETSEL, 0, 0);
        SendMessageW(self.doc, WM_VSCROLL, SB_TOP as WPARAM, 0);
    }

    /// The first button of the models panel: to start a setup, or to end the one that is running.
    unsafe fn show_setup_button(&self) {
        let cancelling = self.setup_cancel.load(Ordering::Relaxed);
        set_text(self.buttons[0], if cancelling && self.setting_up { "Cancelling..." } else if self.setting_up { "Cancel download" } else { "Download / verify" });
        EnableWindow(self.buttons[0], i32::from(!(cancelling && self.setting_up)));
        self.set_button_kinds(&[if self.setting_up { ButtonKind::Secondary } else { ButtonKind::Primary }, ButtonKind::Secondary, ButtonKind::Secondary]);
    }

    /// What a running setup says goes where it can be seen, whichever panel is open.
    unsafe fn show_setup_status(&mut self, text: String) {
        self.setup_status = text.clone();
        if matches!(self.panel, Panel::Models | Panel::Setup) {
            self.say_in_panel(&text);
        } else {
            self.connection = text;
            self.update_status();
        }
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
        set_text(self.toggle_thinking, if self.settings.show_thinking { "Show it in the transcript" } else { "Only a line while it thinks" });
        set_text(self.toggle_safe, if self.settings.safe_commands { "Run safe commands without asking" } else { "Ask before every command" });
        set_text(self.toggle_web, if self.settings.web_tools { "The model can search and read the web" } else { "Off" });
        set_text(self.toggle_language, match self.settings.language.as_str() {
            "ja" => "Japanese, always",
            "en" => "English, always",
            _ => "As you write: Japanese on this system",
        });
        set_text(self.edit_instructions, "Edit...");
    }

    /// The way chosen on the first screen.
    unsafe fn setup_choose(&mut self) {
        let Some(at) = self.list_selection() else {
            self.say_in_panel("Choose one of these.");
            return;
        };
        self.setup_kind = [Kind::Embedded, Kind::Server, Kind::Codex, Kind::Anthropic][at.min(3)];
        if self.setup_kind == Kind::Embedded {
            if engine::find(&self.settings).is_none() {
                self.say_in_panel("straydog.exe was not found: put it beside dogmind-app.exe, or choose another way.");
                return;
            }
            if self.settings.models_folder.is_none() {
                self.settings.models_folder = Some(self.folder.join("models"));
            }
            self.settings.use_provider(Kind::Embedded, "", "", "");
            self.save();
            self.set_panel(Panel::Models);
        } else {
            self.set_panel(Panel::SetupDetails);
        }
    }

    /// The details of a server, of Codex or of Claude are in: on to the workspace, connecting on the way.
    unsafe fn setup_details_done(&mut self) {
        let field = |at: usize| get_text(self.fields[at]).trim().to_string();
        let (url, model, key) = match self.setup_kind {
            Kind::Server => (field(0), field(1), field(2)),
            Kind::Anthropic => (String::new(), field(1), field(0)),
            _ => (String::new(), String::new(), String::new()),
        };
        let candidate = ProviderConfig { name: "x".into(), kind: self.setup_kind, base_url: url.clone(), model: model.clone(), api_key: key.clone() };
        if let Some(problem) = candidate.problem() {
            self.say_in_panel(&problem);
            return;
        }
        self.settings.use_provider(self.setup_kind, &url, &model, &key);
        self.save();
        self.finish_onboarding();
        self.connect();
    }

    /// The first setup ends on the main screen. A session needs a folder, so it gets one to begin in, which the header changes.
    unsafe fn finish_onboarding(&mut self) {
        let chosen = self.settings.workspace.clone().filter(|folder| folder.is_dir());
        let folder = chosen.unwrap_or_else(|| {
            let documents = std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Documents").join("Dogmind"));
            documents.unwrap_or_else(|| self.folder.join("workspace"))
        });
        let _ = std::fs::create_dir_all(&folder);
        self.start_session(folder);
    }

    unsafe fn set_button_kinds(&self, kinds: &[ButtonKind]) {
        for (button, kind) in self.buttons.iter().zip(kinds) {
            set_button_style(*button, *kind, BACKGROUND);
        }
    }

    unsafe fn fill_list(&self) {
        SendMessageW(self.list, LB_RESETCONTENT, 0, 0);
        for _ in &self.list_rows {
            SendMessageW(self.list, LB_ADDSTRING, 0, 0);
        }
        InvalidateRect(self.list, std::ptr::null(), 1);
    }

    unsafe fn update_drafter_chip(&self) {
        set_text(self.drafter_chip, if self.drafter { "Build it too (about 5 GB more to download)" } else { "Leave it out" });
    }

    /// A new session in `folder`, on the chat. A folder too wide for commands still reads and edits, and says so.
    unsafe fn start_session(&mut self, folder: PathBuf) {
        self.park_if_working();
        self.settings.use_workspace(folder.clone());
        self.settings.set_up = true;
        self.onboarding = false;
        self.save();
        self.current_session = None;
        self.workspace = folder.clone();
        self.session_title.clear();
        self.totals = (0, 0, 0, 0);
        let _ = self.agent.commands.send(Command::NewSession(folder.clone()));
        self.look_at_workspace();
        self.set_panel(Panel::Chat);
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
        if let Some(reason) = dogmind_agent::sandbox::refusal(&folder, home.as_deref()) {
            self.queued_notice(reason);
        }
    }

    /// An agent for what comes on screen next, connected as the others are.
    unsafe fn spawn_agent(&self) -> (Handle, Receiver<Event>) {
        let (sender, events) = channel();
        let waker = self.poster.clone();
        let handle = agent::start(
            agent_config(&self.settings, &self.folder, self.workspace.clone(), self.current_mode()),
            sender,
            Box::new(move || waker.wake()),
        );
        let _ = handle.commands.send(Command::SetSafeCommands(self.settings.safe_commands));
        if let Some((provider, model, window)) = &self.provider {
            let _ = handle.commands.send(Command::Provider(provider.clone(), model.clone(), *window));
        }
        (handle, events)
    }

    /// The session on screen is at work: it goes on out of sight, and the window gets a free agent.
    unsafe fn park_if_working(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }
        if !self.running {
            return;
        }
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
            partial: std::mem::take(&mut self.answer),
            tools_running: std::mem::take(&mut self.tools_running),
            queued: std::mem::take(&mut self.queued),
            used: self.used,
            window_tokens: self.window_tokens,
            rate: self.rate,
            totals: self.totals,
            notices: Vec::new(),
            todos: std::mem::take(&mut self.todos),
            web_seen: self.web_seen,
        });
        self.running = false;
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
        if let Some(at) = self.parked.iter().position(|parked| parked.path.as_ref() == Some(&path)) {
            self.take_up(at);
            self.find_opened();
            return;
        }
        self.totals = (0, 0, 0, 0);
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
        self.session_title = parked.title;
        self.tools_running = parked.tools_running;
        self.queued = parked.queued;
        self.used = parked.used;
        self.window_tokens = parked.window_tokens;
        self.rate = parked.rate;
        self.totals = parked.totals;
        self.running = true;
        self.todos = parked.todos;
        self.web_seen = parked.web_seen;
        self.changes.clear();
        self.refresh_changes();
        self.look_at_workspace();
        let messages = self.current_session.as_deref().map_or(Ok(Vec::new()), session::read).unwrap_or_default();
        self.set_panel(Panel::Chat);
        self.show_transcript(&messages);
        for notice in parked.notices {
            self.append(&format!("{notice}\n"), Style::Warn);
        }
        self.answer = parked.partial;
        if !self.answer.is_empty() {
            let shown = self.answer.clone();
            self.append_answer(&shown);
        }
        self.set_activity(if parked.pending.is_some() { String::new() } else { "Thinking…".to_string() });
        self.update_buttons();
        self.update_status();
        if let Some(approval) = parked.pending {
            self.on_event(Event::Approval(approval));
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
        match provider.kind {
            Kind::Embedded => Some(format!("http://127.0.0.1:{}/dashboard", self.settings.port)),
            Kind::Server => Some(format!("{}/dashboard", provider.base_url.trim_end_matches('/').trim_end_matches("/v1"))),
            _ => None,
        }
    }

    unsafe fn toggle_right_panel(&mut self) {
        self.settings.right_panel = !self.settings.right_panel;
        self.save();
        self.layout();
    }

    unsafe fn open_find(&mut self) {
        if self.panel != Panel::Chat {
            self.set_panel(Panel::Chat);
        }
        if !self.has_messages {
            self.say("There is nothing to find in yet.");
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
        SetFocus(self.composer);
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
            self.place_copy_buttons();
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
    unsafe fn refresh_sessions(&self) {
        let (poster, folder) = (self.poster.clone(), self.folder.join("sessions"));
        std::thread::spawn(move || poster.send(Note::Sessions(session::list(&folder))));
    }

    unsafe fn fill_side(&mut self) {
        let top = SendMessageW(self.side, LB_GETTOPINDEX, 0, 0);
        let mut rows = Vec::new();
        let groups = if self.side_hits.is_some() { Vec::new() } else { session::by_workspace(self.all_sessions.clone()) };
        if let Some(hits) = &self.side_hits {
            rows.extend(hits.iter().map(|hit| SideRow::Session { path: hit.path.clone(), title: if hit.title.is_empty() { "(no title)".to_string() } else { hit.title.clone() }, when: hit.snippet.clone() }));
            if hits.is_empty() {
                rows.push(SideRow::Hint("No session has that".into()));
            }
        }
        for (workspace, summaries) in groups {
            let name = workspace.file_name().map_or_else(|| workspace.display().to_string(), |name| name.to_string_lossy().into_owned());
            let collapsed = self.collapsed.contains(&workspace.to_string_lossy().to_lowercase());
            rows.push(SideRow::Workspace { path: workspace, name, collapsed, count: summaries.len() });
            if !collapsed {
                for summary in summaries {
                    let title = if summary.title.is_empty() { "(no title)".to_string() } else { summary.title };
                    rows.push(SideRow::Session { path: summary.path, title, when: summary.created.get(5..16).unwrap_or("").replace('-', "/") });
                }
            }
        }
        if rows.is_empty() {
            rows.push(SideRow::Hint("Your sessions will be here".into()));
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
            Some(SideRow::Session { .. }) => vec![(M_OPEN_SESSION, "Open".into(), false), (M_RENAME_SESSION, "Rename...".into(), false), (0, String::new(), false), (M_DELETE_SESSION, "Delete...".into(), false)],
            Some(SideRow::Workspace { .. }) => vec![(M_NEW_THERE, "New session here".into(), false)],
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
            (M_DELETE_SESSION, SideRow::Session { path, title, .. }) => {
                let working = self.parked.iter().any(|parked| parked.path.as_ref() == Some(&path)) || (self.running && self.current_session.as_ref() == Some(&path));
                if working {
                    self.say("That session is at work: stop it first.");
                    return;
                }
                let text = wide(&format!("Delete the session \"{title}\"? This cannot be undone."));
                let caption = wide("Dogmind");
                if MessageBoxW(self.window, text.as_ptr(), caption.as_ptr(), MB_OKCANCEL | MB_ICONWARNING) == IDOK {
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
            (M_NEW_THERE, SideRow::Workspace { path, .. }) => {
                if path.is_dir() {
                    self.start_session(path);
                }
            }
            _ => {}
        }
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
            rounded(device, RECT { left: area.left + s(2), right: area.right - s(2), top: area.top + s(1), bottom: area.bottom - s(1) }, s(8), Some(RAISED), None);
        }
        let split = s(180);
        draw_text(device, &row.title, RECT { left: area.left + s(12), right: if row.detail.is_empty() { area.right - s(10) } else { area.left + split }, ..area }, self.fonts.strong, ACCENT_HOVER, ONE_LINE);
        draw_text(device, &row.detail, RECT { left: area.left + split, right: area.right - s(10), ..area }, self.fonts.small, MUTED, ONE_LINE);
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
            SideRow::Workspace { name, collapsed, count, .. } => {
                icon::draw(device, if *collapsed { "chevron-right" } else { "chevron-down" }, RECT { left: area.left + s(6), right: area.left + s(22), ..area }, s(14), FAINT);
                icon::draw(device, "folder", RECT { left: area.left + s(24), right: area.left + s(42), ..area }, s(15), MUTED);
                draw_text(device, name, RECT { left: area.left + s(48), right: area.right - s(36), ..area }, self.fonts.strong, MUTED, ONE_LINE);
                if hover {
                    let plus = RECT { left: area.right - s(32), right: area.right - s(6), top: area.top + s(3), bottom: area.bottom - s(3) };
                    rounded(device, plus, s(6), Some(SURFACE), None);
                    icon::draw(device, "plus", plus, s(14), TEXT);
                } else if *collapsed {
                    draw_text(device, &count.to_string(), RECT { right: area.right - s(10), ..area }, self.fonts.small, FAINT, DT_RIGHT | ONE_LINE);
                }
            }
            SideRow::Session { path, title, when } => {
                let current = self.current_session.as_ref() == Some(path);
                if current || hover {
                    let shade = RECT { left: area.left + s(2), right: area.right - s(2), top: area.top + s(1), bottom: area.bottom - s(1) };
                    rounded(device, shade, s(8), Some(if current { RAISED } else { SURFACE }), None);
                }
                let middle = (area.top + area.bottom) / 2;
                draw_text(device, title, RECT { left: area.left + s(14), right: area.right - s(10), top: area.top + s(3), bottom: middle + s(1) }, self.fonts.body, if current { TEXT } else { rgb(205, 201, 194) }, ONE_LINE);
                let parked = self.parked.iter().find(|parked| parked.path.as_ref() == Some(path));
                let state = match parked {
                    Some(parked) if parked.pending.is_some() => Some(("Waiting for you", WARN)),
                    Some(_) => Some(("Working", ACCENT)),
                    None if current && self.pending.is_some() => Some(("Waiting for you", WARN)),
                    None if current && self.running => Some(("Working", ACCENT)),
                    None => None,
                };
                let (detail, color) = state.unwrap_or((when.as_str(), FAINT));
                draw_text(device, detail, RECT { left: area.left + s(14), right: area.right - s(10), top: middle, bottom: area.bottom - s(3) }, self.fonts.small, color, ONE_LINE);
            }
            SideRow::Hint(text) => draw_text(device, text, RECT { left: area.left + s(8), ..area }, self.fonts.small, FAINT, ONE_LINE),
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
        if selected || self.list_hover == Some(item.itemID as usize) {
            rounded(device, RECT { left: area.left + s(2), right: area.right - s(2), top: area.top + s(1), bottom: area.bottom - s(1) }, s(8), Some(RAISED), None);
        }
        let mut left = area.left + s(14);
        if row.active {
            let dot = s(8);
            rounded(device, rect(left, (area.top + area.bottom - dot) / 2, dot, dot), dot / 2, Some(ACCENT), None);
            left += dot + s(10);
        }
        let middle = (area.top + area.bottom) / 2;
        if row.detail.is_empty() {
            draw_text(device, &row.title, RECT { left, right: area.right - s(10), ..area }, self.fonts.body, TEXT, ONE_LINE);
        } else {
            draw_text(device, &row.title, RECT { left, right: area.right - s(10), top: area.top + s(4), bottom: middle + s(1) }, self.fonts.body, TEXT, ONE_LINE);
            draw_text(device, &row.detail, RECT { left, right: area.right - s(10), top: middle, bottom: area.bottom - s(4) }, self.fonts.small, MUTED, ONE_LINE);
        }
    }

    unsafe fn list_selection(&self) -> Option<usize> {
        let at = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        (at >= 0).then_some(at as usize)
    }

    unsafe fn fill_providers(&mut self) {
        self.list_rows = self.settings.providers.iter().map(|provider| ListRow {
            title: provider.name.clone(),
            detail: if provider.kind == Kind::Server { format!("{}  ·  {}", provider.kind.label(), provider.base_url) } else { provider.kind.label().to_string() },
            active: provider.name == self.settings.active,
        }).collect();
        self.fill_list();
        let active = self.settings.providers.iter().position(|provider| provider.name == self.settings.active);
        self.select_provider(self.selected_provider.or(active));
    }

    unsafe fn select_provider(&mut self, at: Option<usize>) {
        self.selected_provider = at;
        let Some(at) = at.filter(|&at| at < self.settings.providers.len()) else { return };
        SendMessageW(self.list, LB_SETCURSEL, at, 0);
        let provider = self.settings.providers[at].clone();
        self.kind = provider.kind;
        set_text(self.kind_chip, provider.kind.label());
        self.show_provider_fields(&provider);
    }

    /// A provider's fields: Straydog on this PC has a VRAM budget and a context, and the others an address and a key.
    unsafe fn show_provider_fields(&mut self, provider: &ProviderConfig) {
        set_text(self.fields[0], &provider.name);
        if self.kind == Kind::Embedded {
            self.labels = ["Name".into(), "VRAM in GiB, or auto".into(), "Context in tokens (empty: its largest)".into(), String::new()];
            set_text(self.fields[1], &settings::show_vram(self.settings.vram));
            set_text(self.fields[2], &self.settings.context.map_or(String::new(), |context| context.to_string()));
            set_text(self.fields[3], "");
            self.panel_log = "The engine takes these when it starts: Use this starts it again with them.".into();
        } else {
            self.labels = ["Name".into(), "URL (…/v1; Claude: empty)".into(), "Model (empty: its first)".into(), "API key: a file, or env:NAME".into()];
            set_text(self.fields[1], &provider.base_url);
            set_text(self.fields[2], &provider.model);
            set_text(self.fields[3], &provider.api_key);
            self.panel_log.clear();
        }
        self.layout();
    }

    unsafe fn store_engine_settings_from_providers(&mut self) -> Result<(), String> {
        let (vram, context) = (get_text(self.fields[1]), get_text(self.fields[2]));
        let context = context.trim();
        self.settings.vram = settings::parse_vram(&vram)?;
        self.settings.context = if context.is_empty() { None } else { Some(context.parse().map_err(|_| "The context is a number of tokens".to_string())?) };
        Ok(())
    }

    unsafe fn provider_from_fields(&self) -> ProviderConfig {
        if self.kind == Kind::Embedded {
            return ProviderConfig { name: get_text(self.fields[0]).trim().to_string(), kind: Kind::Embedded, base_url: String::new(), model: String::new(), api_key: String::new() };
        }
        ProviderConfig {
            name: get_text(self.fields[0]).trim().to_string(),
            kind: self.kind,
            base_url: get_text(self.fields[1]).trim().to_string(),
            model: get_text(self.fields[2]).trim().to_string(),
            api_key: get_text(self.fields[3]).trim().to_string(),
        }
    }

    unsafe fn say_in_panel(&mut self, text: &str) {
        self.panel_log = text.to_string();
        self.layout();
    }

    unsafe fn save_provider(&mut self) -> bool {
        let provider = self.provider_from_fields();
        if let Some(problem) = provider.problem() {
            self.say_in_panel(&problem);
            return false;
        }
        if self.kind == Kind::Embedded {
            if let Err(problem) = self.store_engine_settings_from_providers() {
                self.say_in_panel(&problem);
                return false;
            }
        }
        let clash = self.settings.providers.iter().enumerate().any(|(at, known)| known.name == provider.name && Some(at) != self.selected_provider);
        if clash {
            self.say_in_panel("Another provider has that name.");
            return false;
        }
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
        self.say_in_panel("Saved.");
        true
    }

    /// Starts using the active provider: stops what the last one needed and connects on another thread.
    unsafe fn connect(&mut self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.connected = false;
        self.straydog = false;
        self.model.clear();
        self.rate = None;
        self.window_tokens = 0;
        let Some(provider) = self.settings.active_provider().cloned() else { return };
        if provider.kind != Kind::Embedded {
            self.engine = None;
        } else if self.setting_up {
            // It would want the files the setup is working on. When the setup ends, it connects.
            self.connection = "setting up the model".into();
            self.update_status();
            return;
        }
        self.connection = "connecting".into();
        self.update_status();
        let (poster, generations) = (self.poster.clone(), self.generation.clone());
        let current = move || generations.load(Ordering::SeqCst) == generation;
        match provider.kind {
            Kind::Anthropic => {
                std::thread::spawn(move || {
                    let result = (|| {
                        let key = provider.key()?.ok_or("Claude needs an API key")?;
                        let url = if provider.base_url.is_empty() { anthropic::DEFAULT_URL } else { &provider.base_url };
                        let client = Anthropic::new(url, key)?;
                        let model = if provider.model.is_empty() { anthropic::DEFAULT_MODEL.to_string() } else { provider.model.clone() };
                        let window = client.context_of(&model)?;
                        Ok::<_, String>((client, model, window))
                    })();
                    poster.send(match result {
                        Ok((client, model, window)) => Note::Connected { generation, model, window, straydog: false, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
                    });
                });
            }
            Kind::Embedded => {
                let Some(straydog) = engine::find(&self.settings) else {
                    self.connection = "no engine".into();
                    self.append("straydog.exe was not found: put it beside dogmind-app.exe, or choose another provider in Settings.\n", Style::Warn);
                    self.update_status();
                    return;
                };
                if !engine::is_set_up(&self.folder) {
                    self.connection = "no model yet".into();
                    self.update_status();
                    self.set_panel(Panel::Models);
                    return;
                }
                self.engine = None;
                match Engine::start(&straydog, &self.settings, &self.folder) {
                    Ok(started) => self.engine = Some(started),
                    Err(error) => {
                        self.connection = "the engine did not start".into();
                        self.append(&format!("The engine did not start: {error}\n"), Style::Bad);
                        self.update_status();
                        return;
                    }
                }
                let client = engine::client(&self.settings);
                std::thread::spawn(move || {
                    let mut say = |text: String| poster.send(Note::Status(generation, text));
                    let result = engine::wait_ready(&client, &current, &mut say).and_then(|()| client.describe());
                    poster.send(match result {
                        Ok(info) => Note::Connected { generation, model: if provider.model.is_empty() { info.model } else { provider.model }, window: info.context, straydog: info.straydog, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
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
                        Ok::<_, String>((client, info))
                    })();
                    poster.send(match result {
                        Ok((client, info)) => Note::Connected { generation, model: if provider.model.is_empty() { info.model } else { provider.model }, window: info.context, straydog: info.straydog, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
                    });
                });
            }
            Kind::Codex => {
                std::thread::spawn(move || {
                    let client = Codex::default();
                    let model = if provider.model.is_empty() {
                        client.models().map_err(|error| error.to_string()).and_then(|models| models.into_iter().next().ok_or_else(|| "this plan offers no models".to_string()))
                    } else {
                        Ok(provider.model)
                    };
                    poster.send(match model {
                        Ok(model) => Note::Connected { generation, model, window: codex::context_fallback(), straydog: false, provider: Arc::new(client) },
                        Err(error) => Note::ConnectFailed(generation, error),
                    });
                });
            }
        }
    }

    unsafe fn send_message(&mut self) {
        let text = get_text(self.composer);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        set_text(self.composer, "");
        if text.starts_with('/') {
            if !self.run_command(&text) {
                self.say(&format!("No such command: {text}. /help lists them."));
            }
            return;
        }
        self.show_user(&text);
        if self.running || !self.connected {
            self.queued.push(text);
            self.append("(sent when the current work ends)\n", Style::Tool);
            return;
        }
        self.dispatch(text);
    }

    unsafe fn dispatch(&mut self, text: String) {
        self.running = true;
        self.update_buttons();
        let _ = self.agent.commands.send(Command::Send(text));
    }

    unsafe fn update_buttons(&self) {
        set_text(self.send, if self.running { "square" } else { "arrow-up" });
        if self.running {
            SetTimer(self.window, TIMER_DOTS, 420, None);
        } else {
            KillTimer(self.window, TIMER_DOTS);
        }
        InvalidateRect(self.window, &self.status_area, 0);
    }

    unsafe fn answer_approval(&mut self, decision: Decision) {
        let Some(approval) = self.pending.take() else { return };
        let feedback = get_text(self.feedback);
        set_text(self.feedback, "");
        let _ = approval.answer.send((decision, (!feedback.trim().is_empty()).then_some(feedback)));
        self.layout();
        SetFocus(self.composer);
    }

    unsafe fn show_transcript(&mut self, messages: &[Message]) {
        set_text(self.transcript, "");
        self.shown.clear();
        self.answers.clear();
        self.has_messages = false;
        self.line_start = true;
        self.in_code = false;
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    if let Some(text) = &message.content {
                        self.show_user(typed_text(text));
                    }
                }
                "assistant" => {
                    if let Some(text) = &message.content {
                        self.append_answer(text);
                        self.finish_answer();
                        self.last_answer = text.clone();
                        self.mark_answer(text);
                    }
                    for call in message.tool_calls.iter().flatten() {
                        // A session at work has calls still running, or still waiting for an answer: they say so when they end.
                        if messages.iter().any(|later| later.tool_call_id.as_ref() == Some(&call.id)) {
                            self.append(&format!("✓  {}\n", call.function.name), Style::Tool);
                        }
                    }
                }
                _ => {}
            }
        }
        self.layout();
    }

    /// What a session out of sight says: its state is kept for when it is opened. `true`: its turn has ended.
    unsafe fn absorb(&mut self, at: usize, event: Event) -> bool {
        let parked = &mut self.parked[at];
        match event {
            Event::SessionFile(path) => parked.path = Some(path),
            Event::Content(text) => parked.partial.push_str(&text),
            Event::AssistantEnded => parked.partial.clear(),
            Event::ToolStarted { id, tool, summary } => {
                parked.tools_running.insert(id, (tool, summary.chars().take(160).collect()));
            }
            Event::ToolEnded { id, .. } => {
                parked.tools_running.remove(&id);
            }
            Event::Approval(approval) => {
                parked.pending = Some(approval);
                InvalidateRect(self.side, std::ptr::null(), 0);
                FlashWindow(self.window, 1);
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
            Event::Notice(text) | Event::Failed(text) => parked.notices.push(text),
            Event::Todos(items) => parked.todos = items,
            Event::WebSeen(on) => parked.web_seen = on,
            Event::TurnEnded { .. } => {
                parked.pending = None;
                if parked.queued.is_empty() {
                    return true;
                }
                let next = parked.queued.remove(0);
                let _ = parked.agent.commands.send(Command::Send(next));
            }
            _ => {}
        }
        false
    }

    unsafe fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }
        let mut at = 0;
        while at < self.parked.len() {
            let mut ended = false;
            while let Ok(event) = self.parked[at].events.try_recv() {
                ended |= self.absorb(at, event);
            }
            if ended {
                let parked = self.parked.remove(at);
                let _ = parked.agent.commands.send(Command::Quit);
                if let (Some(path), false) = (parked.path, parked.notices.is_empty()) {
                    self.held_notices.insert(path, parked.notices);
                }
                self.refresh_sessions();
                InvalidateRect(self.side, std::ptr::null(), 0);
                if GetForegroundWindow() != self.window {
                    FlashWindow(self.window, 1);
                }
            } else {
                at += 1;
            }
        }
        while let Ok(note) = self.notes.try_recv() {
            self.on_note(note);
        }
    }

    unsafe fn on_event(&mut self, event: Event) {
        match event {
            Event::SessionStarted { path, workspace, transcript } => {
                let notices = self.held_notices.remove(&path).unwrap_or_default();
                self.workspace = workspace;
                self.session_title = transcript.iter().find(|message| message.role == "user").and_then(|message| message.content.as_deref())
                    .map(|text| typed_text(text).lines().next().unwrap_or("").chars().take(70).collect()).unwrap_or_default();
                if let Some(summary) = self.all_sessions.iter().find(|summary| self.current_session.as_ref() == Some(&summary.path)) {
                    self.session_title = summary.title.clone();
                }
                self.look_at_workspace();
                self.changes.clear();
                self.refresh_changes();
                self.show_transcript(&transcript);
                for notice in notices {
                    self.append(&format!("{notice}\n"), Style::Warn);
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
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.thinking.clear();
                self.answer.clear();
                self.set_activity("Thinking…".into());
            }
            Event::Reasoning(text) => {
                if self.settings.show_thinking {
                    self.append(&text, Style::Thinking);
                    self.line_start = text.ends_with('\n');
                }
                self.thinking.push_str(&text);
                let tail: String = self.thinking.chars().rev().take(120).collect::<Vec<_>>().into_iter().rev().collect();
                self.set_activity(tail.replace(['\n', '\r'], " "));
            }
            Event::Content(text) => {
                if self.answer.is_empty() {
                    self.set_activity(String::new());
                    if self.settings.show_thinking {
                        self.end_line();
                    }
                }
                self.answer.push_str(&text);
                self.append_answer(&text);
            }
            Event::AssistantEnded => {
                self.thinking.clear();
                self.finish_answer();
                if !self.answer.is_empty() {
                    self.last_answer = std::mem::take(&mut self.answer);
                    let answer = self.last_answer.clone();
                    self.mark_answer(&answer);
                }
            }
            Event::ToolStarted { id, tool, summary } => {
                let summary: String = summary.chars().take(160).collect();
                self.set_activity(format!("{tool}  {summary}"));
                self.tools_running.insert(id, (tool, summary));
            }
            Event::ToolEnded { id, ok, declined, elapsed, output } => {
                self.end_line();
                self.set_activity("Thinking…".into());
                let (tool, summary) = self.tools_running.remove(&id).unwrap_or_default();
                if matches!(tool.as_str(), "create_file" | "apply_patch" | "execute_command") {
                    // Several in a row are looked at once.
                    SetTimer(self.window, TIMER_CHANGES, 500, None);
                }
                let first = output.lines().next().unwrap_or("").chars().take(240).collect::<String>();
                if declined {
                    self.append(&format!("✗  {tool}  {summary}  —  {first}\n"), Style::Warn);
                } else if ok {
                    self.append(&format!("✓  {tool}  {summary}  ·  {:.1}s\n", elapsed.as_secs_f64()), Style::Tool);
                    self.show_tool_output(&tool, &output);
                } else {
                    self.append(&format!("✗  {tool}  {summary}  —  {first}\n"), Style::Bad);
                }
            }
            Event::Approval(approval) => {
                let reason = approval.reason.as_ref().map_or(String::new(), |reason| format!("{reason}\r\n"));
                let details: String = approval.details.chars().take(2000).collect();
                let always = match (&approval.rule, approval.always_allowed) {
                    (Some(rule), true) => format!("Always (A) saves: {}\r\n", rule.label()),
                    (None, true) => "Always (A) allows this exact call, for this session.\r\n".to_string(),
                    _ => String::new(),
                };
                set_text(self.note, &format!("{}\r\n{reason}{always}{}", approval.summary, details.replace('\n', "\r\n")));
                EnableWindow(self.always, i32::from(approval.always_allowed));
                self.pending = Some(approval);
                InvalidateRect(self.side, std::ptr::null(), 0);
                self.layout();
                SetFocus(self.window);
                FlashWindow(self.window, 1);
            }
            Event::Usage { used, window, usage } => {
                self.used = used;
                self.window_tokens = window;
                if usage.tokens_per_second.is_some() {
                    self.rate = usage.tokens_per_second;
                }
                if usage.prompt > 0 {
                    self.totals = (self.totals.0 + usage.prompt, self.totals.1 + usage.cached, self.totals.2 + usage.completion, self.totals.3 + 1);
                }
                self.update_status();
                if self.right_width > 0 {
                    self.layout();
                }
            }
            Event::WebSeen(on) => {
                self.web_seen = on;
                self.layout();
            }
            Event::Todos(items) => {
                // Where the panel on the right is not there to show it.
                if self.right_width == 0 && !items.is_empty() {
                    let lines: Vec<String> = items.iter().map(|item| format!("    [{}] {}", item["status"].as_str().unwrap_or(""), item["text"].as_str().unwrap_or(""))).collect();
                    self.append(&format!("{}\n", lines.join("\n")), Style::Tool);
                }
                self.todos = items;
                self.layout();
            }
            Event::Turns(turns) => {
                self.turns = turns;
                if self.turns.is_empty() {
                    self.say("This session has nothing to go back to yet.");
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
                    self.append(&format!("Went back: {count} {} restored.\n", if count == 1 { "file" } else { "files" }), Style::Tool);
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
                    SetFocus(self.composer);
                }
                self.look_at_workspace();
                self.refresh_changes();
            }
            Event::Notice(text) => {
                self.end_line();
                self.append(&format!("{text}\n"), Style::Warn);
            }
            Event::Failed(text) => {
                self.end_line();
                self.append(&format!("{text}\n"), Style::Bad);
            }
            Event::TurnEnded { cancelled } => {
                self.running = false;
                InvalidateRect(self.side, std::ptr::null(), 0);
                SetTimer(self.window, TIMER_CHANGES, 200, None);
                self.finish_answer();
                if cancelled {
                    self.append("(stopped)\n", Style::Tool);
                }
                if self.pending.take().is_some() {
                    self.layout();
                }
                self.set_activity(String::new());
                self.update_buttons();
                self.refresh_sessions();
                if GetForegroundWindow() != self.window {
                    FlashWindow(self.window, 1);
                }
                self.flush_queue();
            }
        }
    }

    unsafe fn flush_queue(&mut self) {
        if !self.running && self.connected && !self.queued.is_empty() {
            let text = self.queued.remove(0);
            self.dispatch(text);
        }
    }

    unsafe fn on_note(&mut self, note: Note) {
        let current = self.generation.load(Ordering::SeqCst);
        match note {
            Note::Status(generation, text) if generation == current => {
                self.connection = text;
                self.update_status();
            }
            Note::Connected { generation, provider, model, window, straydog } if generation == current => {
                self.connected = true;
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
                self.flush_queue();
            }
            Note::ConnectFailed(generation, error) if generation == current => {
                self.connection = "not connected".into();
                self.end_line();
                self.append(&format!("Could not connect: {error}\n"), Style::Bad);
                self.update_status();
            }
            Note::Status(..) | Note::Connected { .. } | Note::ConnectFailed(..) => {}
            Note::Sessions(sessions) => {
                self.all_sessions = sessions;
                if let Some(summary) = self.all_sessions.iter().find(|summary| self.current_session.as_ref() == Some(&summary.path)) {
                    if self.session_title != summary.title {
                        self.session_title = summary.title.clone();
                        self.layout();
                    }
                }
                self.fill_side();
            }
            Note::Workspace { path, files, branch } => {
                if path == self.workspace {
                    self.files = files;
                    self.branch = branch;
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
            Note::Changes(path, found) => {
                if path != self.workspace {
                    return;
                }
                if self.panel != Panel::Diff {
                    self.changes = found.unwrap_or_default();
                    self.layout();
                    return;
                }
                match found {
                    Ok(changes) => {
                        let (added, removed) = changes.iter().fold((0, 0), |(added, removed), change| (added + change.added, removed + change.removed));
                        self.panel_title = if changes.is_empty() { "No changes".into() } else { format!("Changes  +{added} −{removed}") };
                        self.list_rows = changes.iter().map(|change| ListRow { title: change.path.clone(), detail: format!("+{}  −{}", change.added, change.removed), active: false }).collect();
                        self.changes = changes;
                        self.fill_list();
                        if self.changes.is_empty() {
                            self.show_document("Nothing has changed.");
                        } else {
                            let first = self.change_wanted.take().filter(|at| *at < self.changes.len()).unwrap_or(0);
                            SendMessageW(self.list, LB_SETCURSEL, first, 0);
                            self.show_change(first);
                        }
                    }
                    Err(error) => self.show_document(&error),
                }
                self.layout();
            }
            Note::Catalog(result) => {
                if self.panel != Panel::Models {
                    return;
                }
                match result {
                    Ok(entries) => {
                        self.catalog = entries;
                        self.list_rows = self.catalog.iter().map(|entry| ListRow {
                            title: if entry.origin.is_empty() { entry.name.clone() } else { format!("{}  ·  {}", entry.name, entry.origin) },
                            detail: entry.detail.clone(),
                            active: false,
                        }).collect();
                        self.fill_list();
                        SendMessageW(self.list, LB_SETCURSEL, 0, 0);
                        self.offer_context(Some(0));
                    }
                    Err(error) => self.say_in_panel(&error),
                }
            }
            Note::Setup(line) => match line {
                SetupLine::Progress { stage, done, total } => {
                    self.progress = Some(done / total.max(0.001));
                    self.show_setup_status(format!("{stage}: {done:.1} / {total:.1} GB"));
                }
                SetupLine::Text(text) => self.show_setup_status(text),
            },
            Note::SetupDone(result) => {
                let cancelled = self.setup_cancel.swap(false, Ordering::Relaxed);
                self.setting_up = false;
                self.progress = None;
                self.setup_status.clear();
                if self.panel == Panel::Models {
                    self.show_setup_button();
                }
                let told = |app: &mut App, text: &str| {
                    if app.panel == Panel::Chat {
                        app.say(text);
                    } else {
                        app.say_in_panel(text);
                    }
                };
                match result {
                    Ok(()) => {
                        if self.onboarding {
                            self.finish_onboarding();
                        } else if self.panel == Panel::Models {
                            self.set_panel(Panel::Chat);
                        }
                        self.connect();
                    }
                    Err(_) if cancelled => {
                        told(self, "Download stopped. What has arrived stays in the folder, and downloading again goes on from there.");
                        // The engine was stopped for this, and a model it had is still there.
                        if engine::is_set_up(&self.folder) && !self.onboarding {
                            self.connect();
                        }
                    }
                    Err(error) => {
                        told(self, &error);
                        if engine::is_set_up(&self.folder) && !self.onboarding {
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
        self.settings.context = if context.is_empty() { None } else { Some(context.parse().map_err(|_| "The context is a number of tokens".to_string())?) };
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
            self.say_in_panel(&error);
            return;
        }
        let Some(entry) = self.list_selection().and_then(|at| self.catalog.get(at)).cloned() else {
            self.say_in_panel("Choose a model from the list.");
            return;
        };
        let Some(folder) = self.settings.models_folder.clone() else {
            self.say_in_panel("Choose a folder for the model.");
            return;
        };
        let Some(straydog) = engine::find(&self.settings) else {
            self.say_in_panel("straydog.exe was not found: put it beside dogmind-app.exe.");
            return;
        };
        self.setting_up = true;
        self.setup_cancel.store(false, Ordering::Relaxed);
        self.progress = Some(0.0);
        self.show_setup_button();
        self.engine = None; // A verify needs the files to itself, and its VRAM probe needs the card.
        let args = engine::setup_arguments(entry.quant.as_deref(), &folder, &engine::config_path(&self.folder), self.drafter);
        self.show_setup_status(format!("Setting up {}...", entry.name));
        let (poster, cancel) = (self.poster.clone(), self.setup_cancel.clone());
        std::thread::spawn(move || {
            let (lines, seen) = channel();
            let forward = poster.clone();
            let relay = std::thread::spawn(move || {
                while let Ok(line) = seen.recv() {
                    forward.send(Note::Setup(line));
                }
            });
            let waker = poster.clone();
            let result = engine::run_setup(&straydog, &args, &lines, &move || waker.wake(), &cancel);
            drop(lines);
            let _ = relay.join();
            poster.send(Note::SetupDone(result));
        });
    }

    unsafe fn browse_folder(&mut self) -> Option<PathBuf> {
        let title = wide("Choose a folder");
        let mut info: BROWSEINFOW = std::mem::zeroed();
        info.hwndOwner = self.window;
        info.lpszTitle = title.as_ptr();
        info.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let list = SHBrowseForFolderW(&info);
        if list.is_null() {
            return None;
        }
        let mut path = [0u16; 1024];
        let found = SHGetPathFromIDListW(list, path.as_mut_ptr()) != 0;
        CoTaskMemFree(list as _);
        if !found {
            return None;
        }
        let end = path.iter().position(|&unit| unit == 0).unwrap_or(path.len());
        Some(PathBuf::from(String::from_utf16_lossy(&path[..end])))
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
            (Panel::Models, 2) => {
                if let Err(error) = self.store_engine_fields() {
                    self.say_in_panel(&error);
                    return;
                }
                let next = if self.onboarding { Panel::Setup } else { Panel::Chat };
                self.set_panel(next);
            }
            (Panel::Providers, 0) => {
                if self.save_provider() {
                    self.settings.active = get_text(self.fields[0]).trim().to_string();
                    self.save();
                    self.set_panel(Panel::Chat);
                    self.connect();
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
                set_text(self.kind_chip, Kind::Server.label());
                let blank = ProviderConfig { name: String::new(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: String::new() };
                self.show_provider_fields(&blank);
                SetFocus(self.fields[0]);
            }
            (Panel::Providers, 3) => {
                if let Some(at) = self.selected_provider.filter(|&at| at < self.settings.providers.len() && self.settings.providers.len() > 1) {
                    let gone = self.settings.providers.remove(at);
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
                }
            }
            (Panel::Providers, 4) => self.set_panel(Panel::Chat),
            (Panel::Setup, 0) => self.setup_choose(),
            (Panel::SetupDetails, 0) => self.setup_details_done(),
            (Panel::SetupDetails, 1) => self.set_panel(Panel::Setup),
            (Panel::General, 0) => {
                self.connect();
                self.set_panel(Panel::Chat);
            }
            (Panel::General, 1) => self.on_menu(M_LOG),
            (Panel::General, 3) => match self.dashboard_url() {
                Some(url) => open_in_browser(&url),
                None => self.say_in_panel("Straydog is not what is connected, so it has no dashboard to open."),
            },
            (Panel::Instructions, 0) => {
                let text = get_text(self.doc).replace("\r\n", "\n").replace('\r', "\n");
                match std::fs::write(self.folder.join("instructions.md"), text) {
                    Ok(()) => {
                        self.set_panel(Panel::Chat);
                        self.say("Saved. New sessions read these instructions; this one goes on with what it began with.");
                    }
                    Err(error) => {
                        self.say_in_panel(&error.to_string());
                    }
                }
            }
            (Panel::Instructions, 1) => self.set_panel(Panel::Chat),
            (Panel::General, 2) | (Panel::Rename, 1) | (Panel::Sessions, 1) | (Panel::Help, 0) | (Panel::About, 0) | (Panel::Rewind, 3) | (Panel::Permissions, 1) | (Panel::Diff, 0) => self.set_panel(Panel::Chat),
            (Panel::Rewind, 0..=2) => {
                let Some(id) = self.list_selection().and_then(|at| self.rewind_rows.get(at)).copied() else {
                    self.say_in_panel("Choose a turn.");
                    return;
                };
                let chat_gone = self.turns.iter().find(|turn| turn.id == id).is_none_or(|turn| turn.message.is_none());
                if chat_gone && at != 1 {
                    self.say_in_panel("That part of the conversation was compacted or rewound past: only its files can go back.");
                    return;
                }
                let _ = self.agent.commands.send(Command::Rewind { turn: id, files: at != 2, conversation: at != 1 });
                self.set_panel(Panel::Chat);
            }
            (Panel::Permissions, 0) => {
                let Some(rule) = self.list_selection().and_then(|at| self.shown_rules.get(at)).cloned() else {
                    self.say_in_panel("Choose a rule.");
                    return;
                };
                let removed = rules::remove(&self.folder.join("permissions.json"), &rule);
                self.set_panel(Panel::Permissions);
                if let Err(error) = removed {
                    self.say_in_panel(&error);
                }
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
                    self.start_session(folder);
                } else {
                    self.say_in_panel("Choose a folder from the list, or another one.");
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
            self.say_in_panel("A session needs a folder to work in: choose one.");
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

    unsafe fn on_menu(&mut self, id: usize) {
        match id {
            M_OPEN_SESSION | M_RENAME_SESSION | M_DELETE_SESSION | M_NEW_THERE => self.on_session_menu(id),
            M_COMPACT => {
                if !self.running && self.connected {
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
                self.save();
                self.update_status();
            }
            _ if (M_EFFORT..M_EFFORT + EFFORTS.len()).contains(&id) => {
                self.settings.effort = EFFORTS[id - M_EFFORT].0.map(str::to_string);
                let _ = self.agent.commands.send(Command::SetEffort(self.settings.effort.clone()));
                self.save();
                self.update_status();
            }
            _ if (M_KIND..M_KIND + Kind::ALL.len()).contains(&id) => {
                self.kind = Kind::ALL[id - M_KIND];
                set_text(self.kind_chip, self.kind.label());
                let shown = ProviderConfig { name: get_text(self.fields[0]), kind: self.kind, base_url: String::new(), model: String::new(), api_key: String::new() };
                self.show_provider_fields(&shown);
                if self.kind == Kind::Anthropic {
                    set_text(self.fields[3], "env:ANTHROPIC_API_KEY");
                }
            }
            _ if id >= M_PROVIDER && id - M_PROVIDER < self.settings.providers.len() => {
                self.settings.active = self.settings.providers[id - M_PROVIDER].name.clone();
                self.save();
                self.connect();
            }
            _ => {}
        }
    }

    unsafe fn on_chip(&mut self, id: isize) {
        match id {
            ID_MODEL_CHIP => {
                let mut items: Vec<(usize, String, bool)> = self.settings.providers.iter().enumerate().map(|(at, provider)| (M_PROVIDER + at, provider.name.clone(), provider.name == self.settings.active)).collect();
                items.push((0, String::new(), false));
                items.push((M_PROVIDERS, "Providers...".into(), false));
                self.popup(self.model_chip, &items);
            }
            ID_MODE_CHIP => {
                let mode = self.current_mode();
                let mut items: Vec<(usize, String, bool)> = MODES.iter().enumerate().map(|(at, (known, name))| (M_MODE + at, name.to_string(), *known == mode)).collect();
                items.push((0, String::new(), false));
                items.push((M_SAFE, "Allow safe commands without asking".into(), self.settings.safe_commands));
                self.popup(self.mode_chip, &items);
            }
            ID_EFFORT_CHIP => {
                let mut items: Vec<(usize, String, bool)> = EFFORTS.iter().enumerate().map(|(at, (known, name))| (M_EFFORT + at, name.to_string(), *known == self.settings.effort.as_deref())).collect();
                items.push((0, String::new(), false));
                items.push((M_THINKING, "Show the thinking".into(), self.settings.show_thinking));
                self.popup(self.effort_chip, &items);
            }
            ID_KIND_CHIP => {
                let items: Vec<(usize, String, bool)> = Kind::ALL.iter().enumerate().map(|(at, kind)| (M_KIND + at, kind.label().to_string(), *kind == self.kind)).collect();
                self.popup(self.kind_chip, &items);
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
    let result = CallWindowProcW(base, window, message, wparam, lparam);
    if message == WM_IME_ENDCOMPOSITION_ {
        app.composing = false;
        app.update_composer_cue();
    }
    result
}

unsafe extern "system" fn transcript_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let base = std::mem::transmute::<isize, WNDPROC>(app.transcript_procedure);
    let result = CallWindowProcW(base, window, message, wparam, lparam);
    if matches!(message, WM_VSCROLL | WM_MOUSEWHEEL | WM_KEYDOWN | WM_LBUTTONUP | WM_SIZE) {
        app.place_copy_buttons();
    }
    result
}

/// The list boxes, with hover and, in the sidebar, a click that knows where it fell.
unsafe extern "system" fn list_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let base = std::mem::transmute::<isize, WNDPROC>(app.list_procedure);
    let is_side = window == app.side;
    match message {
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            let found = if message == WM_MOUSEMOVE { SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam) } else { 0x10000 };
            let hover = ((found >> 16) & 0xFFFF == 0).then_some((found & 0xFFFF) as usize);
            let slot = if is_side { &mut app.side_hover } else if window == app.suggest { &mut app.suggest_hover } else { &mut app.list_hover };
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
            CallWindowProcW(base, window, message, wparam, lparam)
        }
        WM_RBUTTONUP if is_side => {
            let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
            if (found >> 16) & 0xFFFF == 0 {
                app.on_side_menu((found & 0xFFFF) as usize);
            }
            0
        }
        WM_LBUTTONUP if is_side => {
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
        _ => CallWindowProcW(base, window, message, wparam, lparam),
    }
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    match message {
        WM_WAKE => {
            app.drain();
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
            let device = CreateCompatibleDC(target);
            let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
            let previous = SelectObject(device, bitmap);
            app.paint(device, client);
            BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
            SelectObject(device, previous);
            DeleteObject(bitmap);
            DeleteDC(device);
            EndPaint(window, &paint);
            0
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: scale(app.dpi, 780), y: scale(app.dpi, 520) };
            0
        }
        WM_DPICHANGED => {
            app.dpi = (wparam & 0xffff) as u32;
            app.fonts = Fonts::new(app.dpi);
            set_button_fonts(app.fonts);
            let rect = &*(lparam as *const RECT);
            SetWindowPos(window, std::ptr::null_mut(), rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        WM_TIMER if wparam == TIMER_ENGINE => {
            if let Some(reason) = app.engine.as_mut().and_then(Engine::ended) {
                app.engine = None;
                app.connected = false;
                app.connection = "the engine stopped".into();
                app.end_line();
                app.append(&format!("The engine stopped: {reason}\n"), Style::Bad);
                app.update_status();
            }
            0
        }
        WM_TIMER if wparam == TIMER_COPIED => {
            KillTimer(window, TIMER_COPIED);
            for button in app.copy_buttons.clone() {
                set_text(button, "copy");
            }
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
        WM_LBUTTONUP if app.panel == Panel::Chat => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
            let within = |area: &RECT| x >= area.left && x < area.right && y >= area.top && y < area.bottom;
            if within(&app.folder_hit) {
                app.set_panel(Panel::Start);
            } else if let Some((_, at)) = app.right_hits.iter().find(|(area, _)| within(area)).copied() {
                if at == DASHBOARD_ROW {
                    if let Some(url) = app.dashboard_url() {
                        open_in_browser(&url);
                    }
                } else {
                    app.change_wanted = Some(at);
                    app.set_panel(Panel::Diff);
                }
            }
            0
        }
        WM_TIMER if wparam == TIMER_SEARCH => {
            app.search_now();
            0
        }
        WM_TIMER if wparam == TIMER_DOTS => {
            app.dots += 1;
            if app.activity.ends_with('…') {
                InvalidateRect(window, &app.activity_area, 0);
            }
            0
        }
        WM_CTLCOLORLISTBOX => {
            let side = lparam as HWND == app.side;
            SetBkColor(wparam as HDC, if side { SIDEBAR } else { SURFACE });
            app.brushes[if side { 1 } else { 2 }] as LRESULT
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
            let device = wparam as HDC;
            let raised = lparam as HWND == app.feedback;
            SetTextColor(device, if lparam as HWND == app.composer_cue { FAINT } else { TEXT });
            SetBkColor(device, if raised { RAISED } else { SURFACE });
            app.brushes[if raised { 3 } else { 2 }] as LRESULT
        }
        WM_MEASUREITEM => {
            let item = &mut *(lparam as *mut MEASUREITEMSTRUCT);
            item.itemHeight = if item.CtlID as isize == ID_SUGGEST {
                scale(app.dpi, 36) as u32
            } else if item.CtlID as isize == ID_SIDE {
                match app.side_rows.get(item.itemID as usize) {
                    Some(SideRow::Session { .. }) => scale(app.dpi, 46) as u32,
                    _ => scale(app.dpi, 32) as u32,
                }
            } else {
                scale(app.dpi, 46) as u32
            };
            1
        }
        WM_DRAWITEM => {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            if item.itemID != u32::MAX {
                if item.hwndItem == app.side {
                    app.draw_side_row(item);
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
                }
                return 0;
            }
            if lparam as HWND == app.find_field {
                if code == EN_CHANGE as usize {
                    app.find_update();
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
                    if app.running {
                        app.agent.cancel.store(true, Ordering::Relaxed);
                    } else {
                        app.send_message();
                    }
                }
                ID_PANEL_TOGGLE => app.toggle_right_panel(),
                ID_FIND_PREVIOUS => app.find_go(false),
                ID_FIND_NEXT => app.find_go(true),
                ID_FIND_CLOSE => app.close_find(),
                ID_APPROVE => app.answer_approval(Decision::Approve),
                ID_ALWAYS => app.answer_approval(Decision::Always),
                ID_REJECT => app.answer_approval(Decision::Reject),
                ID_NEW_SESSION => app.set_panel(Panel::Start),
                ID_TAB_MODELS => app.set_panel(Panel::Models),
                ID_DRAFTER => {
                    app.drafter = !app.drafter;
                    app.update_drafter_chip();
                }
                ID_MODEL_CHIP | ID_MODE_CHIP | ID_EFFORT_CHIP | ID_KIND_CHIP => app.on_chip(id),
                ID_SETTINGS | ID_TAB_PROVIDERS => app.set_panel(Panel::Providers),
                ID_TAB_GENERAL => app.set_panel(Panel::General),
                ID_HELP => app.set_panel(Panel::Help),
                ID_ABOUT => app.set_panel(Panel::About),
                _ if (ID_COPY_POOL..ID_COPY_POOL + COPY_POOL as isize).contains(&id) => app.copy_answer((id - ID_COPY_POOL) as usize),
                ID_TOGGLE_THINKING => app.on_menu(M_THINKING),
                ID_TOGGLE_SAFE => app.on_menu(M_SAFE),
                ID_TOGGLE_LANGUAGE => {
                    app.settings.language = match app.settings.language.as_str() {
                        "auto" => "ja",
                        "ja" => "en",
                        _ => "auto",
                    }.to_string();
                    app.save();
                    let language = app.settings.language.clone();
                    let _ = app.agent.commands.send(Command::SetLanguage(language.clone()));
                    for parked in &app.parked {
                        let _ = parked.agent.commands.send(Command::SetLanguage(language.clone()));
                    }
                    app.update_toggles();
                }
                ID_EDIT_INSTRUCTIONS => app.set_panel(Panel::Instructions),
                ID_TOGGLE_WEB => {
                    app.settings.web_tools = !app.settings.web_tools;
                    app.save();
                    app.apply_web();
                    app.update_toggles();
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
                }
                ID_LIST if code == LBN_DBLCLK as usize && app.panel == Panel::Sessions => app.on_button(0),
                ID_LIST if code == LBN_SELCHANGE as usize && app.panel == Panel::Diff => {
                    if let Some(at) = app.list_selection() {
                        app.show_change(at);
                    }
                }
                ID_LIST if code == LBN_SELCHANGE as usize && app.panel == Panel::Providers => {
                    let selected = app.list_selection();
                    app.select_provider(selected);
                }
                ID_LIST if code == LBN_DBLCLK as usize && app.panel == Panel::Start => app.on_button(0),
                _ if (ID_BUTTONS..ID_BUTTONS + 5).contains(&id) => app.on_button((id - ID_BUTTONS) as usize),
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(window);
            0
        }
        WM_DESTROY => {
            app.save();
            app.agent.cancel.store(true, Ordering::Relaxed);
            let _ = app.agent.commands.send(Command::Quit);
            for parked in &app.parked {
                parked.agent.cancel.store(true, Ordering::Relaxed);
                let _ = parked.agent.commands.send(Command::Quit);
            }
            app.engine = None;
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Keys that belong to the window whichever control has the focus. `true`: taken.
unsafe fn take_key(app: &mut App, message: &MSG) -> bool {
    if message.message != WM_KEYDOWN {
        return false;
    }
    let key = message.wParam as u16;
    let shift = GetKeyState(VK_SHIFT as i32) < 0;
    let control = GetKeyState(VK_CONTROL as i32) < 0;
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
    if control && key == 0x42 {
        // Ctrl+B
        app.toggle_right_panel();
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
                app.set_panel(Panel::Start);
                return true;
            }
            0xBC => {
                // Ctrl+,
                app.set_panel(Panel::Providers);
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
    if key == VK_ESCAPE {
        if app.pending.is_some() {
            app.answer_approval(Decision::Reject);
        } else if app.running {
            app.agent.cancel.store(true, Ordering::Relaxed);
        } else if app.panel != Panel::Chat && app.settings.workspace.is_some() {
            app.onboarding = false;
            app.set_panel(Panel::Chat);
        }
        return true;
    }
    if key == VK_RETURN && message.hwnd == app.composer && !shift {
        app.send_message();
        return true;
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

pub fn run(folder_argument: Option<PathBuf>) -> ExitCode {
    // SAFETY: the Win32 calls below run on this one thread, on handles they created; `APP` is set once before any message
    // reaches the procedure and is only used on this thread.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        prefer_dark_theme();
        start_gdiplus();
        register_button_class();
        let library = wide("Msftedit.dll");
        LoadLibraryW(library.as_ptr());

        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("DogmindApp");
        let background = CreateSolidBrush(BACKGROUND);
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
        let dark: i32 = 1;
        DwmSetWindowAttribute(window, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, std::ptr::addr_of!(dark).cast(), 4);
        // The title bar in the sidebar's colour, so that the window reads as one surface. Windows 10 ignores these.
        let caption = SIDEBAR;
        DwmSetWindowAttribute(window, DWMWA_CAPTION_COLOR as u32, std::ptr::addr_of!(caption).cast(), 4);
        let border = SIDEBAR;
        DwmSetWindowAttribute(window, DWMWA_BORDER_COLOR as u32, std::ptr::addr_of!(border).cast(), 4);

        let folder = settings::data_folder();
        let saved = Settings::load(&folder.join("settings.json"));
        let chosen = folder_argument.filter(|path| path.is_dir()).or_else(|| saved.workspace.clone().filter(|path| path.is_dir()));
        let needs_folder = chosen.is_none();
        // Until a folder is chosen the start panel stays over the chat, and nothing can be sent.
        let workspace = chosen.unwrap_or_else(|| Path::new(".").to_path_buf());

        let (event_sender, events) = channel();
        let (notes_sender, notes) = channel();
        let poster = Poster { notes: notes_sender, window: window as isize };
        let waker = poster.clone();
        let mode = match saved.mode.as_str() {
            "accept_edits" => Mode::AcceptEdits,
            "auto" => Mode::Auto,
            _ => Mode::Default,
        };
        let handle = agent::start(
            agent_config(&saved, &folder, workspace.clone(), mode),
            event_sender,
            Box::new(move || waker.wake()),
        );
        let _ = handle.commands.send(Command::SetSafeCommands(saved.safe_commands));

        let dpi = GetDpiForWindow(window);
        SetWindowPos(window, std::ptr::null_mut(), 0, 0, scale(dpi, 1200), scale(dpi, 800), SWP_NOMOVE | SWP_NOZORDER);
        // Each at the size it is shown, not one scaled down.
        SendMessageW(window, WM_SETICON, ICON_SMALL as WPARAM, window_icon(scale(dpi, 16)) as LPARAM);
        SendMessageW(window, WM_SETICON, ICON_BIG as WPARAM, window_icon(scale(dpi, 32)) as LPARAM);
        let fonts = Fonts::new(dpi);
        set_button_fonts(fonts);
        let list_style = WS_VSCROLL | WS_CLIPSIBLINGS | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | LBS_OWNERDRAWVARIABLE) as u32;
        let edit_style = WS_TABSTOP | ES_AUTOHSCROLL as u32;
        // What is found stays selected while the focus is in the find field.
        let transcript = rich_edit(window, WS_VSCROLL | WS_CLIPSIBLINGS | ES_READONLY as u32 | ES_NOHIDESEL as u32 | WS_TABSTOP, BACKGROUND, 11);
        SendMessageW(transcript, EM_SETEDITSTYLE, SES_HIDEGRIDLINES, SES_HIDEGRIDLINES as LPARAM);
        let composer = rich_edit(window, WS_VSCROLL | WS_TABSTOP | WS_CLIPSIBLINGS | ES_WANTRETURN as u32, SURFACE, 11);
        SendMessageW(composer, EM_SETTEXTMODE, (TM_PLAINTEXT | TM_MULTILEVELUNDO | TM_MULTICODEPAGE) as WPARAM, 0);
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
            brushes: [background, CreateSolidBrush(SIDEBAR), CreateSolidBrush(SURFACE), CreateSolidBrush(RAISED)],
            shapes: Vec::new(),
            new_session: make_button(window, ID_NEW_SESSION, "plus|New session", ButtonKind::Secondary, SIDEBAR),
            settings_button: make_button(window, ID_SETTINGS, "settings|Settings", ButtonKind::Ghost, SIDEBAR),
            side,
            side_rows: Vec::new(),
            side_hover: None,
            collapsed: HashSet::new(),
            all_sessions: Vec::new(),
            current_session: None,
            transcript,
            composer,
            composer_cue: control(window, "STATIC", "Message Dogmind...    Enter sends, Shift+Enter starts a new line", SS_NOPREFIX, 0, 0),
            send: make_button(window, ID_SEND, "arrow-up", ButtonKind::Round, SURFACE),
            model_chip: chip(ID_MODEL_CHIP, SURFACE),
            mode_chip: chip(ID_MODE_CHIP, SURFACE),
            effort_chip: chip(ID_EFFORT_CHIP, SURFACE),
            help_button: make_button(window, ID_HELP, "circle-question-mark", ButtonKind::Icon, SIDEBAR),
            about_button: make_button(window, ID_ABOUT, "info", ButtonKind::Icon, SIDEBAR),
            doc: rich_edit(window, WS_VSCROLL | ES_READONLY as u32, BACKGROUND, 10),
            copy_buttons: (0..COPY_POOL).map(|at| make_button(window, ID_COPY_POOL + at as isize, "copy", ButtonKind::Icon, BACKGROUND)).collect(),
            answers: Vec::new(),
            copy_targets: vec![None; COPY_POOL],
            transcript_area: rect(0, 0, 0, 0),
            shown: Vec::new(),
            replaying: false,
            transcript_rect: (0, 0, 0),
            text_twips: 9000,
            code_language: String::new(),
            composing: false,
            composer_procedure: 0,
            folder_hit: rect(0, 0, 0, 0),
            straydog: false,
            toggle_language: make_button(window, ID_TOGGLE_LANGUAGE, "", ButtonKind::Chip, BACKGROUND),
            edit_instructions: make_button(window, ID_EDIT_INSTRUCTIONS, "", ButtonKind::Chip, BACKGROUND),
            find_open: false,
            find_field: control(window, "EDIT", "", edit_style, 0, 0),
            find_previous: make_button(window, ID_FIND_PREVIOUS, "chevron-up", ButtonKind::Icon, SURFACE),
            find_next: make_button(window, ID_FIND_NEXT, "chevron-down", ButtonKind::Icon, SURFACE),
            find_close: make_button(window, ID_FIND_CLOSE, "x", ButtonKind::Icon, SURFACE),
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
            branch: None,
            session_title: String::new(),
            totals: (0, 0, 0, 0),
            side_target: None,
            rename_target: None,
            rename_seed: None,
            resume_paths: Vec::new(),
            parked: Vec::new(),
            provider: None,
            held_notices: HashMap::new(),
            turns: Vec::new(),
            rewind_rows: Vec::new(),
            shown_rules: Vec::new(),
            changes: Vec::new(),
            change_wanted: None,
            right_width: 0,
            right_toggle: make_button(window, ID_PANEL_TOGGLE, "panel-right", ButtonKind::Icon, BACKGROUND),
            right_hits: Vec::new(),
            todos: Vec::new(),
            git: None,
            web_seen: false,
            toggle_web: make_button(window, ID_TOGGLE_WEB, "", ButtonKind::Chip, BACKGROUND),
            context_auto: None,
            onboarding: false,
            setup_kind: Kind::Embedded,
            sidebar_width: 0,
            diff_top: 0,
            note: control(window, "EDIT", "", (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32, 0, 0),
            approve: make_button(window, ID_APPROVE, "Approve (Y)", ButtonKind::Primary, SURFACE),
            always: make_button(window, ID_ALWAYS, "Always (A)", ButtonKind::Secondary, SURFACE),
            reject: make_button(window, ID_REJECT, "Reject (N)", ButtonKind::Secondary, SURFACE),
            feedback: control(window, "EDIT", "", edit_style, 0, 0),
            activity_area: rect(0, 0, 0, 0),
            status_area: rect(0, 0, 0, 0),
            list,
            list_rows: Vec::new(),
            list_hover: None,
            fields: std::array::from_fn(|_| control(window, "EDIT", "", edit_style, 0, 0)),
            labels: std::array::from_fn(|_| String::new()),
            buttons: std::array::from_fn(|at| make_button(window, ID_BUTTONS + at as isize, "", ButtonKind::Secondary, BACKGROUND)),
            kind_chip: make_button(window, ID_KIND_CHIP, "", ButtonKind::Chip, BACKGROUND),
            drafter_chip: make_button(window, ID_DRAFTER, "", ButtonKind::Chip, BACKGROUND),
            tab_providers: make_button(window, ID_TAB_PROVIDERS, "Providers", ButtonKind::Tab, BACKGROUND),
            tab_models: make_button(window, ID_TAB_MODELS, "Models on this PC", ButtonKind::Tab, BACKGROUND),
            tab_general: make_button(window, ID_TAB_GENERAL, "General", ButtonKind::Tab, BACKGROUND),
            toggle_thinking: make_button(window, ID_TOGGLE_THINKING, "", ButtonKind::Chip, BACKGROUND),
            toggle_safe: make_button(window, ID_TOGGLE_SAFE, "", ButtonKind::Chip, BACKGROUND),
            panel_title: String::new(),
            panel_log: String::new(),
            progress: None,
            kind: Kind::Server,
            drafter: false,
            pending: None,
            tools_running: HashMap::new(),
            running: false,
            has_messages: false,
            queued: Vec::new(),
            workspace,
            answer: String::new(),
            last_answer: String::new(),
            pending_text: String::new(),
            line_start: true,
            in_code: false,
            heading: false,
            bold: false,
            inline_code: false,
            thinking: String::new(),
            activity: String::new(),
            dots: 0,
            connected: false,
            connection: String::new(),
            model: String::new(),
            used: 0,
            window_tokens: 0,
            rate: None,
            catalog: Vec::new(),
            start_folders: Vec::new(),
            setting_up: false,
            setup_cancel: Arc::new(AtomicBool::new(false)),
            setup_status: String::new(),
            selected_provider: None,
            list_procedure: 0,
            transcript_procedure: 0,
        });
        APP.store(Box::into_raw(app), Ordering::Relaxed);
        let app = &mut *APP.load(Ordering::Relaxed);
        SendMessageW(app.composer, EM_SETEVENTMASK, 0, ENM_CHANGE as LPARAM);
        for edit in app.fields.into_iter().chain([app.feedback, app.note, app.composer_cue, app.find_field, app.side_search]) {
            SendMessageW(edit, WM_SETFONT, app.fonts.body as WPARAM, 1);
        }
        let cue = wide("Why not? (optional, goes to the model with a reject)");
        SendMessageW(app.feedback, EM_SETCUEBANNER, 0, cue.as_ptr() as LPARAM);
        let cue = wide("Find in this conversation");
        SendMessageW(app.find_field, EM_SETCUEBANNER, 0, cue.as_ptr() as LPARAM);
        let cue = wide("Search sessions");
        SendMessageW(app.side_search, EM_SETCUEBANNER, 0, cue.as_ptr() as LPARAM);
        app.list_procedure = SetWindowLongPtrW(app.side, GWLP_WNDPROC, list_subclass as *const () as isize);
        SetWindowLongPtrW(app.list, GWLP_WNDPROC, list_subclass as *const () as isize);
        app.composer_procedure = SetWindowLongPtrW(app.composer, GWLP_WNDPROC, composer_subclass as *const () as isize);
        SetWindowLongPtrW(app.suggest, GWLP_WNDPROC, list_subclass as *const () as isize);
        app.transcript_procedure = SetWindowLongPtrW(app.transcript, GWLP_WNDPROC, transcript_subclass as *const () as isize);
        app.dark_controls();
        app.update_chips();
        app.layout();
        ShowWindow(window, SW_SHOW);
        SetTimer(window, TIMER_ENGINE, 1000, None);
        app.update_status();
        app.refresh_sessions();
        if !needs_folder {
            app.look_at_workspace();
            app.refresh_changes();
        }
        SetFocus(app.composer);
        // Someone who has used the app before setups came in has been through it, in effect.
        let first_run = !app.settings.set_up && app.settings.workspace.is_none() && !engine::is_set_up(&app.folder);
        if first_run {
            app.set_panel(Panel::Setup);
        } else {
            app.connect();
            if needs_folder {
                app.set_panel(Panel::Start);
            }
        }

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            if take_key(&mut *APP.load(Ordering::Relaxed), &message) {
                continue;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    ExitCode::SUCCESS
}
