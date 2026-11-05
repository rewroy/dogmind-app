//! Opening a page of the workspace in a headless browser, inside the sandbox: what its scripts printed and threw, and what it shows. It is the same sandbox as any command's, with no
//! network, so what a page takes from a CDN does not load; this says so plainly, as nothing else would tell: the page just sits there with no error.

use std::cell::RefCell;
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
/// `msWindowsTaskManager` is off because Edge's part that tells the Windows Task Manager of its processes (`WindowsTaskManagerHandler::DoTasksUpdated`, in msedge.dll) reads through a
/// null when the registration it makes fails, which it does in a container: the browser ended with 0xc0000005 in about 6 runs of 10 on this machine, always there, and in none of
/// 90 with the feature off. A browser that does not know the name (Chrome) ignores it.
/// The flags, with what a debug build is told to add (for a look at what the browser does in the sandbox).
fn flags() -> String {
    #[cfg(debug_assertions)]
    if let Ok(more) = std::env::var("DOGMIND_PAGE_FLAGS") {
        return format!("{FLAGS} {more}");
    }
    FLAGS.to_string()
}
const FLAGS: &str = "--headless=new --no-sandbox --disable-gpu --no-first-run --disable-extensions --disable-background-networking --disable-sync --disable-component-update --disable-features=msWindowsTaskManager --enable-logging=stderr --log-level=0 --allow-file-access-from-files";

/// The command line of a run that dumps the page's DOM, as the sandbox's shell is given it: `%TEMP%` is the container's own folder, where the run's profile is.
fn page_line(program: &Path, flags: &str, wait: u64, profile: &str, address: &str) -> String {
    format!("\"{}\" {flags} --window-size=1280,800 --virtual-time-budget={wait} --user-data-dir=\"%TEMP%\\{profile}\" --dump-dom \"{address}\"", program.display())
}

/// The command line of a run that takes a picture of `frame` (a path) into `shot`, in a window of `window`.
fn shot_line(program: &Path, flags: &str, window: (u32, u32), wait: u64, profile: &str, shot: &Path, frame: &str) -> String {
    format!("\"{}\" {flags} --window-size={},{} --virtual-time-budget={wait} --user-data-dir=\"%TEMP%\\{profile}\" --screenshot=\"{}\" \"{frame}\"", program.display(), window.0, window.1, shot.display())
}

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

/// What a model's steps are run with, in the page: Playwright's way of saying them, done with the page's own DOM. The helpers `click`, `fill`, `press`, `check`, `select`, `wait`, `text`,
/// `count` and `visible` are the names here; what models write of Playwright itself (`page.evaluate(fn)`, `evaluate(fn)`, `page.click(sel)`, `page.fill(sel, v)`, `page.press(sel, key)`,
/// `page.waitForTimeout(ms)`, `page.locator(sel).click()`, `page.$eval`, `page.$$eval`) is mapped onto them, a few forms and no more, and any other name is the error it is, with the
/// helpers there are in it. A browser that stays open
/// to be driven cannot run in the sandbox (node there cannot make the pipes Playwright speaks over, and a headless Edge that is kept open falls
/// over within a second, where one that loads a page and ends does not), so the steps go in the page and run in the load that is read. A reload they make as a statement (`location.reload()`,
/// `await reload()`, `await page.reload()`) divides them into parts, a part in each load, and `with_steps` says why, and how, below.
const STEPS_PRELUDE: &str = r#"<script>
(async () => {
  const wait = (ms) => new Promise((done) => setTimeout(done, ms));
  // (which call of the helpers was made last, and what it was: if the steps take the page to another document, that is where they were)
  let calls = 0, last = null;
  const said = (value) => { try { const text = JSON.stringify(value); return text.length > 40 ? text.slice(0, 37) + '...' : text; } catch (error) { return '?'; } };
  const note = (name, ...args) => { calls += 1; last = { n: calls, call: name + '(' + args.filter((arg) => arg !== undefined).map(said).join(', ') + ')' }; };
  const find = (selector) => { const found = document.querySelector(selector); if (!found) throw new Error('nothing matches ' + selector); return found; };
  const click = async (selector) => { note('click', selector); find(selector).click(); await wait(50); };
  const fill = async (selector, value) => { note('fill', selector, value); const field = find(selector); field.focus(); field.value = value; field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new Event('change', { bubbles: true })); await wait(50); };
  const check = async (selector, on = true) => { note('check', selector); const box = find(selector); if (box.checked !== on) box.click(); await wait(50); };
  const select = async (selector, value) => { note('select', selector, value); const list = find(selector); list.value = value; list.dispatchEvent(new Event('change', { bubbles: true })); await wait(50); };
  const KEY = /^((Control|Shift|Alt|Meta)\+)*(Enter|Tab|Escape|Backspace|Delete|Space|Home|End|PageUp|PageDown|Arrow(Up|Down|Left|Right)|F\d{1,2}|.)$/;
  const press = async (first, second) => {
    note('press', first, second);
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
  // (what a model writes of Playwright, on the helpers above; `evaluate` runs the function in this page itself, as the steps are, and gives what it gives)
  const evaluate = async (fn, ...args) => { const made = typeof fn === 'string' ? (0, eval)(fn) : fn; return typeof made === 'function' ? made(...args) : made; };
  const locator = (selector) => ({ click: () => click(selector), fill: (value) => fill(selector, value), press: (key) => press(selector, key), check: () => check(selector), textContent: async () => find(selector).textContent, isVisible: async () => visible(selector), count: async () => count(selector) });
  const page = {
    evaluate, locator,
    click: (selector) => click(selector), fill: (selector, value) => fill(selector, value), press: (selector, key) => press(selector, key), check: (selector) => check(selector), selectOption: (selector, value) => select(selector, value),
    waitForTimeout: (ms) => wait(ms), reload: () => reload(),
    textContent: async (selector) => find(selector).textContent, isVisible: async (selector) => visible(selector),
    $eval: (selector, fn, ...args) => evaluate(fn, find(selector), ...args),
    $$eval: (selector, fn, ...args) => evaluate(fn, [...document.querySelectorAll(selector)], ...args),
  };
  // (a reload that is not at the steps' top level, in a function or a callback, is made here, and runs them from the start again, as the page loads again)
  const reload = async () => { location.reload(); await new Promise(() => {}); };
  // (what a person sees of the page: the text the browser itself lays out, hidden elements left out; cut where a character is whole)
  const seen = () => { try { return Array.from(document.body.innerText || '').slice(0, 2000).join(''); } catch (error) { return ''; } };
  await new Promise((done) => document.readyState === 'complete' ? done() : addEventListener('load', done));
"#;
const STEPS_END: &str = r#"  ];
  // Which part is due is kept in the tab's own storage, under this run's name: nothing there is the first load, `due:N` is what a reload of the steps' own asked for, and anything
  // else (a part that was taken and not ended by a reload of the steps': the page's own script reloaded it) is no part due, so that none runs twice.
  let state = null;
  try { state = sessionStorage.getItem(key); } catch (error) { state = undefined; }
  if (state === undefined && parts.length > 1) { console.error('DOGMIND-STEPS-FAILED: this page does not let the steps keep which part is next across a reload (its sessionStorage is refused)'); return; }
  const index = state === null || state === undefined ? 0 : state.startsWith('due:') ? Number(state.slice(4)) : -1;
  if (!(index >= 0 && index < parts.length)) return;
  const keep = (value) => { try { sessionStorage.setItem(key, value); } catch (error) {} };
  keep('ran:' + index);
  console.log('DOGMIND-STEPS-PART ' + (index + 1) + '/' + parts.length);
  // A step can take the page to another document (a form it submits, a link it follows, `location.href = ...`): the new document has none of the steps, so these end there, and neither DONE nor
  // FAILED is said. Which call was the last, and where the page went, is said before it goes. (A reload is not that: the parts are cut at the ones the steps make, and the rest is the page's own.)
  let underway = true;
  if (window.navigation) {
    navigation.addEventListener('navigate', (event) => {
      if (!underway || event.navigationType === 'reload' || event.destination.sameDocument) return;
      underway = false;
      console.log('DOGMIND-STEPS-LEFT:' + encodeURIComponent(JSON.stringify({ step: last, to: event.destination.url })));
    });
  }
  try {
    await parts[index]();
    underway = false;
    if (index + 1 < parts.length) {
      keep('due:' + (index + 1));
      location.reload();
      await new Promise(() => {});
    }
    keep('done');
    console.log('DOGMIND-STEPS-DONE');
  } catch (error) {
    underway = false;
    keep('done');
    // (a name that is not here: the ones that are, once, as the model has no other place to read them)
    const hint = error instanceof ReferenceError || /is not a function/.test(String(error && error.message)) ? ' (here: click, fill, press, check, select, wait, reload, text, count, visible, evaluate, and page.click/fill/press/check/selectOption/waitForTimeout/reload/textContent/isVisible/evaluate/$eval/$$eval/locator(sel); the rest is plain JavaScript in the page)' : '';
    console.error('DOGMIND-STEPS-FAILED: ' + error.message + hint);
  }
  console.log('DOGMIND-STEPS-TEXT:' + encodeURIComponent(seen()));
})();
</script>"#;
/// How much more of its own time the browser is given for a plain page than the page is read at: the script that says its words waits for the page's time, and the browser must still be going.
const READ_MARGIN_MS: u64 = 400;

/// What a plain page of HTML is opened with, in a copy, so that the words it shows are the browser's own account of them (its style sheets, its media queries and its window applied), and
/// not a guess from the DOM, which knows none of them: a script that waits for the page's time and says them on the console line the steps' run uses (`DOGMIND-STEPS-TEXT`, percent-encoded,
/// cut where a character is whole). It does not touch the page.
fn reading_script(wait: u64) -> String {
    format!(
        "<script>\n(async () => {{\n  await new Promise((done) => document.readyState === 'complete' ? done() : addEventListener('load', done));\n  await new Promise((done) => setTimeout(done, {wait}));\n  let text = '';\n  try {{ text = Array.from(document.body.innerText || '').slice(0, 2000).join(''); }} catch (error) {{}}\n  console.log('DOGMIND-STEPS-TEXT:' + encodeURIComponent(text));\n}})();\n</script>"
    )
}

/// The most parts the steps are cut into by reloads. A page that reloads more often than that is not what a check is made of.
const MOST_PARTS: usize = 5;

/// How far a reload statement reaches, if `rest` begins with one: `location.reload()`, `window.location.reload()`, `await reload()` and `await page.reload()` (the `await` is as
/// optional as the `;` after), with plain arguments if any.
fn reload_statement(rest: &str) -> Option<usize> {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| Regex::new(r"^(?:await\s+)?(?:(?:window\.)?location\.reload|reload|page\.reload)\s*\([^()]*\)[ \t]*;?").expect("the pattern is valid"));
    pattern.find(rest).map(|found| found.end())
}

