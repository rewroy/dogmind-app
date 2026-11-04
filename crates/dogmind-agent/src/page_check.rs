//! Opening a page of the workspace in a headless browser, inside the sandbox: what its scripts printed and threw, and what it shows. It is the same sandbox as any command's, with no
//! network, so what a page takes from a CDN does not load; this says so plainly, as nothing else would tell: the page just sits there with no error.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

/// A page that has not finished in this long will not: a big WebGL drawing under the software renderer ends in a crash at about 25 s, with nothing more said.
const LIMIT: Duration = Duration::from_secs(20);
/// (not `--disable-breakpad` or `--disable-crash-reporter`: with those the browser crashes in about half its runs in the sandbox, against about 1 in 30 finally without. What keeps a crash
/// from raising a dialog is the error mode and the job's limit, in `sandbox`.)
/// The flags, with what a debug build is told to add (for a look at what the browser does in the sandbox).
fn flags() -> String {
    #[cfg(debug_assertions)]
    if let Ok(more) = std::env::var("DOGMIND_PAGE_FLAGS") {
        return format!("{FLAGS} {more}");
    }
    FLAGS.to_string()
}
const FLAGS: &str = "--headless=new --no-sandbox --disable-gpu --no-first-run --disable-extensions --disable-background-networking --disable-sync --disable-component-update --enable-logging=stderr --log-level=0 --allow-file-access-from-files";
/// How long a page runs before it is read, unless the model asks for longer, for an animation, and the most it may.
const WAIT_MS: u64 = 1500;
const MOST_WAIT_MS: u64 = 10_000;

/// A size to look at a page at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct View {
    label: String,
    width: u32,
    height: u32,
}

/// The views asked for: `desktop`, `mobile` or `WIDTHxHEIGHT`, the desktop alone when none is. At most four, as each is a browser's run.
fn views_of(arguments: &Value) -> Result<Vec<View>, String> {
    let asked: Vec<String> = arguments.get("views").and_then(Value::as_array).map(|views| views.iter().filter_map(Value::as_str).map(|view| view.trim().to_ascii_lowercase()).collect()).unwrap_or_default();
    let asked = if asked.is_empty() { vec!["desktop".to_string()] } else { asked };
    let mut views = Vec::new();
    for name in asked.iter().take(4) {
        let (width, height) = match name.as_str() {
            "desktop" => (1280, 800),
            "mobile" => (390, 844),
            other => {
                let sized = other.split_once('x').and_then(|(width, height)| Some((width.trim().parse::<u32>().ok()?, height.trim().parse::<u32>().ok()?)));
                let Some((width, height)) = sized else { return Err(format!("views: {other} is not desktop, mobile or WIDTHxHEIGHT")) };
                (width.clamp(240, 2560), height.clamp(240, 4000))
            }
        };
        let view = View { label: name.clone(), width, height };
        if !views.contains(&view) {
            views.push(view);
        }
    }
    Ok(views)
}

/// A file as a browser's address, for a page that frames it: what would end the address or the attribute early is escaped.
fn file_url(path: &Path) -> String {
    let plain = path.display().to_string().trim_start_matches("\\\\?\\").replace('\\', "/");
    let mut out = String::from("file:///");
    for c in plain.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '"' => out.push_str("%22"),
            '&' => out.push_str("%26"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\'' => out.push_str("%27"),
            _ => out.push(c),
        }
    }
    out
}

/// What a model's steps are run with, in the page: Playwright's way of saying them, done with the page's own DOM. A browser that stays open
/// to be driven cannot run in the sandbox (node there cannot make the pipes Playwright speaks over, and a headless Edge that is kept open falls
/// over within a second, where one that loads a page and ends does not), so the steps go in the page and run in the one load that is read.
const STEPS_PRELUDE: &str = r#"<script>
(async () => {
  const wait = (ms) => new Promise((done) => setTimeout(done, ms));
  const find = (selector) => { const found = document.querySelector(selector); if (!found) throw new Error('nothing matches ' + selector); return found; };
  const click = async (selector) => { find(selector).click(); await wait(50); };
  const fill = async (selector, value) => { const field = find(selector); field.focus(); field.value = value; field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new Event('change', { bubbles: true })); await wait(50); };
  const check = async (selector, on = true) => { const box = find(selector); if (box.checked !== on) box.click(); await wait(50); };
  const select = async (selector, value) => { const list = find(selector); list.value = value; list.dispatchEvent(new Event('change', { bubbles: true })); await wait(50); };
  const KEY = /^((Control|Shift|Alt|Meta)\+)*(Enter|Tab|Escape|Backspace|Delete|Space|Home|End|PageUp|PageDown|Arrow(Up|Down|Left|Right)|F\d{1,2}|.)$/;
  const press = async (first, second) => {
    // (as Playwright: press(selector, key), or press(key) on what has the focus; a key named first is taken as one)
    let [selector, key] = second === undefined ? [null, first] : [first, second];
    if (second !== undefined && KEY.test(first) && !KEY.test(second)) [selector, key] = [second, first];
    const target = selector ? find(selector) : (document.activeElement || document.body);
    for (const type of ['keydown', 'keypress', 'keyup']) target.dispatchEvent(new KeyboardEvent(type, { key, bubbles: true, cancelable: true }));
    if (key === 'Enter' && target.form) target.form.requestSubmit();
    await wait(50);
  };
  const text = (selector) => find(selector).textContent.trim();
  const count = (selector) => document.querySelectorAll(selector).length;
  const visible = (selector) => { const found = document.querySelector(selector); if (!found) return false; const box = found.getBoundingClientRect(); return box.width > 0 && box.height > 0 && getComputedStyle(found).visibility !== 'hidden'; };
  await new Promise((done) => document.readyState === 'complete' ? done() : addEventListener('load', done));
  try {
    await (async () => {
"#;
const STEPS_END: &str = r#"
    })();
    console.log('DOGMIND-STEPS-DONE');
  } catch (error) {
    console.error('DOGMIND-STEPS-FAILED: ' + error.message);
  }
})();
</script>"#;
/// The page with the steps at its end and its folder as its base, so that what it names by a relative path is what it would have had.
fn with_steps(html: &str, base: &str, steps: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = html.to_string();
    // (one with a base of its own keeps it)
    if !lower.contains("<base") {
        let tag = format!("<base href=\"{base}\">");
        let at = lower.find("<head").and_then(|head| lower[head..].find('>').map(|end| head + end + 1)).or_else(|| lower.find("<html").and_then(|html| lower[html..].find('>').map(|end| html + end + 1))).unwrap_or(0);
        out.insert_str(at, &tag);
    }
    // (what the steps say cannot end the script they are in)
    let script = format!("{STEPS_PRELUDE}{}{STEPS_END}", steps.replace("</script", "<\\/script").replace("</SCRIPT", "<\\/SCRIPT"));
    match out.to_ascii_lowercase().rfind("</body>") {
        Some(at) => out.insert_str(at, &script),
        None => out.push_str(&script),
    }
    out
}

/// The page in a frame of exactly this size: the browser's window has a least width of about 500 pixels whatever it is asked, and
/// lays a page out a little smaller than the window, so a phone's width is only had inside a frame. The picture is cut to the frame.
/// `src`: the page's address, a file's or a local server's, escaped already.
fn frame_page(src: &str, view: &View) -> String {
    format!("<!doctype html><meta charset=\"utf-8\"><style>html,body{{margin:0;overflow:hidden;background:#fff}}iframe{{border:0;display:block}}</style><iframe src=\"{src}\" width=\"{}\" height=\"{}\"></iframe>", view.width, view.height)
}

