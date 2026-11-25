<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-wordmark-dark.png">
    <img src="assets/logo-wordmark.png" alt="Dogmind" width="240">
  </picture>
</p>

<p align="center"><a href="README.ja.md">日本語</a></p>

Dogmind is an AI agent on your own Windows PC. It reads and changes the files of a folder you choose and runs commands there,
asking you first.

- **A model on this PC**: its engine, [Straydog](https://github.com/rewroy/Straydog), runs the model on this PC's GPU.
- **Or another**: a model on another machine, an OpenAI-compatible service, Codex or Claude.
- **Light**: a native Win32 app that uses no GPU memory to draw, so the VRAM stays with the model.

## Install

Windows 11, no administrator rights needed. Run `DogmindSetup-VERSION.exe` from [Releases](https://github.com/rewroy/dogmind-app/releases).

To run a model on this PC you need an NVIDIA GPU with a recent driver and about 76 GB free on the drive. Without one, the other
connections still work.

The app is not signed yet. If Windows says *Windows protected your PC*, choose *More info > Run anyway*; with Smart App Control on, Windows will not run it.

## Getting started

The first start asks how to reach a model and which folder to work in (`/setup` asks again).

- Enter sends, Shift+Enter starts a new line, Esc stops
- `/` for commands, `@` for a file of the workspace
- Ctrl+N for a new session, Ctrl+, for the settings

## Connections

Choose one in *Settings > Models and connections*.

| Kind | What it is | Key |
| --- | --- | --- |
| Straydog on this PC | `straydog.exe` beside the app | none |
| OpenAI-compatible | Straydog on another machine, or any service with such an API | pasted, or `env:NAME` |
| Codex | A ChatGPT plan through the official Codex CLI (`codex login` first) | the CLI's |
| Claude | The Messages API | pasted, or `env:ANTHROPIC_API_KEY` |

Keys are kept in Windows' Credential Manager, never in the settings file.

## Safety

- Commands run in a sandbox (an AppContainer): they can write only to the workspace, and reach the network only when you allow it.
- Edits and commands ask first, as the mode says. Once web content is in a conversation, what it may have asked for is asked every time.
- Conversations can be encrypted in *Settings > Privacy* (off by default).

## Privacy

Dogmind sends no conversation or file to its developers, and it has no telemetry. It reaches the network only for these:

- **The connection you chose.** With Straydog on this PC, the conversation stays on this PC. With another machine, an
  OpenAI-compatible service, Codex or Claude, the conversation goes there (under that service's terms).
- **Web search and reading (on by default).** Search words go to DuckDuckGo (then Bing, or Brave with a Brave key), and a page read
  comes from its site. Turn it off with *Web search and reading* in the settings.
- **Update checks (on by default).** Every six hours the app asks GitHub for a new version, sending only its own version; updates and
  the engine are downloaded from GitHub too. *Update automatically* in *Settings > About* stops them.
- **Getting a model for this PC.** The engine downloads it from Hugging Face.
- **Page checks.** The libraries a checked page uses are fetched from well-known CDNs.
- **MCP servers you add, and commands you allow the network.**

Problem reports are not sent by themselves either.

## Updating and removing

When a new version is out, the app tells you and updates with one button. To remove it, use *Settings > General > Remove this app
from this PC*.

## When something goes wrong

`/report` makes a problem report: attach it to an [issue](https://github.com/rewroy/dogmind-app/issues) with what you were doing.
Nothing is sent by itself. Report a security problem privately, as [SECURITY.md](SECURITY.md) says.

## More

The details of using it, encrypting conversations, keeping the engine's state, how updates work, building, and the agent's tools and
sandbox are in the [guide](docs/guide.md).

## License

MIT (`LICENSE`). The licenses of what ships with it are in `LICENSE.txt` beside the app.
