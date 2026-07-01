<div align="center">

# snippet

**An open-source AI coding agent that lives in your terminal — and that you can drive from your phone.**

One Rust binary: a durable terminal UI *and* a headless daemon you can remote-control over an authenticated tunnel. Bring your own model — Claude, GPT, Gemini, DeepSeek, a local model, or a ChatGPT subscription.

[Quickstart](#quickstart) · [Remote control](#remote-control-from-anywhere) · [Configure](#configure-your-models) · [How it works](#how-it-works) · [Mobile app](https://github.com/wacht-platform/snippet-native)

_Built by the team behind [Wacht](https://wacht.dev) — open-source infrastructure for AI-native apps._

</div>

---

`snippet` is a coding agent you actually own. It runs on your machine with full read/write/shell access to your project, keeps every session durable on disk, and — because the harness loop is fully decoupled from the UI — the **same engine** powers the on-device TUI and the `serve` daemon. Start a task at your desk, then check in, steer it, or kick off new work from your phone on the same session.

No cloud middleman, no lock-in, no subscription of ours. Your keys, your machine, your code.

## Features

- **Terminal-native** — a fast, durable TUI. Loop state is persisted (compressed msgpack) with automatic **checkpoints**, so you can resume any conversation exactly where you left off and rewind a bad turn.
- **Drive it from your phone or Mac** — run `snippet serve` on your dev box and control it from the [mobile/desktop app](https://github.com/wacht-platform/snippet-native) over an authenticated [cloudflared](https://github.com/cloudflare/cloudflared) tunnel: chat, browse & edit files, view git diffs, run commands, manage checkpoints.
- **Any model** — `openai-compatible`, `openai`, `anthropic`, `gemini`, `openrouter`, or a **ChatGPT subscription** (OAuth). Configure several providers as **profiles** and switch the active one — even **per conversation**.
- **Real tool surface** — read/write/edit, shell, recursive regex search, structural code mapping, image reading, and web search (with an Exa key).
- **Parallel sub-agents** — the agent orchestrates scoped work across background **lanes** that share the workspace and report back with exact `file:line` references, keeping its own context lean.
- **Agent Skills** — drop a `SKILL.md` folder in `~/.snippet/skills/`; the agent discovers and loads it on demand (the open [Agent Skills](https://agentskills.io) standard).
- **Background processes** — start dev servers / watchers detached; the agent tracks, tails, and kills them.
- **Prompts as files** — every system-prompt layer lives in `prompts/` and is embedded at compile time. Tune the agent by editing Markdown, not Rust.

## Quickstart

Requires [Rust](https://rustup.rs).

```sh
git clone https://github.com/wacht-platform/snippet-service
cd snippet-service
cargo run                  # builds + launches the TUI
```

First run drops you into an interactive model setup (including the ChatGPT-subscription OAuth flow) and writes your config for you. Then just describe a task.

```sh
cargo build --release      # optimized binary at target/release/snippet
```

In the TUI: **Up/Down** walk input history · **Ctrl-R** resumes a conversation · **`/model`** switches models · **`/mode`** toggles manual approval (confirm each edit/command) · long histories auto-compact so a session runs indefinitely.

## Remote control from anywhere

`serve` runs the agent headless and exposes it over an authenticated tunnel — purely additive, the on-device TUI is unaffected.

```sh
snippet serve              # fetches cloudflared if needed, opens a tunnel, forks to background
snippet serve --status     # reprint the QR / connection string
snippet serve --stop       # stop cleanly (tears the tunnel down)
```

It prints a **QR code + connection string** (`{url, token}`) — scan or paste it into the [**snippet mobile & desktop app**](https://github.com/wacht-platform/snippet-native) (Android + macOS) and you're driving your machine's agent from your pocket. Every endpoint is gated by a bearer token (constant-time comparison); secrets are stored `0600` and never returned by the API.

Flags: `--port` (default 8787) · `--token` (auto-generated) · `--no-tunnel` (localhost only) · `--public-url` (bring your own tunnel). Linux and macOS.

## Configure your models

Config lives at `~/.snippet/config.toml` (the TUI manages it for you). Keep several providers configured and switch freely:

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

## How it works

- **One engine, two front-ends.** The harness loop is UI-agnostic; the TUI and `serve` are thin shells over it, so a session started in either is the same on-disk session under `~/.snippet/workspaces/`.
- **Durable + resumable.** Every session persists loop state, history, and checkpoints — resume, rewind, or attach from another device.
- **Layered prompts.** `prompts/{operating_style,sandbox_environment,artifact_discipline,coding_agent_layer,conversation_agent_layer}.md` are composed into the system prompt; per-turn steering is injected fresh each turn (kept out of the cached prefix for token efficiency).
- **Tools:** `read_file`, `read_image`, `write_file`, `append_file`, `edit_file`, `replace_file_content`, `list_files`, `search_files`, `search_content` (regex), `view_outline`, `code_map`, `bash` (+ `background`), `search_skills`/`skill`, and `delegate_task` for parallel lanes.

## From the team behind Wacht

snippet is an open-source project from **[Wacht](https://wacht.dev)** — open-source infrastructure for AI-native apps: identity, organizations, machine auth, webhooks, notifications, and an agent runtime, built as one product on one model instead of six vendors stitched together.

If you're building AI-native apps, that's where to look next → **[wacht.dev](https://wacht.dev)**.

## Contributing

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). `cargo build` must pass before a PR.

## License

Copyright (C) 2026 snipextt. Licensed under the **GNU Affero General Public License v3.0 or later** (AGPL-3.0-or-later) — see [LICENSE](LICENSE). Because `serve` can run as a network service, the AGPL's network-use clause applies: if you run a modified version for others over a network, you must offer them its source.
