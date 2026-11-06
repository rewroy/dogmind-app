//! A file of the workspace as a browser shows it, in the panel on the right, through the WebView2 that comes with Windows.
//! The app's own window has no browser in it: this one exists only while a preview is open, and its memory goes when it closes.
//!
//! What it shows was written by a model, perhaps after reading a stranger's page, so it is held like one: it sees the workspace
//! under a made-up host name and nothing else on this machine, and it cannot reach the network, leave that host, open a window,
//! download or ask for a permission. "Open in browser" is there for a page that needs more, and that is the person's own step.
//! Markdown and source files are written as a page here, with every character of the file escaped.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{AddScriptToExecuteOnDocumentCreatedCompletedHandler, CallDevToolsProtocolMethodCompletedHandler, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, DevToolsProtocolEventReceivedEventHandler, DownloadStartingEventHandler, ExecuteScriptCompletedHandler, LaunchingExternalUriSchemeEventHandler, NavigationCompletedEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler, PermissionRequestedEventHandler, WebResourceRequestedEventHandler};
use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::lang::{t, tf};
use crate::markdown;
use crate::paint::theme;

/// `.example` is reserved: no such host can exist anywhere else.
const HOST: &str = "dogmind-preview.example";
/// Where a written page finds the face shipped beside the app, on a Windows that has no copy of its own: the browser does not see the
/// fonts this process holds, and nothing is installed for it. The two files are served from here, and nothing else is.
const FONT_HOST: &str = "dogmind-fonts.example";

/// Every connection of the browser goes to a proxy that is not there, WebSockets and addresses by number too, which a filter on
/// requests would not see. The workspace's host is served inside the browser and does not go that way.
const CUT_OFF: &str = "--proxy-server=http://127.0.0.1:9 --proxy-bypass-list=<-loopback> --force-webrtc-ip-handling-policy=disable_non_proxied_udp --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --no-first-run --blink-settings=defaultTextEncodingName=UTF-8";

/// Run in every document before its own scripts: `window.print` is a function that does nothing, and cannot be written over or redefined by the page.
const NO_PRINT: &str = "(function(){var none=function print(){};try{Object.defineProperty(window,'print',{value:none,writable:false,configurable:false,enumerable:true});}catch(e){}})();";

/// The folder the person picks, in the Explorer's own window; none when they cancel.
pub unsafe fn choose_folder(owner: isize, title: &str) -> Option<PathBuf> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    dialog.SetOptions(dialog.GetOptions().ok()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
    dialog.SetTitle(&HSTRING::from(title)).ok()?;
    dialog.Show(Some(HWND(owner as _))).ok()?;
    let name = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let chosen = name.to_string().ok().map(PathBuf::from);
    CoTaskMemFree(Some(name.0 as *const _));
    chosen
}

/// The pictures the person picks to go with a message, in the Explorer's own window; none when they cancel.
pub unsafe fn choose_images(owner: isize, title: &str, kind_name: &str) -> Vec<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH};
    let chosen = || -> windows::core::Result<Vec<PathBuf>> {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(dialog.GetOptions()? | FOS_ALLOWMULTISELECT | FOS_FORCEFILESYSTEM)?;
        dialog.SetTitle(&HSTRING::from(title))?;
        let (name, spec) = (HSTRING::from(kind_name), HSTRING::from("*.png;*.jpg;*.jpeg;*.gif;*.webp"));
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) }])?;
        dialog.Show(Some(HWND(owner as _)))?;
        let items = dialog.GetResults()?;
        let mut paths = Vec::new();
        for at in 0..items.GetCount()? {
            let name = items.GetItemAt(at)?.GetDisplayName(SIGDN_FILESYSPATH)?;
            if let Ok(path) = name.to_string() {
                paths.push(PathBuf::from(path));
            }
            CoTaskMemFree(Some(name.0 as *const _));
        }
        Ok(paths)
    };
    chosen().unwrap_or_default()
}

/// How many files a page's requests are kept for, to look at their times.
const MOST_WATCHED: usize = 500;

/// The workspace files the page being shown has asked for, as the browser's requests say it: what a change to would show in the page.
#[derive(Default)]
struct Requested {
    root: Option<PathBuf>,
    files: Vec<PathBuf>,
}

impl Requested {
    fn note(&mut self, address: &str) {
        let Some(file) = self.root.as_deref().and_then(|root| path_of(root, address)) else { return };
        if self.files.len() < MOST_WATCHED && !self.files.contains(&file) {
            self.files.push(file);
        }
    }
}

/// A device name Windows opens in place of a file of that name, with or without an extension.
fn is_a_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit())
}

/// The file a request of the made-up host is for, under `root`: the reverse of [`address_of`]. None for another host, and for any address that does not name a file below `root` by plain
/// names (a part that is `..`, holds a separator or a drive, ends in a dot or a space, or is a device).
pub fn path_of(root: &Path, address: &str) -> Option<PathBuf> {
    let rest = address.get(..8).filter(|scheme| scheme.eq_ignore_ascii_case("https://")).map(|_| &address[8..])?;
    let (host, path) = rest.split_once('/')?;
    if !host.eq_ignore_ascii_case(HOST) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut file = root.to_path_buf();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let mut bytes = Vec::with_capacity(part.len());
        let mut at = part.bytes();
        while let Some(byte) = at.next() {
            if byte == b'%' {
                let (high, low) = (at.next()?, at.next()?);
                bytes.push(u8::from_str_radix(std::str::from_utf8(&[high, low]).ok()?, 16).ok()?);
            } else {
                bytes.push(byte);
            }
        }
        let name = String::from_utf8(bytes).ok()?;
        if name == "." || name == ".." || name.contains(['/', '\\', ':', '\0', '*', '?', '"', '<', '>', '|']) || name.ends_with(['.', ' ']) || is_a_device(&name) {
            return None;
        }
        file.push(name);
    }
    (file != root).then_some(file)
}

/// The address a `Network.requestWillBeSent` event of the browser is for.
fn requested_address(parameters: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(parameters).ok()?;
    event.pointer("/request/url")?.as_str().map(str::to_string)
}

/// When what a page loaded last changed: the file itself and the files it asked for. None when the file is gone.
pub fn newest_of(file: &Path, loaded: &[PathBuf]) -> Option<std::time::SystemTime> {
    let mut newest = std::fs::metadata(file).and_then(|data| data.modified()).ok()?;
    for other in loaded {
        if let Ok(written) = std::fs::metadata(other).and_then(|data| data.modified()) {
            newest = newest.max(written);
        }
    }
    Some(newest)
}