/// A page a server on this machine gives: http on localhost, 127.0.0.1 or [::1], with a port or none. Nothing that could leave the
/// machine, and nothing cmd would read into on the browser's command line (`%`, a quote, a space).
fn served_address(url: &str) -> Result<String, String> {
    let refused = || format!("{url}: a url here is a server's on this machine, as http://localhost:8000/ or http://127.0.0.1:8000/page.html");
    let lower = url.to_ascii_lowercase();
    let rest = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")).ok_or_else(refused)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if authority.starts_with("[::1]") { "[::1]" } else { authority.split(':').next().unwrap_or("") };
    let port = &authority[host.len()..];
    let port_ok = port.is_empty() || (port.len() > 1 && port.starts_with(':') && port[1..].chars().all(|c| c.is_ascii_digit()));
    if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") || !port_ok || url.chars().any(|c| matches!(c, '%' | '"' | '\\') || c.is_whitespace() || c.is_control()) {
        return Err(refused());
    }
    Ok(url.to_string())
}
/// Tries of the browser for the page, and again for each picture. It ends with 0xc0000005 in about 6 runs of 10 on some machines, whatever the page, however long it was left alone, and whether
/// the run before it ended well or not, so each try is as likely to end well as the first was; a run that crashes is over in about 1.5 s. Ten tries are lost about once in a hundred
/// (4 were lost one time in six, and a picture's 3 two in five), and cost 15 s at the most when it is crashes that they go on, with nothing between them; a run that hangs is not tried again.
const TRIES: usize = 10;
/// No new try after this long in all: runs that crash late, or hang, would otherwise go on for minutes.
const MOST_TRYING: Duration = Duration::from_secs(45);
/// How the browser's runs went, for the test that measures it: the page's runs and those that did not end well, the pictures' and those that left none.
#[cfg(test)]
static BROWSER_RUNS: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
#[cfg(test)]
fn note_run(picture: bool, bad: bool) {
    BROWSER_RUNS[usize::from(picture) * 2].fetch_add(1, Ordering::Relaxed);
    BROWSER_RUNS[usize::from(picture) * 2 + 1].fetch_add(u64::from(bad), Ordering::Relaxed);
}
const MOST_LINES: usize = 25;
const MOST_LINE: usize = 400;
const MOST_TEXT: usize = 400;
/// (one at a time in this process, as a browser's run takes a good part of the machine; against another process it is the names below that keep them apart)
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
/// Each run of the browser has names of its own in the sandbox's one temporary folder, which every session, every copy of the app and every test run shares: the profile, the
/// frame and the shot of one run were once the same for all, and a run that ended removed the profile another was using, and wrote over the other's frame and shot.
static RUNS_NAMED: AtomicU64 = AtomicU64::new(0);

/// The id of a run: this process's, and the count of runs it has named.
fn next_run() -> String {
    format!("{}-{}", std::process::id(), RUNS_NAMED.fetch_add(1, Ordering::Relaxed))
}

fn profile_of(run: &str) -> String {
    format!("dogmind-profile-{run}")
}

fn frame_of(run: &str) -> String {
    format!("dogmind-frame-{run}.html")
}

fn shot_of(run: &str) -> String {
    format!("dogmind-shot-{run}.png")
}

fn steps_page_of(run: &str) -> String {
    format!("dogmind-steps-{run}.html")
}

fn browser() -> Option<(PathBuf, &'static str)> {
    const PLACES: [(&str, &str, &str); 6] = [
        ("ProgramFiles(x86)", "Microsoft\\Edge\\Application\\msedge.exe", "Edge"),
        ("ProgramFiles", "Microsoft\\Edge\\Application\\msedge.exe", "Edge"),
        ("LOCALAPPDATA", "Microsoft\\Edge\\Application\\msedge.exe", "Edge"),
        ("ProgramFiles", "Google\\Chrome\\Application\\chrome.exe", "Chrome"),
        ("ProgramFiles(x86)", "Google\\Chrome\\Application\\chrome.exe", "Chrome"),
        ("LOCALAPPDATA", "Google\\Chrome\\Application\\chrome.exe", "Chrome"),
    ];
    PLACES.iter().find_map(|(variable, path, name)| Some((PathBuf::from(std::env::var_os(variable)?).join(path), *name)).filter(|(found, _)| found.is_file()))
}

