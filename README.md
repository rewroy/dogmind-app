# Dogmind

[日本語](README.ja.md)

Dogmind as a native Windows application. For a server, run `straydog`; for a terminal, run `dogmind`; this is the one with a
window. It is a product of its own, which uses Straydog as a program: put a new `straydog.exe` beside it and that is the
update.

- **Light.** Win32, drawn with Direct2D and DirectWrite on the CPU, and RichEdit for the text: no GPU memory for drawing, so that
  the VRAM stays with the model, and a WebView2 only while a preview is open. IME input, selection and copy are the system's own.
  Japanese is set in BIZ UDGothic, shipped beside the app and used from there: nothing is installed on the PC.
- **Engine beside it.** `straydog.exe` runs as a child process, only when it is used, and is reached only by its command line
  and its OpenAI-compatible API. The window's process never loads the GPU's libraries.
- **Sessions by workspace.** The sidebar lists each workspace's sessions; a new one starts by choosing its folder.
- **Any model.** Providers are set in *Settings > Models and connections*, which the model chip's *Connections...* and `/providers`
  also open:

| Kind | What it is | Key |
| --- | --- | --- |
| Straydog on this PC | `straydog.exe` beside this app (or on PATH) | none |
| OpenAI-compatible | Straydog on another machine (`http://192.168.0.100:8080/v1`), or any service with such an API (`https://.../v1`): name its model | pasted (kept in Credential Manager), or `env:NAME` |
| Codex | A ChatGPT plan through the official Codex CLI, which you install yourself (`codex login` first) | the CLI's |
| Claude | The Messages API, by default `claude-opus-5-5` | pasted (kept in Credential Manager), or `env:ANTHROPIC_API_KEY` |

A key is never stored in the settings, which only say where it is. A key sent to an `http://` server crosses the network
as plain text.

## Using it

It runs on Windows 11, without administrator rights. Run `DogmindSetup-VERSION.exe` (as `DogmindSetup-0.1.0.exe`), which puts it in `%LOCALAPPDATA%\Programs\Dogmind`, in the Start menu and in Installed apps. Its first page names that folder, and *Change...* opens Windows's folder picker to choose another: one that holds other things gets a `Dogmind` folder inside it, and one this user cannot write in without an administrator (`C:\Program Files`), or that is where settings and sessions are kept, is refused with the reason. An update goes where the install is, and does not offer the choice. Or extract the whole zip to a folder of your own and start `dogmind-app.exe`.
To run a model on this PC it takes an NVIDIA GPU with a current driver, and about 76 GB free on a drive (75 GB to download; 70 GB stay). Without one, use another computer, a service, Codex or Claude.
*Settings > Models on this PC* lists what Straydog can set up here. A model that needs more free graphics memory than the card has now is listed too, greyed and marked not recommended with what it needs, for whoever wants to try it.
The first run asks how to reach a model and where to work; `/setup` goes through it again. Type `/` for the commands and `@` for the workspace's files. Enter sends, Shift+Enter starts a new line, Esc stops; Ctrl+N
starts a session, Ctrl+, opens the settings, Ctrl+Shift+C copies the last answer. A right click on a session renames or
deletes it.

While the model works, Enter puts what you type into the work before its next step, and Ctrl+Enter keeps it for after the work. Each
waiting message has buttons to send it into the work now, to edit it, or to discard it. Each message shows its time; an answer's
branch button starts a new session with the conversation up to that answer, to go on from there another way. From the second turn,
a rail at the transcript's right edge has a mark for each of your messages: the one you are reading is longer, and a click goes to it.

## Updating, removing, and when something goes wrong

