<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-wordmark-dark.png">
    <img src="assets/logo-wordmark.png" alt="Dogmind" width="240">
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
For Qwen3.8, *Faster-writing data* fetches its drafter (about 5 GB to download, 0.9 GB kept), whatever the graphics memory; turned on for a model that is set up already, it fetches the drafter alone while the engine keeps running, and the engine starts again with it once no work is going on.
The first run asks how to reach a model and where to work; `/setup` goes through it again. Type `/` for the commands and `@` for the workspace's files. Enter sends, Shift+Enter starts a new line, Esc stops; Ctrl+N
starts a session, Ctrl+, opens the settings, Ctrl+Shift+C copies the last answer. A right click on a session renames or
deletes it.

While the model works, Enter puts what you type into the work before its next step, and Ctrl+Enter keeps it for after the work. Each
waiting message has buttons to send it into the work now, to edit it, or to discard it. Each message shows its time; an answer's
branch button starts a new session with the conversation up to that answer, to go on from there another way. From the second turn,
a rail at the transcript's right edge has a mark for each of your messages: the one you are reading is longer, and a click goes to it.

Some steps come as cards in the conversation. When the agent asks you something, a card holds the question with a button for
each choice (its suggestion marked) and a field for your own answer, and the session shows *Waiting* until you answer. In plan
mode (`/plan`) it puts its plan up as a card with *Carry it out* and *Change...*: nothing is changed until you carry it out. Under
an answer that changed files, a card lists them with the lines added and removed: a click opens that file's diff, and its other
button opens the file or shows it in Explorer. The cards stay when a session is opened again. `/goal TEXT` keeps a goal for the
session, shown as a thin bar under the title and told to the model again after a compaction, and `/goal clear` ends it.
`/trajectory` shows where a session's time went, turn by turn. A request that plainly asks for no changes ("just look", "don't
change anything") makes that turn read-only, and a line says so before anything is refused; a phrase with an object ("without
changing the tests") only limits that object. Keys and tokens that a tool brings back are hidden on screen; the model still gets
them as they were.

