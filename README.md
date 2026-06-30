# snippet

`snippet` is a Rust coding agent with a durable terminal UI and an optional
headless mode you can drive remotely. The harness loop is fully decoupled from
the UI — the same engine powers the on-device TUI and the `serve` daemon, so a
session started in either is the same on-disk session.

- **Durable runtime** — loop state is persisted (compressed msgpack) with
  checkpoints; resume any conversation where you left off.
- **Multiple model providers** — `openai-compatible`, `openai`, `anthropic`,
  `gemini`, `openrouter`, and `chatgpt` (drive it with a ChatGPT subscription
  via OAuth, set up from the TUI).
- **Provider profiles** — configure several providers at once and switch the
  active one; no editing files to swap models.
- **Real tool surface** — full read/write/edit access to the workspace plus a
  shell, structural code mapping, and parallel sub-agents.
- **Remote control** — run headless on a box and drive it from elsewhere (e.g. a
  phone) over an authenticated tunnel.
- **Prompts as files** — every system-prompt layer lives in `prompts/` and is
  embedded at compile time; none are inlined in Rust.

## Build

```sh
cargo build --release      # binary at target/release/snippet
cargo run                  # build + launch the TUI
```

## Configure

Configuration lives at `~/.snippet/config.toml` (override with `--config`). The
fastest way to set it up is the TUI itself — it has an interactive profile/model
form (including the ChatGPT subscription OAuth flow) and writes the file for you.

A minimal manual config:

```toml
workspace = "."
manual_approval = false        # prompt before bash / edits when true
theme = "midnight"             # midnight | light | high-contrast | ember

[model]
provider = "openai-compatible" # or openai | anthropic | gemini | openrouter | chatgpt
base_url = "https://api.openai.com/v1"
model = "gpt-4.1"
api_key = "replace-with-api-key"
max_retries = 4
initial_retry_ms = 750
max_retry_ms = 8000
```

To keep several providers configured at once, define `[setups.<name>]` profiles
and point `active_setup` at one (the TUI manages this for you):

```toml
active_setup = "anthropic"

[setups.anthropic]
provider = "anthropic"
model = "claude-opus-4-8"
api_key = "..."

[setups.local]
provider = "openai-compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder"
```

Secrets are written with `0600` permissions and the API key is never exposed by
the remote endpoints.

## Run (TUI)

```sh
cargo run                              # default config
cargo run -- --config path/to.toml     # explicit config
cargo run -- --resume <conversation-id>
```

In the TUI:

- **Up/Down** at the prompt edges walk your input history.
- **Mouse scroll** moves the transcript; hold **Shift** to select text.
- **Ctrl-R** resumes a previous conversation; `resume_on_start = true` in the
  config does it automatically.
- **`/mode`** toggles manual approval (confirm each bash command / edit inline).
- Long histories auto-compact so a session can run indefinitely.

## Remote control (`serve`)

`serve` runs the agent headless and exposes it over an authenticated
[cloudflared](https://github.com/cloudflare/cloudflared) tunnel so you can drive
it from another device (the on-device TUI is unaffected — `serve` is purely
additive and remote-only).

```sh
snippet serve              # fetch cloudflared if needed, tunnel, fork to background
snippet serve --status     # reprint the QR / connection string
snippet serve --stop       # stop the daemon (tears the tunnel down cleanly)
```

On start it prints a **QR code + connection string** (`{url, token}`) to scan or
paste from a client. cloudflared is downloaded once (with a progress bar) and
cached under `~/.snippet/bin/`. Every endpoint is gated by a bearer token using a
constant-time comparison:

- `GET /health` — liveness.
- `GET /sessions` — list sessions on the device (with a running flag).
- `POST /sessions` — open/resume a session in a chosen folder.
- `GET /fs` — browse the filesystem to pick a folder.
- `WS /attach` — stream session state and send input/approvals/stop.

Flags: `--port` (default 8787), `--token` (generated if omitted), `--no-tunnel`
(bind localhost only, for testing), `--public-url` (bring your own tunnel),
`--tunnel-token` (run a named cloudflared tunnel). Linux and macOS.

## Tools

The agent has: `read_file`, `read_image`, `write_file`, `append_file`,
`edit_file`, `replace_file_content`, `list_files`, `search_files`,
`search_content`, `view_outline`, `code_map`, and `bash`. `web_search` /
`web_read` are added when an Exa API key is configured, and the conversation
agent can `delegate_task` to run scoped work in parallel sub-agents that share
the workspace. Tool calls are accepted both natively and as inline fenced blocks:

```tool:bash
{"command":"cargo test"}
```

## Prompts

Every system-prompt layer is a `.md` file in `prompts/`, embedded at compile time:

- `prompts/operating_style.md`
- `prompts/sandbox_environment.md`
- `prompts/artifact_discipline.md`
- `prompts/coding_agent_layer.md`
- `prompts/conversation_agent_layer.md`

`coding_system_prompt()` joins the first four; `conversation_system_prompt()`
appends the conversation layer for the user-facing thread.

## State

Sessions persist under `~/.snippet/workspaces/<workspace>/` (loop state +
conversation history + checkpoints), which is what lets both the TUI and `serve`
list, resume, and attach to the same sessions.

## Mobile / desktop client

[`snippet-mobile`](https://github.com/wacht-platform/snippet-mobile) is a Flutter
client (Android + macOS) that drives a `serve` daemon over the tunnel — browse
files, view diffs, run commands, and chat with the agent from another device.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). `cargo build`
should pass before opening a PR.

## License

Copyright (C) 2026 snipextt.

This program is free software: licensed under the **GNU Affero General Public
License v3.0 or later** (AGPL-3.0-or-later). It comes with NO WARRANTY. Because
`serve` can run as a network service, the AGPL's network-use clause applies: if
you run a modified version for others over a network, you must offer them its
source. See [LICENSE](LICENSE) for the full text.