New versions are published at [Releases](https://github.com/rewroy/dogmind-app/releases). The app asks GitHub for the latest release
at most once a day, and says so quietly when there is a newer one, with a button that opens its page: it never downloads or installs
anything by itself, and *Settings > About* has *Check for updates*, and a switch to turn the daily check off. The request carries
nothing about you but the app's version. To update, run the new `DogmindSetup-VERSION.exe`: it asks a running copy to close (one that is working stays open and says so; nothing is ended by force), checks each file by its hash, replaces only what changed and takes away only what the last version put there. Every file takes its place or none does: a failure puts back what was there, so the program is as it was, and running it again goes on from there. From a zip: quit from the tray (closing the window leaves the app running), and extract the new zip over the old folder. Settings and sessions are in `%LOCALAPPDATA%\DogmindApp` and stay.

To remove it, *Settings > General > Remove this app from this PC* takes away the settings, sessions, logs, its keys in Credential Manager, what was opened to the sandbox and its AppContainer
profile, and the model unless you keep it. In its data folder it takes only what it made there, by name: anything else stays, and it says what. Straydog's own sandbox profile stays, as the engine also runs as a server of its own. Installed apps opens the same page, and closing it then takes away the program and its entries: only the files its installer put there, and a folder only once it is empty. From a zip, delete the folder it runs from. Workspaces stay as they are.

A crash leaves a text file in `%LOCALAPPDATA%\DogmindApp\crashes`, and the next start says so. `/report`, or *About > Write a problem report*, writes `report.txt` in `%LOCALAPPDATA%\DogmindApp`, with no key, server address or
conversation in it, and says where it is: read it, and attach it to an [issue](https://github.com/rewroy/dogmind-app/issues) with what you were doing. Nothing is sent by itself. Whoever makes a release keeps its `dogmind_app.pdb` and `straydog.pdb`: the addresses in a crash are read with them.
Please report a security problem privately, as [SECURITY.md](SECURITY.md) says, and not in a public issue.

The app is not signed yet. Windows shows *Windows protected your PC* for it (*More info > Run anyway*), and with Smart App Control on, Windows will not run it at all.

## Licenses

The icon is `assets/app.ico`, made from the artwork `assets/app.png` by `scripts/make_icon.py`.

This repository is MIT (`LICENSE`). What it has taken from Dogmind, the agent it follows, stays under Dogmind's
Apache-2.0 (`assets/licenses/dogmind-LICENSE.txt`): `assets/dogmind/` as Dogmind writes it, and the system prompt, tool schemas
and agent loop that began as Dogmind's and were changed here (see *What it has of Dogmind* below). The icons in `assets/lucide/`
are Lucide's (ISC, `assets/lucide/LICENSE`).

`scripts/package.py` puts one `LICENSE.txt` beside the executable, with Dogmind's (Apache-2.0), Straydog's (MIT),
Lucide's (ISC), BIZ UDGothic's (SIL OFL 1.1), the CUDA toolkit's, the WebView2 SDK's, the bundled tools' and every Rust crate in the app and in `straydog.exe` with its license's words. After a dependency changes, here or in Straydog, run `python scripts/licenses.py`.

## Building

```bat
set "RUSTFLAGS=-C target-feature=+crt-static --remap-path-prefix=%USERPROFILE%=~"
cargo build --release -p dogmind-app -p dogmind-setup
```

A release is built so: a panic's message names the path of its source, and without the remap the builder's user folder is in
every one. `RUSTFLAGS` takes the place of `.cargo/config.toml`'s flags, so the static C runtime is said again.

`python -I scripts\package.py` then makes `dist\DogmindApp`, its zip and `dist\DogmindSetup-VERSION.exe`. The installer is `crates/dogmind-setup` and no tool besides: the package is on the end of that one executable, deflated file by file, and read as a stream. To try one in a scratch folder, `DogmindSetup.exe --test --dir FOLDER`: it writes no Start menu entry or registry key, only what they would say, in that folder. Without `--test`, `--dir FOLDER` only chooses the folder the first page starts with.

To release: raise `version` in the workspace's `Cargo.toml`, build and package as above, and publish a GitHub release tagged
`vVERSION` (as `v0.1.0`) with `dist\DogmindSetup-VERSION.exe` and `dist\DogmindApp-VERSION.zip`, keeping the PDBs. The app's
update check reads the latest published release: drafts and pre-releases are not offered.

No GPU toolkit is needed. To use the engine, copy a `straydog.exe` built by Straydog's README, with the toolkit's `cublas64_13.dll` and `cublasLt64_13.dll`, next to `dogmind-app.exe`.

A package also has `tools\` beside the exe, so that a person installs nothing and a command does the same on every machine: Python with common modules, Node.js with npm and npx, and Git, first on every sandboxed command's PATH, ahead of anything installed. `python -I scripts/fetch_tools.py` fetches them by the hashes pinned in `scripts/tools.lock.json`, into a cache outside this tree, and `python -I scripts/package.py` puts them together and adds their licenses. A development build with no `tools\` beside it uses what the machine has, or the folder `DOGMIND_TOOLS` names.

## What it has of Dogmind, and what not

The agent loop, its stop conditions and retries; the system prompt; `read_file`, `list_files`, `search_files`, `apply_patch`,
`create_file`, `execute_command`, `show_diff` and `todo`; the approval model (*default*, *accept edits*, *auto*, the Paw Note);
three-stage compaction; sessions, in its own files; `DOGMIND.md`.

Not yet: Toybox, plugins, general subagents and Dog Nose Work.

What it has of its own for a long look, as Deepseek Harness keeps one out of the conversation: `explore` hands a question to a
helper with a context of its own and only the looking tools, and only its answer joins the conversation. A file of more than 800
lines read with no place in it comes as its outline, a turn is told at each 96 KB of files it has read, and a long command
output or search result is kept whole in `%TEMP%\dogmind-output` for a week, its report saying where. A command left running in
the background shows, at each `command_status`, what it has printed since the last look: a dev server's ready line among it.

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
- What the model reads is this app's own, as small as a local model needs it: the system prompt in `crates/dogmind-agent/src/prompt.rs`
  and the tool schemas in `tools.rs`. They began as Dogmind's, and only its checkpoint prompt and Codex policy are still taken as they are.
  `scripts/sync_dogmind.py` generates `assets/dogmind`, which is what Dogmind says now: `--check` says when that has changed, and a change
  is read and ported by hand if it earns its tokens.
- `UPSTREAM.toml` says which commits were followed; `python scripts/upstream.py` lists what changed since in the files that
  matter, and `--mark` moves it after a port.

```bat
python scripts\upstream.py
..\Dogmind\.venv\Scripts\python.exe scripts\sync_dogmind.py
cargo test
python scripts\upstream.py --mark
```

Vocabulary follows Dogmind's `AGENTS.md`: Paw Note is the explanation before an approval.
