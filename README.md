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

It is not signed yet: at a warning, choose *More info > Run anyway* (with Smart App Control on, it will not run).

## Getting started

The first start asks how to reach a model and which folder to work in (`/setup` asks again).

- Enter sends, Shift+Enter starts a new line, Esc stops
- `/` for commands, `@` for a file of the workspace
- Ctrl+N for a new session, Ctrl+, for the settings

## Connections

Choose one in *Settings > Models and connections*.

| Kind | What it is | Key |
| --- | --- | --- |
| Straydog on this PC | The newest Straydog release, fetched by the app when this is chosen (about 0.6 GB), and a model | none |
| OpenAI-compatible | Straydog on another machine, or any service with such an API | pasted, or `env:NAME` |
| Codex | A ChatGPT plan through the official Codex CLI (`codex login` first) | the CLI's |
| Claude | The Messages API | pasted, or `env:ANTHROPIC_API_KEY` |

Keys are kept in Windows' Credential Manager, never in the settings file.

## Safety

- Commands run in a sandbox (an AppContainer): they write only to the workspace and reach the network only when you allow it.
- Edits and commands ask before they run.
- Conversations can be encrypted in *Settings > Privacy* (off by default).

## Privacy

Nothing is sent to the developers and there is no telemetry. The network is reached only for these ([details](docs/guide.md#privacy)):

- the connection you chose (Straydog on this PC stays on this PC)
- web search and reading (on by default)
- update checks (on by default; only the version is sent)
- getting a model, a checked page's libraries and the bundled guidance
- MCP servers you add and commands you allow

## Updating and removing

When a new version is out, the app tells you and updates with one button. To remove it, use *Settings > General > Remove this app
from this PC*.

## When something goes wrong

`/report` makes a report to attach to an [issue](https://github.com/rewroy/dogmind-app/issues). Security problems: [SECURITY.md](SECURITY.md).

## More

The details are in the [guide](docs/guide.md).

## License

MIT (`LICENSE`).