/// The end of the string, template or comment that begins at `at` in `bytes`, or `at` itself if none does. A template's `${...}` holds code, which has strings of its own.
fn skip_quoted(bytes: &[u8], at: usize) -> usize {
    let end = bytes.len();
    match bytes[at] {
        quote @ (b'"' | b'\'') => {
            let mut i = at + 1;
            while i < end && bytes[i] != quote && bytes[i] != b'\n' {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            (i + 1).min(end)
        }
        b'`' => {
            let mut i = at + 1;
            while i < end && bytes[i] != b'`' {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    let mut depth = 1;
                    i += 2;
                    while i < end && depth > 0 {
                        match bytes[i] {
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            b'"' | b'\'' | b'`' | b'/' => {
                                let after = skip_quoted(bytes, i);
                                if after > i {
                                    i = after;
                                    continue;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            (i + 1).min(end)
        }
        b'/' if bytes.get(at + 1) == Some(&b'/') => bytes[at..].iter().position(|byte| *byte == b'\n').map_or(end, |line| at + line),
        b'/' if bytes.get(at + 1) == Some(&b'*') => bytes[at + 2..].windows(2).position(|pair| pair == b"*/").map_or(end, |close| at + 2 + close + 2),
        _ => at,
    }
}

/// The steps cut at each reload they make as a statement of their own, outside strings and comments and outside any bracket (a reload in a function, a callback or the block of a
/// condition is not a cut: it is the page being loaded again from inside them, as it was). A reload ends the part before it; the part after it begins where it ends.
fn split_steps(steps: &str) -> Vec<String> {
    let bytes = steps.as_bytes();
    let (mut parts, mut start, mut at, mut depth) = (Vec::new(), 0, 0, 0i32);
    // (the last byte of code seen, and whether a line ended since: a statement begins after `;` or `}`, or on a line of its own)
    let (mut last, mut line_ended) = (None::<u8>, true);
    while at < bytes.len() {
        let byte = bytes[at];
        let after = skip_quoted(bytes, at);
        if after > at {
            if byte == b'/' {
                // (a comment: a line that ends in it has ended)
                line_ended |= steps[at..after].contains('\n') || after >= bytes.len() || bytes[after] == b'\n';
            } else {
                (last, line_ended) = (Some(byte), false);
            }
            at = after;
            continue;
        }
        match byte {
            b'\n' => line_ended = true,
            b' ' | b'\t' | b'\r' => {}
            b'(' | b'[' | b'{' => (depth, last, line_ended) = (depth + 1, Some(byte), false),
            b')' | b']' | b'}' => (depth, last, line_ended) = (depth - 1, Some(byte), false),
            _ => {
                let word_begins = byte.is_ascii_alphabetic() && (at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || matches!(bytes[at - 1], b'_' | b'$' | b'.')));
                let statement_begins = last.is_none() || matches!(last, Some(b';' | b'}')) || line_ended;
                if depth == 0 && word_begins && statement_begins {
                    if let Some(length) = reload_statement(&steps[at..]) {
                        parts.push(steps[start..at].to_string());
                        (at, start) = (at + length, at + length);
                        (last, line_ended) = (Some(b';'), false);
                        continue;
                    }
                }
                (last, line_ended) = (Some(byte), false);
            }
        }
        at += 1;
    }
    parts.push(steps[start..].to_string());
    parts
}

/// The steps in the parts a reload makes of them, at most `MOST_PARTS`, and how many more there were.
fn step_parts(steps: &str) -> (Vec<String>, usize) {
    let mut parts = split_steps(steps);
    let more = parts.len().saturating_sub(MOST_PARTS);
    parts.truncate(MOST_PARTS);
    (parts, more)
}

/// The numbers of the parts of the steps that began, in the order they did, from what the browser said: a line `DOGMIND-STEPS-PART 2/3` for each.
fn parts_begun(err: &str) -> Vec<usize> {
    err.lines().filter_map(|line| line.split_once("DOGMIND-STEPS-PART ").and_then(|(_, rest)| rest.split('/').next()?.trim().parse().ok())).collect()
}

/// The page with a script of ours at its end and its folder as its base, so that what it names by a relative path is what it would have had.
fn with_script(html: &str, base: &str, script: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = html.to_string();
    // (one with a base of its own keeps it)
    if !lower.contains("<base") {
        let tag = format!("<base href=\"{base}\">");
        let at = lower.find("<head").and_then(|head| lower[head..].find('>').map(|end| head + end + 1)).or_else(|| lower.find("<html").and_then(|html| lower[html..].find('>').map(|end| html + end + 1))).unwrap_or(0);
        out.insert_str(at, &tag);
    }
    match out.to_ascii_lowercase().rfind("</body>") {
        Some(at) => out.insert_str(at, script),
        None => out.push_str(script),
    }
    out
}

/// The page with the steps at its end. `key`: the name under which the page keeps, in the tab's storage, which part of the steps is next.
fn with_steps(html: &str, base: &str, steps: &str, key: &str) -> String {
    // (what the steps say cannot end the script they are in)
    let (parts, _) = step_parts(steps);
    let parts: String = parts.iter().map(|part| format!("    async () => {{\n{}\n    }},\n", part.replace("</script", "<\\/script").replace("</SCRIPT", "<\\/SCRIPT"))).collect();
    with_script(html, base, &format!("{STEPS_PRELUDE}  const key = '{key}';\n  const parts = [\n{parts}{STEPS_END}"))
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
/// Tries of the browser for the page, and again for each picture: a safety net now. The browser ended with 0xc0000005 in about 6 runs of 10 here, whatever the page, however long it was left
/// alone, and whether the run before it ended well or not (the cause is in the flags above and in `sandbox::run_browser`), and in none of the 90 runs of a page, of a page with steps and of
/// pictures after those were set right (2 of 60 picture runs hung, with no end and no output, and a hung run is not tried again). Ten tries cost nothing when the first ends well,
/// and 15 s at the most when it is crashes that they go on, with nothing between them, as another build of Edge or another machine may still bring; a run that hangs is not tried again.
const TRIES: usize = 10;
/// No new try after this long in all: runs that crash late, or hang, would otherwise go on for minutes.
const MOST_TRYING: Duration = Duration::from_secs(45);
/// What a normal run of the browser takes, with some to spare for a slow start: a run took 2 to 3 s here (1.9 s for the page's DOM, 2.6 s for a picture), so this is about three times that.
/// The page's own wait is added to it (`run_limit`).
const RUN_BASE: Duration = Duration::from_secs(8);

/// How long one run of the browser may take, from the wait it was given: a run that has not ended by then has hung (the GPU process of the browser waits for a registration of its crash handler
/// in the container, and nothing ends it), and it is ended early, to be tried once more, and not left for the whole of [`LIMIT`]. Never more than that.
fn run_limit(wait: u64) -> Duration {
    (RUN_BASE + Duration::from_millis(wait)).min(LIMIT)
}

/// Whether a run that did not give what was wanted is tried again, when it hung: once. A hang is the browser's and not the page's when the page's other run went well (a picture) or when it
/// printed nothing at all (a page's own console would have been heard), and the browser that hangs once does not hang the next time, as far as 60 runs here say.
fn tries_again_after_a_hang(hangs: usize) -> bool {
    hangs <= 1
}

/// Whether a run was ended by the time it was given, and not by itself.
fn hung(done: &crate::sandbox::Output) -> bool {
    done.status.is_none() && done.ended == "timed out"
}
/// How the browser's runs went, for the test that measures it: the page's runs and those that did not end well, the pictures' and those that left none.
#[cfg(test)]
static BROWSER_RUNS: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
#[cfg(test)]
fn note_run(picture: bool, bad: bool) {
    BROWSER_RUNS[usize::from(picture) * 2].fetch_add(1, Ordering::Relaxed);
    BROWSER_RUNS[usize::from(picture) * 2 + 1].fetch_add(u64::from(bad), Ordering::Relaxed);
}
/// One run of the browser in a check, for the trajectory: what it was for (`page` or `picture`), how it ended (its exit code as the check says it, or `timed out`), and how long it took.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserRun {
    pub of: &'static str,
    pub end: String,
    pub ms: u64,
}

thread_local! {
    /// The runs of the check this thread is making or has made: the agent runs a tool on its own thread, and reads them when the call ends.
    static RUNS_OF_THIS_CHECK: RefCell<Vec<BrowserRun>> = const { RefCell::new(Vec::new()) };
}

/// Before a check: whatever an earlier one left is not this one's.
pub fn forget_runs() {
    RUNS_OF_THIS_CHECK.with(|runs| runs.borrow_mut().clear());
}

/// The runs of the check that ended on this thread, in order.
pub fn take_runs() -> Vec<BrowserRun> {
    RUNS_OF_THIS_CHECK.with(|runs| std::mem::take(&mut *runs.borrow_mut()))
}

fn note_browser_run(of: &'static str, end: String, started: std::time::Instant) {
    RUNS_OF_THIS_CHECK.with(|runs| runs.borrow_mut().push(BrowserRun { of, end, ms: started.elapsed().as_millis() as u64 }));
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

/// Whether the page itself wrote to the console, or threw: a line of the browser's log that is a console line, not a browser extension's and not one of our own scripts' (`DOGMIND-STEPS-`).
fn page_wrote_to_console(err: &str) -> bool {
    err.lines().any(|line| line.contains("INFO:CONSOLE") && !line.contains("chrome-extension") && !line.contains("DOGMIND-STEPS-"))
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

/// Elements that have no end tag.
const VOID: [&str; 14] = ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// The attributes of an opening tag, as the browser's own serializer writes them (or as a person did): name in lower case, and the value if it has one. `>` inside a quoted value does
/// not end the tag. Returns the attributes and where the tag ends in `dom` (just after its `>`).
fn tag_of(dom: &str, at: usize) -> (String, Vec<(String, Option<String>)>, usize) {
    let bytes = dom.as_bytes();
    let (mut i, mut attributes) = (at + 1, Vec::new());
    while i < bytes.len() && !(bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'>' | b'/')) {
        i += 1;
    }
    let name = dom[at + 1..i].to_ascii_lowercase();
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'>' {
            break;
        }
        let from = i;
        while i < bytes.len() && !(bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'=' | b'>' | b'/')) {
            i += 1;
        }
        let attribute = dom[from..i].to_ascii_lowercase();
        let mut value = None;
        let mut after = i;
        while after < bytes.len() && bytes[after].is_ascii_whitespace() {
            after += 1;
        }
        if after < bytes.len() && bytes[after] == b'=' {
            after += 1;
            while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                after += 1;
            }
            let begin = after;
            if after < bytes.len() && matches!(bytes[after], b'"' | b'\'') {
                let quote = bytes[after];
                after += 1;
                while after < bytes.len() && bytes[after] != quote {
                    after += 1;
                }
                value = Some(dom[begin + 1..after.min(bytes.len())].to_string());
                after = (after + 1).min(bytes.len());
            } else {
                while after < bytes.len() && !(bytes[after].is_ascii_whitespace() || bytes[after] == b'>') {
                    after += 1;
                }
                value = Some(dom[begin..after].to_string());
            }
            i = after;
        }
        if !attribute.is_empty() {
            attributes.push((attribute, value));
        }
    }
    (name, attributes, (i + 1).min(bytes.len()))
}

/// Whether an element is not seen as the page's DOM itself says: it has the `hidden` attribute, or `aria-hidden="true"`, or a `style` of `display:none` or `visibility:hidden`, or it is
/// a `dialog` that is not open, or what a browser does not show of a page whatever it says (a script, a style, a template, the head, what `noscript` holds when scripts run, and what an svg holds
/// that is not drawn: its title, its description and its definitions; its `text` is drawn, and is a chart's axis).
fn is_unseen(name: &str, attributes: &[(String, Option<String>)]) -> bool {
    static STYLE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let style = STYLE.get_or_init(|| Regex::new(r"(?i)(?:^|;)\s*(?:display\s*:\s*none|visibility\s*:\s*hidden)\b").expect("the pattern is valid"));
    let has = |wanted: &str| attributes.iter().any(|(attribute, _)| attribute == wanted);
    let value = |wanted: &str| attributes.iter().find(|(attribute, _)| attribute == wanted).and_then(|(_, value)| value.as_deref());
    matches!(name, "script" | "style" | "template" | "head" | "noscript" | "title" | "desc" | "defs" | "metadata")
        || has("hidden")
        || value("aria-hidden").is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
        || value("style").is_some_and(|text| style.is_match(text))
        || (name == "dialog" && !has("open"))
}

/// The words of a page's DOM that are not in an element it says is not seen (see `is_unseen`), in one line. The browser's own account of what it shows (`document.body.innerText`, which a
/// script of ours in the page says, in the copy of a page that has steps and in the copy that a plain page of HTML is opened in) is used when there is one. This is what is left where
/// there is none (a page a server gives, which is not copied; a page that is not HTML; a copy that could not be made or did not get to say it), and it gives away only what the DOM
/// says: an element that a style sheet hides by its class, a media query's rule for another width, and what a framework's `hidden` class does are read here as shown, and the report says so.
fn visible_text(dom: &str) -> String {
    // (the same bytes in lower case, to look for the end tag of what is not seen)
    let (bytes, lower, mut text, mut at) = (dom.as_bytes(), dom.to_ascii_lowercase(), String::new(), 0);
    while at < bytes.len() {
        let Some(open) = dom[at..].find('<').map(|found| at + found) else {
            text.push_str(&dom[at..]);
            break;
        };
        text.push_str(&dom[at..open]);
        text.push(' ');
        let rest = &dom[open..];
        if rest.starts_with("<!--") {
            at = rest.find("-->").map_or(bytes.len(), |end| open + end + 3);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            at = rest.find('>').map_or(bytes.len(), |end| open + end + 1);
        } else if rest.starts_with("</") {
            at = rest.find('>').map_or(bytes.len(), |end| open + end + 1);
        } else if rest.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic) {
            let (name, attributes, end) = tag_of(dom, open);
            at = end;
            if is_unseen(&name, &attributes) && !VOID.contains(&name.as_str()) && !rest[..end - open].ends_with("/>") {
                // (to its end tag, with those of the same name inside it counted: what the DOM holds is balanced, as the browser's own tree)
                let (mut depth, mut scan) = (1usize, end);
                let (open_tag, close_tag) = (format!("<{name}"), format!("</{name}"));
                while depth > 0 {
                    let Some(close) = lower[scan..].find(&close_tag).map(|found| scan + found) else {
                        scan = bytes.len();
                        break;
                    };
                    // (those of the same name that open before that end tag each need one of their own)
                    let mut from = scan;
                    while let Some(found) = lower[from..close].find(&open_tag) {
                        let after = from + found + open_tag.len();
                        if lower[after..].starts_with(|next: char| next.is_ascii_whitespace() || matches!(next, '>' | '/')) {
                            depth += 1;
                        }
                        from = after;
                    }
                    depth -= 1;
                    scan = dom[close..].find('>').map_or(bytes.len(), |found| close + found + 1);
                }
                at = scan;
            }
        } else {
            text.push('<');
            at = open + 1;
        }
    }
    let text = text.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">");
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MOST_TEXT).collect()
}