/// What the look at the shown file found, against what it found before.
#[derive(Debug, PartialEq, Eq)]
pub enum Seen {
    Same,
    /// Written again: shown again.
    Changed,
    /// The file is not there any more: the old page does not stay.
    Gone,
}

pub fn seen_since(before: Option<std::time::SystemTime>, now: Option<std::time::SystemTime>) -> Seen {
    match (before, now) {
        (_, None) => Seen::Gone,
        (before, Some(now)) if before != Some(now) => Seen::Changed,
        _ => Seen::Same,
    }
}

/// What the preview can show. Anything else is not for this panel.
/// When anything a page can use last changed: the file, what lies beside it and one folder down (its styles, scripts and pictures). What a page that has loaded is watched by is what it asked for
/// ([`newest_of`]); this is for one that has asked for nothing yet.
pub fn newest_beside(file: &Path) -> Option<std::time::SystemTime> {
    const MOST: usize = 2000;
    let own = std::fs::metadata(file).and_then(|data| data.modified()).ok()?;
    let mut newest = own;
    let mut seen = 0;
    let folder = file.parent()?;
    let mut note = |entry: &std::fs::DirEntry| {
        seen += 1;
        if let Ok(written) = entry.metadata().and_then(|data| data.modified()) {
            newest = newest.max(written);
        }
    };
    for entry in std::fs::read_dir(folder).ok()?.flatten().take(MOST) {
        let name = entry.file_name();
        let kept_out = matches!(name.to_str(), Some(".git" | "node_modules" | "target" | ".venv"));
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if !kept_out {
                for inner in std::fs::read_dir(entry.path()).into_iter().flatten().flatten().take(MOST) {
                    if inner.file_type().is_ok_and(|kind| kind.is_file()) {
                        note(&inner);
                    }
                }
            }
        } else {
            note(&entry);
        }
    }
    Some(newest)
}

/// What the browser shows as it is.
fn shows_as_it_is(path: &Path) -> bool {
    const SHOWN: [&str; 10] = ["html", "htm", "svg", "png", "jpg", "jpeg", "gif", "webp", "bmp", "pdf"];
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| SHOWN.contains(&extension.to_ascii_lowercase().as_str()))
}

/// What this app writes a page for.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Written {
    Markdown,
    /// In the colours of this language, when `highlight` knows it.
    Source(String),
    /// A CSV (`,`) or TSV (`\t`) file as a table (`table`).
    Table(char),
    /// An Excel workbook's first sheet as a table (`office`).
    Workbook,
}