/// What in a page cannot load or run with no network, worked out from its text, as nothing the browser prints says so.
#[derive(Debug, Default, PartialEq, Eq)]
struct Needs {
    /// Addresses that cannot load.
    remote: Vec<String>,
    /// A page's own scripts that need one of those, and so did not run.
    did_not_run: Vec<String>,
    /// Scripts that are there and should have run.
    run: Vec<String>,
    /// What it names that is not there.
    missing: Vec<String>,
    /// A script with `type="module"` and a file of its own, which a browser refuses from `file://`.
    local_module: bool,
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let pattern = Regex::new(&format!(r#"(?i)\b{name}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)).expect("the pattern is valid");
    let found = pattern.captures(tag)?;
    Some(found.get(1).or(found.get(2)).or(found.get(3))?.as_str().to_string())
}

fn is_remote(address: &str) -> bool {
    let lower = address.to_ascii_lowercase();
    // (also with backslashes, and however many: looked at here as a file, that is the app itself, outside the sandbox, giving another machine the person's sign-in for what a page said)
    let bare = lower.replace('\\', "/");
    lower.starts_with("http://") || lower.starts_with("https://") || bare.starts_with("//") || bare.starts_with("/\\") || bare.trim_start_matches('/').starts_with("?/unc")
}

/// A page's address of a file of its own, without what only a server would read: `app.js?v=2` is `app.js`.
fn file_part(address: &str) -> &str {
    address.split(['?', '#']).next().unwrap_or(address)
}

/// Through the page's import map, the longest name that fits first.
fn mapped(name: &str, map: &[(String, String)]) -> String {
    let fits = map.iter().filter(|(from, _)| from == name || (from.ends_with('/') && name.starts_with(from.as_str()))).max_by_key(|(from, _)| from.len());
    match fits {
        Some((from, to)) if from == name => to.clone(),
        Some((from, to)) => format!("{to}{}", &name[from.len()..]),
        None => name.to_string(),
    }
}

/// The remote addresses a module needs, following its imports through the files beside it.
fn follow(file: &Path, map: &[(String, String)], seen: &mut HashSet<PathBuf>, remote: &mut Vec<String>, missing: &mut Vec<String>, root: &Path) {
    if !seen.insert(file.to_path_buf()) || seen.len() > 200 || crate::safety::reaches_out(file) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else {
        missing.push(file.strip_prefix(root).unwrap_or(file).display().to_string());
        return;
    };
    imports_of(&text, file.parent().unwrap_or(root), map, seen, remote, missing, root);
}

fn imports_of(text: &str, folder: &Path, map: &[(String, String)], seen: &mut HashSet<PathBuf>, remote: &mut Vec<String>, missing: &mut Vec<String>, root: &Path) {
    let pattern = Regex::new(r#"(?m)(?:^|[\s;])(?:import|export)\s[^'"\n;]*?from\s*['"]([^'"]+)['"]|(?:^|[\s;])import\s*['"]([^'"]+)['"]|\bimport\(\s*['"]([^'"]+)['"]"#).expect("the pattern is valid");
    for found in pattern.captures_iter(text) {
        let Some(name) = found.get(1).or(found.get(2)).or(found.get(3)) else { continue };
        let target = mapped(name.as_str(), map);
        if is_remote(&target) {
            if !remote.contains(&target) {
                remote.push(target);
            }
        } else if target.starts_with('.') || target.starts_with('/') {
            let local = if let Some(from_root) = target.strip_prefix('/') { root.join(from_root) } else { folder.join(&target) };
            follow(&local, map, seen, remote, missing, root);
        }
    }
}

fn needs_of(page: &Path) -> Needs {
    let mut needs = Needs::default();
    let Ok(text) = std::fs::read_to_string(page) else { return needs };
    let root = page.parent().unwrap_or(Path::new("."));
    let scripts = Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>").expect("the pattern is valid");
    let mut map = Vec::new();
    for found in scripts.captures_iter(&text) {
        if attribute(&found[1], "type").is_some_and(|kind| kind.eq_ignore_ascii_case("importmap")) {
            if let Ok(value) = serde_json::from_str::<Value>(&found[2]) {
                if let Some(imports) = value.get("imports").and_then(Value::as_object) {
                    map.extend(imports.iter().filter_map(|(from, to)| Some((from.clone(), to.as_str()?.to_string()))));
                }
            }
        }
    }
    let links = Regex::new(r"(?is)<link\b[^>]*>").expect("the pattern is valid");
    for link in links.find_iter(&text) {
        if attribute(link.as_str(), "rel").is_some_and(|rel| rel.eq_ignore_ascii_case("stylesheet")) {
            match attribute(link.as_str(), "href") {
                Some(href) if is_remote(&href) => needs.remote.push(href),
                Some(href) if !root.join(file_part(&href)).is_file() => needs.missing.push(href),
                _ => {}
            }
        }
    }
    for (at, found) in scripts.captures_iter(&text).enumerate() {
        let (tag, body) = (&found[1], &found[2]);
        let kind = attribute(tag, "type").unwrap_or_default().to_ascii_lowercase();
        if kind == "importmap" || !(kind.is_empty() || kind == "module" || kind.contains("javascript")) {
            continue;
        }
        let module = kind == "module";
        let source = attribute(tag, "src");
        let label = source.clone().unwrap_or_else(|| format!("the page's script {}", at + 1));
        if let Some(address) = &source {
            if is_remote(address) {
                if !needs.remote.contains(address) {
                    needs.remote.push(address.clone());
                }
                continue;
            }
            if !root.join(file_part(address)).is_file() {
                needs.missing.push(address.clone());
                continue;
            }
            needs.local_module |= module;
        }
        if !module {
            needs.run.push(label);
            continue;
        }
        let (mut remote, mut missing, mut seen) = (Vec::new(), Vec::new(), HashSet::new());
        match &source {
            Some(address) => follow(&root.join(file_part(address)), &map, &mut seen, &mut remote, &mut missing, root),
            None => imports_of(body, root, &map, &mut seen, &mut remote, &mut missing, root),
        }
        if remote.is_empty() {
            needs.run.push(label);
        } else {
            needs.did_not_run.push(label);
            for address in remote {
                if !needs.remote.contains(&address) {
                    needs.remote.push(address);
                }
            }
        }
        needs.missing.extend(missing);
    }
    needs
}

/// What the page printed and threw, from all the browser says of itself. Long paths to the workspace go, and every line a browser's own extension or service prints.
fn console_lines(err: &str, folder_url: &str) -> Vec<String> {
    let start = Regex::new(r#"INFO:CONSOLE:\d+\] "(.*)"#).expect("the pattern is valid");
    let end = Regex::new(r#"^(.*)", source: (.*) \((\d+)\)\s*$"#).expect("the pattern is valid");
    let (mut lines, mut open): (Vec<String>, Option<String>) = (Vec::new(), None);
    let finish = |text: &str, source: &str, line: &str, lines: &mut Vec<String>| {
        if source.starts_with("chrome-extension:") || source.starts_with("chrome:") || source.starts_with("edge:") {
            return;
        }
        // (the browser's own word on a page from a server, which no page here sends and none can change: an ad feature its policy names that it does not know)
        if text.starts_with("Error with Permissions-Policy header: Unrecognized feature") {
            return;
        }
        let place = source.replace(folder_url, "").replace('\\', "/");
        let place = place.rsplit('/').next().unwrap_or("").to_string();
        let said: String = text.trim().chars().take(MOST_LINE).collect();
        lines.push(if place.is_empty() { said } else { format!("{place}:{line}: {said}") });
    };
    for line in err.lines() {
        if let Some(found) = start.captures(line) {
            let rest = found[1].to_string();
            match end.captures(&format!("{rest}\n").replace('\n', "")) {
                Some(whole) => finish(&whole[1], &whole[2], &whole[3], &mut lines),
                None => open = Some(rest),
            }
        } else if let Some(text) = open.as_mut() {
            match end.captures(line) {
                Some(whole) => {
                    let joined = format!("{text} {}", &whole[1]);
                    finish(&joined, &whole[2], &whole[3], &mut lines);
                    open = None;
                }
                None => {
                    text.push(' ');
                    text.push_str(line.trim());
                }
            }
        }
    }
    lines
}

/// The title and the words a person would read, from the page as its scripts left it.
fn shown(dom: &str) -> (String, String) {
    let title = Regex::new(r"(?is)<title[^>]*>(.*?)</title\s*>").expect("the pattern is valid").captures(dom).map_or(String::new(), |found| found[1].trim().to_string());
    let hidden = Regex::new(r"(?is)<(script|style|head|svg|template)\b.*?</(script|style|head|svg|template)\s*>").expect("the pattern is valid");
    let text = hidden.replace_all(dom, " ");
    let text = Regex::new(r"(?s)<[^>]*>").expect("the pattern is valid").replace_all(&text, " ");
    let text = text.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">");
    (title, text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MOST_TEXT).collect())
}

/// `needs`: what a file page was read to need; a page a local server gives has none of that known.
fn report(page_name: &str, browser_name: &str, finished: bool, needs: Option<&Needs>, console: &[String], dom: &str) -> String {
    let served = needs.is_none();
    let unknown = Needs::default();
    let needs = needs.unwrap_or(&unknown);
    let mut out = if served { format!("{page_name} in headless {browser_name}, with no network but this machine's own servers.\n") } else { format!("{page_name} in headless {browser_name}, with no network.\n") };
    if !finished {
        out.push_str("NOT CHECKED: the browser crashed or was stopped each time it was tried. This browser sometimes crashes in the sandbox, whatever the page, and a big WebGL drawing stops it too, so this says nothing about the page. What it had said by then is below. Running this check again changes nothing, and do not install anything to check it another way: say in your answer that the page was not checked.\n");
    }
    if !needs.remote.is_empty() {
        out.push_str(&format!("NOT LOADED, as a CDN library cannot load under this check: {}.\n", needs.remote.join(", ")));
    }
    if !needs.did_not_run.is_empty() {
        out.push_str(&format!("DID NOT RUN, as they need those, and nothing about them is known: {}. Do not set up another way to check them: say in your answer that they have not been run.\n", needs.did_not_run.join(", ")));
    }
    if !needs.missing.is_empty() {
        out.push_str(&format!("MISSING: {}.\n", needs.missing.join(", ")));
    }
    if needs.local_module {
        out.push_str("A browser refuses a module script from file:// when the page is opened by double-click, which this check allows: that page needs a local server to be opened, or a plain script. To check it as served, start one in the background (as `python -m http.server 8000 --bind 127.0.0.1`) and give this its `url`.\n");
    }
    if served {
        // (what a server gives is not read here beforehand: its console says what ran)
    } else if needs.run.is_empty() && needs.did_not_run.is_empty() {
        out.push_str("It has no script of its own.\n");
    } else if !needs.run.is_empty() {
        out.push_str(&format!("Ran: {}.\n", needs.run.join(", ")));
    }
    if console.is_empty() {
        out.push_str(if needs.remote.is_empty() && finished { "Console: nothing, so no errors.\n" } else { "Console: nothing, which says little with that not loaded.\n" });
    } else {
        out.push_str("Console:\n");
        for line in console.iter().take(MOST_LINES) {
            out.push_str(&format!("- {line}\n"));
        }
        if console.len() > MOST_LINES {
            out.push_str(&format!("- and {} more\n", console.len() - MOST_LINES));
        }
    }
    if finished {
        let (title, text) = shown(dom);
        out.push_str(&format!("Shows: title \"{title}\"; text: {}\n", if text.is_empty() { "none" } else { &text }));
    }
    out
}

/// A page of the workspace, by its path; or, by `url`, what a server on this machine gives, as one the model started in the background.
pub fn check_page(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let steps = arguments.get("steps").and_then(Value::as_str).filter(|steps| !steps.trim().is_empty());
    let served = arguments.get("url").and_then(Value::as_str).map(str::trim).filter(|url| !url.is_empty());
    let plain = |path: &Path| path.display().to_string().trim_start_matches("\\\\?\\").to_string();
    let root = std::fs::canonicalize(workspace).map_err(|error| format!("{}: {error}", workspace.display()))?;
    let (page, name, needs) = match served {
        Some(url) => {
            if steps.is_some() {
                return Err("steps run in a page file of the workspace: give its path rather than a url".to_string());
            }
            (None, served_address(url)?, None)
        }
        None => {
            let given = arguments.get("path").and_then(Value::as_str).filter(|path| !path.is_empty()).ok_or("path is required: the page, under the workspace, or url: a server's on this machine")?;
            let path = if Path::new(given).is_absolute() { PathBuf::from(given) } else { workspace.join(given) };
            let page = if path.is_dir() { path.join("index.html") } else { path };
            if !page.is_file() {
                return Err(format!("{given}: no such page"));
            }
            let full = std::fs::canonicalize(&page).map_err(|error| format!("{given}: {error}"))?;
            if !full.starts_with(&root) {
                return Err(format!("{given} is outside the workspace: this opens only a page in it"));
            }
            let extension = page.extension().map_or(String::new(), |extension| extension.to_string_lossy().to_ascii_lowercase());
            if !matches!(extension.as_str(), "html" | "htm" | "xhtml" | "svg") {
                return Err(format!("{given} is not a page: this opens .html"));
            }
            // (its name goes on a command line that asks no one, and cmd works out `%CMDCMDLINE:~45,1%` even inside quotes: a file named so would be a command of the model's choosing)
            if page.to_string_lossy().contains('%') {
                return Err(format!("{given}: a name with % in it cannot be opened from here. Rename it first"));
            }
            let name = page.strip_prefix(workspace).unwrap_or(&page).display().to_string();
            let needs = needs_of(&page);
            (Some(page), name, Some(needs))
        }
    };
    let Some((program, browser_name)) = browser() else {
        return Err("neither Edge nor Chrome is on this machine, so the page could not be opened: say that it has not been run".to_string());
    };
    let views = views_of(arguments)?;
    // (steps take time of their own, as their waits count in it)
    let wait = arguments.get("wait_ms").and_then(Value::as_u64).unwrap_or(if steps.is_some() { 2 * WAIT_MS } else { WAIT_MS }).min(MOST_WAIT_MS);
    let cwd = page.as_deref().and_then(Path::parent).unwrap_or(workspace).to_path_buf();
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = crate::sandbox::temp_folder();
    if let Some(temp) = &temp {
        forget_old_pictures(temp);
        forget_old_leftovers(temp, LEFTOVER_KEPT);
    }
    // (what this check makes in that folder goes when it ends, whichever way)
    let mut made = Removing(Vec::new());
    // With steps, what is opened is a copy of the page with them in it, in the sandbox's own folder, that reads the page's own files from where they are.
    let (mut steps_copy, mut opened) = (None, page.clone());
    if let (Some(page), Some(steps)) = (&page, steps) {
        let temp = temp.as_ref().ok_or("the sandbox's folder could not be found, so the steps could not be run")?;
        let html = std::fs::read_to_string(page).map_err(|error| format!("{name}: {error}"))?;
        let base = file_url(page.parent().unwrap_or(workspace)) + "/";
        let file_name = steps_page_of(&next_run());
        let copy = temp.join(&file_name);
        made.0.push(copy.clone());
        std::fs::write(&copy, with_steps(&html, &base, steps)).map_err(|error| format!("{}: {error}", copy.display()))?;
        (steps_copy, opened) = (Some(file_name), Some(copy));
    }
    // What the browser is given, on its command line and in the pictures' frame.
    let (address, src) = match &opened {
        Some(file) => (plain(file), file_url(file)),
        None => (name.clone(), name.clone()),
    };
    let browser_flags = flags();
    // (the browser sometimes crashes in the sandbox, whatever the page, so one crash says nothing of the page: it is tried again)
    let (mut dom, mut err, mut finished, mut how) = (String::new(), String::new(), false, String::new());
    let (trying, mut profiles) = (std::time::Instant::now(), Profiles::new(&cwd, workspace));
    for _ in 0..TRIES {
        let started = std::time::Instant::now();
        let profile = profiles.next();
        let line = format!("\"{}\" {browser_flags} --window-size=1280,800 --virtual-time-budget={wait} --user-data-dir=\"%TEMP%\\{profile}\" --dump-dom \"{address}\"", program.display());
        let done = crate::sandbox::run_browser(&line, &cwd, workspace, LIMIT, cancel)?;
        (dom, err) = (crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr));
        #[cfg(debug_assertions)]
        if std::env::var_os("DOGMIND_PAGE_DEBUG").is_some() {
            eprintln!("--- browser, {:?} {:?}:\n{err}\n--- dom: {} bytes", done.status, done.ended, dom.len());
        }
        finished = done.status == Some(0) && !dom.trim().is_empty();
        how = done.status.map_or(done.ended.to_string(), |code| format!("exit code {:#x}", code as u32));
        #[cfg(test)]
        note_run(false, !finished);
        if finished || cancel.load(Ordering::Relaxed) || started.elapsed() > LIMIT / 2 || trying.elapsed() > MOST_TRYING || err.contains("INFO:CONSOLE") && !err.contains("chrome-extension") {
            break;
        }
    }
    profiles.clear();
    // (what its console names is said from the workspace, or from the server's root)
    let base_url = match served {
        Some(url) => url.splitn(4, '/').take(3).collect::<Vec<_>>().join("/") + "/",
        None => format!("file:///{}/", plain(&root).replace('\\', "/")),
    };
    let mut console = console_lines(&err, &base_url);
    let mut steps_said = String::new();
    if let (Some(page), Some(copy)) = (&page, &steps_copy) {
        // (the copy's lines are the page's own; and whether the steps got to their end is said apart)
        let file_name = page.file_name().map_or(String::new(), |file| file.to_string_lossy().into_owned());
        let done = console.iter().any(|line| line.ends_with("DOGMIND-STEPS-DONE"));
        let failed = console.iter().find_map(|line| line.split_once("DOGMIND-STEPS-FAILED: ").map(|(_, why)| why.to_string()));
        console.retain(|line| !line.contains("DOGMIND-STEPS-"));
        console.iter_mut().for_each(|line| *line = line.replace(copy.as_str(), &file_name));
        steps_said = match (done, failed) {
            (_, Some(why)) => format!("STEPS FAILED: {why}. What it shows, and its pictures, are the page as the steps left it.\n"),
            (true, None) => "Steps: all ran. What it shows, and its pictures, are the page after them.\n".to_string(),
            (false, None) => format!("Steps: they had not ended when the page was read, after {wait} ms of its time: give a larger wait_ms.\n"),
        };
    }
    let said = format!("{}{steps_said}", report(&name, browser_name, finished, needs.as_ref(), &console, &dom));
    if !finished {
        return Ok(format!("{said}The browser's end: {how}.\n"));
    }
    // What it looks like, for a model that can see, and it costs that model nothing if it does not ask. Not of a page that draws with WebGL, which ends the browser here.
    let Some(temp) = temp else { return Ok(said) };
    let mut out = said;
    // Each picture by a name of its own: one name for every call had a later check, or another session's, write over a picture an answer
    // still showed.
    let taken_at = chrono::Local::now().format("%Y%m%d-%H%M%S%3f").to_string();
    for view in &views {
        if cancel.load(Ordering::Relaxed) {
            out.push_str(&format!("No picture at {} was taken: the turn was stopped.\n", view.label));
            continue;
        }
        let tag = view.label.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        let run = next_run();
        let (frame, shot, picture) = (temp.join(frame_of(&run)), temp.join(shot_of(&run)), temp.join(format!("dogmind-page-{taken_at}-{tag}.png")));
        let _ = std::fs::remove_file(&picture);
        // (with steps, each picture is of the page after them, as they run again in its load)
        if std::fs::write(&frame, frame_page(&src, view)).is_err() {
            out.push_str(&format!("No picture at {} could be taken.\n", view.label));
            continue;
        }
        // (a window larger than the frame by what the browser takes of it, and at least its least width)
        let (window_width, window_height) = (view.width.max(600) + 64, view.height + 160);
        let (mut taken, mut said_there) = (false, Vec::new());
        let (trying, mut profiles) = (std::time::Instant::now(), Profiles::new(&cwd, workspace));
        for _ in 0..TRIES {
            if cancel.load(Ordering::Relaxed) || trying.elapsed() > MOST_TRYING {
                break;
            }
            let _ = std::fs::remove_file(&shot);
            let started = std::time::Instant::now();
            let profile = profiles.next();
            let line = format!("\"{}\" {browser_flags} --window-size={window_width},{window_height} --virtual-time-budget={wait} --user-data-dir=\"%TEMP%\\{profile}\" --screenshot=\"{}\" \"{}\"", program.display(), shot.display(), plain(&frame));
            let done = crate::sandbox::run_browser(&line, &cwd, workspace, LIMIT, cancel);
            #[cfg(debug_assertions)]
            if std::env::var_os("DOGMIND_PAGE_DEBUG").is_some() {
                if let Ok(done) = &done {
                    eprintln!("--- picture, {:?} {:?}: shot {} bytes\n{}", done.status, done.ended, std::fs::metadata(&shot).map_or(0, |meta| meta.len()), crate::sandbox::decode(&done.stderr));
                }
            }
            // (a browser that fell over while writing it leaves an empty file, or one that cannot be read)
            let shot_image = image::open(&shot);
            #[cfg(test)]
            note_run(true, shot_image.is_err());
            if let Ok(whole) = shot_image {
                if whole.crop_imm(0, 0, view.width, view.height).save_with_format(&picture, image::ImageFormat::Png).is_ok() {
                    taken = true;
                    said_there = done.map(|done| page_console(&crate::sandbox::decode(&done.stderr), &base_url, page.as_deref().zip(steps_copy.as_deref()))).unwrap_or_default();
                    break;
                }
            }
            // (a sandbox that could not run it, or a run that hung and was ended, will not do better the next time)
            if done.is_err() || started.elapsed() > LIMIT / 2 {
                break;
            }
        }
        profiles.clear();
        let _ = std::fs::remove_file(&shot);
        let _ = std::fs::remove_file(&frame);
        out.push_str(&if taken {
            format!("How it looks at {} ({}x{}), without WebGL drawing: `read_file` {}\n", view.label, view.width, view.height, picture.display())
        } else {
            format!("No picture at {} could be taken: the browser ended without one each time it was tried, which says nothing of the page. Do not build other pages to measure the layout instead; say in your answer that there is no picture.\n", view.label)
        });
        // What the page said at this size, where it says other than above: the console above is of the desktop's width, and a model that
        // measured a phone's layout from a script was given the desktop's numbers.
        if taken && !said_there.is_empty() && said_there != console {
            out.push_str(&format!("Console at {}:\n", view.label));
            for said in said_there.iter().take(MOST_LINES) {
                out.push_str(&format!("  {said}\n"));
            }
        }
    }
    Ok(out)
}

/// What the page itself said in a run, as the check reports it: the steps' own markers left out, and the copy the steps ran in named as the page.
/// `steps`: the page, and the name of the copy the steps ran in.
fn page_console(err: &str, base_url: &str, steps: Option<(&Path, &str)>) -> Vec<String> {
    let mut lines = console_lines(err, base_url);
    if let Some((page, copy)) = steps {
        let file_name = page.file_name().map_or(String::new(), |file| file.to_string_lossy().into_owned());
        lines.retain(|line| !line.contains("DOGMIND-STEPS-"));
        lines.iter_mut().for_each(|line| *line = line.replace(copy, &file_name));
    }
    lines
}

/// Files a check makes in the sandbox's folder, removed when it ends, however it ends.
struct Removing(Vec<PathBuf>);

impl Drop for Removing {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// The browser's profiles of the tries of one loop, each a name of its own in the sandbox's folder, all removed together when the loop is done (a try does not wait for the one before to be
/// cleared): only these, as others may be in use. Not stopped by the turn's Stop, which is what most often ends a run early: under it, the removal ended as it began and the profile
/// stayed. A browser that did not start has its profile go too, and so has one whose loop ended by an error.
struct Profiles<'a> {
    names: Vec<String>,
    cwd: &'a Path,
    workspace: &'a Path,
}

impl<'a> Profiles<'a> {
    fn new(cwd: &'a Path, workspace: &'a Path) -> Self {
        Profiles { names: Vec::new(), cwd, workspace }
    }

    /// The name of the next try's profile.
    fn next(&mut self) -> String {
        let name = profile_of(&next_run());
        self.names.push(name.clone());
        name
    }

    fn clear(&mut self) {
        if self.names.is_empty() {
            return;
        }
        let line = self.names.drain(..).map(|name| format!("rmdir /s /q \"%TEMP%\\{name}\"")).collect::<Vec<_>>().join(" & ");
        let _ = crate::sandbox::run(&line, self.cwd, self.workspace, false, Duration::from_secs(20), &AtomicBool::new(false));
    }
}

impl Drop for Profiles<'_> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// How long what a run left is kept: a run lasts minutes at most, and a copy of the app that is stopped mid-run, or crashes, leaves its profile, frame and shot.
const LEFTOVER_KEPT: Duration = Duration::from_secs(3600);

/// A run's id as `next_run` makes it: the process's id and the run's count.
fn is_run_id(text: &str) -> bool {
    text.split_once('-').is_some_and(|(process, count)| [process, count].iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())))
}

