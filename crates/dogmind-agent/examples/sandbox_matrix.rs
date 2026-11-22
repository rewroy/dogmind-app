//! The command sandbox as a release will meet it: lines that Dogmind's work runs again and again, each run the way the agent runs a command (execute_command:
//! the sandbox, the bundle's tools on PATH, the rewrites of the line), in a workspace under the users' folder and in one outside it, with 60 s for each. venv,
//! git init and node's child processes all failed there only once a person or a comparison run had met them.
//!
//!     cargo run -p dogmind-agent --example sandbox_matrix -- OUT.json [LAST.json]
//!
//! DOGMIND_TOOLS names the bundle's tools folder (honoured by a debug build), and DOGMIND_MATRIX_OUTSIDE the folder outside the users' folder to work in
//! (C:\dogmind-matrix when it is not set). It prints a table of how each line went, and why a line that failed did (the sandbox, a tool that is not there,
//! the network that is closed by default), and writes all of it to OUT.json. Given LAST.json, the table of the version before, it names the lines that went
//! through then and do not now, and ends with 1 when there is one: a release is not made with any.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// One line: its name in the table, what it runs, and what its output must hold for it to have gone through (beside exit code 0).
struct Line {
    name: &'static str,
    runs: &'static str,
    holds: &'static str,
}

const LINES: &[Line] = &[
    Line { name: "python -c", runs: "python -c \"print(6*7)\"", holds: "42" },
    Line { name: "pip install (a local wheel)", runs: "python -m pip install --no-index --no-deps --disable-pip-version-check -q tiny-0.1-py3-none-any.whl && python -c \"import tiny; print(tiny.VALUE)\"", holds: "tiny-ok" },
    Line { name: "venv", runs: "python -m venv .venv && .venv\\Scripts\\python -c \"print('venv-ok')\"", holds: "venv-ok" },
    Line { name: "pytest", runs: "python -m pytest -q -p no:cacheprovider test_a.py", holds: "1 passed" },
    Line { name: "http.server, then curl", runs: "start /b python -m http.server 8765 >nul 2>&1 & python -c \"import time; time.sleep(2)\" & curl -s -o nul -w \"code %{http_code}\" http://127.0.0.1:8765/", holds: "code 200" },
    Line { name: "node -e", runs: "node -e \"console.log(6*7)\"", holds: "42" },
    Line { name: "npm init -y", runs: "npm init -y >nul && type package.json | findstr /c:\"\\\"name\\\"\"", holds: "name" },
    Line { name: "npm install (a local package)", runs: "npm install --no-audit --no-fund --offline ./localpkg >nul && node -e \"console.log(require('localpkg').value)\"", holds: "localpkg-ok" },
    Line { name: "node --test", runs: "node --test t.test.js", holds: "pass 1" },
    Line { name: "node execSync", runs: "node -e \"console.log(require('child_process').execSync('echo exec-ok').toString().trim())\"", holds: "exec-ok" },
    Line { name: "git init", runs: "git init", holds: "" },
    Line { name: "git add, commit, log", runs: "git add -A && git commit -q -m first && git log --oneline", holds: "first" },
    Line { name: "git branch, switch", runs: "git branch feature && git switch -q feature && git branch --show-current", holds: "feature" },
    Line { name: "git diff", runs: "echo more>> a.txt && git diff --stat", holds: "a.txt" },
    Line { name: "git stash", runs: "git stash && git stash list", holds: "stash@{0}" },
    Line { name: "cargo new", runs: "cargo new --vcs none -q hello && dir /b hello", holds: "Cargo.toml" },
    Line { name: "cargo build, run", runs: "cd hello && cargo run -q", holds: "Hello, world!" },
    Line { name: "cargo test", runs: "cd hello && cargo test -q", holds: "test result: ok" },
    Line { name: "dir", runs: "dir", holds: "a.txt" },
    Line { name: "copy, move, type", runs: "copy a.txt b.txt >nul && move b.txt c.txt >nul && type c.txt", holds: "one" },
    Line { name: "mkdir, rmdir", runs: "mkdir d1\\d2 && rmdir /s /q d1 && if not exist d1 echo gone", holds: "gone" },
    Line { name: "del", runs: "del c.txt && if not exist c.txt echo deleted", holds: "deleted" },
    Line { name: "findstr", runs: "findstr /n one a.txt", holds: "1:one" },
    Line { name: "PowerShell Get-ChildItem", runs: "powershell -NoProfile -Command \"Get-ChildItem -Name a.txt\"", holds: "a.txt" },
    Line { name: "PowerShell Get-Content", runs: "powershell -NoProfile -Command \"Get-Content a.txt\"", holds: "one" },
    Line { name: "curl https (the network as set by default)", runs: "curl -s -o nul -w \"code %{http_code}\" https://example.com/", holds: "code 200" },
];