pub fn written(path: &Path) -> Option<Written> {
    const SOURCE: [&str; 43] = ["txt", "log", "json", "jsonc", "toml", "yaml", "yml", "ini", "cfg", "conf", "xml", "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "c", "h", "cc", "cpp", "hpp", "cs", "java", "kt", "go", "swift", "rb", "php", "scala", "dart", "sh", "bash", "zsh", "ps1", "bat", "cmd", "sql", "css", "scss", "vue", "lua"];
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if matches!(name.as_str(), "makefile" | "dockerfile") {
        return Some(Written::Source(name));
    }
    let extension = name.rsplit_once('.')?.1.to_string();
    match extension.as_str() {
        "md" | "markdown" => Some(Written::Markdown),
        "csv" => Some(Written::Table(',')),
        "tsv" => Some(Written::Table('\t')),
        "xlsx" | "xlsm" => Some(Written::Workbook),
        known if SOURCE.contains(&known) => Some(Written::Source(extension)),
        _ => None,
    }
}

/// What the preview can show. Anything else is not for this panel.
pub fn can_show(path: &str) -> bool {
    let path = Path::new(path);
    shows_as_it_is(path) || written(path).is_some()
}

/// What is there to be looked at and not only read as code, and has a place of its own in the panel on the right.
pub fn is_page(path: &str) -> bool {
    let path = Path::new(path);
    shows_as_it_is(path) || matches!(written(path), Some(Written::Markdown | Written::Table(_) | Written::Workbook))
}

/// A colour as a page writes it.
fn css(color: u32) -> String {
    format!("#{:02x}{:02x}{:02x}", color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF)
}

/// A file as a page: nothing runs in it, and only pictures of its own folder are let in. What it refers to is under `base`.
fn page_of(body: &str, base: &str) -> String {
    const SHEET: &str = "html{color-scheme:SCHEME}*{box-sizing:border-box}body{margin:0;padding:18px 20px;background:BACK;color:TEXT;font:15px/1.65 'Segoe UI Variable Text','BIZ UDGothic','Yu Gothic UI','Segoe UI',sans-serif;overflow-wrap:anywhere}h1,h2,h3,h4,h5,h6{line-height:1.3;margin:1.4em 0 .5em}h1{font-size:1.6em;border-bottom:1px solid LINE;padding-bottom:.25em}h2{font-size:1.3em}h3{font-size:1.1em}h1:first-child,h2:first-child,h3:first-child{margin-top:0}p{margin:0 0 .9em}a{color:ACCENT}ul,ol{margin:0 0 .9em;padding-left:1.6em}li{margin:.2em 0}blockquote{margin:0 0 .9em;padding:.1em 0 .1em 1em;border-left:3px solid LINE;color:MUTED}hr{border:0;border-top:1px solid LINE;margin:1.2em 0}img{max-width:100%}code{font:.9em Consolas,monospace;background:CODE;padding:.1em .3em;border-radius:4px}pre{background:CODE;padding:12px 14px;border-radius:8px;overflow:auto;margin:0 0 .9em}pre code{background:none;padding:0;font-size:13px;line-height:1.5}table{border-collapse:collapse;margin:0 0 .9em}th,td{border:1px solid LINE;padding:.3em .7em}th{background:CODE}.k{color:KEYWORD}.s{color:STRING}.c{color:MUTED}.u{color:MUTED}.n{color:NUMBER}pre.source{margin:0;padding:0;background:none;font:13px/1.55 Consolas,monospace;counter-reset:n;white-space:pre}pre.source .l{display:block;min-height:1.55em;counter-increment:n}pre.source .l::before{content:counter(n);display:inline-block;width:3.2em;margin-right:1em;text-align:right;color:FAINT;user-select:none}pre.source .m{background:MARK}pre.source .m:focus{outline:none}.cut{color:MUTED;font-size:13px;margin-top:1em}table.data{font-size:13px;margin:0}table.data th{position:sticky;top:0;text-align:left}table.data td.n{text-align:right;font-variant-numeric:tabular-nums}";
    let sheet = SHEET.replace("SCHEME", if theme::is_light() { "light" } else { "dark" })
        .replace("BACK", &css(theme::background())).replace("TEXT", &css(theme::text())).replace("LINE", &css(theme::line())).replace("ACCENT", &css(theme::accent_hover()))
        .replace("MUTED", &css(theme::muted())).replace("FAINT", &css(theme::faint())).replace("CODE", &css(theme::code_back()))
        .replace("MARK", &css(theme::raised())).replace("KEYWORD", &css(theme::accent_hover())).replace("STRING", &css(theme::good())).replace("NUMBER", &css(theme::warn()));
    let allowed = format!("default-src 'none'; style-src 'unsafe-inline'; img-src https://{HOST} data:; font-src https://{FONT_HOST}");
    format!("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{allowed}\"><base href=\"{base}\"><style>{}{sheet}</style></head><body>{body}</body></html>", shipped_faces())
}

/// The shipped face for a written page: Windows's own copy where there is one, by either of the names it may go by, and else the file
/// beside the app.
fn shipped_faces() -> String {
    let [regular, bold] = crate::paint::BUNDLED_FILES;
    let face = crate::paint::BUNDLED_FACE;
    // (a test's way to see the file used on a Windows that has its own copy)
    let local = |names: [&str; 2]| if cfg!(debug_assertions) && std::env::var_os("DOGMIND_TEST_FONT_BY_ADDRESS").is_some() { String::new() } else { format!("local('{}'),local('{}'),", names[0], names[1]) };
    format!(
        "@font-face{{font-family:'{face}';src:{}url(https://{FONT_HOST}/{regular});font-weight:400}}@font-face{{font-family:'{face}';src:{}url(https://{FONT_HOST}/{bold});font-weight:700}}",
        local([face, "BIZUDGothic-Regular"]),
        local(["BIZ UDGothic Bold", "BIZUDGothic-Bold"])
    )
}

/// The shipped font file a request is for, when it names one of them exactly.
fn shipped_font(address: &str) -> Option<PathBuf> {
    let rest = address.get(..8).filter(|scheme| scheme.eq_ignore_ascii_case("https://")).map(|_| &address[8..])?;
    let (host, name) = rest.split_once('/')?;
    if !host.eq_ignore_ascii_case(FONT_HOST) {
        return None;
    }
    let name = crate::paint::BUNDLED_FILES.into_iter().find(|file| file.eq_ignore_ascii_case(name))?;
    crate::paint::font_folders().into_iter().map(|folder| folder.join(name)).find(|file| file.is_file())
}

/// The page for a file of the workspace. NavigateToString takes no more than 2 MB, so a long file is shown from its start, and says so.
fn written_page(root: &Path, file: &Path, kind: &Written, marked: Option<(usize, usize)>) -> Result<String, String> {
    const MOST_TEXT: usize = 256 * 1024;
    const MOST_PAGE: usize = 1800 * 1024;
    let bytes = std::fs::read(file).map_err(|error| format!("{}: {error}", file.display()))?;
    // A workbook is a zip of XML, not text: its first sheet as a table, and the names of all its sheets.
    if *kind == Written::Workbook {
        let (rows, sheets, total) = crate::office::first_sheet(&bytes, crate::table::MOST_ROWS, crate::table::MOST_COLUMNS)
            .map_err(|error| tf("This workbook cannot be shown: {0}", &[&error]))?;
        let mut body = String::new();
        if sheets.len() > 1 {
            body.push_str(&format!("<p class=\"cut\">{}</p>", markdown::escape(&tf("Sheets: {0} (the first is shown)", &[&sheets.join(", ")]))));
        }
        body.push_str(&crate::table::html_of(&rows, total, &|rows, columns| {
            tf("{0} rows and {1} columns: the first {2} rows and {3} columns are shown.", &[&rows.to_string(), &columns.to_string(),
                &crate::table::MOST_ROWS.min(rows).to_string(), &crate::table::MOST_COLUMNS.min(columns).to_string()])
        }));
        // (as a text's page: what NavigateToString takes; the cells were bounded, so this is a last guard)
        if body.len() > MOST_PAGE {
            let mut cut = MOST_PAGE;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push_str(&format!("</table><p class=\"cut\">{}</p>", markdown::escape(&tf("Only the first {0} KB are shown.", &[&(MOST_PAGE / 1024)]))));
        }
        let base = file.parent().and_then(|folder| address_of(root, folder)).unwrap_or_else(|| format!("https://{HOST}"));
        return Ok(page_of(&body, &format!("{base}/")));
    }
    // (as the agent reads a file: what PowerShell's > writes is UTF-16, and a Japanese Windows keeps much of its text in Shift_JIS)
    let Some((whole, encoding)) = dogmind_agent::tools::text_of_file(&bytes) else {
        return Err(t("This file is not text, so it has no preview.").to_string());
    };
    let mut end = whole.len().min(MOST_TEXT);
    while !whole.is_char_boundary(end) {
        end -= 1;
    }
    let text = &whole[..end];
    let mut body = match kind {
        Written::Markdown => markdown::to_html(text),
        Written::Source(language) => markdown::source_html(text, language, marked),
        Written::Workbook => String::new(),
        Written::Table(separator) => crate::table::html(text, *separator, &|rows, columns| {
            tf("{0} rows and {1} columns: the first {2} rows and {3} columns are shown.", &[&rows.to_string(), &columns.to_string(),
                &crate::table::MOST_ROWS.min(rows).to_string(), &crate::table::MOST_COLUMNS.min(columns).to_string()])
        }),
    };
    if body.len() > MOST_PAGE {
        // (colour takes room: plain, it fits)
        body = markdown::source_html(text, "", marked);
    }
    if end < whole.len() {
        body.push_str(&format!("<p class=\"cut\">{}</p>", markdown::escape(&tf("Only the first {0} KB are shown.", &[&(MOST_TEXT / 1024)]))));
    }
    // (what is not UTF-8 says what it was read as: a Shift_JIS file that is shown right is not seen to be one otherwise)
    if encoding != "utf-8" {
        let name = match encoding {
            "utf-16" => "UTF-16".to_string(),
            "shift_jis" => "Shift_JIS".to_string(),
            _ => t("an encoding it does not know").to_string(),
        };
        body.push_str(&format!("<p class=\"cut\">{}</p>", markdown::escape(&tf("Shown as {0}.", &[&name]))));
    }
    let base = file.parent().and_then(|folder| address_of(root, folder)).unwrap_or_else(|| format!("https://{HOST}"));
    Ok(page_of(&body, &format!("{base}/")))
}

/// The address of `file` under the made-up host for `root`, or none when it is not inside it.
pub fn address_of(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let mut address = format!("https://{HOST}");
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else { return None };
        address.push('/');
        for byte in part.to_string_lossy().bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => address.push(byte as char),
                _ => address.push_str(&format!("%{byte:02X}")),
            }
        }
    }
    Some(address)
}