The panel on the right (the button at the right of the title, or `/panel`) has *Overview* and *Files*. *Files* shows the workspace
as a tree, folders first, with the files this session changed marked, and follows the disk as it changes. A click previews a file
beside the conversation (Markdown, HTML, pictures, code and text, CSV and TSV as tables, an Excel workbook's first sheet), a double
click opens it in its app, and a right click opens it, shows it in Explorer, copies its path or mentions it in your message
(`@path`).

## Updating, removing, and when something goes wrong

New versions are published at [Releases](https://github.com/rewroy/dogmind-app/releases). The app asks GitHub for the newest release
(a pre-release too) every six hours, and again when its window comes back from the tray if the last look is an hour old; *Settings > About* has *Check for updates*, and a switch (*Update automatically*) to turn these looks off. The request carries
nothing about you but the app's version. A copy that the installer put in its folder also downloads the newer release's setup in the background (not on a metered connection, unless you press
*Check for updates*), and only if GitHub lists it with its size and SHA-256: the download is kept as `.partial` in the `updates` folder of the app's data until its size and hash are those GitHub published, and deleted if they are not, or if you cancel it
(*Settings > About* shows its progress and a *Cancel*). Then a card at the top of the main screen, whatever page is open, says the update is ready, with *Update now (Dogmind restarts)* and *Later* (while the window is hidden, the tray says so once for each new version): only that press, when no work is going on or when it ends, runs the setup
(`DogmindSetup-VERSION.exe --update --dir FOLDER --wait-pid PID`, which asks nothing, waits for the app to end, updates the folder, puts back what was there if it fails, and starts the app again). A copy unpacked from the zip
is only told, with a button that opens the release's page. To update by hand, run the new `DogmindSetup-VERSION.exe`: it asks a running copy to close (one that is working stays open and says so; nothing is ended by force), checks each file by its hash, replaces only what changed and takes away only what the last version put there. Every file takes its place or none does: a failure puts back what was there, so the program is as it was, and running it again goes on from there. From a zip: quit from the tray (closing the window leaves the app running), and extract the new zip over the old folder. Settings and sessions are in `%LOCALAPPDATA%\DogmindApp` and stay.

The setup that is downloaded is not always all of it. A release also has `DogmindSetup-VERSION.parts` beside its setup, which says where each file is in it, and the installer writes down, in the manifest of the install, the hash, size and last-write time of each file it put there (an install made before 0.1.5 has none, and its first update downloads the whole setup, and reads every file of the folder once). When the release lists the `.parts` file with its size and SHA-256 as well and the manifest has those records, the app downloads only the program and the files that are new or have changed, by HTTP ranges, and puts a setup together in the `updates` folder that leaves the other files to the install. The `.parts` file is checked by the size and SHA-256 GitHub published for it, and each part by what it says: the program by its hash, each file by inflating it and comparing its size and SHA-256. The installer finds the files left out in the folder, by their records, or by their hashes where a file's time is not the one written down, and if one is not as it was it changes nothing and says to download the full setup from the release page. Whatever is not as it should be on the way, the whole setup is downloaded as above, and nothing more is said of it. The installer trusts those records only on the app's own update (`--update`): a file that still has the size and time written down for it is not read again. Running the setup yourself checks every file, and is the way to repair an install.

To remove it, *Settings > General > Remove this app from this PC* takes away the settings, sessions, logs, its keys in Credential Manager, what was opened to the sandbox and its AppContainer
profile, and the model unless you keep it. In its data folder it takes only what it made there, by name: anything else stays, and it says what. Straydog's own sandbox profile stays, as the engine also runs as a server of its own. Installed apps opens the same page, and closing it then takes away the program and its entries: only the files its installer put there, and a folder only once it is empty. From a zip, delete the folder it runs from. Workspaces stay as they are.

A crash leaves a text file in `%LOCALAPPDATA%\DogmindApp\crashes`, and the next start says so. `/report`, or *About > Write a problem report*, writes `report.txt` in `%LOCALAPPDATA%\DogmindApp`, with no key, server address or
conversation in it, and says where it is. It does hold the last line an engine or a server said of a failure (the line shown under the app's own sentence), with any key or header in it hidden. Read it, and attach it to an [issue](https://github.com/rewroy/dogmind-app/issues) with what you were doing. Nothing is sent by itself. Whoever makes a release keeps its `dogmind_app.pdb` and `straydog.pdb`: the addresses in a crash are read with them.
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

Work goes on the next version's branch, `dev/X.Y.Z`, and `main` holds released versions only. To release: raise `version` in the workspace's `Cargo.toml` on `dev/X.Y.Z`, build and package as above, merge `dev/X.Y.Z` into `main` through a pull request with a merge commit (never a squash), and publish a GitHub pre-release tagged
`vVERSION` (as `v0.1.0`) on that merge commit with `dist\DogmindSetup-VERSION.exe`, `dist\DogmindSetup-VERSION.parts` (an app fetches an update by its parts only with that; without it, the whole setup) and `dist\DogmindApp-VERSION.zip`, keeping the PDBs. The app's
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

It can ask you a question with choices (`ask_user`), and in plan mode it puts its plan up for your approval (`submit_plan`) and
carries it out once you approve.

For a long look, `explore` hands a question to a helper with a context of its own and only the looking tools, and only its
answer joins the conversation. A file of more than 800 lines read with no place in it comes as its outline, a turn is told at
each 96 KB of files it has read, and any long tool result (a command's output, a search, an edit's report, a diff, an MCP tool's answer) is kept whole in
`%TEMP%\dogmind-output` for a week, its report saying where to read the rest; so is what a compaction trims of an old output. A command left running in the background shows, at each `command_status`, what it has printed since the
last look: a dev server's ready line among it.

`execute_command` runs in an AppContainer, `DogmindApp.Exec`: a command can write only in the workspace, read
that, the per-user toolchains on PATH and what every app may read, and has no network unless it was approved for it, each
time. What it starts ends with it. What cannot be contained does not run: there is no fallback, and a workspace that is a
whole drive or holds your user folder cannot run commands at all (it still reads and edits, and says so when it starts).

AppContainers work by ACEs, so the first command in a workspace writes one on that folder and its tree, and on the toolchain
folders, and these stay. To take them away: `icacls <folder> /remove <the profile's SID> /t`. The app's own file tools run
outside the container and pass through the approvals instead.

## Background work

A command started with `background: true` goes on while the agent does other things, or ends its turn. When it ends, any way it ends, the session wakes itself: the app writes a notice, which the
transcript shows as a row of its own and the model reads as the app's word, not yours (what the command printed in it is data, never instructions), and one turn runs with nobody sending anything.
`wake_on`, a regular expression, wakes it once, at the first line the command prints that matches (`Listening on` for a server), and the command goes on. `wake_at` wakes it later, after 60 seconds
to a day with a note, with at most five waits open; `command_stop` ends a command or cancels a wait, and that wakes nothing. A notice that comes during a turn waits for its end, and notices that come
together are one turn.

So that nothing runs away, a session wakes itself at most 30 times in a row with no word from you, and 60 times in an hour; past either it stops and the window says why (a message from you lets it
go on after the first, the hour's passing after the second). Your Stop ends the turn, and the commands and waits of that session. A woken turn asks as any turn does and waits for you: the taskbar
button flashes until you look, the tray says so when the window is hidden, and nothing approves for it. The panel on the right lists what is waiting, each with a stop of its own, and the list of
sessions marks one that has something waiting. Closing the window leaves all of it going, in the tray; Quit from the tray ends it. Commands and waits do not outlive the app, and nothing watches
files or listens for anything from outside it.

## Straydog

The app shares no code with Straydog, which moves on its own. It starts `straydog.exe`, runs `straydog setup`, reads the bundles
from `fetch --help`, and speaks HTTP: all of that is in `crates/dogmind-app/src/engine.rs`, so that following Straydog is mostly
reading.
