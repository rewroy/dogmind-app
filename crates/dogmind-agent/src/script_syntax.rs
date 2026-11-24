//! The syntax errors of a page's scripts, or of a script file, as a browser would refuse them: each script is parsed by the engine code mode runs, as a script or
//! as a module, and never run. A model that wrote a 1,332-line page with its thinking closed had written `1 and 0` in it; it found that only by thinking the
//! whole page over again, and wrote all of it a second time (439 s). Told at its write, it would have changed the one line.

use std::path::Path;

use regex::Regex;
use rquickjs::{qjs, Context, Ctx, Module, Runtime};

/// What is parsed at the most: past this a file is left alone, as a parse of it would be the cost of the write.
const MOST_BYTES: usize = 4 * 1024 * 1024;

/// One script of a file, where it begins in the file and how a browser takes it.
struct Script {
    line: usize,
    body: String,
    module: bool,
}

/// The syntax errors of the scripts of the file at `path`, a page (`.html`, `.htm`) or a script (`.js`, `.mjs`), each as `line N: SyntaxError: ...` with N the file's
/// own line. None for any other file, one that cannot be read, or one with none.
pub fn errors_in(path: &Path) -> Vec<String> {
    let extension = path.extension().map(|extension| extension.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if !matches!(extension.as_str(), "html" | "htm" | "js" | "mjs") {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    if bytes.len() > MOST_BYTES {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let scripts = match extension.as_str() {
        "html" | "htm" => scripts_of_page(&text),
        // (a `.js` with an import or export at the start of a line is a module, as a page that loads it with type=module takes it)
        _ => vec![Script { line: 1, body: text.to_string(), module: extension == "mjs" || Regex::new(r"(?m)^\s*(import|export)\b").expect("the pattern is valid").is_match(&text) }],
    };
    errors_of(&scripts)
}

/// The scripts written in a page, with the line each body begins on: those a browser runs as JavaScript (no type, a JavaScript type, or a module), and not a
/// data block, an import map, a template or one with a `src` of its own.
fn scripts_of_page(html: &str) -> Vec<Script> {
    let pattern = Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>").expect("the pattern is valid");
    let kind = Regex::new(r#"(?i)\btype\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).expect("the pattern is valid");
    let source = Regex::new(r"(?i)\bsrc\s*=").expect("the pattern is valid");
    pattern.captures_iter(html).filter_map(|found| {
        let (tag, body) = (found.get(1)?, found.get(2)?);
        if source.is_match(tag.as_str()) || body.as_str().trim().is_empty() {
            return None;
        }
        let given = kind.captures(tag.as_str()).and_then(|kind| kind.get(1).or(kind.get(2)).or(kind.get(3))).map(|kind| kind.as_str().trim().to_ascii_lowercase()).unwrap_or_default();
        let module = given == "module";
        if !(given.is_empty() || module || given.contains("javascript") || given.contains("ecmascript")) {
            return None;
        }
        Some(Script { line: html[..body.start()].matches('\n').count() + 1, body: body.as_str().to_string(), module })
    }).collect()
}

/// The first syntax error of each script, as the engine says it, at the file's line.
fn errors_of(scripts: &[Script]) -> Vec<String> {
    if scripts.is_empty() {
        return Vec::new();
    }
    let Ok(runtime) = Runtime::new() else { return Vec::new() };
    runtime.set_memory_limit(256 * 1024 * 1024);
    runtime.set_max_stack_size(2 * 1024 * 1024);
    let Ok(context) = Context::full(&runtime) else { return Vec::new() };
    context.with(|ctx| scripts.iter().filter_map(|script| parse_error(&ctx, script)).collect())
}

/// What the engine says of a script it is given to parse alone: None where it parses.
fn parse_error(ctx: &Ctx<'_>, script: &Script) -> Option<String> {
    if script.module {
        Module::declare(ctx.clone(), "page.js", script.body.as_bytes().to_vec()).err()?;
    } else {
        let mut source = script.body.as_bytes().to_vec();
        let length = source.len();
        source.push(0);
        let name = c"page.js";
        // SAFETY: the source ends with the NUL the engine reads up to, and lives through the call; what a parse that went well gives back is freed here, unused.
        let parsed = unsafe {
            let flags = (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
            let value = qjs::JS_Eval(ctx.as_raw().as_ptr(), source.as_ptr().cast(), length as _, name.as_ptr(), flags);
            let failed = qjs::JS_VALUE_GET_TAG(value) == qjs::JS_TAG_EXCEPTION;
            if !failed {
                qjs::JS_FreeValue(ctx.as_raw().as_ptr(), value);
            }
            !failed
        };
        if parsed {
            return None;
        }
    }
    let thrown = ctx.catch();
    let error = thrown.as_exception()?;
    let message = error.message().unwrap_or_default();
    let name = error.as_object().get::<_, Option<String>>("name").ok().flatten().unwrap_or_else(|| "SyntaxError".to_string());
    // (a module's imports are looked for once it is parsed, and none is here to be found: `could not load module 'three'` is not the page's, and is not said)
    if name != "SyntaxError" {
        return None;
    }
    // (the line in the script, from where the engine says it was: `at page.js:12:5`)
    let at = error.stack().as_deref().and_then(|stack| {
        let after = &stack[stack.find("page.js:")? + "page.js:".len()..];
        after.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<usize>().ok()
    });
    let line = at.map_or(script.line, |at| script.line + at - 1);
    Some(format!("line {line}: {name}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = crate::in_container::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    /// A page's scripts are parsed as the browser takes them, a classic one in sloppy mode and a module as a module, at the file's own lines; data and a script
    /// with a source of its own are not.
    #[test]
    fn a_pages_scripts_are_parsed_and_their_errors_said_at_the_files_lines() {
        let page = "<!DOCTYPE html>\n<title>t</title>\n<script>\nvar a = 1;\nwith (Math) { a = max(a, 2); }\n</script>\n<script type=\"module\">\nimport * as T from './three.module.js';\nconst x = 2;\nemit(x, 1 and 0);\n</script>\n<script type=\"application/json\">{not json at all</script>\n<script src=\"a.js\">broken (</script>\n";
        let (_dir, path) = write("page.html", page);
        let said = errors_in(&path);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("line 10: SyntaxError"), "{said:?}");
        let (_dir, clean) = write("clean.html", "<script>let n = 0; function f() { return n + 1; }</script><script type=module>export const a = 1;</script>");
        assert!(errors_in(&clean).is_empty());
        // (a module that imports what an import map names, which is not here to be found, is not an error of the page's)
        let (_dir, mapped) = write("mapped.html", "<script type=\"importmap\">{\"imports\":{\"three\":\"https://unpkg.com/three\"}}</script><script type=\"module\">import * as THREE from 'three';\nconst s = new THREE.Scene();</script>");
        assert!(errors_in(&mapped).is_empty(), "{:?}", errors_in(&mapped));
    }

    /// A script file: a module when it imports or exports, a classic script otherwise; another kind of file is not looked at.
    #[test]
    fn a_script_file_is_parsed_and_other_files_are_not() {
        let (_dir, module) = write("main.js", "import { a } from './a.js';\nconsole.log(a;\n");
        let said = errors_in(&module);
        assert!(said.len() == 1 && said[0].starts_with("line 2: SyntaxError"), "{said:?}");
        let (_dir, classic) = write("old.js", "function f() {\n  return 1\n}\nf(\n");
        assert_eq!(errors_in(&classic).len(), 1);
        let (_dir, fine) = write("fine.js", "const a = [1, 2].map(x => x * 2);\n");
        assert!(errors_in(&fine).is_empty());
        let (_dir, text) = write("notes.txt", "1 and 0 (");
        assert!(errors_in(&text).is_empty());
    }

    /// Nothing in what is parsed runs: a script that would print, loop or throw when run is only parsed.
    #[test]
    fn nothing_parsed_is_run() {
        let (_dir, path) = write("run.html", "<script>while (true) {}</script><script>throw new Error('ran')</script><script type=module>await new Promise(() => {});</script>");
        assert!(errors_in(&path).is_empty());
    }
}