/// Whether a request stays on the workspace's host, or holds its own content.
pub fn stays_inside(address: &str) -> bool {
    let lower = address.to_ascii_lowercase();
    let inside = format!("https://{HOST}/");
    lower.starts_with(&inside) || lower == inside.trim_end_matches('/') || lower.starts_with("data:") || lower.starts_with("blob:") || lower == "about:blank" || lower == "about:srcdoc"
}

pub struct Preview {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    /// Requests that were turned away since the page was opened.
    pub blocked: Rc<Cell<u32>>,
    /// What is held against the page, by name.
    pub guards: Vec<&'static str>,
    root: RefCell<Option<PathBuf>>,
    /// The folder and the file shown now, and the lines a link named in it.
    shown: RefCell<Option<(PathBuf, PathBuf, Option<(usize, usize)>)>>,
    /// What the page asked for, from the made-up host.
    requested: Rc<RefCell<Requested>>,
    /// The page was taken away because its file was gone: what is in the browser is nothing, and a reload opens the file again.
    cleared: Cell<bool>,
    /// The page coming has lines a link named: once it has loaded, the view goes to them.
    to_lines: Rc<Cell<bool>>,
    environment: Rc<RefCell<Option<ICoreWebView2Environment>>>,
}

// What has been made so far, for the window to pick up on `done`: a browser starts on its own time.
thread_local! {
    static MADE: RefCell<Option<Result<Preview, String>>> = const { RefCell::new(None) };
}

pub fn take_made() -> Option<Result<Preview, String>> {
    MADE.with(|made| made.borrow_mut().take())
}

fn text_of(mut filled: impl FnMut(&mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut text = PWSTR::null();
    if filled(&mut text).is_err() || text.is_null() {
        return String::new();
    }
    // SAFETY: the browser gave this string for the caller to free.
    unsafe {
        let out = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0 as *const _));
        out
    }
}

/// Starts making one in `parent`, with its files under `data`. `done` runs on this thread when it is ready or has failed, and
/// [`take_made`] then has the result.
pub fn make(parent: isize, data: &Path, done: impl Fn() + Clone + 'static) -> Result<(), String> {
    let options = CoreWebView2EnvironmentOptions::default();
    // SAFETY: the options live until the call below has taken them.
    unsafe { options.set_additional_browser_arguments(CUT_OFF.to_string()) };
    let folder = HSTRING::from(data.as_os_str());
    let guards_file = data.join("guards.txt");
    let failed = {
        let done = done.clone();
        move |error: String| {
            MADE.with(|made| *made.borrow_mut() = Some(Err(error)));
            done();
        }
    };
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |code, environment| {
        let failed = failed.clone();
        let done = done.clone();
        let Some(environment) = environment.filter(|_| code.is_ok()) else {
            failed(format!("{code:?}"));
            return Ok(());
        };
        let held = environment.clone();
        let on_failure = failed.clone();
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |code, controller| {
            let Some(controller) = controller.filter(|_| code.is_ok()) else {
                failed(format!("{code:?}"));
                return Ok(());
            };
            // SAFETY: on the thread that made it, as everything here is.
            match unsafe { set_up(controller, &held) } {
                Ok(preview) => {
                    // (a debug build says what it holds against the page, for whoever looks in it from outside)
                    if cfg!(debug_assertions) {
                        let _ = std::fs::write(guards_file.as_path(), preview.guards.join("\n"));
                    }
                    MADE.with(|made| *made.borrow_mut() = Some(Ok(preview)));
                    done();
                }
                Err(error) => failed(error.to_string()),
            }
            Ok(())
        }));
        // SAFETY: the window lives as long as the app, which outlives this.
        if let Err(error) = unsafe { environment.CreateCoreWebView2Controller(HWND(parent as _), &handler) } {
            on_failure(error.to_string());
        }
        Ok(())
    }));
    // SAFETY: the strings and the handler live through the call.
    unsafe { CreateCoreWebView2EnvironmentWithOptions(&HSTRING::new(), &folder, &ICoreWebView2EnvironmentOptions::from(options), &handler) }.map_err(|error| error.to_string())
}