/// Whether a name in the sandbox's folder is what a run of the browser makes (and nothing else is touched: the folder is every command's).
fn is_run_leftover(name: &str) -> bool {
    let id_between = |before: &str, after: &str| name.strip_prefix(before).and_then(|rest| rest.strip_suffix(after)).is_some_and(is_run_id);
    id_between("dogmind-profile-", "") || id_between("dogmind-frame-", ".html") || id_between("dogmind-shot-", ".png") || id_between("dogmind-steps-", ".html")
}

/// What runs that did not end by themselves left in the folder, by name and by age: a run may be going on in another process.
fn forget_old_leftovers(temp: &Path, kept: Duration) {
    for entry in std::fs::read_dir(temp).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry.metadata().ok().and_then(|meta| meta.modified().ok()).and_then(|at| at.elapsed().ok()).is_some_and(|age| age >= kept);
        if old && is_run_leftover(&name) {
            let _ = if entry.file_type().is_ok_and(|kind| kind.is_dir()) { std::fs::remove_dir_all(entry.path()) } else { std::fs::remove_file(entry.path()) };
        }
    }
}

/// The pictures of checks more than a week old: each has a name of its own, and they would gather in the sandbox's temporary folder.
fn forget_old_pictures(temp: &Path) {
    const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);
    for entry in std::fs::read_dir(temp).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry.metadata().ok().and_then(|meta| meta.modified().ok()).and_then(|at| at.elapsed().ok()).is_some_and(|age| age > KEPT);
        if name.starts_with("dogmind-page-") && name.ends_with(".png") && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_week_old_picture_goes_and_a_new_one_and_other_files_stay() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new, other) = (dir.path().join("dogmind-page-20261020-101010000-desktop.png"), dir.path().join("dogmind-page-20261103-101010000-mobile.png"), dir.path().join("notes.png"));
        for file in [&old, &new, &other] {
            std::fs::write(file, b"png").unwrap();
        }
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        for file in [&old, &other] {
            std::fs::File::options().write(true).open(file).unwrap().set_modified(long_ago).unwrap();
        }
        forget_old_pictures(dir.path());
        assert!(!old.exists() && new.exists() && other.exists());
    }

    #[test]
    fn every_run_of_the_browser_has_names_of_its_own() {
        let (first, second) = (next_run(), next_run());
        assert_ne!(first, second);
        assert!(first.starts_with(&format!("{}-", std::process::id())) && is_run_id(&first), "{first}");
        let names = |run: &str| [profile_of(run), frame_of(run), shot_of(run), steps_page_of(run)];
        for (one, other) in names(&first).iter().zip(names(&second).iter()) {
            assert_ne!(one, other);
        }
        for name in names(&first) {
            assert!(name.contains(&first) && is_run_leftover(&name), "{name}");
        }
        // (threads of a process count from the one count, so that two sessions of the app at once do not share a name; and another process has its own id in it)
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(|| (0..50).map(|_| next_run()).collect::<Vec<_>>())).collect();
        let all: HashSet<String> = threads.into_iter().flat_map(|thread| thread.join().unwrap()).collect();
        assert_eq!(all.len(), 400);
        assert_ne!(profile_of("100-0"), profile_of("101-0"));
    }

    #[test]
    fn the_tries_of_one_loop_each_have_a_profile_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let mut profiles = Profiles::new(dir.path(), dir.path());
        let names: Vec<String> = (0..TRIES).map(|_| profiles.next()).collect();
        assert_eq!(names.iter().collect::<HashSet<_>>().len(), TRIES);
        assert!(names.iter().all(|name| is_run_leftover(name)), "{names:?}");
        // (none of them was made, so the sandbox has nothing to remove: it is not run for them)
        profiles.names.clear();
    }

    #[test]
    fn a_sweep_removes_only_old_names_a_run_makes_and_leaves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (old_folders, old_files) = (["dogmind-profile-4242-7"], ["dogmind-frame-4242-7.html", "dogmind-shot-4242-7.png", "dogmind-steps-4242-7.html"]);
        // What is not a run's, however old: the names the browser's runs once had, a name that only begins as theirs, a picture, and what a command made.
        let kept_files = ["dogmind-frame.html", "dogmind-shot.png", "dogmind-steps.html", "dogmind-frame-4242-7.txt", "dogmind-frame-x-7.html", "dogmind-shot-4242-7-8.png", "dogmind-steps-4242-.html", "dogmind-page-20261020-101010000-desktop.png", "my-dogmind-frame-4242-7.html", "notes-4242-7"];
        let kept_folders = ["dogmind-page", "dogmind-profile", "dogmind-profile-4242", "dogmind-profile-a-b", "dogmind-profile-4242-7-1", "work-4242-7"];
        for folder in old_folders.iter().chain(&kept_folders) {
            std::fs::create_dir_all(dir.path().join(folder).join("Default")).unwrap();
            std::fs::write(dir.path().join(folder).join("Default").join("Preferences"), b"{}").unwrap();
        }
        for file in old_files.iter().chain(&kept_files) {
            std::fs::write(dir.path().join(file), b"x").unwrap();
        }
        // A run still going: the same names, new.
        let (live_folder, live_file) = (dir.path().join("dogmind-profile-4242-8"), dir.path().join("dogmind-frame-4242-8.html"));
        std::fs::create_dir_all(&live_folder).unwrap();
        std::fs::write(&live_file, b"x").unwrap();
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
        for name in old_files.iter().chain(&kept_files).chain(&old_folders).chain(&kept_folders) {
            let path = dir.path().join(name);
            let mut options = std::fs::File::options();
            options.write(true);
            if path.is_dir() {
                std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, 0x0200_0000);
            }
            options.open(&path).unwrap().set_modified(long_ago).unwrap();
        }
        forget_old_leftovers(dir.path(), LEFTOVER_KEPT);
        for gone in old_folders.iter().chain(&old_files) {
            assert!(!dir.path().join(gone).exists(), "{gone} should have gone");
        }
        for stays in kept_files.iter().chain(&kept_folders) {
            assert!(dir.path().join(stays).exists(), "{stays} should have stayed");
        }
        assert!(live_folder.exists() && live_file.exists(), "a run that is going on keeps its names");
    }

    fn page(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let index = dir.path().join("index.html");
        (dir, index)
    }

    /// Nothing here is opened: the first of these, looked at as a file, was the app going to that machine with the person's sign-in, for what a page said.
    #[test]
    fn an_address_on_another_machine_is_remote_however_it_is_written() {
        for address in ["\\\\127.0.0.1\\share\\x.js", "\\\\?\\UNC\\127.0.0.1\\share\\x.js", "//127.0.0.1/share/x.js", "/\\127.0.0.1\\share\\x.js", "https://x/y.js", "HTTP://x/y.js"] {
            assert!(is_remote(address), "{address}");
        }
        for address in ["x.js", "./x.js", "sub\\x.js", "/x.js", "..\\x.js"] {
            assert!(!is_remote(address), "{address}");
        }
        let (_dir, index) = page(&[("index.html", "<script src=\"\\\\127.0.0.1\\nothing-here\\x.js\"></script>")]);
        assert_eq!(needs_of(&index).remote, ["\\\\127.0.0.1\\nothing-here\\x.js"]);
        // Through an import, which only names what begins with a `/` or a `.`.
        let (_dir, index) = page(&[("index.html", "<script type=\"module\">import '/\\\\127.0.0.1\\nothing-here\\x.js';</script>")]);
        assert_eq!(needs_of(&index).missing, Vec::<String>::new(), "not looked at, so not said to be missing");
    }

    #[test]
    fn a_files_own_address_with_a_query_is_that_file() {
        let (_dir, index) = page(&[("index.html", "<link rel=\"stylesheet\" href=\"a.css?v=1\"><script src=\"b.js?v=2#x\"></script>"), ("a.css", ""), ("b.js", "")]);
        let needs = needs_of(&index);
        assert_eq!(needs.missing, Vec::<String>::new());
        assert_eq!(needs.run, ["b.js?v=2#x"]);
    }

    #[test]
    fn a_name_cmd_would_work_out_is_not_put_on_its_line() {
        let (dir, _) = page(&[("a%CMDCMDLINE:~45,1%calc%CMDCMDLINE:~45,1%.html".replace(':', "_").as_str(), "<p>x</p>")]);
        let said = check_page(&serde_json::json!({"path": "a%CMDCMDLINE_~45,1%calc%CMDCMDLINE_~45,1%.html"}), dir.path(), &AtomicBool::new(false)).unwrap_err();
        assert!(said.contains("% in it"), "{said}");
    }

    #[test]
    fn a_module_that_needs_a_cdn_through_an_import_map_is_said_not_to_have_run() {
        let (_dir, index) = page(&[
            ("index.html", r#"<script type="importmap">{"imports":{"three":"https://unpkg.com/three@0.161.0/build/three.module.js","three/addons/":"https://unpkg.com/three@0.161.0/examples/jsm/"}}</script><script type="module" src="main.js"></script><script>console.log(1)</script>"#),
            ("main.js", "import * as THREE from 'three';\nimport { OrbitControls } from 'three/addons/controls/OrbitControls.js';\nimport './own.js';\n"),
            ("own.js", "export const a = 1;\n"),
        ]);
        let needs = needs_of(&index);
        assert_eq!(needs.remote, ["https://unpkg.com/three@0.161.0/build/three.module.js", "https://unpkg.com/three@0.161.0/examples/jsm/controls/OrbitControls.js"]);
        assert_eq!(needs.did_not_run, ["main.js"]);
        assert_eq!(needs.run, ["the page's script 3"]);
        assert!(needs.local_module);
        let said = report("index.html", "Edge", true, Some(&needs), &[], "<html><body>hi</body></html>");
        assert!(said.contains("DID NOT RUN"), "{said}");
        assert!(said.contains("a CDN library cannot load"), "{said}");
        assert!(!said.contains("no errors"), "it does not say all is well of what did not run: {said}");
    }

    #[test]
    fn a_plain_script_from_a_cdn_and_files_that_are_not_there_are_said() {
        let (_dir, index) = page(&[("index.html", r#"<link rel="stylesheet" href="gone.css"><script src="https://cdn.example/three.min.js"></script><script src="missing.js"></script><script src="own.js"></script>"#), ("own.js", "THREE;")]);
        let needs = needs_of(&index);
        assert_eq!(needs.remote, ["https://cdn.example/three.min.js"]);
        assert_eq!(needs.missing, ["gone.css", "missing.js"]);
        assert_eq!(needs.run, ["own.js"]);
        assert!(!needs.local_module);
    }

    #[test]
    fn only_what_the_page_said_is_kept_of_what_the_browser_says() {
        let err = concat!(
            "[1:2:1102/222026.041:ERROR:content\\browser\\x.cc:108] Encountered error\n",
            "[1:2:1102/222026.293:INFO:CONSOLE:5] \"LOG-FROM-PAGE\", source: file:///C:/work/index.html (5)\n",
            "[1:2:1102/222026.293:INFO:CONSOLE:6233] \"[ProtocolLaunch] wiring\", source: chrome-extension://abc/background.rollup.js (6233)\n",
            "[1:2:1102/222026.324:INFO:CONSOLE:9] \"shader says: ERROR: 0:1: 'nonsense' : undeclared identifier\n",
            "ERROR: 0:1: '}' : syntax error\n",
            "\", source: file:///C:/work/sub/a.js (9)\n",
        );
        let lines = console_lines(err, "file:///C:/work/");
        assert_eq!(lines[0], "index.html:5: LOG-FROM-PAGE");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].starts_with("a.js:9: shader says: ERROR: 0:1: 'nonsense'"), "{lines:?}");
        assert!(lines[1].contains("syntax error"), "{lines:?}");
    }

    #[test]
    fn what_a_page_shows_is_its_words() {
        let (title, text) = shown("<html><head><title> T </title><style>x{}</style></head><body><div>ran</div><script>let a</script> <p>two&nbsp;words</p></body></html>");
        assert_eq!((title.as_str(), text.as_str()), ("T", "ran two words"));
    }

    #[test]
    fn a_page_outside_the_workspace_or_not_a_page_is_refused_out_loud() {
        let (dir, _) = page(&[("index.html", "x"), ("a.txt", "x")]);
        let inner = dir.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let cancel = AtomicBool::new(false);
        let outside = check_page(&serde_json::json!({"path": "../index.html"}), &inner, &cancel).unwrap_err();
        assert!(outside.contains("outside the workspace"), "{outside}");
        assert!(check_page(&serde_json::json!({"path": "a.txt"}), dir.path(), &cancel).unwrap_err().contains("not a page"));
        assert!(check_page(&serde_json::json!({"path": "gone.html"}), dir.path(), &cancel).unwrap_err().contains("no such page"));
    }

    #[test]
    fn views_are_named_or_sized_and_the_desktop_is_the_default() {
        let at = |views: Value| views_of(&serde_json::json!({"path": "a.html", "views": views})).map(|views| views.iter().map(|view| (view.label.clone(), view.width, view.height)).collect::<Vec<_>>());
        assert_eq!(views_of(&serde_json::json!({"path": "a.html"})).unwrap(), [View { label: "desktop".into(), width: 1280, height: 800 }]);
        assert_eq!(at(serde_json::json!(["Mobile", "desktop", "mobile"])).unwrap(), [("mobile".to_string(), 390, 844), ("desktop".to_string(), 1280, 800)]);
        assert_eq!(at(serde_json::json!(["1280x2400", "10x99999"])).unwrap(), [("1280x2400".to_string(), 1280, 2400), ("10x99999".to_string(), 240, 4000)]);
        assert!(at(serde_json::json!(["tablet"])).unwrap_err().contains("tablet"));
    }

    #[test]
    fn the_frame_names_the_page_by_an_address_nothing_in_its_name_can_end() {
        let url = file_url(Path::new("C:\\Users\\me\\My Site\\a&b \"#1\".html"));
        assert_eq!(url, "file:///C:/Users/me/My%20Site/a%26b%20%22%231%22.html");
        let frame = frame_page(&file_url(Path::new("C:\\w\\index.html")), &View { label: "mobile".into(), width: 390, height: 844 });
        assert!(frame.contains("src=\"file:///C:/w/index.html\" width=\"390\" height=\"844\""), "{frame}");
    }

    #[test]
    fn a_url_is_a_server_on_this_machine_and_nothing_cmd_would_read_into() {
        for good in ["http://localhost:8000/", "http://127.0.0.1:5173/app/index.html?x=1&y=2", "http://[::1]:8080", "http://localhost/", "HTTP://LOCALHOST:3000/a"] {
            assert_eq!(served_address(good).as_deref(), Ok(good), "{good}");
        }
        for bad in ["http://example.com/", "http://localhost.evil.com/", "http://127.0.0.2/", "http://user@localhost/", "http://localhost:80a/", "file:///C:/x.html", "http://localhost:8000/%CMDCMDLINE%", "http://localhost:8000/a\"b", "http://localhost:8000/a b", "ftp://localhost/", "http://localhost:/"] {
            assert!(served_address(bad).is_err(), "{bad}");
        }
    }

    /// With the real browser and a real server, both in the real sandbox: what a command serves on this machine, the check can read.
    #[test]
    #[ignore = "starts a server and Edge in the sandbox"]
    fn a_page_a_local_server_gives_is_checked_by_its_url() {
        let (dir, _) = page(&[
            ("index.html", "<!DOCTYPE html><title>served</title><div id=o>wait</div><script type=module src=app.mjs></script>"),
            ("app.mjs", "import { word } from './word.mjs'; document.getElementById('o').textContent = word; console.log('module ran: ' + location.origin);"),
            ("word.mjs", "export const word = 'from a module';"),
        ]);
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (folder, halt) = (dir.path().to_path_buf(), stop.clone());
        let server = std::thread::spawn(move || crate::sandbox::run("python -m http.server 8765 --bind 127.0.0.1", &folder, &folder, false, Duration::from_secs(90), &halt));
        // (Python in the sandbox takes a few seconds to be listening the first time)
        std::thread::sleep(Duration::from_secs(8));
        let said = check_page(&serde_json::json!({"url": "http://127.0.0.1:8765/index.html"}), dir.path(), &AtomicBool::new(false));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = server.join();
        let said = said.unwrap();
        println!("{said}");
        assert!(said.contains("module ran: http://127.0.0.1:8765") && said.contains("text: from a module"), "{said}");
        assert!(!said.contains("Permissions-Policy"), "the browser's own words are not the page's: {said}");
        assert!(said.contains("How it looks at desktop"), "{said}");
    }

    /// With the real browser, in the real sandbox: a server on this machine that no command started (this test's own, outside the sandbox)
    /// is out of the browser's reach. Loopback from an app container reaches only what runs in the same container: what this app's commands
    /// serve, and nothing else on the machine, so a page cannot lead the check to another program's local service.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_server_outside_the_sandbox_cannot_be_read() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = std::sync::Arc::new(AtomicBool::new(false));
        let told = served.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut asked = [0u8; 2048];
                let _ = stream.read(&mut asked);
                told.store(true, std::sync::atomic::Ordering::Relaxed);
                let body = "<!DOCTYPE html><title>outside</title><div>SECRET-OUTSIDE-THE-SANDBOX</div>";
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let said = check_page(&serde_json::json!({"url": format!("http://127.0.0.1:{port}/")}), dir.path(), &AtomicBool::new(false));
        println!("{said:?}");
        let text = match &said { Ok(text) | Err(text) => text.clone() };
        // (a browser that did not run says nothing of what it could reach)
        assert!(!text.contains("NOT CHECKED"), "the browser did not run, so this shows nothing: {text}");
        assert!(!text.contains("SECRET-OUTSIDE-THE-SANDBOX"), "{text}");
        assert!(!served.load(std::sync::atomic::Ordering::Relaxed), "the browser reached a server outside the sandbox");
    }

    /// With the real browser, in the real sandbox: a phone's width is the width the page is laid out at, not the browser window's least.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_picture_at_a_phones_width_is_laid_out_at_it() {
        let (dir, _) = page(&[("index.html", "<!DOCTYPE html><title>w</title><style>body{margin:0}.w{height:40px;background:#cc3333}@media(max-width:600px){.w{background:#3366cc}}</style><div class=w id=w></div><script>document.getElementById('w').textContent=innerWidth;console.log('width ' + innerWidth)</script>")]);
        let said = check_page(&serde_json::json!({"path": "index.html", "views": ["desktop", "mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        // (what a script measures at the phone's width is said for it; at the desktop's too, where the picture's frame is not the window the
        // console above was read in)
        assert!(said.contains("Console at mobile:\n  index.html:1: width 390"), "{said}");
        let path = |label: &str| said.lines().find(|line| line.starts_with(&format!("How it looks at {label} "))).and_then(|line| line.split("`read_file` ").nth(1)).map(PathBuf::from).unwrap_or_else(|| panic!("{label}: {said}"));
        let (desktop, mobile) = (image::open(path("desktop")).unwrap().to_rgb8(), image::open(path("mobile")).unwrap().to_rgb8());
        assert_eq!((desktop.width(), desktop.height(), mobile.width(), mobile.height()), (1280, 800, 390, 844));
        assert_eq!(mobile.get_pixel(370, 20).0, [0x33, 0x66, 0xcc], "the phone's rules, across the whole of its width");
        assert_eq!(desktop.get_pixel(1260, 20).0, [0xcc, 0x33, 0x33]);
    }

    /// With the real browser, in the real sandbox: what it was measured to do.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_real_page_reports_its_errors_and_what_it_shows() {
        let (dir, _) = page(&[
            ("index.html", "<!DOCTYPE html><title>t</title><div id=ok>start</div><canvas id=c></canvas><script type=module src=bad.mjs></script><script>console.log('FROM-PAGE');document.getElementById('ok').textContent='ran';const gl=document.getElementById('c').getContext('webgl2');const s=gl.createShader(gl.FRAGMENT_SHADER);gl.shaderSource(s,'void main(){ nonsense }');gl.compileShader(s);console.log('shader: '+gl.getShaderInfoLog(s));missing.call();</script>"),
            ("bad.mjs", "let x = ;\n"),
            ("cdn.html", "<!DOCTYPE html><title>c</title><script type=module>import * as T from 'https://unpkg.com/three@0.161.0/build/three.module.js'; console.log('NEVER')</script>"),
        ]);
        let cancel = AtomicBool::new(false);
        let said = check_page(&serde_json::json!({"path": "index.html"}), dir.path(), &cancel).unwrap();
        println!("{said}");
        assert!(said.contains("How it looks") || said.contains("No picture"), "a picture, or that there is none: {said}");
        for wanted in ["FROM-PAGE", "undeclared identifier", "missing is not defined", "SyntaxError", "text: ran", "file://"] {
            assert!(said.contains(wanted), "{wanted}: {said}");
        }
        let said = check_page(&serde_json::json!({"path": "cdn.html"}), dir.path(), &cancel).unwrap();
        println!("{said}");
        assert!(said.contains("DID NOT RUN"), "{said}");
        assert!(!said.contains("NEVER"), "{said}");
    }

    #[test]
    fn steps_go_at_the_end_of_the_page_with_its_folder_as_base_and_cannot_end_their_script() {
        let page = with_steps("<!DOCTYPE html><html><head><title>t</title></head><body><p>x</p></body></html>", "file:///C:/w/", "await click('#a'); console.log('</script>')");
        assert!(page.contains("<head><base href=\"file:///C:/w/\"><title>"), "{page}");
        let (steps_at, body_end) = (page.find("click('#a')").unwrap(), page.rfind("</body>").unwrap());
        assert!(page.find("<p>x</p>").unwrap() < steps_at && steps_at < body_end, "{page}");
        assert!(page.contains("console.log('<\\/script>')") && page.matches("</script>").count() == 1, "{page}");
        let bare = with_steps("<p>no head</p>", "file:///C:/w/", "1");
        assert!(bare.starts_with("<base href=") && bare.ends_with("</script>"), "{bare}");
        let based = with_steps("<head><base href=\"x/\"></head>", "file:///C:/w/", "1");
        assert_eq!(based.matches("<base").count(), 1, "a page's own base stays");
    }

    /// With the real browser, in the real sandbox: the steps type and click as a person would, and the page is read after them.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_page_is_driven_by_its_steps() {
        let (dir, _) = page(&[
            ("index.html", "<!DOCTYPE html><title>todo</title><link rel=stylesheet href=style.css><input id=t><button id=add>add</button><ul id=l></ul><script src=app.js></script>"),
            ("style.css", "li{color:#c33}"),
            ("app.js", "document.getElementById('add').onclick=()=>{const li=document.createElement('li');li.textContent=document.getElementById('t').value;document.getElementById('l').append(li);console.log('added '+li.textContent)};"),
        ]);
        let steps = "await fill('#t', 'milk'); await click('#add'); console.log('items ' + count('li') + ' ' + text('li'));";
        let said = check_page(&serde_json::json!({"path": "index.html", "steps": steps, "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        assert!(said.contains("Steps: all ran") && said.contains("items 1 milk") && said.contains("app.js:1: added milk"), "{said}");
        assert!(said.contains("text: add milk") || said.contains("milk"), "the page as the steps left it: {said}");
        assert!(said.contains("How it looks at mobile"), "{said}");
        let failed = check_page(&serde_json::json!({"path": "index.html", "steps": "await click('#nothing')"}), dir.path(), &AtomicBool::new(false)).unwrap();
        assert!(failed.contains("STEPS FAILED: nothing matches #nothing"), "{failed}");
    }

    /// `press` as Playwright has it, `press(selector, key)`, which a real model wrote and which failed as "nothing matches Enter"; the key alone,
    /// on what has the focus; and the key first, as this once took it.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_key_is_pressed_as_playwright_says_it() {
        let (dir, _) = page(&[
            ("index.html", "<!DOCTYPE html><title>t</title><form id=f><input id=t></form><ul id=l></ul><script>document.getElementById('f').onsubmit=(e)=>{e.preventDefault();const li=document.createElement('li');li.textContent=document.getElementById('t').value;document.getElementById('l').append(li)};</script>"),
        ]);
        let steps = "await fill('#t', 'a'); await press('#t', 'Enter'); await fill('#t', 'b'); await press('Enter'); await fill('#t', 'c'); await press('Enter', '#t'); console.log('items ' + count('li'));";
        let said = check_page(&serde_json::json!({"path": "index.html", "steps": steps}), dir.path(), &AtomicBool::new(false)).unwrap();
        assert!(said.contains("Steps: all ran") && said.contains("items 3"), "{said}");
    }

    /// What a batch of plain checks came to: the checks that said NOT CHECKED, the pictures not taken, and the browser's own runs behind them
    /// (a run that crashed is tried again, so the checks alone hide most of the crashes).
    #[derive(Debug, Default)]
    struct Batch {
        checks: u64,
        not_checked: u64,
        no_picture: u64,
        page_runs: u64,
        page_runs_bad: u64,
        picture_runs: u64,
        picture_runs_bad: u64,
        /// Profiles, frames and shots of this process's runs still in the folder when the checks were done.
        left_behind: u64,
        seconds: u64,
    }

    impl Batch {
        fn line(&self) -> String {
            format!(
                "checks={} not_checked={} no_picture={} page_runs={} page_runs_bad={} picture_runs={} picture_runs_bad={} left_behind={} seconds={}",
                self.checks, self.not_checked, self.no_picture, self.page_runs, self.page_runs_bad, self.picture_runs, self.picture_runs_bad, self.left_behind, self.seconds
            )
        }

        fn of(line: &str) -> Option<Batch> {
            let field = |name: &str| line.split_whitespace().find_map(|part| part.strip_prefix(&format!("{name}="))).and_then(|value| value.parse::<u64>().ok());
            Some(Batch {
                checks: field("checks")?,
                not_checked: field("not_checked")?,
                no_picture: field("no_picture")?,
                page_runs: field("page_runs")?,
                page_runs_bad: field("page_runs_bad")?,
                picture_runs: field("picture_runs")?,
                picture_runs_bad: field("picture_runs_bad")?,
                left_behind: field("left_behind")?,
                seconds: field("seconds")?,
            })
        }
    }

    /// `checks` plain checks of one page, one after another, in this process.
    fn plain_checks(checks: u64) -> Batch {
        let (dir, _) = page(&[("index.html", "<!DOCTYPE html><title>plain</title><p>hello</p>")]);
        for counter in &BROWSER_RUNS {
            counter.store(0, Ordering::Relaxed);
        }
        let started = std::time::Instant::now();
        let mut batch = Batch { checks, ..Batch::default() };
        for _ in 0..checks {
            let said = check_page(&serde_json::json!({"path": "index.html"}), dir.path(), &AtomicBool::new(false)).unwrap();
            batch.not_checked += u64::from(said.contains("NOT CHECKED"));
            batch.no_picture += said.matches("No picture at").count() as u64;
        }
        let runs: Vec<u64> = BROWSER_RUNS.iter().map(|counter| counter.load(Ordering::Relaxed)).collect();
        (batch.page_runs, batch.page_runs_bad, batch.picture_runs, batch.picture_runs_bad) = (runs[0], runs[1], runs[2], runs[3]);
        let ours = format!("-{}-", std::process::id());
        let temp = crate::sandbox::temp_folder().unwrap();
        batch.left_behind = std::fs::read_dir(temp).unwrap().flatten().filter(|entry| entry.file_name().to_str().is_some_and(|name| is_run_leftover(name) && name.contains(&ours))).count() as u64;
        batch.seconds = started.elapsed().as_secs();
        batch
    }

    fn batch_size() -> u64 {
        std::env::var("DOGMIND_PAGE_BATCH").ok().and_then(|text| text.parse().ok()).unwrap_or(10)
    }

    /// The other process of the two-at-once test: the same checks in a process of its own, reported on a line the first reads. Without the first's variable it does nothing.
    #[test]
    #[ignore = "starts Edge in the sandbox; run by the test that runs two checks at once"]
    fn a_batch_of_plain_checks_for_a_second_process() {
        if std::env::var_os("DOGMIND_PAGE_PEER").is_none() {
            return;
        }
        println!("BATCH {}", plain_checks(batch_size()).line());
    }

    /// With the real browser, in the real sandbox: plain checks one after another, and then two processes at once (as two sessions of the app, a
    /// session and a test run, or two copies of the app are), counted by the checks that were lost and by the browser's runs behind them. How many
    /// runs the browser loses depends on the machine (on one, about 6 in 10 with the runs alone or at once alike), so that is printed and not asserted;
    /// what is asserted is that no run leaves its profile, frame or shot behind, which another run could not have taken from it. Run it alone, by name:
    /// `DOGMIND_PAGE_BATCH=10 cargo test -p dogmind-agent --target-dir target-ui plain_checks_alone_and_two_at_once -- --ignored --nocapture`.
    #[test]
    #[ignore = "starts Edge in the sandbox, 4 x DOGMIND_PAGE_BATCH times"]
    fn plain_checks_alone_and_two_at_once_are_counted() {
        let alone = plain_checks(batch_size());
        println!("ALONE   {}", alone.line());
        let mut peer = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["page_check::tests::a_batch_of_plain_checks_for_a_second_process", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env("DOGMIND_PAGE_PEER", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut told = String::new();
        let mut stdout = peer.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            std::io::Read::read_to_string(&mut stdout, &mut told).ok();
            told
        });
        let mine = plain_checks(batch_size());
        println!("TOGETHER mine  {}", mine.line());
        // (the other ends by itself; this stops only the process it started, if it has not)
        let ended = std::time::Instant::now();
        while peer.try_wait().unwrap().is_none() {
            if ended.elapsed() > Duration::from_secs(15 * 60) {
                let _ = peer.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let told = reader.join().unwrap();
        let theirs = told.lines().find_map(|line| line.split_once("BATCH ")).and_then(|(_, line)| Batch::of(line)).unwrap_or_else(|| panic!("the other process said nothing usable: {told}"));
        println!("TOGETHER theirs {}", theirs.line());
        for (which, batch) in [("alone", &alone), ("mine", &mine), ("theirs", &theirs)] {
            assert_eq!(batch.left_behind, 0, "{which}: {}", batch.line());
        }
    }
}