/// The words a person sees, from the steps' own run: the page lays its text out itself, and says it on a line of the console, percent-encoded so that nothing in it can break the
/// line. `None` where there is no such line (no steps, or they did not get to their end).
fn steps_text(err: &str) -> Option<String> {
    let line = err.lines().rev().find(|line| line.contains("DOGMIND-STEPS-TEXT:"))?;
    let encoded = line.split_once("DOGMIND-STEPS-TEXT:")?.1.split(['"', ' ']).next().unwrap_or("");
    let text = percent_decoded(encoded);
    Some(text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MOST_TEXT).collect())
}

/// `text` with its `%XX` bytes read as such, as a UTF-8 text (a `%` that is not followed by two hex digits stays as it is).
fn percent_decoded(text: &str) -> String {
    let (mut bytes, raw) = (Vec::new(), text.as_bytes());
    let mut i = 0;
    while i < raw.len() {
        let hex = if raw[i] == b'%' { text.get(i + 1..i + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok()) } else { None };
        match hex {
            Some(byte) => {
                bytes.push(byte);
                i += 3;
            }
            None => {
                bytes.push(raw[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where the steps took the page, when they did: the call they had made last (its number and what it was), if they had made one, and the address the page went to.
#[derive(Debug, PartialEq, Eq)]
struct Left {
    step: Option<(u64, String)>,
    to: String,
}

/// What the page said when the steps took it to another document, from the browser's log: a line `DOGMIND-STEPS-LEFT:` with a JSON text, percent-encoded.
fn steps_left(err: &str) -> Option<Left> {
    let line = err.lines().find(|line| line.contains("DOGMIND-STEPS-LEFT:"))?;
    let encoded = line.split_once("DOGMIND-STEPS-LEFT:")?.1.split(['"', ' ']).next().unwrap_or("");
    let said: Value = serde_json::from_str(&percent_decoded(encoded)).ok()?;
    let step = said.get("step").and_then(|step| Some((step.get("n")?.as_u64()?, step.get("call")?.as_str()?.to_string())));
    Some(Left { step, to: said.get("to")?.as_str()?.to_string() })
}

/// What is said of steps that took the page to another document. The address is told as the model would name it: a file of the workspace by its path, and the copy the steps ran in (a form
/// that submits to its own address comes back to it) as the page.
/// `base_url`: the workspace as the browser names it; `copy`: the file name of the copy; `page_name`: the page's name.
fn left_words(left: &Left, base_url: &str, copy: &str, page_name: &str) -> String {
    let (path, rest) = left.to.split_at(left.to.find(['?', '#']).unwrap_or(left.to.len()));
    let place = if path.ends_with(copy) { format!("{page_name}{rest}") } else { format!("{}{rest}", path.strip_prefix(base_url).map_or_else(|| path.to_string(), percent_decoded)) };
    let at = match &left.step {
        Some((n, call)) => format!("at step {n} (`{call}`, the last call they had made)"),
        None => "before any call of theirs was made (a `location` they set)".to_string(),
    };
    format!("STEPS LEFT THE PAGE: they went to {place} {at}. The steps after it did not run, as that page has none of them: what it shows, and its pictures, are that page. Giving a larger wait_ms changes nothing.\n")
}

/// The title and the words a person would read, from the page as its scripts left it.
fn shown(dom: &str) -> (String, String) {
    let title = Regex::new(r"(?is)<title[^>]*>(.*?)</title\s*>").expect("the pattern is valid").captures(dom).map_or(String::new(), |found| found[1].trim().to_string());
    (title, visible_text(dom))
}

/// How a run of the browser ended, from what it gives: its exit code (none when it was stopped), the dump of the page and the browser's own log lines.
#[derive(Debug, PartialEq, Eq)]
enum EndOfRun {
    /// Ended well, with the page.
    Read,
    /// Ended well, and had nothing to read: it could not load the page. The browser's own reason, when its log has one (`net::ERR_CONNECTION_REFUSED`).
    NotLoaded(Option<String>),
    /// Ended with an exception code or another that is not zero: it crashed.
    Crashed,
    /// Stopped, by the time limit or by the person.
    Stopped,
}

/// The browser's own reason for a page it could not load, as its log says it: a net error, or a file error.
fn load_failure(log: &str) -> Option<String> {
    let found = Regex::new(r"(?:net::)?ERR_[A-Z0-9_]{4,}").expect("the pattern is valid").find(log)?;
    Some(found.as_str().to_string())
}

fn end_of_run(status: Option<i32>, dump: &str, log: &str) -> EndOfRun {
    match status {
        Some(0) if !dump.trim().is_empty() => EndOfRun::Read,
        Some(0) => EndOfRun::NotLoaded(load_failure(log)),
        Some(_) => EndOfRun::Crashed,
        None => EndOfRun::Stopped,
    }
}

/// What is said when the browser ended well and the page did not load. A `url` that the sandbox refuses is one case (a server outside it, or a port it may not reach), and a path that is not
/// a page the browser can open is another; neither is the browser crashing, and the model was sent off to look for that.
fn not_loaded_words(reason: Option<&str>) -> String {
    let why = reason.map_or(String::new(), |reason| format!(" ({reason})"));
    format!("THE PAGE COULD NOT BE LOADED: the browser ran and ended normally, and had nothing to read{why}. For a `url`, the server has to be inside what the sandbox may reach: one that a command of this session started is, and another program's, on this machine or another, is not. Running this check again changes nothing until the address or the page is changed, and do not install anything to check it another way: say in your answer that the page was not checked.\n")
}

/// `needs`: what a file page was read to need; a page a local server gives has none of that known.
/// What reading a run adds to what the browser printed.
#[derive(Default)]
struct Reading {
    /// Said in place of the browser's crash when it ended well and could not load the page.
    not_loaded: Option<String>,
    /// Said in place of the browser's crash when it was the steps that did not end (they began, and the browser was stopped with them still going).
    steps_lost: Option<String>,
    /// What heads the console, if not `Console`: the console is that of the run that reads the page, at 1280x800, whatever views were asked for. (The words it shows are read there too.)
    console_head: Option<String>,
    /// The words the page said it shows, at the end of the steps or of its time, where they ended; none where the browser was not asked to say them.
    seen: Option<String>,
}

/// The console's heading when none of the views asked for is the desktop's: the first block is still the console of a run at 1280x800, and under a plain "Console" a model that asked
/// only for a phone's width read it as the phone's.
fn console_head(views: &[View]) -> Option<String> {
    (!views.iter().any(|view| view.label == "desktop")).then(|| "Console (as read at 1280x800)".to_string())
}

fn report(page_name: &str, browser_name: &str, finished: bool, needs: Option<&Needs>, console: &[String], dom: &str, reading: &Reading) -> String {
    let served = needs.is_none();
    let unknown = Needs::default();
    let needs = needs.unwrap_or(&unknown);
    let mut out = if served { format!("{page_name} in headless {browser_name}, with no network but this machine's own servers.\n") } else { format!("{page_name} in headless {browser_name}, with no network.\n") };
    if let (false, Some(lost)) = (finished, &reading.steps_lost) {
        out.push_str(lost);
    } else if let (false, Some(words)) = (finished, &reading.not_loaded) {
        out.push_str(words);
    } else if !finished {
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
    let head = reading.console_head.as_deref().unwrap_or("Console");
    if console.is_empty() {
        out.push_str(&if needs.remote.is_empty() && finished { format!("{head}: nothing, so no errors.\n") } else { format!("{head}: nothing, which says little with that not loaded.\n") });
    } else {
        out.push_str(&format!("{head}:\n"));
        for line in console.iter().take(MOST_LINES) {
            out.push_str(&format!("- {line}\n"));
        }
        if console.len() > MOST_LINES {
            out.push_str(&format!("- and {} more\n", console.len() - MOST_LINES));
        }
    }
    if finished {
        let (title, from_dom) = shown(dom);
        let text = reading.seen.clone().unwrap_or_else(|| from_dom);
        // (where the words were read, when that is not what a person looking at the page would have: the window that was not asked for, and the DOM, which no style sheet is applied to)
        let mut notes = Vec::new();
        if reading.console_head.is_some() {
            notes.push("read at 1280x800");
        }
        if reading.seen.is_none() {
            notes.push("from the page's DOM, with no style sheet applied: what a class or a media query hides is counted as shown");
        }
        let label = if notes.is_empty() { "Shows".to_string() } else { format!("Shows ({})", notes.join("; ")) };
        out.push_str(&format!("{label}: title \"{title}\"; text: {}\n", if text.is_empty() { "none" } else { &text }));
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
    let (parts, more_parts) = steps.map_or((Vec::new(), 0), step_parts);
    // (each load after the first costs time of its own: a page's own, as a plain page is given)
    let wait = arguments.get("wait_ms").and_then(Value::as_u64).unwrap_or(if steps.is_some() { 2 * WAIT_MS } else { WAIT_MS }).min(MOST_WAIT_MS) + parts.len().saturating_sub(1) as u64 * WAIT_MS;
    let cwd = page.as_deref().and_then(Path::parent).unwrap_or(workspace).to_path_buf();
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = crate::sandbox::temp_folder();
    if let Some(temp) = &temp {
        forget_old_pictures(temp);
        forget_old_leftovers(temp, LEFTOVER_KEPT);
    }
    // (what this check makes in that folder goes when it ends, whichever way)
    let mut made = Removing(Vec::new());
    // With steps, what is opened is a copy of the page with them in it, in the sandbox's own folder, that reads the page's own files from where they are. A plain page of HTML is opened in a
    // copy too, with a script that says the words the browser shows of it (the DOM a dump gives does not know a rule of a style sheet); a page that cannot be copied is opened as it is.
    let (mut copy_name, mut opened) = (None, page.clone());
    if let (Some(page), Some(steps)) = (&page, steps) {
        let temp = temp.as_ref().ok_or("the sandbox's folder could not be found, so the steps could not be run")?;
        let html = std::fs::read_to_string(page).map_err(|error| format!("{name}: {error}"))?;
        let base = file_url(page.parent().unwrap_or(workspace)) + "/";
        let run = next_run();
        let file_name = steps_page_of(&run);
        let copy = temp.join(&file_name);
        made.0.push(copy.clone());
        std::fs::write(&copy, with_steps(&html, &base, steps, &format!("dogmind-steps-{run}"))).map_err(|error| format!("{}: {error}", copy.display()))?;
        (copy_name, opened) = (Some(file_name), Some(copy));
    } else if let (Some(page), Some(temp), None) = (&page, &temp, steps) {
        let html_page = page.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm"));
        if let (true, Ok(html)) = (html_page, std::fs::read_to_string(page)) {
            let base = file_url(page.parent().unwrap_or(workspace)) + "/";
            let file_name = steps_page_of(&next_run());
            let copy = temp.join(&file_name);
            made.0.push(copy.clone());
            if std::fs::write(&copy, with_script(&html, &base, &reading_script(wait))).is_ok() {
                (copy_name, opened) = (Some(file_name), Some(copy));
            }
        }
    }
    // What the browser is given, on its command line and in the pictures' frame (the pictures of a plain page are of the page itself, not of its copy).
    let (address, src) = match &opened {
        Some(file) => (plain(file), file_url(if steps.is_some() { file } else { page.as_deref().unwrap_or(file) })),
        None => (name.clone(), name.clone()),
    };
    // (the script that reads a plain page's words waits for the page's time to be up, so the browser is given a little more of it than that)
    let run_wait = if steps.is_none() && copy_name.is_some() { wait + READ_MARGIN_MS } else { wait };
    let browser_flags = flags();
    // (the browser sometimes crashes in the sandbox, whatever the page, so one crash says nothing of the page: it is tried again)
    let (mut dom, mut err, mut finished, mut how) = (String::new(), String::new(), false, String::new());
    let mut ended_as = EndOfRun::Stopped;
    let mut hangs = 0;
    // (a page with steps may take as long as its steps do; one without has a limit of its own, and a run that hangs is ended early)
    let limit = if steps.is_some() { LIMIT } else { run_limit(run_wait) };
    let (trying, mut profiles) = (std::time::Instant::now(), Profiles::new(&cwd, workspace));
    for _ in 0..TRIES {
        let started = std::time::Instant::now();
        let profile = profiles.next();
        let line = page_line(&program, &browser_flags, run_wait, &profile, &address);
        let done = crate::sandbox::run_browser(&line, &cwd, workspace, limit, cancel)?;
        (dom, err) = (crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr));
        #[cfg(debug_assertions)]
        if std::env::var_os("DOGMIND_PAGE_DEBUG").is_some() {
            eprintln!("--- browser, {:?} {:?}:\n{err}\n--- dom: {} bytes", done.status, done.ended, dom.len());
        }
        ended_as = end_of_run(done.status, &dom, &err);
        finished = ended_as == EndOfRun::Read;
        how = done.status.map_or(done.ended.to_string(), |code| format!("exit code {:#x}", code as u32));
        note_browser_run("page", how.clone(), started);
        #[cfg(test)]
        note_run(false, !finished);
        // (a run that hung and printed nothing, not so much as a console line of the page's, is the browser's: it is tried once more)
        let silent_hang = hung(&done) && !page_wrote_to_console(&err);
        hangs += usize::from(silent_hang);
        let again = silent_hang && tries_again_after_a_hang(hangs);
        if finished || cancel.load(Ordering::Relaxed) || trying.elapsed() > MOST_TRYING || (!again && (started.elapsed() > LIMIT / 2 || page_wrote_to_console(&err))) {
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
    let (mut steps_said, mut steps_lost) = (String::new(), None);
    if let (Some(page), Some(copy)) = (&page, &copy_name) {
        // (the copy's lines are the page's own; and whether the steps got to their end is said apart)
        let file_name = page.file_name().map_or(String::new(), |file| file.to_string_lossy().into_owned());
        let done = console.iter().any(|line| line.ends_with("DOGMIND-STEPS-DONE"));
        let failed = console.iter().find_map(|line| line.split_once("DOGMIND-STEPS-FAILED: ").map(|(_, why)| why.to_string()));
        let began = parts_begun(&err);
        let left = if steps.is_some() { steps_left(&err) } else { None };
        // (a part that began more than once is the page loading itself again, and with it the steps, from where it was: not the browser's doing, and not a crash)
        let again = began.iter().find(|part| began.iter().filter(|other| other == part).count() > 1).copied();
        console.retain(|line| !line.contains("DOGMIND-STEPS-"));
        console.iter_mut().for_each(|line| *line = line.replace(copy.as_str(), &file_name));
        if steps.is_some() {
            let in_parts = if parts.len() > 1 { format!(" (in {} parts, one after each reload they make)", parts.len()) } else { String::new() };
            steps_said = match (done, failed, again) {
                // (the steps that went on in the page they left, until it was gone, may have failed on what the new page does not have, or have ended: the page they left is what happened)
                _ if left.is_some() => left.as_ref().map_or(String::new(), |left| left_words(left, &base_url, copy, &name)),
                (_, Some(why), _) => format!("STEPS FAILED: {why}. What it shows, and its pictures, are the page as the steps left it.\n"),
                (_, None, Some(part)) => format!("STEPS DID NOT END: part {part} of them began {} times, as the page was loaded again each time: the page reloads itself again and again, and the steps begin again with it. They cannot be run on a page that does that: look at it without steps, or take its own reload out for the check.\n", began.iter().filter(|other| **other == part).count()),
                (true, None, None) => format!("Steps: all ran{in_parts}. What it shows, and its pictures, are the page after them.\n"),
                (false, None, None) => format!("Steps: they had not ended when the page was read, after {wait} ms of its time: give a larger wait_ms.\n"),
            };
            let more_note = if more_parts > 0 { format!("The steps reload the page {more_parts} more times than this runs them for: the parts after the {MOST_PARTS}th did not run.\n") } else { String::new() };
            steps_said.push_str(&more_note);
            // (stopped with them going on: that says what the steps and the page did, and nothing of a crash)
            if !finished && (again.is_some() || how == "timed out") {
                if again.is_none() {
                    steps_said = more_note;
                }
                steps_lost = Some(match (again, began.is_empty()) {
                    (Some(_), _) => "NOT CHECKED: the steps began again and again until the browser was stopped, which is not the browser crashing: see the steps below. Running this check again changes nothing until that is changed.\n".to_string(),
                    (None, true) => format!("NOT CHECKED: the browser was stopped after {} s and the steps had not begun: the page had not finished loading, with its own scripts and anything it waits for (or it loads itself again and again). This is not the browser crashing.\n", LIMIT.as_secs()),
                    (None, false) => format!("NOT CHECKED: the browser was stopped after {} s with the steps still going: the page or the steps are slow or do not end (a wait, a loop, a load that waits for the network). This is not the browser crashing. Shorter steps, or a page that settles, will be read.\n", LIMIT.as_secs()),
                });
            }
        }
    }
    // (a run that ended well with nothing to read is a page that did not load, and not a crash: unless the steps say what the page did)
    let not_loaded = match &ended_as {
        EndOfRun::NotLoaded(reason) if steps_lost.is_none() => Some(not_loaded_words(reason.as_deref())),
        _ => None,
    };
    // (the words that the page the steps left said at their end are not of the page that is shown)
    let seen = copy_name.as_ref().filter(|_| steps.is_none() || steps_left(&err).is_none()).and_then(|_| steps_text(&err));
    let reading = Reading { not_loaded, steps_lost, console_head: console_head(&views), seen };
    let said = format!("{}{steps_said}", report(&name, browser_name, finished, needs.as_ref(), &console, &dom, &reading));
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
        let (mut taken, mut said_there, mut picture_hangs) = (false, Vec::new(), 0);
        let (trying, mut profiles) = (std::time::Instant::now(), Profiles::new(&cwd, workspace));
        for _ in 0..TRIES {
            if cancel.load(Ordering::Relaxed) || trying.elapsed() > MOST_TRYING {
                break;
            }
            let _ = std::fs::remove_file(&shot);
            let started = std::time::Instant::now();
            let profile = profiles.next();
            let line = shot_line(&program, &browser_flags, (window_width, window_height), wait, &profile, &shot, &plain(&frame));
            let done = crate::sandbox::run_browser(&line, &cwd, workspace, run_limit(wait), cancel);
            note_browser_run("picture", done.as_ref().map_or_else(|_| "not started".to_string(), |done| done.status.map_or(done.ended.to_string(), |code| format!("exit code {:#x}", code as u32))), started);
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
                    said_there = done.map(|done| page_console(&crate::sandbox::decode(&done.stderr), &base_url, page.as_deref().zip(copy_name.as_deref()))).unwrap_or_default();
                    break;
                }
            }
            // (a sandbox that could not run it will not do better the next time; a run that hung is the browser's, as the page's own run went well, and is tried once more)
            let hung_now = done.as_ref().is_ok_and(hung);
            picture_hangs += usize::from(hung_now);
            if done.is_err() || (hung_now && !tries_again_after_a_hang(picture_hangs)) || (!hung_now && started.elapsed() > LIMIT / 2) {
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
    fn the_runs_of_a_check_are_told_in_order_once_and_not_to_the_next_check() {
        forget_runs();
        let began = std::time::Instant::now();
        note_browser_run("page", "exit code 0xc0000005".to_string(), began);
        note_browser_run("page", "exit code 0x0".to_string(), began);
        note_browser_run("picture", "timed out".to_string(), began);
        let runs = take_runs();
        assert_eq!(runs.iter().map(|run| (run.of, run.end.as_str())).collect::<Vec<_>>(), [("page", "exit code 0xc0000005"), ("page", "exit code 0x0"), ("picture", "timed out")]);
        assert!(take_runs().is_empty(), "told once");
        note_browser_run("page", "exit code 0x0".to_string(), began);
        forget_runs();
        assert!(take_runs().is_empty(), "what an earlier check left is not the next one's");
        // (and what another thread runs is that thread's own)
        note_browser_run("page", "exit code 0x0".to_string(), began);
        assert!(std::thread::spawn(take_runs).join().unwrap().is_empty());
        forget_runs();
    }

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
        let said = report("index.html", "Edge", true, Some(&needs), &[], "<html><body>hi</body></html>", &Reading::default());
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
    fn what_a_page_shows_leaves_out_what_its_own_dom_says_is_not_seen() {
        let dom = r#"<html><head><title>T</title></head><body>
            <p id="empty" class="empty" hidden>EMPTY-MESSAGE</p>
            <p style="color: red; display:none">NONE-A</p>
            <p style='DISPLAY : NONE'>NONE-B</p>
            <div style="visibility:hidden">INVISIBLE <span>INNER</span></div>
            <div aria-hidden="true">ARIA</div>
            <div aria-hidden="false">ARIA-FALSE-SEEN</div>
            <div class="hidden" title="a > b" data-x='hidden'>CLASS-NAME-ONLY-SEEN</div>
            <section hidden><section>NESTED</section><div>MORE-NESTED</div></section>after-the-nest
            <template><b>TEMPLATE</b></template><noscript>NOSCRIPT</noscript><script>let a = "<p>SCRIPT</p>"</script><style>p{}</style>
            <dialog>CLOSED-DIALOG</dialog><dialog open>OPEN-DIALOG</dialog>
            <img src="a.png" hidden><input type="hidden" value="v"><br hidden>
            <!-- COMMENT <p hidden> -->
            <ul><li>milk</li><li style="display:none">gone</li><li>eggs &amp; bread</li></ul>
            <svg><title>SVG-TITLE</title><desc>SVG-DESC</desc><defs><text>SVG-DEF</text></defs><text>SVG-AXIS</text><text>SVG-AXIS-2</text></svg>
            <button disabled>Add</button> 1 &lt; 2
        </body></html>"#;
        let (title, text) = shown(dom);
        assert_eq!(title, "T");
        for gone in ["EMPTY-MESSAGE", "NONE-A", "NONE-B", "INVISIBLE", "INNER", "ARIA ", "NESTED", "MORE-NESTED", "TEMPLATE", "NOSCRIPT", "SCRIPT", "CLOSED-DIALOG", "COMMENT", "gone", "SVG-TITLE", "SVG-DESC", "SVG-DEF"] {
            assert!(!text.contains(gone), "{gone} is not seen: {text}");
        }
        assert_eq!(text, "ARIA-FALSE-SEEN CLASS-NAME-ONLY-SEEN after-the-nest OPEN-DIALOG milk eggs & bread SVG-AXIS SVG-AXIS-2 Add 1 < 2", "{text}");
    }

    /// The report says where the words were read when that is not what a person looking at the page would have: at the desktop's size when it was not asked for, and from the DOM.
    /// The steps that take the page to another document are told from the line the page said before it went.
    #[test]
    fn steps_that_left_the_page_are_told_with_the_call_and_where_they_went() {
        let said = |value: &str| format!("[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-LEFT:{}\", source: file:///x (5)\n", value.replace('%', "%25").replace('"', "%22").replace('{', "%7B").replace('}', "%7D").replace(' ', "%20"));
        let err = said(r##"{"step":{"n":2,"call":"press(\"#q\", \"Enter\")"},"to":"file:///C:/work/results.html?q=milk%20tea"}"##);
        let left = steps_left(&err).expect("a line");
        assert_eq!(left, Left { step: Some((2, "press(\"#q\", \"Enter\")".to_string())), to: "file:///C:/work/results.html?q=milk%20tea".to_string() });
        let words = left_words(&left, "file:///C:/work/", "dogmind-steps-7.html", "index.html");
        assert!(words.starts_with("STEPS LEFT THE PAGE: they went to results.html?q=milk%20tea at step 2 (`press(\"#q\", \"Enter\")`"), "{words}");
        assert!(words.contains("did not run") && words.contains("that page") && words.contains("larger wait_ms changes nothing"), "{words}");
        // a call before none: a location they set; a form that comes back to its own copy is the page
        let bare = steps_left(&said(r##"{"step":null,"to":"file:///C:/Users/x/dogmind-steps-7.html#top"}"##)).expect("a line");
        let words = left_words(&bare, "file:///C:/work/", "dogmind-steps-7.html", "index.html");
        assert!(words.contains("went to index.html#top before any call of theirs was made"), "{words}");
        // an address outside the workspace is told whole; a line that is not that is none
        let outside = steps_left(&said(r##"{"step":{"n":1,"call":"click(\"#a\")"},"to":"file:///C:/other/b.html"}"##)).unwrap();
        assert!(left_words(&outside, "file:///C:/work/", "c.html", "index.html").contains("went to file:///C:/other/b.html at step 1"));
        assert_eq!(steps_left("nothing of that here"), None);
        assert_eq!(steps_left("INFO:CONSOLE \"DOGMIND-STEPS-LEFT:not%20json\", source: x (1)"), None);
        assert_eq!(percent_decoded("a%20b%E3%81%82%zz%4"), "a bあ%zz%4");
    }

    #[test]
    fn the_steps_note_their_calls_and_say_when_they_leave_the_page() {
        let page = with_steps("<body></body>", "file:///C:/w/", "await press('#q', 'Enter')", "k");
        for wanted in ["note('press', first, second)", "note('click', selector)", "navigation.addEventListener('navigate'", "event.navigationType === 'reload'", "DOGMIND-STEPS-LEFT:", "underway = false;"] {
            assert!(page.contains(wanted), "{wanted}: {page}");
        }
        // they say it only while a part is going, and the reload that cuts them is not leaving
        assert!(page.find("let underway = true").unwrap() < page.find("await parts[index]()").unwrap() && page.find("await parts[index]()").unwrap() < page.find("underway = false;\n    if (index + 1").unwrap(), "{page}");
    }

    #[test]
    fn the_words_a_page_shows_say_where_they_were_read() {
        let needs = Needs::default();
        let dom = "<html><head><title>T</title></head><body><p>from the dom</p><p class=\"hidden\">CLASS</p></body></html>";
        let says = |reading: &Reading| report("index.html", "Edge", true, Some(&needs), &[], dom, reading).lines().find(|line| line.starts_with("Shows")).unwrap().to_string();
        assert_eq!(says(&Reading { seen: Some("from the browser".to_string()), ..Reading::default() }), "Shows: title \"T\"; text: from the browser", "the browser's own account needs no note");
        let from_dom = says(&Reading::default());
        assert!(from_dom.starts_with("Shows (from the page's DOM, with no style sheet applied") && from_dom.ends_with("text: from the dom CLASS"), "{from_dom}");
        let phone = says(&Reading { console_head: Some("Console (as read at 1280x800)".to_string()), seen: Some("x".to_string()), ..Reading::default() });
        assert_eq!(phone, "Shows (read at 1280x800): title \"T\"; text: x");
        let both = says(&Reading { console_head: Some("Console (as read at 1280x800)".to_string()), ..Reading::default() });
        assert!(both.starts_with("Shows (read at 1280x800; from the page's DOM"), "{both}");
    }

    #[test]
    fn a_plain_page_is_given_a_script_that_says_its_words_and_nothing_else() {
        let page = with_script("<!DOCTYPE html><html><head><title>t</title></head><body><p>x</p></body></html>", "file:///C:/w/", &reading_script(1500));
        assert!(page.contains("<head><base href=\"file:///C:/w/\"><title>"), "{page}");
        let (script_at, body_end) = (page.find("setTimeout(done, 1500)").unwrap(), page.rfind("</body>").unwrap());
        assert!(page.find("<p>x</p>").unwrap() < script_at && script_at < body_end, "{page}");
        assert!(page.contains("DOGMIND-STEPS-TEXT:") && page.contains("document.body.innerText") && page.matches("<script>").count() == 1, "{page}");
        // the steps' page is made of the same
        assert!(with_steps("<body></body>", "file:///C:/w/", "await click('#a')", "k").contains("<base href=\"file:///C:/w/\">"));
    }

    #[test]
    fn a_line_of_the_page_is_told_from_one_of_ours_and_from_a_browsers_extension() {
        let ours = "[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-TEXT:abc\", source: file:///x (5)\n";
        let theirs = "[1:2:3/4:INFO:CONSOLE:5] \"hello\", source: file:///x (5)\n";
        assert!(!page_wrote_to_console(ours) && !page_wrote_to_console("nothing at all") && page_wrote_to_console(&format!("{ours}{theirs}")));
        assert!(!page_wrote_to_console("[1:2:3/4:INFO:CONSOLE:5] \"x\", source: chrome-extension://abc/x.js (5)\n"));
    }

    #[test]
    fn the_words_the_page_said_it_shows_are_read_from_its_console_line() {
        let said = "[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-TEXT:%E7%89%9B%E4%B9%B3%E3%82%92%E8%B2%B7%E3%81%86%20%E5%89%8A%E9%99%A4%0A2%20items%2C%20100%25\", source: file:///x (5)\n";
        assert_eq!(steps_text(said).as_deref(), Some("牛乳を買う 削除 2 items, 100%"));
        assert_eq!(steps_text("[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-TEXT:\", source: file:///x (5)\n").as_deref(), Some(""));
        assert_eq!(steps_text("nothing of the steps here"), None);
        // the last one, when the page said it more than once; and what is cut is cut by characters
        let twice = format!("\"DOGMIND-STEPS-TEXT:first\", source: x (1)\n\"DOGMIND-STEPS-TEXT:{}\", source: x (1)\n", "%E3%81%82".repeat(MOST_TEXT + 50));
        assert_eq!(steps_text(&twice).unwrap().chars().count(), MOST_TEXT);
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

    /// A run has a limit of its own from the wait it was given, a hang is tried once more and no more, and only a run that was ended by its time is a hang.
    #[test]
    fn a_run_that_hangs_is_ended_early_and_tried_once_more() {
        assert_eq!(run_limit(WAIT_MS), Duration::from_millis(9500), "about 10 s at the usual wait: three or four times a run");
        assert!(run_limit(0) < run_limit(WAIT_MS) && run_limit(WAIT_MS) < run_limit(5_000), "it follows the wait");
        assert_eq!(run_limit(MOST_WAIT_MS), Duration::from_secs(18), "at the longest wait a page may ask for");
        assert_eq!(run_limit(60_000), LIMIT, "and never more than the longest a run may take");
        assert!(tries_again_after_a_hang(1), "the first hang is tried again");
        assert!(!tries_again_after_a_hang(2), "and the second is not: the page's own bound is MOST_TRYING, and a browser that hangs twice will not do better");
        let ended = |status: Option<i32>, ended: &'static str| crate::sandbox::Output { status, ended, ..crate::sandbox::Output::default() };
        assert!(hung(&ended(None, "timed out")));
        assert!(!hung(&ended(None, "cancelled")), "a Stop of the person's is not a hang");
        assert!(!hung(&ended(Some(0), "")) && !hung(&ended(Some(0xC000_0005u32 as i32), "")));
    }

    /// What the run gives tells the cases apart: ended well with the page, ended well with nothing (the page did not load), crashed, stopped.
    #[test]
    fn a_page_that_did_not_load_is_told_from_a_browser_that_crashed_or_was_stopped() {
        assert_eq!(end_of_run(Some(0), "<html>x</html>", ""), EndOfRun::Read);
        let refused = "[1105/133001.123:ERROR:chrome\\browser\\net.cc(1)] Failed to load http://127.0.0.1:9/: net::ERR_CONNECTION_REFUSED\n";
        assert_eq!(end_of_run(Some(0), "  \n", refused), EndOfRun::NotLoaded(Some("net::ERR_CONNECTION_REFUSED".to_string())));
        assert_eq!(end_of_run(Some(0), "", "nothing it says of why"), EndOfRun::NotLoaded(None));
        assert_eq!(end_of_run(Some(0), "", "ERR_FILE_NOT_FOUND"), EndOfRun::NotLoaded(Some("ERR_FILE_NOT_FOUND".to_string())));
        assert_eq!(end_of_run(Some(0xC000_0005u32 as i32), "", refused), EndOfRun::Crashed, "an exception code, whatever its log says");
        assert_eq!(end_of_run(Some(1), "", ""), EndOfRun::Crashed);
        assert_eq!(end_of_run(None, "", refused), EndOfRun::Stopped, "stopped by the time limit or the person");
        // the words: the reason when the log has one, and not the crash's
        let said = report("http://127.0.0.1:9/", "Edge", false, None, &[], "", &Reading { not_loaded: Some(not_loaded_words(Some("net::ERR_CONNECTION_REFUSED"))), ..Reading::default() });
        assert!(said.contains("COULD NOT BE LOADED") && said.contains("net::ERR_CONNECTION_REFUSED") && said.contains("inside what the sandbox may reach"), "{said}");
        assert!(!said.contains("crashed") && !said.contains("NOT CHECKED"), "{said}");
        // a crash and a stop keep their words
        let crash = report("p", "Edge", false, None, &[], "", &Reading::default());
        assert!(crash.contains("NOT CHECKED: the browser crashed or was stopped"), "{crash}");
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

    /// With the real browser, in the real sandbox: what a page hides is not in what it shows, with steps and without them. (A style sheet that hides by class is known to the browser, which says
    /// the words in both; the DOM alone does not know it.)
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn what_a_page_hides_is_not_in_the_words_it_shows() {
        let (dir, _) = page(&[(
            "index.html",
            "<!DOCTYPE html><title>todo</title><style>.gone{display:none}</style><ul id=l><li>milk</li></ul><p id=empty hidden>HIDDEN-ATTRIBUTE</p><p style=\"display:none\">HIDDEN-STYLE</p><p class=gone>HIDDEN-BY-CLASS</p><p>visible words</p>",
        )]);
        let without = check_page(&serde_json::json!({"path": "index.html", "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{without}");
        let shows = |said: &str| said.lines().find(|line| line.starts_with("Shows")).map(str::to_string).unwrap_or_default();
        assert!(shows(&without).contains("milk visible words"), "{without}");
        assert!(!shows(&without).contains("HIDDEN-ATTRIBUTE") && !shows(&without).contains("HIDDEN-STYLE") && !shows(&without).contains("HIDDEN-BY-CLASS"), "{without}");
        let with = check_page(&serde_json::json!({"path": "index.html", "steps": "await wait(10);", "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{with}");
        assert!(shows(&with).contains("milk visible words"), "{with}");
        assert!(!shows(&with).contains("HIDDEN-ATTRIBUTE") && !shows(&with).contains("HIDDEN-STYLE") && !shows(&with).contains("HIDDEN-BY-CLASS"), "{with}");
    }

    /// With the real browser, in the real sandbox: the words are the browser's own with the page's styles (a class that hides, a media query), a chart's axis in svg is among them and its title
    /// is not, and asked only for a phone's width they are said to be read at the desktop's.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn the_words_are_the_browsers_own_and_say_the_size_they_were_read_at() {
        let (dir, _) = page(&[(
            "index.html",
            "<!DOCTYPE html><title>r</title><style>.hidden{display:none}.wide{display:block}@media(max-width:600px){.wide{display:none}}</style><p class=hidden>TAILWIND-HIDDEN</p><p class=wide>WIDE-ONLY</p><p>always</p><svg width=100 height=40><title>CHART-TITLE</title><text x=0 y=20>AXIS-LABEL</text></svg>",
        )]);
        let shows = |said: &str| said.lines().find(|line| line.starts_with("Shows")).map(str::to_string).unwrap_or_default();
        let desktop = check_page(&serde_json::json!({"path": "index.html", "views": ["desktop"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{desktop}");
        assert!(shows(&desktop).starts_with("Shows: title \"r\"; text: "), "{desktop}");
        for wanted in ["WIDE-ONLY", "always", "AXIS-LABEL"] {
            assert!(shows(&desktop).contains(wanted), "{wanted}: {desktop}");
        }
        for gone in ["TAILWIND-HIDDEN", "CHART-TITLE"] {
            assert!(!shows(&desktop).contains(gone), "{gone}: {desktop}");
        }
        let phone = check_page(&serde_json::json!({"path": "index.html", "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{phone}");
        assert!(shows(&phone).starts_with("Shows (read at 1280x800): "), "{phone}");
        assert!(shows(&phone).contains("WIDE-ONLY"), "read at the desktop's width, where it is shown: {phone}");
    }

    /// With the real browser, in the real sandbox: steps that take the page to another document (a form submitted by Enter, a link followed, a location set) are told so, with the call and
    /// the address, and what is shown is the page they went to, whatever the later steps tried on the page they left.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn steps_that_leave_the_page_say_so_and_the_page_shown_is_the_one_they_went_to() {
        let (dir, _) = page(&[
            ("index.html", "<!DOCTYPE html><title>search</title><form action=\"results.html\" method=\"get\"><input id=q name=q><button>Go</button></form><a id=go href=\"other.html\">go on</a>"),
            ("results.html", "<!DOCTYPE html><title>results</title><p>RESULTS-PAGE-WORDS</p>"),
            ("other.html", "<!DOCTYPE html><title>other</title><p>OTHER-PAGE-WORDS</p>"),
        ]);
        let check = |steps: &str| {
            let said = check_page(&serde_json::json!({"path": "index.html", "steps": steps, "views": ["desktop"]}), dir.path(), &AtomicBool::new(false)).unwrap();
            println!("{said}");
            said
        };
        let form = check("await fill('#q', 'milk'); await press('#q', 'Enter'); await click('#result-in-the-old-page'); console.log('later');");
        assert!(form.contains("STEPS LEFT THE PAGE: they went to results.html?q=milk at step 2 (`press(\"#q\", \"Enter\")`"), "{form}");
        assert!(form.contains("RESULTS-PAGE-WORDS") && !form.contains("give a larger wait_ms") && !form.contains("STEPS FAILED") && !form.contains("Steps: all ran"), "{form}");
        let link = check("await click('#go');");
        assert!(link.contains("STEPS LEFT THE PAGE: they went to other.html at step 1 (`click(\"#go\")`"), "{link}");
        assert!(link.contains("OTHER-PAGE-WORDS"), "{link}");
        let set = check("location.href = 'other.html'; await wait(500);");
        assert!(set.contains("STEPS LEFT THE PAGE: they went to other.html before any call of theirs was made"), "{set}");
        // and steps that stay are as they were
        let stays = check("await fill('#q', 'milk'); await wait(10);");
        assert!(stays.contains("Steps: all ran") && !stays.contains("LEFT THE PAGE"), "{stays}");
    }

    /// With the real browser, in the real sandbox: asked only for a phone's width, the first console is labelled as the desktop's size, and the phone's own is under its name.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_console_read_at_the_desktops_size_says_so_when_only_a_phones_width_was_asked() {
        let (dir, _) = page(&[("index.html", "<!DOCTYPE html><title>w</title><script>console.log('innerW ' + innerWidth)</script>")]);
        let said = check_page(&serde_json::json!({"path": "index.html", "views": ["320x568"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        assert!(said.contains("Console (as read at 1280x800):\n- index.html:1: innerW 1") && !said.contains("\nConsole:\n"), "{said}");
        assert!(said.contains("Console at 320x568:\n  index.html:1: innerW 320"), "{said}");
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
        let page = with_steps("<!DOCTYPE html><html><head><title>t</title></head><body><p>x</p></body></html>", "file:///C:/w/", "await click('#a'); console.log('</script>')", "k");
        assert!(page.contains("<head><base href=\"file:///C:/w/\"><title>"), "{page}");
        let (steps_at, body_end) = (page.find("click('#a')").unwrap(), page.rfind("</body>").unwrap());
        assert!(page.find("<p>x</p>").unwrap() < steps_at && steps_at < body_end, "{page}");
        assert!(page.contains("console.log('<\\/script>')") && page.matches("</script>").count() == 1, "{page}");
        let bare = with_steps("<p>no head</p>", "file:///C:/w/", "1", "k");
        assert!(bare.starts_with("<base href=") && bare.ends_with("</script>"), "{bare}");
        let based = with_steps("<head><base href=\"x/\"></head>", "file:///C:/w/", "1", "k");
        assert_eq!(based.matches("<base").count(), 1, "a page's own base stays");
    }

    #[test]
    fn the_first_console_says_it_is_of_the_desktops_size_when_the_desktop_was_not_asked_for() {
        let heads = |views: Value| console_head(&views_of(&serde_json::json!({ "views": views })).unwrap());
        assert_eq!(heads(serde_json::json!(["320x568"])).as_deref(), Some("Console (as read at 1280x800)"));
        assert_eq!(heads(serde_json::json!(["mobile", "1024x768"])).as_deref(), Some("Console (as read at 1280x800)"));
        assert_eq!(heads(serde_json::json!(["mobile", "desktop"])), None);
        assert_eq!(heads(serde_json::json!(["desktop"])), None);
        assert_eq!(console_head(&views_of(&serde_json::json!({})).unwrap()), None, "no views: the desktop");
        let (needs, lines) = (Needs::default(), vec!["a.js:1: scrollW: 1248".to_string()]);
        let said = |reading: &Reading, console: &[String]| report("index.html", "Edge", true, Some(&needs), console, "<html><body>hi</body></html>", reading);
        let labelled = Reading { console_head: Some("Console (as read at 1280x800)".to_string()), ..Reading::default() };
        assert!(said(&Reading::default(), &lines).contains("Console:\n- a.js:1: scrollW: 1248\n"), "{}", said(&Reading::default(), &lines));
        assert!(said(&labelled, &lines).contains("Console (as read at 1280x800):\n- a.js:1: scrollW: 1248\n"), "{}", said(&labelled, &lines));
        assert!(said(&Reading::default(), &[]).contains("Console: nothing, so no errors.\n"));
        assert!(said(&labelled, &[]).contains("Console (as read at 1280x800): nothing, so no errors.\n"));
    }

    #[test]
    fn a_reload_the_steps_make_as_a_statement_divides_them_into_parts() {
        // what a real model wrote: the part before the reload runs in the first load, the part after it in the next
        let steps = "await fill('#input', 'x'); await click('#add-btn'); console.log('before reload:', count('li')); location.reload(); await wait(800); console.log('after reload:', count('li'))";
        let parts = split_steps(steps);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].ends_with("count('li')); ") && !parts[0].contains("reload();"), "{parts:?}");
        assert!(parts[1].starts_with(" await wait(800);") && parts[1].contains("after reload"), "{parts:?}");
        // each of the forms, with the `await` and the `;` as they are or are not
        for reload in ["location.reload()", "window.location.reload();", "await reload();", "await reload()", "await page.reload();", "page.reload()", "location.reload(true);", "  await  page.reload( )  ;"] {
            let parts = split_steps(&format!("await click('#a');\n{reload}\nawait click('#b');"));
            assert_eq!(parts.len(), 2, "{reload}: {parts:?}");
            assert!(parts[0].contains("#a") && !parts[0].contains("reload") && parts[1].contains("#b") && !parts[1].contains("reload"), "{reload}: {parts:?}");
        }
        // several, and a reload alone
        assert_eq!(split_steps("a(); location.reload(); b(); await reload(); c();").len(), 3);
        assert_eq!(split_steps("location.reload()").len(), 2);
        assert_eq!(split_steps("await click('#a')"), vec!["await click('#a')".to_string()]);
    }

    #[test]
    fn a_reload_in_a_string_a_comment_a_function_or_a_condition_is_not_a_cut() {
        for steps in [
            "console.log('location.reload()'); await click('#a');",
            "console.log(\"await reload();\"); await click('#a');",
            "console.log(`${1 + 1} location.reload() ${'}'} await reload();`); await click('#a');",
            "// location.reload();\nawait click('#a');",
            "/* location.reload();\n await reload(); */ await click('#a');",
            "await click('#a'); // then location.reload();",
            "const again = () => { location.reload(); }; await again();",
            "const again = async function () { await page.reload(); }; await again();",
            "setTimeout(() => location.reload(), 10); await wait(50);",
            "if (count('li') > 2) { location.reload(); }",
            "if (count('li') > 2) location.reload();",
            "const where = window.mylocation.reload(); await click('#a'); foo.location.reload();",
            "await click('#a'); const x = ['a', 'b'].map((n) => { location.reload(); return n; });",
        ] {
            assert_eq!(split_steps(steps), vec![steps.to_string()], "{steps}");
        }
        // one that is a statement, with others around that are not
        let parts = split_steps("console.log('location.reload()'); const f = () => { location.reload(); };\nawait reload();\n// location.reload();\nawait click('#b');");
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("const f") && parts[1].contains("#b"), "{parts:?}");
    }

    #[test]
    fn the_parts_are_at_most_so_many_and_the_rest_is_counted() {
        let many = (0..8).map(|n| format!("log({n}); location.reload();")).collect::<String>() + " log(9);";
        assert_eq!(split_steps(&many).len(), 9);
        let (parts, more) = step_parts(&many);
        assert_eq!((parts.len(), more), (MOST_PARTS, 9 - MOST_PARTS));
        assert_eq!(step_parts("log(1)"), (vec!["log(1)".to_string()], 0));
        // the page carries those parts, in order, each as a function of its own, with the name that keeps which is next
        let page = with_steps("<body></body>", "file:///C:/w/", "log(1); location.reload(); log(2);", "dogmind-steps-9-1");
        assert!(page.contains("const key = 'dogmind-steps-9-1';"), "{page}");
        let (first, second) = (page.find("log(1);").unwrap(), page.find(" log(2);").unwrap());
        assert!(first < second && page.matches("    async () => {\n").count() == 2, "{page}");
        assert!(page.contains("sessionStorage") && !page.contains("location.reload(); log(2)"), "the reload between the parts is the page's, not a line of the steps");
    }

    #[test]
    fn what_the_browser_said_of_the_parts_that_began_is_read_in_order() {
        let said = "[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-PART 1/3\", source: file:///x (5)\n[1:2:3/4:INFO:CONSOLE:5] \"fine\", source: a.js (1)\n[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-PART 2/3\", source: file:///x (5)\n[1:2:3/4:INFO:CONSOLE:5] \"DOGMIND-STEPS-PART 2/3\", source: file:///x (5)\n";
        assert_eq!(parts_begun(said), vec![1, 2, 2]);
        assert!(parts_begun("nothing of the steps here").is_empty());
    }

    #[test]
    fn what_models_write_of_playwright_is_mapped_onto_the_helpers_and_the_rest_says_what_there_is() {
        let page = with_steps("<body><p>x</p></body>", "file:///C:/w/", "await page.evaluate(() => 1)", "k");
        // each form, as a name the steps can use
        for form in [
            "const evaluate = async (fn, ...args)",
            "evaluate, locator,",
            "click: (selector) => click(selector)",
            "fill: (selector, value) => fill(selector, value)",
            "press: (selector, key) => press(selector, key)",
            "waitForTimeout: (ms) => wait(ms)",
            "locator = (selector) => ({ click: () => click(selector)",
            "$eval: (selector, fn, ...args) => evaluate(fn, find(selector), ...args)",
            "$$eval: (selector, fn, ...args) => evaluate(fn, [...document.querySelectorAll(selector)], ...args)",
            "textContent: async (selector) => find(selector).textContent",
            "isVisible: async (selector) => visible(selector)",
        ] {
            assert!(page.contains(form), "{form}");
        }
        // the page and the helpers are made before the steps, which can then use them, and the steps' own words are after
        assert!(page.find("const page = {").unwrap() < page.find("await page.evaluate(() => 1)").unwrap());
        // a name that is not here is said with the ones that are
        let hint = STEPS_END;
        for helper in ["click", "fill", "press", "check", "select", "wait", "text", "count", "visible", "evaluate", "waitForTimeout", "locator(sel)", "$eval", "$$eval"] {
            assert!(hint.contains(helper), "{helper}");
        }
        assert!(hint.contains("ReferenceError") && hint.contains("is not a function"));
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

    /// With the real browser, in the real sandbox: steps that reload the page are run in parts, one in each load. What the first part put in the page's storage is there for the
    /// second, and neither runs twice (they once ran from the top at every load, until the browser was stopped).
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn steps_that_reload_the_page_run_a_part_in_each_load() {
        let (dir, _) = page(&[(
            "index.html",
            "<!DOCTYPE html><title>todo</title><input id=t><button id=add>add</button><ul id=l></ul><script>const items = JSON.parse(localStorage.getItem('items') || '[]'); const draw = () => { document.getElementById('l').innerHTML = items.map((x) => '<li>' + x + '</li>').join(''); }; document.getElementById('add').onclick = () => { items.push(document.getElementById('t').value); localStorage.setItem('items', JSON.stringify(items)); draw(); }; draw();</script>",
        )]);
        let steps = "await fill('#t', 'milk'); await click('#add'); console.log('before ' + count('li')); location.reload(); await wait(100); console.log('after ' + count('li') + ' ' + text('li'));";
        let said = check_page(&serde_json::json!({"path": "index.html", "steps": steps, "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        assert_eq!(said.matches("before 1").count(), 1, "{said}");
        assert_eq!(said.matches("after 1 milk").count(), 1, "the item is there after the reload, once: {said}");
        assert!(said.contains("Steps: all ran (in 2 parts") && !said.contains("STEPS"), "{said}");
        // what it shows is the page after the last part
        assert!(said.contains("text: add milk") || said.contains("milk"), "{said}");
    }

    /// With the real browser, in the real sandbox: a page whose own script reloads it once is not run twice by its steps, and steps that reload from inside a function neither loop nor end the browser.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn a_reload_the_page_makes_or_the_steps_make_in_a_function_does_not_run_them_again() {
        let (dir, _) = page(&[
            ("self.html", "<!DOCTYPE html><title>self</title><p>x</p><script>if (!sessionStorage.getItem('self')) { sessionStorage.setItem('self', '1'); location.reload(); }</script>"),
            ("plain.html", "<!DOCTYPE html><title>plain</title><p>x</p>"),
            ("loop.html", "<!DOCTYPE html><title>loop</title><p>x</p><script>sessionStorage.clear(); setTimeout(() => location.reload(), 200);</script>"),
        ]);
        let said = check_page(&serde_json::json!({"path": "self.html", "steps": "console.log('steps ran');", "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        assert_eq!(said.matches("steps ran").count(), 1, "{said}");
        let started = std::time::Instant::now();
        let said = check_page(&serde_json::json!({"path": "plain.html", "steps": "console.log('before'); const again = () => location.reload(); await again(); console.log('after');", "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        // (the load that follows the reload finds no part due, and runs nothing: each line once, and no loop to the end of the browser's time)
        assert!(said.matches(": before
").count() == 1 && said.matches(": after
").count() == 1 && !said.contains("crashed"), "{said}");
        assert!(started.elapsed() < Duration::from_secs(40), "it did not loop until the browser was stopped");
        // a page that clears what the steps keep and reloads itself over and over does loop: that is said as the page's doing, not as the browser's crash
        let said = check_page(&serde_json::json!({"path": "loop.html", "steps": "console.log('x');", "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        assert!(said.contains("STEPS DID NOT END: part 1 of them began") && !said.contains("crashed at random") && !said.contains("sometimes crashes"), "{said}");
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

    /// With the real browser, in the real sandbox: the Playwright forms a real model wrote (`page.evaluate`, `evaluate`, `page.click`, `page.fill`, `page.press`,
    /// `page.waitForTimeout`, `page.locator(..).click()`, `page.$eval`) run, and a name that is not here fails saying which are.
    #[test]
    #[ignore = "starts Edge in the sandbox"]
    fn playwrights_forms_run_as_the_helpers_do() {
        let (dir, _) = page(&[(
            "index.html",
            "<!DOCTYPE html><title>todo</title><form id=f><input id=t><button id=add type=button>add</button></form><input type=checkbox id=c><select id=s><option value=a>a<option value=b>b</select><ul id=l></ul><script>document.getElementById('add').onclick=()=>{const li=document.createElement('li');li.textContent=document.getElementById('t').value;document.getElementById('l').append(li)};document.getElementById('f').onsubmit=(e)=>{e.preventDefault();document.getElementById('add').click()};</script>",
        )]);
        let steps = r#"
            await page.fill('#t', 'milk');
            await page.click('#add');
            console.log('r1 ' + await page.evaluate(() => document.querySelectorAll('li').length));
            console.log('r2 ' + await evaluate((a, b) => a + b, 2, 3));
            console.log('r3 ' + await page.evaluate('document.title'));
            console.log('r4 ' + await page.$eval('li', (el, tail) => el.textContent + tail, '!'));
            console.log('r5 ' + await page.$$eval('li', (els) => els.map((el) => el.textContent).join(',')));
            console.log('r6 ' + await page.textContent('li') + ' ' + await page.isVisible('li') + ' ' + await page.isVisible('#nope'));
            await page.waitForTimeout(100);
            await page.locator('#t').fill('eggs');
            await page.locator('#add').click();
            await page.press('#t', 'Enter');
            console.log('r7 ' + await page.locator('li').count() + ' ' + await page.locator('li').textContent() + ' ' + await page.locator('li').isVisible());
            await page.check('#c');
            await page.selectOption('#s', 'b');
            console.log('r8 ' + document.getElementById('c').checked + ' ' + document.getElementById('s').value + ' ' + await evaluate(async () => 'async'));
        "#;
        let said = check_page(&serde_json::json!({"path": "index.html", "steps": steps, "views": ["mobile"]}), dir.path(), &AtomicBool::new(false)).unwrap();
        println!("{said}");
        for wanted in ["Steps: all ran", "r1 1", "r2 5", "r3 todo", "r4 milk!", "r5 milk", "r6 milk true false", "r7 3 milk true", "r8 true b async"] {
            assert!(said.contains(wanted), "{wanted}: {said}");
        }
        for (steps, wanted) in [("await page.goto('x')", "page.goto is not a function (here: click, fill, press, check, select, wait, reload, text, count, visible, evaluate"), ("await foo(1)", "foo is not defined (here: click, fill")] {
            let said = check_page(&serde_json::json!({"path": "index.html", "steps": steps}), dir.path(), &AtomicBool::new(false)).unwrap();
            assert!(said.contains(&format!("STEPS FAILED: {wanted}")), "{wanted}: {said}");
        }
    }

    /// How the browser's own runs end, counted by their exit codes, on the command lines `check_page` runs (`page_line` and `shot_line`) and nothing between: a page that does
    /// nothing, for `DOGMIND_PROBE_RUNS` runs (30), as `DOGMIND_PROBE_MODE` says: `dump` (the page's DOM), `read` (the page in the copy that says its words), `shot` (a picture of it) or `steps` (the page with steps in it).
    /// `DOGMIND_PAGE_FLAGS` adds flags to the browser's. `DOGMIND_PROBE_KEEP=FOLDER` keeps what the browser said in each run.
    /// `DOGMIND_PROBE_RUNS=30 DOGMIND_PROBE_MODE=shot cargo test -p dogmind-agent --target-dir target-ui the_browsers_runs_are_counted_by_how_they_end -- --ignored --nocapture`.
    #[test]
    #[ignore = "starts Edge in the sandbox, DOGMIND_PROBE_RUNS times"]
    fn the_browsers_runs_are_counted_by_how_they_end() {
        let runs: usize = std::env::var("DOGMIND_PROBE_RUNS").ok().and_then(|text| text.parse().ok()).unwrap_or(30);
        let mode = std::env::var("DOGMIND_PROBE_MODE").unwrap_or_else(|_| "dump".to_string());
        let keep = std::env::var_os("DOGMIND_PROBE_KEEP").map(PathBuf::from);
        let (program, _) = browser().expect("Edge or Chrome is on this machine");
        let (dir, index) = page(&[("index.html", "<!DOCTYPE html><title>plain</title><p>hello</p>")]);
        let temp = crate::sandbox::temp_folder().expect("the sandbox's folder");
        let (flags, wait) = (flags(), WAIT_MS);
        let mut made = Removing(Vec::new());
        let own = next_run();
        let (steps_copy, frame, shot) = (temp.join(steps_page_of(&own)), temp.join(frame_of(&own)), temp.join(shot_of(&own)));
        made.0.extend([steps_copy.clone(), frame.clone(), shot.clone()]);
        let html = std::fs::read_to_string(&index).unwrap();
        std::fs::write(&steps_copy, with_steps(&html, &(file_url(dir.path()) + "/"), "await click('p'); console.log('steps done');", "dogmind-steps-probe")).unwrap();
        let read_copy = temp.join(steps_page_of(&format!("{own}-read")));
        made.0.push(read_copy.clone());
        std::fs::write(&read_copy, with_script(&html, &(file_url(dir.path()) + "/"), &reading_script(wait))).unwrap();
        std::fs::write(&frame, frame_page(&file_url(&index), &View { label: "desktop".into(), width: 1280, height: 800 })).unwrap();
        let mut profiles = Profiles::new(dir.path(), dir.path());
        let (mut ends, mut bad, mut seconds) = (std::collections::BTreeMap::<String, usize>::new(), 0usize, 0f64);
        let (mut hung_first, mut saved) = (0usize, 0usize);
        for at in 0..runs {
            let _ = std::fs::remove_file(&shot);
            let profile = profiles.next();
            let line = match mode.as_str() {
                "shot" => shot_line(&program, &flags, (1344, 960), wait, &profile, &shot, &frame.display().to_string()),
                "steps" => page_line(&program, &flags, 2 * wait, &profile, &steps_copy.display().to_string()),
                "read" => page_line(&program, &flags, wait + READ_MARGIN_MS, &profile, &read_copy.display().to_string()),
                _ => page_line(&program, &flags, wait, &profile, &index.display().to_string()),
            };
            let started = std::time::Instant::now();
            // (as the check runs it, when asked to with DOGMIND_PROBE_LIKE_THE_CHECK: with the limit its wait gives, and a run that hung tried once more)
            let like_the_check = std::env::var_os("DOGMIND_PROBE_LIKE_THE_CHECK").is_some();
            let limit = if like_the_check { run_limit(if mode == "steps" { 2 * wait } else { wait }) } else { LIMIT };
            let mut done = crate::sandbox::run_browser(&line, dir.path(), dir.path(), limit, &AtomicBool::new(false));
            if like_the_check && done.as_ref().is_ok_and(hung) {
                hung_first += 1;
                let again = crate::sandbox::run_browser(&line, dir.path(), dir.path(), limit, &AtomicBool::new(false));
                saved += usize::from(again.as_ref().is_ok_and(|again| again.status == Some(0)));
                done = again;
            }
            seconds += started.elapsed().as_secs_f64();
            let (end, well) = match &done {
                Ok(done) => {
                    let end = done.status.map_or(done.ended.to_string(), |code| format!("{:#x}", code as u32));
                    let well = done.status == Some(0) && if mode == "shot" { image::open(&shot).is_ok() } else { !done.stdout.is_empty() };
                    (end, well)
                }
                Err(error) => (format!("error: {error}"), false),
            };
            *ends.entry(end).or_default() += 1;
            bad += usize::from(!well);
            if let (Some(keep), Ok(done)) = (&keep, &done) {
                let _ = std::fs::create_dir_all(keep);
                let name = format!("{mode}-{at:03}-{}.txt", if well { "good" } else { "bad" });
                let _ = std::fs::write(keep.join(name), format!("{:?} {:?}\n--- stdout {} bytes\n--- stderr\n{}", done.status, done.ended, done.stdout.len(), crate::sandbox::decode(&done.stderr)));
            }
        }
        profiles.clear();
        println!("PROBE mode={mode} runs={runs} bad={bad} hung_first={hung_first} saved_by_a_second_try={saved} ends={ends:?} seconds_each={:.2} flags={flags}", seconds / runs.max(1) as f64);
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