unsafe fn set_up(controller: ICoreWebView2Controller, environment: &ICoreWebView2Environment) -> windows::core::Result<Preview> {
    let webview = controller.CoreWebView2()?;
    let settings = webview.Settings()?;
    settings.SetAreHostObjectsAllowed(false)?;
    settings.SetIsWebMessageEnabled(false)?;
    settings.SetAreDefaultScriptDialogsEnabled(false)?;
    settings.SetIsStatusBarEnabled(false)?;
    settings.SetAreDevToolsEnabled(false)?;
    if let Ok(more) = settings.cast::<ICoreWebView2Settings4>() {
        more.SetIsPasswordAutosaveEnabled(false)?;
        more.SetIsGeneralAutofillEnabled(false)?;
    }
    // The browser's own keys (print, save the page, find, reload, zoom) are the page's to ask for and not the person's by accident: the print dialog's "Save as PDF" writes wherever it is pointed.
    let mut keys_off = false;
    if let Ok(more) = settings.cast::<ICoreWebView2Settings3>() {
        more.SetAreBrowserAcceleratorKeysEnabled(false)?;
        keys_off = true;
    }
    let blocked = Rc::new(Cell::new(0u32));
    let mut token = 0i64;
    // What is held against the page, by name, for the log line of a debug build and for the tests.
    let mut guards: Vec<&'static str> = vec!["navigation", "new_window", "resources", "permissions", "downloads"];

    // A page does not leave its host, whether the link is its own idea or a click. A frame's navigation is not a top-level one, and is another event.
    let count = blocked.clone();
    webview.add_NavigationStarting(&NavigationStartingEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let address = text_of(|text| args.Uri(text));
            if !stays_inside(&address) {
                args.SetCancel(true)?;
                count.set(count.get() + 1);
            }
        }
        Ok(())
    })), &mut token)?;
    let count = blocked.clone();
    webview.add_FrameNavigationStarting(&NavigationStartingEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let address = text_of(|text| args.Uri(text));
            if !stays_inside(&address) {
                args.SetCancel(true)?;
                count.set(count.get() + 1);
            }
        }
        Ok(())
    })), &mut token)?;
    guards.push("frame_navigation");
    if keys_off {
        guards.push("browser_keys_off");
    }
    // A scheme the system hands to another program (mailto:, ms-settings:, a custom one): never launched, whatever the person might have answered the runtime's own question. (A runtime too old for
    // the interface has no such event, and the other checks are what there is.)
    if let Ok(more) = webview.cast::<ICoreWebView2_18>() {
        let count = blocked.clone();
        more.add_LaunchingExternalUriScheme(&LaunchingExternalUriSchemeEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                args.SetCancel(true)?;
                count.set(count.get() + 1);
            }
            Ok(())
        })), &mut token)?;
        guards.push("external_scheme");
    }
    let count = blocked.clone();
    webview.add_NewWindowRequested(&NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            args.SetHandled(true)?;
            count.set(count.get() + 1);
        }
        Ok(())
    })), &mut token)?;
    // Refused at once, and counted, so that the panel can say why a page looks bare.
    webview.AddWebResourceRequestedFilter(&HSTRING::from("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
    // Held here and let go on closing: a handler that kept it would keep the browser's processes alive.
    let environment = Rc::new(RefCell::new(Some(environment.clone())));
    let (count, responses) = (blocked.clone(), environment.clone());
    let requested = Rc::new(RefCell::new(Requested::default()));
    webview.add_WebResourceRequested(&WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let request = args.Request()?;
            let address = text_of(|text| request.Uri(text));
            if let Some(file) = shipped_font(&address) {
                if let Some(responses) = responses.borrow().as_ref() {
                    let stream = windows::Win32::UI::Shell::SHCreateStreamOnFileEx(&HSTRING::from(file.as_os_str()), (windows::Win32::System::Com::STGM_READ | windows::Win32::System::Com::STGM_SHARE_DENY_WRITE).0, 0, false, None)?;
                    // (asked for as a font is, across origins: the page's own is none)
                    let response = responses.CreateWebResourceResponse(&stream, 200, &HSTRING::from("OK"), &HSTRING::from("Content-Type: font/ttf\r\nAccess-Control-Allow-Origin: *"))?;
                    args.SetResponse(&response)?;
                }
                return Ok(());
            }
            if !stays_inside(&address) {
                if let Some(responses) = responses.borrow().as_ref() {
                    let response = responses.CreateWebResourceResponse(None, 403, &HSTRING::from("Blocked by Dogmind's preview"), &HSTRING::new())?;
                    args.SetResponse(&response)?;
                }
                count.set(count.get() + 1);
            }
        }
        Ok(())
    })), &mut token)?;
    webview.add_PermissionRequested(&PermissionRequestedEventHandler::create(Box::new(|_, args| {
        if let Some(args) = args {
            args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)?;
        }
        Ok(())
    })), &mut token)?;
    if let Ok(downloads) = webview.cast::<ICoreWebView2_4>() {
        downloads.add_DownloadStarting(&DownloadStartingEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                args.SetCancel(true)?;
            }
            Ok(())
        })), &mut token)?;
    }
    // A page's own call to print is the same door as the key: it does nothing, in every document and every frame, before any script of the page runs.
    webview.AddScriptToExecuteOnDocumentCreated(&HSTRING::from(NO_PRINT), &AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(|_, _| Ok(()))))?;
    guards.push("print_off");
    // What was just written is what is shown, not what the cache has.
    let answered = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
    webview.CallDevToolsProtocolMethod(&HSTRING::from("Network.setCacheDisabled"), &HSTRING::from(r#"{"cacheDisabled":true}"#), &answered)?;
    // What the page asks for, to know what a change would show in it. The handler of resource requests is never asked about the workspace's own host (it is served inside the browser), so
    // this comes from the browser's network events, which see every request of the page and its frames.
    let asked = requested.clone();
    let receiver = webview.GetDevToolsProtocolEventReceiver(&HSTRING::from("Network.requestWillBeSent"))?;
    receiver.add_DevToolsProtocolEventReceived(&DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let parameters = text_of(|text| args.ParameterObjectAsJson(text));
            if let Some(address) = requested_address(&parameters) {
                asked.borrow_mut().note(&address);
            }
        }
        Ok(())
    })), &mut token)?;
    webview.CallDevToolsProtocolMethod(&HSTRING::from("Network.enable"), &HSTRING::from("{}"), &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(()))))?;
    guards.push("watches_requests");
    // A source file opened at a link's lines is shown at them: the page has no script of its own, so this comes from here once it has loaded.
    let to_lines = Rc::new(Cell::new(false));
    let wanted = to_lines.clone();
    webview.add_NavigationCompleted(&NavigationCompletedEventHandler::create(Box::new(move |sender, _| {
        if wanted.replace(false) {
            if let Some(webview) = sender {
                let done = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
                webview.ExecuteScript(&HSTRING::from("var here = document.getElementById('here'); if (here) { here.scrollIntoView({ block: 'center' }); }"), &done)?;
            }
        }
        Ok(())
    })), &mut token)?;
    Ok(Preview { controller, webview, blocked, guards, root: RefCell::new(None), shown: RefCell::new(None), requested, cleared: Cell::new(false), to_lines, environment })
}

