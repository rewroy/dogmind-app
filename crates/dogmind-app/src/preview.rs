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
use webview2_com::{CallDevToolsProtocolMethodCompletedHandler, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, DownloadStartingEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler, PermissionRequestedEventHandler, WebResourceRequestedEventHandler};
use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::lang::{t, tf};
use crate::markdown;
use crate::paint::theme;

/// `.example` is reserved: no such host can exist anywhere else.
const HOST: &str = "dogmind-preview.example";

/// Every connection of the browser goes to a proxy that is not there, WebSockets and addresses by number too, which a filter on
/// requests would not see. The workspace's host is served inside the browser and does not go that way.
const CUT_OFF: &str = "--proxy-server=http://127.0.0.1:9 --proxy-bypass-list=<-loopback> --force-webrtc-ip-handling-policy=disable_non_proxied_udp --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --no-first-run --blink-settings=defaultTextEncodingName=UTF-8";

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

/// What the preview can show. Anything else is not for this panel.
/// When anything a page can use last changed: the file, what lies beside it and one folder down (its styles, scripts and pictures).
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
}

pub fn written(path: &Path) -> Option<Written> {
    const SOURCE: [&str; 45] = ["txt", "log", "csv", "tsv", "json", "jsonc", "toml", "yaml", "yml", "ini", "cfg", "conf", "xml", "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "c", "h", "cc", "cpp", "hpp", "cs", "java", "kt", "go", "swift", "rb", "php", "scala", "dart", "sh", "bash", "zsh", "ps1", "bat", "cmd", "sql", "css", "scss", "vue", "lua"];
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if matches!(name.as_str(), "makefile" | "dockerfile") {
        return Some(Written::Source(name));
    }
    let extension = name.rsplit_once('.')?.1.to_string();
    match extension.as_str() {
        "md" | "markdown" => Some(Written::Markdown),
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
    shows_as_it_is(path) || written(path) == Some(Written::Markdown)
}

/// A colour as a page writes it.
fn css(color: u32) -> String {
    format!("#{:02x}{:02x}{:02x}", color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF)
}

/// A file as a page: nothing runs in it, and only pictures of its own folder are let in. What it refers to is under `base`.
fn page_of(body: &str, base: &str) -> String {
    const SHEET: &str = "html{color-scheme:SCHEME}*{box-sizing:border-box}body{margin:0;padding:18px 20px;background:BACK;color:TEXT;font:15px/1.65 'Segoe UI Variable Text','BIZ UDGothic','Yu Gothic UI','Segoe UI',sans-serif;overflow-wrap:anywhere}h1,h2,h3,h4,h5,h6{line-height:1.3;margin:1.4em 0 .5em}h1{font-size:1.6em;border-bottom:1px solid LINE;padding-bottom:.25em}h2{font-size:1.3em}h3{font-size:1.1em}h1:first-child,h2:first-child,h3:first-child{margin-top:0}p{margin:0 0 .9em}a{color:ACCENT}ul,ol{margin:0 0 .9em;padding-left:1.6em}li{margin:.2em 0}blockquote{margin:0 0 .9em;padding:.1em 0 .1em 1em;border-left:3px solid LINE;color:MUTED}hr{border:0;border-top:1px solid LINE;margin:1.2em 0}img{max-width:100%}code{font:.9em Consolas,monospace;background:CODE;padding:.1em .3em;border-radius:4px}pre{background:CODE;padding:12px 14px;border-radius:8px;overflow:auto;margin:0 0 .9em}pre code{background:none;padding:0;font-size:13px;line-height:1.5}table{border-collapse:collapse;margin:0 0 .9em}th,td{border:1px solid LINE;padding:.3em .7em}th{background:CODE}.k{color:KEYWORD}.s{color:STRING}.c{color:MUTED}.n{color:NUMBER}pre.source{margin:0;padding:0;background:none;font:13px/1.55 Consolas,monospace;counter-reset:n;white-space:pre}pre.source .l{display:block;min-height:1.55em;counter-increment:n}pre.source .l::before{content:counter(n);display:inline-block;width:3.2em;margin-right:1em;text-align:right;color:FAINT;user-select:none}.cut{color:MUTED;font-size:13px;margin-top:1em}";
    let sheet = SHEET.replace("SCHEME", if theme::is_light() { "light" } else { "dark" })
        .replace("BACK", &css(theme::background())).replace("TEXT", &css(theme::text())).replace("LINE", &css(theme::line())).replace("ACCENT", &css(theme::accent_hover()))
        .replace("MUTED", &css(theme::muted())).replace("FAINT", &css(theme::faint())).replace("CODE", &css(theme::code_back()))
        .replace("KEYWORD", &css(theme::accent_hover())).replace("STRING", &css(theme::good())).replace("NUMBER", &css(theme::warn()));
    let allowed = format!("default-src 'none'; style-src 'unsafe-inline'; img-src https://{HOST} data:");
    format!("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{allowed}\"><base href=\"{base}\"><style>{sheet}</style></head><body>{body}</body></html>")
}

/// The page for a file of the workspace. NavigateToString takes no more than 2 MB, so a long file is shown from its start, and says so.
fn written_page(root: &Path, file: &Path, kind: &Written) -> Result<String, String> {
    const MOST_TEXT: usize = 256 * 1024;
    const MOST_PAGE: usize = 1800 * 1024;
    let bytes = std::fs::read(file).map_err(|error| format!("{}: {error}", file.display()))?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(t("This file is not text, so it has no preview.").to_string());
    }
    let whole = String::from_utf8_lossy(&bytes);
    let mut end = whole.len().min(MOST_TEXT);
    while !whole.is_char_boundary(end) {
        end -= 1;
    }
    let text = &whole[..end];
    let mut body = match kind {
        Written::Markdown => markdown::to_html(text),
        Written::Source(language) => markdown::source_html(text, language),
    };
    if body.len() > MOST_PAGE {
        // (colour takes room: plain, it fits)
        body = markdown::source_html(text, "");
    }
    if end < whole.len() {
        body.push_str(&format!("<p class=\"cut\">{}</p>", markdown::escape(&tf("Only the first {0} KB are shown.", &[&(MOST_TEXT / 1024)]))));
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
    lower.starts_with(&inside) || lower == inside.trim_end_matches('/') || lower.starts_with("data:") || lower.starts_with("blob:") || lower == "about:blank"
}

pub struct Preview {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    /// Requests that were turned away since the page was opened.
    pub blocked: Rc<Cell<u32>>,
    root: RefCell<Option<PathBuf>>,
    /// The folder and the file shown now.
    shown: RefCell<Option<(PathBuf, PathBuf)>>,
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
    let blocked = Rc::new(Cell::new(0u32));
    let mut token = 0i64;

    // A page does not leave its host, whether the link is its own idea or a click.
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
    webview.add_WebResourceRequested(&WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let request = args.Request()?;
            let address = text_of(|text| request.Uri(text));
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
    // What was just written is what is shown, not what the cache has.
    let answered = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
    webview.CallDevToolsProtocolMethod(&HSTRING::from("Network.setCacheDisabled"), &HSTRING::from(r#"{"cacheDisabled":true}"#), &answered)?;
    Ok(Preview { controller, webview, blocked, root: RefCell::new(None), shown: RefCell::new(None), environment })
}

impl Preview {
    /// Shows `file`, under `root`: only what is inside `root` can be reached from it.
    pub unsafe fn open(&self, root: &Path, file: &Path) -> Result<(), String> {
        let address = address_of(root, file).ok_or_else(|| format!("{} is not in the workspace", file.display()))?;
        let mapped = self.webview.cast::<ICoreWebView2_3>().map_err(|error| format!("this WebView2 is too old for a preview: {error}"))?;
        let host = HSTRING::from(HOST);
        if self.root.borrow().as_deref() != Some(root) {
            let _ = mapped.ClearVirtualHostNameToFolderMapping(&host);
            mapped.SetVirtualHostNameToFolderMapping(&host, &HSTRING::from(root.as_os_str()), COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS).map_err(|error| error.to_string())?;
            *self.root.borrow_mut() = Some(root.to_path_buf());
        }
        self.blocked.set(0);
        *self.shown.borrow_mut() = Some((root.to_path_buf(), file.to_path_buf()));
        match written(file) {
            Some(kind) => {
                let page = written_page(root, file, &kind)?;
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

    /// What shows behind a page, before it comes too.
    unsafe fn background(&self, color: u32) {
        if let Ok(controller) = self.controller.cast::<ICoreWebView2Controller2>() {
            let _ = controller.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: (color & 0xFF) as u8, G: ((color >> 8) & 0xFF) as u8, B: ((color >> 16) & 0xFF) as u8 });
        }
    }

    pub unsafe fn reload(&self) {
        self.blocked.set(0);
        // (a page written here is written again: the browser has only a string of it)
        let written_now = self.shown.borrow().clone().filter(|(_, file)| written(file).is_some());
        match written_now {
            Some((root, file)) => {
                let _ = self.open(&root, &file);
            }
            None => {
                let _ = self.webview.Reload();
            }
        }
    }

    /// The theme has changed: a page written here follows it.
    pub unsafe fn retheme(&self) {
        if self.shown.borrow().as_ref().is_some_and(|(_, file)| written(file).is_some()) {
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
        let page = written_page(root, &file, &Written::Markdown).unwrap();
        assert!(!page.contains("<script>") && page.contains("&lt;script&gt;"), "{page}");
        assert!(page.contains("<base href=\"https://dogmind-preview.example/doc/\">"), "{page}");
        assert!(page.contains("default-src 'none'") && page.contains("<img src=\"p.png\""), "{page}");
        assert!(!page.contains("BACK") && !page.contains("SCHEME"), "every colour is filled in");
    }

    #[test]
    fn a_long_file_is_cut_and_says_so_and_a_binary_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("long.txt");
        std::fs::write(&long, "あ\n".repeat(200_000)).unwrap();
        let page = written_page(dir.path(), &long, &Written::Source("txt".to_string())).unwrap();
        assert!(page.len() < 2 << 20, "{}", page.len());
        assert!(page.contains("class=\"cut\""));
        let binary = dir.path().join("b.txt");
        std::fs::write(&binary, b"MZ\0\0").unwrap();
        assert!(written_page(dir.path(), &binary, &Written::Source("txt".to_string())).is_err());
    }

    #[test]
    fn an_address_stays_under_the_root_and_is_spelled_for_the_web() {
        let root = Path::new("C:\\work");
        assert_eq!(address_of(root, Path::new("C:\\work\\a b\\デモ.html")).unwrap(), "https://dogmind-preview.example/a%20b/%E3%83%87%E3%83%A2.html");
        assert!(address_of(root, Path::new("C:\\other\\a.html")).is_none());
        assert!(address_of(root, Path::new("C:\\work\\..\\secret.html")).is_none(), "out through a dot-dot");
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
}
