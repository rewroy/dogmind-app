//! Opening a page of the workspace in a headless browser, inside the sandbox: what its scripts printed and threw, and what it shows. It is the same sandbox as any command's, with no
//! network, so what a page takes from a CDN does not load; this says so plainly, as nothing else would tell: the page just sits there with no error.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

/// A page that has not finished in this long will not: a big WebGL drawing under the software renderer ends in a crash at about 25 s, with nothing more said.
const LIMIT: Duration = Duration::from_secs(20);
/// (not `--disable-breakpad` or `--disable-crash-reporter`: with those the browser crashes in about half its runs in the sandbox, against about 1 in 30 finally without. What keeps a crash
/// from raising a dialog is the error mode and the job's limit, in `sandbox`.)
const FLAGS: &str = "--headless=new --no-sandbox --disable-gpu --no-first-run --disable-extensions --disable-background-networking --disable-sync --disable-component-update --enable-logging=stderr --log-level=0 --allow-file-access-from-files --window-size=1000,650 --virtual-time-budget=1500";
const TRIES: usize = 4;
const MOST_LINES: usize = 25;
const MOST_LINE: usize = 400;
const MOST_TEXT: usize = 400;
/// (one at a time: they share a profile folder, and it goes when they are done)
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

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

fn report(page_name: &str, browser_name: &str, finished: bool, needs: &Needs, console: &[String], dom: &str) -> String {
    let mut out = format!("{page_name} in headless {browser_name}, with no network.\n");
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
        out.push_str("A browser refuses a module script from file:// when the page is opened by double-click, which this check allows: that page needs a local server to be opened, or a plain script.\n");
    }
    if needs.run.is_empty() && needs.did_not_run.is_empty() {
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

pub fn check_page(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let given = arguments.get("path").and_then(Value::as_str).filter(|path| !path.is_empty()).ok_or("path is required: the page, under the workspace")?;
    let path = if Path::new(given).is_absolute() { PathBuf::from(given) } else { workspace.join(given) };
    let page = if path.is_dir() { path.join("index.html") } else { path };
    if !page.is_file() {
        return Err(format!("{given}: no such page"));
    }
    let (full, root) = (std::fs::canonicalize(&page).map_err(|error| format!("{given}: {error}"))?, std::fs::canonicalize(workspace).map_err(|error| format!("{given}: {error}"))?);
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
    let Some((program, browser_name)) = browser() else {
        return Err("neither Edge nor Chrome is on this machine, so the page could not be opened: say that it has not been run".to_string());
    };
    let needs = needs_of(&page);
    let plain = |path: &Path| path.display().to_string().trim_start_matches("\\\\?\\").to_string();
    let cwd = page.parent().unwrap_or(workspace).to_path_buf();
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let line = format!("\"{}\" {FLAGS} --user-data-dir=\"%TEMP%\\dogmind-page\" --dump-dom \"{}\"", program.display(), plain(&page));
    // (the browser sometimes crashes in the sandbox, whatever the page, so one crash says nothing of the page: it is tried again)
    let (mut dom, mut err, mut finished, mut how) = (String::new(), String::new(), false, String::new());
    for _ in 0..TRIES {
        let started = std::time::Instant::now();
        let done = crate::sandbox::run_browser(&line, &cwd, workspace, LIMIT, cancel)?;
        let _ = crate::sandbox::run("rmdir /s /q \"%TEMP%\\dogmind-page\"", &cwd, workspace, false, Duration::from_secs(20), cancel);
        (dom, err) = (crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr));
        finished = done.status == Some(0) && !dom.trim().is_empty();
        how = done.status.map_or(done.ended.to_string(), |code| format!("exit code {:#x}", code as u32));
        if finished || started.elapsed() > LIMIT / 2 || err.contains("INFO:CONSOLE") && !err.contains("chrome-extension") {
            break;
        }
        // (its crashes come together: a real run lost all four tries inside 6.5 s, and that page opened ten times of ten afterwards. Why is not known.)
        std::thread::sleep(Duration::from_secs(3));
    }
    let folder_url = format!("file:///{}/", plain(&root).replace('\\', "/"));
    let name = page.strip_prefix(workspace).unwrap_or(&page).display().to_string();
    let said = report(&name, browser_name, finished, &needs, &console_lines(&err, &folder_url), &dom);
    if !finished {
        return Ok(format!("{said}The browser's end: {how}.\n"));
    }
    // What it looks like, for a model that can see, and it costs that model nothing if it does not ask. Not of a page that draws with WebGL, which ends the browser here.
    let picture = crate::sandbox::temp_folder().map(|temp| temp.join("dogmind-page.png"));
    let Some(picture) = picture else { return Ok(said) };
    let _ = std::fs::remove_file(&picture);
    let shot = format!("\"{}\" {FLAGS} --user-data-dir=\"%TEMP%\\dogmind-page\" --screenshot=\"{}\" \"{}\"", program.display(), picture.display(), plain(&page));
    for _ in 0..3 {
        let _ = std::fs::remove_file(&picture);
        let _ = crate::sandbox::run_browser(&shot, &cwd, workspace, LIMIT, cancel);
        let _ = crate::sandbox::run("rmdir /s /q \"%TEMP%\\dogmind-page\"", &cwd, workspace, false, Duration::from_secs(20), cancel);
        // (a browser that fell over while writing it leaves an empty file)
        if std::fs::metadata(&picture).is_ok_and(|file| file.len() > 0) {
            return Ok(format!("{said}How it looks, without WebGL drawing: `read_file` {}\n", picture.display()));
        }
    }
    Ok(format!("{said}No picture of it could be taken.\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let said = report("index.html", "Edge", true, &needs, &[], "<html><body>hi</body></html>");
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
}