impl Preview {
    /// Shows `file`, under `root`: only what is inside `root` can be reached from it. `lines`: those a link named, lit and in view, in a
    /// source file.
    pub unsafe fn open(&self, root: &Path, file: &Path, lines: Option<(usize, usize)>) -> Result<(), String> {
        let address = address_of(root, file).ok_or_else(|| format!("{} is not in the workspace", file.display()))?;
        let mapped = self.webview.cast::<ICoreWebView2_3>().map_err(|error| format!("this WebView2 is too old for a preview: {error}"))?;
        let host = HSTRING::from(HOST);
        if self.root.borrow().as_deref() != Some(root) {
            let _ = mapped.ClearVirtualHostNameToFolderMapping(&host);
            mapped.SetVirtualHostNameToFolderMapping(&host, &HSTRING::from(root.as_os_str()), COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS).map_err(|error| error.to_string())?;
            *self.root.borrow_mut() = Some(root.to_path_buf());
        }
        self.blocked.set(0);
        self.cleared.set(false);
        *self.requested.borrow_mut() = Requested { root: Some(root.to_path_buf()), files: Vec::new() };
        *self.shown.borrow_mut() = Some((root.to_path_buf(), file.to_path_buf(), lines));
        match written(file) {
            Some(kind) => {
                let page = written_page(root, file, &kind, lines)?;
                self.to_lines.set(lines.is_some() && matches!(kind, Written::Source(_)));
                self.background(theme::background());
                self.webview.NavigateToString(&HSTRING::from(page)).map_err(|error| error.to_string())
            }
            None => {
                // (a page that sets no background of its own means white)
                self.background(crate::paint::rgb(255, 255, 255));
                self.webview.Navigate(&HSTRING::from(address)).map_err(|error| error.to_string())
            }
        }
    }

    /// When what the page shown now loaded last changed: the file and what the page asked for, wherever in the workspace it lies. Before it has asked for anything, the folder's look.
    pub fn newest_loaded(&self, file: &Path) -> Option<std::time::SystemTime> {
        let loaded = self.requested.borrow().files.clone();
        if loaded.is_empty() { newest_beside(file) } else { newest_of(file, &loaded) }
    }

    /// How many files the page shown now has asked for.
    pub fn loaded_count(&self) -> usize {
        self.requested.borrow().files.len()
    }

    /// The file is gone: the old page does not stay, and the panel says so.
    pub unsafe fn clear(&self) {
        self.cleared.set(true);
        *self.requested.borrow_mut() = Requested { root: self.root.borrow().clone(), files: Vec::new() };
        let _ = self.webview.Navigate(&HSTRING::from("about:blank"));
    }

