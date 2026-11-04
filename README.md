<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-wordmark-dark.png">
    <img src="assets/logo-wordmark.png" alt="Dogmind" width="360">
  </picture>
</p>

<p align="center"><a href="README.ja.md">日本語</a></p>

Dogmind is an AI agent on your own Windows PC, as a native application: it reads and changes the files of a folder you choose,
and runs commands there, asking you first. Its engine is [Straydog](https://github.com/rewroy/Straydog), a separate program
that runs a model on this PC: put a new `straydog.exe` beside the app and that is the engine's update. It can also use a model
on another machine, a service, Codex or Claude.

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

New versions are published at [Releases](https://github.com/rewroy/dogmind-app/releases). The app asks GitHub for the newest release
(a pre-release too) at most once a day, and *Settings > About* has *Check for updates*, and a switch (*Update automatically*) to turn the daily check off. The request carries
nothing about you but the app's version. A copy that the installer put in its folder also downloads the newer release's setup in the background (not on a metered connection, unless you press
*Check for updates*), and only if GitHub lists it with its size and SHA-256: the download is kept as `.partial` in the `updates` folder of the app's data until its size and hash are those GitHub published, and deleted if they are not, or if you cancel it
(*Settings > About* shows its progress and a *Cancel*). Then a row above the input says the update is ready, with *Restart to update*: only that press, when no work is going on or when it ends, runs the setup
(`DogmindSetup-VERSION.exe --update --dir FOLDER --wait-pid PID`, which asks nothing, waits for the app to end, updates the folder, puts back what was there if it fails, and starts the app again). A copy unpacked from the zip
is only told, with a button that opens the release's page. To update by hand, run the new `DogmindSetup-VERSION.exe`: it asks a running copy to close (one that is working stays open and says so; nothing is ended by force), checks each file by its hash, replaces only what changed and takes away only what the last version put there. Every file takes its place or none does: a failure puts back what was there, so the program is as it was, and running it again goes on from there. From a zip: quit from the tray (closing the window leaves the app running), and extract the new zip over the old folder. Settings and sessions are in `%LOCALAPPDATA%\DogmindApp` and stay.

To remove it, *Settings > General > Remove this app from this PC* takes away the settings, sessions, logs, its keys in Credential Manager, what was opened to the sandbox and its AppContainer
profile, and the model unless you keep it. In its data folder it takes only what it made there, by name: anything else stays, and it says what. Straydog's own sandbox profile stays, as the engine also runs as a server of its own. Installed apps opens the same page, and closing it then takes away the program and its entries: only the files its installer put there, and a folder only once it is empty. From a zip, delete the folder it runs from. Workspaces stay as they are.

A crash leaves a text file in `%LOCALAPPDATA%\DogmindApp\crashes`, and the next start says so. `/report`, or *About > Write a problem report*, writes `report.txt` in `%LOCALAPPDATA%\DogmindApp`, with no key, server address or
conversation in it, and says where it is: read it, and attach it to an [issue](https://github.com/rewroy/dogmind-app/issues) with what you were doing. Nothing is sent by itself. Whoever makes a release keeps its `dogmind_app.pdb` and `straydog.pdb`: the addresses in a crash are read with them.
Please report a security problem privately, as [SECURITY.md](SECURITY.md) says, and not in a public issue.

The app is not signed yet. Windows shows *Windows protected your PC* for it (*More info > Run anyway*), and with Smart App Control on, Windows will not run it at all.

## Licenses

The icon is `assets/app.ico`, made from the artwork `assets/app.png` by `scripts/make_icon.py`.

This repository is MIT (`LICENSE`). The icons in `assets/lucide/` are Lucide's (ISC, `assets/lucide/LICENSE`).

`scripts/package.py` puts one `LICENSE.txt` beside the executable, with this app's (MIT), Straydog's (MIT),
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
update check offers the newest release by its tag's version, pre-releases included, and never a draft; a tag that is not a version is passed over.

No GPU toolkit is needed. To use the engine, copy a `straydog.exe` built by Straydog's README, with the toolkit's `cublas64_13.dll` and `cublasLt64_13.dll`, next to `dogmind-app.exe`.

A package also has `tools\` beside the exe, so that a person installs nothing and a command does the same on every machine: Python with common modules, Node.js with npm and npx, and Git, first on every sandboxed command's PATH, ahead of anything installed. `python -I scripts/fetch_tools.py` fetches them by the hashes pinned in `scripts/tools.lock.json`, into a cache outside this tree, and `python -I scripts/package.py` puts them together and adds their licenses. A development build with no `tools\` beside it uses what the machine has, or the folder `DOGMIND_TOOLS` names.

## The agent

Its tools read, search, create and edit files (`read_file`, `list_files`, `find_files`, `search_files`, `create_file`,
`apply_patch`), run commands (`execute_command`), show what changed (`show_diff`), keep a to-do list, check a web page in a
browser, and search and read the web when that is turned on. Edits and commands ask first, as the mode says (*default*,
*accept edits*, *auto*), each with a Paw Note: the explanation shown before an approval. A long session is compacted in three
stages, and every session is kept in a file of its own. A project's `DOGMIND.md` (or its `AGENTS.md` or `CLAUDE.md`) says how it
wants to be worked on. What the model reads is kept as small as a local model needs it: the system prompt is in
`crates/dogmind-agent/src/prompt.rs` and the tool schemas in `tools.rs`.

For a long look, `explore` hands a question to a helper with a context of its own and only the looking tools, and only its
answer joins the conversation. A file of more than 800 lines read with no place in it comes as its outline, a turn is told at
each 96 KB of files it has read, and a long command output or search result is kept whole in `%TEMP%\dogmind-output` for a week,
its report saying where. A command left running in the background shows, at each `command_status`, what it has printed since the
last look: a dev server's ready line among it.

`execute_command` runs in an AppContainer, `DogmindApp.Exec`: a command can write only in the workspace, read
that, the per-user toolchains on PATH and what every app may read, and has no network unless it was approved for it, each
time. What it starts ends with it. What cannot be contained does not run: there is no fallback, and a workspace that is a
whole drive or holds your user folder cannot run commands at all (it still reads and edits, and says so when it starts).

AppContainers work by ACEs, so the first command in a workspace writes one on that folder and its tree, and on the toolchain
folders, and these stay. To take them away: `icacls <folder> /remove <the profile's SID> /t`. The app's own file tools run
outside the container and pass through the approvals instead.

## Straydog

The app shares no code with Straydog, which moves on its own. It starts `straydog.exe`, runs `straydog setup`, reads the bundles
from `fetch --help`, and speaks HTTP: all of that is in `crates/dogmind-app/src/engine.rs`, so that following Straydog is mostly
reading.
