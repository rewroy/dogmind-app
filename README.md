# Dogmind App

Dogmind as a native Windows application. For a server, run `straydog`; for a terminal, run `dogmind`; this is the one with a
window. It is a product of its own, which uses Straydog as a program: put a new `straydog.exe` beside it and that is the
update.

- **Light.** Plain Win32 and RichEdit: no WebView and no GPU memory for drawing, so that the VRAM stays with the model. IME
  input, selection and copy are the system's own.
- **Engine beside it.** `straydog.exe` runs as a child process, only when it is used, and is reached only by its command line
  and its OpenAI-compatible API. The window's process never loads the GPU's libraries.
- **Sessions by workspace.** The sidebar lists each workspace's sessions; a new one starts by choosing its folder.
- **Any model.** Providers are set under *Model > Providers*:

| Kind | What it is | Key |
| --- | --- | --- |
| Straydog on this PC | `straydog.exe` beside this app (or on PATH) | none |
| OpenAI-compatible | Straydog on another machine (`http://192.168.0.100:8080/v1`), or any service with such an API (`https://.../v1`): name its model | a file, or `env:NAME` |
| Codex | A ChatGPT plan through the official Codex CLI (`codex login` first) | the CLI's |
| Claude | The Messages API, by default `claude-opus-5-5` | a file, or `env:ANTHROPIC_API_KEY` |

A key is never stored in the settings, which only say where it is. A key sent to an `http://` server crosses the network
as plain text.

## Using it

The first run asks how to reach a model and where to work; `/setup` goes through it again. Type `/` for the commands and `@` for the workspace's files. Enter sends, Shift+Enter starts a new line, Esc stops; Ctrl+N
starts a session, Ctrl+, opens the settings, Ctrl+Shift+C copies the last answer. A right click on a session renames or
deletes it.

## Licenses

The icon is `assets/app.ico`, made from the artwork `assets/app.png` by `scripts/make_icon.py`.

MIT (`LICENSE`). `scripts/package.py` puts one `LICENSE.txt` beside the executable, with Dogmind's (Apache-2.0), Straydog's (MIT),
Lucide's (ISC) and the list of Rust crates. After a dependency changes, run `python scripts/licenses.py`.

## Building

```bat
cargo build --release -p dogmind-app
```

No GPU toolkit is needed. To use the engine, copy a `straydog.exe` built by Straydog's README next to `dogmind-app.exe`.

## What it has of Dogmind, and what not

The agent loop, its stop conditions and retries; the system prompt; `read_file`, `list_files`, `search_files`, `apply_patch`,
`create_file`, `execute_command`, `show_diff` and `todo`; the approval model (*default*, *accept edits*, *auto*, the Paw Note);
three-stage compaction; sessions, in its own files; `DOGMIND.md`.

Not yet: memory (Dogheart), Toybox, web and browser tools, plugins, subagents, Dog Nose Work and the Dashboard.

`execute_command` runs in an AppContainer, `DogmindApp.Exec`, as in Dogmind: a command can write only in the workspace, read
that, the per-user toolchains on PATH and what every app may read, and has no network unless it was approved for it, each
time. What it starts ends with it. What cannot be contained does not run: there is no fallback, and a workspace that is a
whole drive or holds your user folder cannot run commands at all (it still reads and edits, and says so when it starts).

AppContainers work by ACEs, so the first command in a workspace writes one on that folder and its tree, and on the toolchain
folders, and these stay. To take them away: `icacls <folder> /remove <the profile's SID> /t`. The app's own file tools run
outside the container and pass through the approvals instead.

## Following the two projects

Both move on their own. They are kept at a distance so that following them is mostly reading:

- The app shares no code with Straydog. It starts `straydog.exe`, runs `straydog setup`, reads the bundles from `fetch --help`,
  and speaks HTTP: all of that is in `crates/dogmind-app/src/engine.rs`.
- What the model reads comes from Dogmind, not from a copy typed here. `scripts/sync_dogmind.py` generates `assets/dogmind`,
  and lists in one place what the app leaves out. A sentence or a parameter it no longer finds is an error, not a silent
  drift. `--check` says whether the assets still match.
- `UPSTREAM.toml` says which commits were followed; `python scripts/upstream.py` lists what changed since in the files that
  matter, and `--mark` moves it after a port.

```bat
python scripts\upstream.py
..\Dogmind\.venv\Scripts\python.exe scripts\sync_dogmind.py
cargo test
python scripts\upstream.py --mark
```

Vocabulary follows Dogmind's `AGENTS.md`: Paw Note is the explanation before an approval.