    /// What shows behind a page, before it comes too.
    unsafe fn background(&self, color: u32) {
        if let Ok(controller) = self.controller.cast::<ICoreWebView2Controller2>() {
            let _ = controller.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: (color & 0xFF) as u8, G: ((color >> 8) & 0xFF) as u8, B: ((color >> 16) & 0xFF) as u8 });
        }
    }

    pub unsafe fn reload(&self) {
        self.blocked.set(0);
        // (a page written here is written again: the browser has only a string of it)
        let written_now = self.shown.borrow().clone().filter(|(_, file, _)| self.cleared.get() || written(file).is_some());
        match written_now {
            Some((root, file, lines)) => {
                let _ = self.open(&root, &file, lines);
            }
            None => {
                let _ = self.webview.Reload();
            }
        }
    }

    /// The theme has changed: a page written here follows it.
    pub unsafe fn retheme(&self) {
        if self.shown.borrow().as_ref().is_some_and(|(_, file, _)| written(file).is_some()) {
            self.reload();
        }
    }

    pub unsafe fn place(&self, left: i32, top: i32, right: i32, bottom: i32) {
        let _ = self.controller.SetBounds(RECT { left, top, right, bottom });
        let _ = self.controller.SetIsVisible(true);
    }

    pub unsafe fn hide(&self) {
        let _ = self.controller.SetIsVisible(false);
    }

    /// The window moved: a browser draws in a process of its own and has to be told.
    pub unsafe fn moved(&self) {
        let _ = self.controller.NotifyParentWindowPositionChanged();
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        *self.environment.borrow_mut() = None;
        // SAFETY: on the thread that made it. Its processes end with this.
        let _ = unsafe { self.controller.Close() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_page_shows_what_hides_in_a_file_and_shows_the_rest_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, "# 見出し \u{202e}tluser\n\n```rust\nlet a = \"\u{2066}\";\n```\n\nplain 👍🏽 text\n").unwrap();
        let page = written_page(dir.path(), &file, &Written::Markdown, None).unwrap();
        assert!(!page.contains('\u{202e}') && !page.contains('\u{2066}'), "none of it reaches the page");
        assert!(page.contains("&lt;U+202E&gt;") && page.contains("&lt;U+2066&gt;") && page.contains("class=\"u\""), "{page}");
        assert!(page.contains("見出し") && page.contains("👍🏽"), "what is ordinary is as it was");
        let source = dir.path().join("a.rs");
        std::fs::write(&source, "fn main() { // \u{202e}\n}\n").unwrap();
        let page = written_page(dir.path(), &source, &Written::Source("rs".to_string()), None).unwrap();
        assert!(!page.contains('\u{202e}') && page.contains("&lt;U+202E&gt;"), "{page}");
        assert!(page.contains(".u{color:"), "the mark has the muted colour of the page");
    }

    #[test]
    fn it_shows_pages_pictures_text_and_nothing_else() {
        assert!(can_show("a/index.HTML") && can_show("b.svg") && can_show("c.png"));
        assert!(can_show("README.md") && can_show("src/main.rs") && can_show("Makefile") && can_show("notes.TXT"));
        assert!(!can_show("noextension") && !can_show("a.exe") && !can_show("b.zip"));
        assert!(is_page("a.html") && is_page("a.png") && is_page("README.MD"), "what has a place of its own in the panel");
        assert!(!is_page("src/main.rs") && !is_page("a.txt"));
        assert_eq!(written(Path::new("a/b.markdown")), Some(Written::Markdown));
        assert_eq!(written(Path::new("a/B.RS")), Some(Written::Source("rs".to_string())));
        assert_eq!(written(Path::new("Dockerfile")), Some(Written::Source("dockerfile".to_string())));
        assert_eq!(written(Path::new("a.html")), None, "the browser's own");
    }

    #[test]
    fn a_written_page_lets_nothing_run_and_finds_its_pictures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("doc")).unwrap();
        let file = root.join("doc").join("a.md");
        std::fs::write(&file, "# t\n\n<script>alert(1)</script>\n\n![p](p.png)\n").unwrap();
        let page = written_page(root, &file, &Written::Markdown, None).unwrap();
        assert!(!page.contains("<script>") && page.contains("&lt;script&gt;"), "{page}");
        assert!(page.contains("<base href=\"https://dogmind-preview.example/doc/\">"), "{page}");
        assert!(page.contains("default-src 'none'") && page.contains("<img src=\"p.png\""), "{page}");
        assert!(!page.contains("BACK") && !page.contains("SCHEME"), "every colour is filled in");
    }

    /// A text file that PowerShell's `>` wrote (UTF-16 LE with its mark) and one that Excel or an older editor wrote in Shift_JIS are shown with their characters, and say what they were read as.
    #[test]
    fn a_utf16_file_and_a_shift_jis_file_are_shown_with_their_own_characters() {
        let dir = tempfile::tempdir().unwrap();
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "# 見出し\r\n\r\n本文です\r\n".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let wide = dir.path().join("wide.md");
        std::fs::write(&wide, &utf16).unwrap();
        let page = written_page(dir.path(), &wide, &Written::Markdown, None).unwrap();
        assert!(page.contains("見出し") && page.contains("本文です") && page.contains("UTF-16"), "{page}");
        // ("日本語,売上" and its row, as Excel writes a CSV on a Japanese Windows)
        let sjis = dir.path().join("sales.csv");
        std::fs::write(&sjis, [0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, b',', 0x94, 0x84, 0x8F, 0xE3, b'\r', b'\n', b'1', b',', b'2', b'\r', b'\n']).unwrap();
        assert_eq!(written(&sjis), Some(Written::Table(',')), "a CSV is a table");
        let page = written_page(dir.path(), &sjis, &Written::Table(','), None).unwrap();
        assert!(page.contains("<th>日本語</th>") && page.contains("<th>売上</th>") && page.contains("Shift_JIS") && !page.contains('\u{FFFD}'), "{page}");
        // (plain UTF-8 says nothing of it)
        let plain = dir.path().join("a.txt");
        std::fs::write(&plain, "見出し\n").unwrap();
        let page = written_page(dir.path(), &plain, &Written::Source("txt".to_string()), None).unwrap();
        assert!(page.contains("見出し") && !page.contains("Shown as") && !page.contains("UTF-16"), "{page}");
    }

    #[test]
    fn a_long_file_is_cut_and_says_so_and_a_binary_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("long.txt");
        std::fs::write(&long, "あ\n".repeat(200_000)).unwrap();
        let page = written_page(dir.path(), &long, &Written::Source("txt".to_string()), None).unwrap();
        assert!(page.len() < 2 << 20, "{}", page.len());
        assert!(page.contains("class=\"cut\""));
        let binary = dir.path().join("b.txt");
        std::fs::write(&binary, b"MZ\0\0").unwrap();
        assert!(written_page(dir.path(), &binary, &Written::Source("txt".to_string()), None).is_err());
    }

    #[test]
    fn an_address_stays_under_the_root_and_is_spelled_for_the_web() {
        let root = Path::new("C:\\work");
        assert_eq!(address_of(root, Path::new("C:\\work\\a b\\デモ.html")).unwrap(), "https://dogmind-preview.example/a%20b/%E3%83%87%E3%83%A2.html");
        assert!(address_of(root, Path::new("C:\\other\\a.html")).is_none());
        assert!(address_of(root, Path::new("C:\\work\\..\\secret.html")).is_none(), "out through a dot-dot");
    }

    /// A frame's address is judged as a top-level one is: the page's own host stays, whatever leaves it does not, and neither does a scheme another program is started by.
    #[test]
    fn a_frame_or_a_scheme_that_leaves_the_host_is_not_inside() {
        for outside in ["https://example.com/", "mailto:someone@example.com", "ms-settings:privacy", "ms-msdt:/id", "tel:+81", "slack://open", "javascript:alert(1)", "ftp://dogmind-preview.example/", "file:///C:/Windows/win.ini", "https://dogmind-preview.example@evil.example/", "https://dogmind-preview.example:444@evil.example/"] {
            assert!(!stays_inside(outside), "{outside}");
        }
        for inside in ["https://dogmind-preview.example/frame.html", "about:blank", "about:srcdoc", "data:text/html,x"] {
            assert!(stays_inside(inside), "{inside}");
        }
    }

    /// The script that takes `window.print` away is a function that ends at once and a property the page cannot change.
    #[test]
    fn the_script_that_turns_printing_off_leaves_it_nothing_to_write_over() {
        assert!(NO_PRINT.contains("Object.defineProperty(window,'print'") && NO_PRINT.contains("writable:false") && NO_PRINT.contains("configurable:false"));
        assert!(!NO_PRINT.contains("window.print()"), "it does not call it");
    }

    #[test]
    fn a_request_names_a_file_below_the_root_and_never_one_outside_it() {
        let root = Path::new("C:\\work");
        // (the reverse of address_of)
        let file = Path::new("C:\\work\\a b\\デモ.html");
        assert_eq!(path_of(root, &address_of(root, file).unwrap()).as_deref(), Some(file));
        assert_eq!(path_of(root, "https://dogmind-preview.example/css/site.css?v=3#top").as_deref(), Some(Path::new("C:\\work\\css\\site.css")));
        assert_eq!(path_of(root, "HTTPS://Dogmind-Preview.example/Img/x.PNG").as_deref(), Some(Path::new("C:\\work\\Img\\x.PNG")));
        for not in [
            "https://dogmind-preview.example/", "https://dogmind-preview.example", "https://dogmind-preview.example/../secret.txt", "https://dogmind-preview.example/a/%2e%2e/b.txt",
            "https://dogmind-preview.example/a%2F..%2Fb", "https://dogmind-preview.example/a%5Cb", "https://dogmind-preview.example/C:/Windows/win.ini", "https://dogmind-preview.example/C%3A/x",
            "https://dogmind-preview.example/a/CON", "https://dogmind-preview.example/nul.txt", "https://dogmind-preview.example/x./y", "https://dogmind-preview.example/x%20/y",
            "https://dogmind-preview.example/%ff%fe", "https://dogmind-preview.example/%zz", "https://dogmind-preview.example/%4", "https://other.example/a.css", "http://dogmind-preview.example/a.css",
            "https://dogmind-preview.example.evil.com/a.css", "https://dogmind-fonts.example/BIZUDGothic-Regular.ttf", "data:text/css,x",
        ] {
            assert_eq!(path_of(root, not), None, "{not}");
        }
        // (and whatever a request is, what comes out is inside the root)
        for odd in ["https://dogmind-preview.example/a/./b", "https://dogmind-preview.example//a//b", "https://dogmind-preview.example/a%00b"] {
            if let Some(found) = path_of(root, odd) {
                assert!(found.starts_with(root) && !found.components().any(|part| matches!(part, std::path::Component::ParentDir)), "{odd}: {found:?}");
            }
        }
    }

    #[test]
    fn what_a_page_loaded_from_a_folder_up_or_two_down_is_what_it_is_watched_by() {
        let dir = tempfile::tempdir().unwrap();
        let (page, style, picture) = (dir.path().join("pages").join("a.html"), dir.path().join("style.css"), dir.path().join("assets").join("img").join("x.png"));
        std::fs::create_dir_all(picture.parent().unwrap()).unwrap();
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        for file in [&page, &style, &picture] {
            std::fs::write(file, "x").unwrap();
        }
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for file in [&page, &style, &picture] {
            std::fs::File::options().write(true).open(file).unwrap().set_modified(long_ago).unwrap();
        }
        let loaded = [style.clone(), picture.clone()];
        let before = newest_of(&page, &loaded);
        assert_eq!(before, Some(long_ago));
        assert_eq!(newest_beside(&page), Some(long_ago));
        std::fs::write(&picture, "changed").unwrap();
        assert_eq!(newest_beside(&page), Some(long_ago), "the folder's look does not see it");
        assert_eq!(seen_since(before, newest_of(&page, &loaded)), Seen::Changed, "a picture two levels down");
        let after = newest_of(&page, &loaded);
        std::fs::File::options().write(true).open(&style).unwrap().set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(seen_since(after, newest_of(&page, &loaded)), Seen::Changed, "a style one level up");
        let kept = newest_of(&page, &loaded);
        assert_eq!(seen_since(kept, newest_of(&page, &loaded)), Seen::Same);
        // (a file it asked for that is gone is not a reason to fail: the page shows what it can)
        std::fs::remove_file(&picture).unwrap();
        assert!(newest_of(&page, &loaded).is_some());
    }

    #[test]
    fn a_file_that_is_deleted_is_gone_and_one_that_comes_back_is_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("a.html");
        std::fs::write(&page, "x").unwrap();
        let shown = newest_of(&page, &[]);
        assert!(shown.is_some());
        std::fs::remove_file(&page).unwrap();
        assert_eq!(seen_since(shown, newest_of(&page, &[])), Seen::Gone);
        assert_eq!(seen_since(None, newest_of(&page, &[])), Seen::Gone, "and stays gone");
        std::fs::write(&page, "back").unwrap();
        assert_eq!(seen_since(None, newest_of(&page, &[])), Seen::Changed, "written again, it is shown again");
    }

    #[test]
    fn the_address_of_a_network_event_is_read_from_its_request() {
        let event = r#"{"requestId":"1","loaderId":"2","request":{"url":"https://dogmind-preview.example/style.css?v=1","method":"GET"},"type":"Stylesheet"}"#;
        assert_eq!(requested_address(event).as_deref(), Some("https://dogmind-preview.example/style.css?v=1"));
        for not in ["", "not json", "{}", r#"{"request":{}}"#, r#"{"request":{"url":3}}"#] {
            assert_eq!(requested_address(not), None, "{not}");
        }
    }

    #[test]
    fn a_page_is_watched_by_its_requests_and_the_cap_holds() {
        let mut asked = Requested { root: Some(PathBuf::from("C:\\work")), files: Vec::new() };
        asked.note("https://dogmind-preview.example/a.css");
        asked.note("https://dogmind-preview.example/a.css");
        asked.note("https://example.com/b.css");
        asked.note("https://dogmind-preview.example/../c.css");
        assert_eq!(asked.files, [PathBuf::from("C:\\work\\a.css")]);
        for at in 0..MOST_WATCHED + 20 {
            asked.note(&format!("https://dogmind-preview.example/f{at}.png"));
        }
        assert_eq!(asked.files.len(), MOST_WATCHED);
        let mut none = Requested::default();
        none.note("https://dogmind-preview.example/a.css");
        assert!(none.files.is_empty(), "with no root there is nothing to map to");
    }

    #[test]
    fn only_its_own_host_counts_as_inside() {
        for inside in ["https://dogmind-preview.example/a.html", "HTTPS://DOGMIND-PREVIEW.EXAMPLE/x", "data:image/png;base64,AA", "blob:https://x/1", "about:blank"] {
            assert!(stays_inside(inside), "{inside}");
        }
        for outside in ["https://unpkg.com/three", "http://dogmind-preview.example/a", "https://dogmind-preview.example.evil.com/a", "file:///C:/secret.txt", "http://127.0.0.1:8080/"] {
            assert!(!stays_inside(outside), "{outside}");
        }
    }

    /// A written page has the shipped face: Windows's copy first, and the file beside the app at an address that serves the two files
    /// by their own names, and nothing else.
    #[test]
    fn the_shipped_face_is_served_by_its_names_and_nothing_else_is() {
        for not in [
            "https://dogmind-fonts.example/",
            "https://dogmind-fonts.example/../BIZUDGothic-Regular.ttf",
            "https://dogmind-fonts.example/sub/BIZUDGothic-Regular.ttf",
            "https://dogmind-fonts.example/BIZUDGothic-Regular.ttf?x",
            "https://dogmind-fonts.example/python.exe",
            "http://dogmind-fonts.example/BIZUDGothic-Regular.ttf",
            "https://dogmind-fonts.example.evil.com/BIZUDGothic-Regular.ttf",
            "https://dogmind-preview.example/BIZUDGothic-Regular.ttf",
        ] {
            assert!(shipped_font(not).is_none(), "{not}");
        }
        if crate::paint::font_folders().iter().any(|folder| folder.join("BIZUDGothic-Bold.ttf").is_file()) {
            assert!(shipped_font("HTTPS://Dogmind-Fonts.example/bizudgothic-bold.TTF").is_some_and(|file| file.ends_with("BIZUDGothic-Bold.ttf")));
        }
        let page = page_of("x", "https://dogmind-preview.example/");
        assert!(page.contains("font-src https://dogmind-fonts.example\"") && page.contains("src:local('BIZ UDGothic'),local('BIZUDGothic-Regular'),url(https://dogmind-fonts.example/BIZUDGothic-Regular.ttf);font-weight:400"), "{page}");
        assert!(page.contains("url(https://dogmind-fonts.example/BIZUDGothic-Bold.ttf);font-weight:700"), "{page}");
    }
}