/// What a workspace starts with for the lines: a wheel and a package to install with no network, a test of each kind, and a text file.
fn fill(workspace: &Path) {
    std::fs::create_dir_all(workspace.join("localpkg")).unwrap();
    std::fs::write(workspace.join("a.txt"), "one\n").unwrap();
    std::fs::write(workspace.join("test_a.py"), "def test_a():\n    assert 1 + 1 == 2\n").unwrap();
    std::fs::write(workspace.join("t.test.js"), "const test = require('node:test');\nconst assert = require('node:assert');\ntest('adds', () => assert.strictEqual(1 + 1, 2));\n").unwrap();
    std::fs::write(workspace.join("localpkg").join("package.json"), "{\"name\": \"localpkg\", \"version\": \"1.0.0\", \"main\": \"index.js\"}\n").unwrap();
    std::fs::write(workspace.join("localpkg").join("index.js"), "module.exports = { value: 'localpkg-ok' };\n").unwrap();
    // (a wheel is a zip of the module and the three files that say what it is)
    let wheel = std::fs::File::create(workspace.join("tiny-0.1-py3-none-any.whl")).unwrap();
    let mut zip = zip::ZipWriter::new(wheel);
    let files = [
        ("tiny.py", "VALUE = 'tiny-ok'\n"),
        ("tiny-0.1.dist-info/METADATA", "Metadata-Version: 2.1\nName: tiny\nVersion: 0.1\n"),
        ("tiny-0.1.dist-info/WHEEL", "Wheel-Version: 1.0\nGenerator: dogmind\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
        ("tiny-0.1.dist-info/RECORD", "tiny.py,,\ntiny-0.1.dist-info/METADATA,,\ntiny-0.1.dist-info/WHEEL,,\ntiny-0.1.dist-info/RECORD,,\n"),
    ];
    for (name, text) in files {
        zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zip, text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// Why a line that did not go through did not: from what it said, as near as words say it.
fn why(report: &str, seconds: f64) -> &'static str {
    let said = report.to_lowercase();
    if seconds >= 59.0 || said.contains("timed out") {
        "hung (the sandbox?)"
    } else if said.contains("is not recognized") || said.contains("認識されていません") || said.contains("no module named") {
        "a tool that is not there (or a module of one)"
    } else if said.contains("network") || said.contains("could not resolve host") || said.contains("ネットワーク") {
        "the network (closed by default)"
    } else if said.contains("access is denied") || said.contains("アクセスが拒否") || said.contains("permission denied") || said.contains("winerror 5") || said.contains("0xc0000022") || said.contains("ntcreatedirectoryobject") || said.contains("cannot lock ref") {
        "the sandbox"
    } else {
        "other: see what it said"
    }
}

fn run_all(place: &str, workspace: &Path) -> Value {
    let _ = std::fs::remove_dir_all(workspace);
    std::fs::create_dir_all(workspace).unwrap();
    fill(workspace);
    let cancel = AtomicBool::new(false);
    let mut bench = dogmind_agent::tools::Workbench::new(workspace);
    let mut results = serde_json::Map::new();
    for line in LINES {
        let began = Instant::now();
        let said = bench.run("execute_command", &json!({"command": line.runs, "timeout_seconds": 60}), workspace, &cancel);
        let seconds = began.elapsed().as_secs_f64();
        let report = said.unwrap_or_else(|error| format!("error: {error}"));
        let exit = report.lines().rev().find_map(|row| row.strip_prefix("exit_code: ")).and_then(|code| code.trim().parse::<i64>().ok());
        let ok = exit == Some(0) && report.contains(line.holds) && seconds < 59.0;
        let cause = if ok { "" } else { why(&report, seconds) };
        eprintln!("{place:8} {:44} {:5} {seconds:5.1} s {cause}", line.name, if ok { "ok" } else { "FAIL" });
        results.insert(line.name.to_string(), json!({"ok": ok, "exit": exit, "seconds": (seconds * 10.0).round() / 10.0, "why": cause, "said": report.chars().take(600).collect::<String>()}));
    }
    // (a server a line left behind goes with the job; what git makes read-only goes with the folder)
    let _ = std::process::Command::new("cmd").args(["/c", "rmdir", "/s", "/q"]).arg(workspace).status();
    Value::Object(results)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = PathBuf::from(args.first().expect("OUT.json"));
    let users = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).expect("LOCALAPPDATA").join("Temp").join("dogmind-matrix");
    let outside = std::env::var_os("DOGMIND_MATRIX_OUTSIDE").map_or_else(|| PathBuf::from(r"C:\dogmind-matrix"), PathBuf::from);
    let now = json!({
        "tools": std::env::var("DOGMIND_TOOLS").unwrap_or_default(),
        "under the users' folder": run_all("users", &users.join("ws")),
        "outside it": run_all("outside", &outside.join("ws")),
    });
    std::fs::write(&out, serde_json::to_string_pretty(&now).unwrap()).unwrap();
    // The table.
    println!("| line | under the users' folder | outside it |\n| --- | --- | --- |");
    for line in LINES {
        let cell = |place: &str| {
            let result = &now[place][line.name];
            if result["ok"].as_bool() == Some(true) { format!("ok ({} s)", result["seconds"]) } else { format!("FAIL: {} ({} s)", result["why"].as_str().unwrap_or(""), result["seconds"]) }
        };
        println!("| {} | {} | {} |", line.name, cell("under the users' folder"), cell("outside it"));
    }
    // What went through the last time and does not now.
    let Some(last) = args.get(1).and_then(|path| std::fs::read_to_string(path).ok()).and_then(|text| serde_json::from_str::<Value>(&text).ok()) else { return };
    let mut fallen = Vec::new();
    for place in ["under the users' folder", "outside it"] {
        for line in LINES {
            if last[place][line.name]["ok"].as_bool() == Some(true) && now[place][line.name]["ok"].as_bool() != Some(true) {
                fallen.push(format!("{} ({place}): {}", line.name, now[place][line.name]["why"].as_str().unwrap_or("")));
            }
        }
    }
    if !fallen.is_empty() {
        println!("\nWent through the last time, and not now:");
        for line in &fallen {
            println!("- {line}");
        }
        std::process::exit(1);
    }
    println!("\nNothing that went through the last time fails now.");
}
