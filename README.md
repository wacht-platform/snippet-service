# snippet

`snippet` is a Rust coding-agent harness with a durable TUI runtime. It reuses the useful shape of the existing agent engine without pulling in platform-specific runtime pieces:

- `NativeToolDefinition`, `GeneratedToolCall`, and model output types.
- Leaked tool-call salvage, expanded into first-class inline tool submission.
- Standardized tool results and bounded inline output.
- Durable JSON loop state with resume support.
- Coding-only runtime prompt files under `prompts/runtime/`, with a local coding-agent layer added on top.
- A coding-only tool surface: `read_file`, `write_file`, `edit_file`, `list_files`, `bash`, and `complete`.

Inline tool calls are accepted as fenced blocks:

```tool:bash
{"command":"cargo test"}
```

Create `snippet.toml` in the repository root:

```toml
workspace = "."
state_path = ".snippet/state.json"
resume_on_start = false

[model]
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-4.1"
api_key = "replace-with-api-key"
max_retries = 4
initial_retry_ms = 750
max_retry_ms = 8000
```

Run the TUI:

```sh
cargo run
```

Use a non-default config path only when needed:

```sh
cargo run -- --config path/to/snippet.toml
```

State is saved to the configured `state_path`. Resume from the TUI with `Ctrl-R`, or set `resume_on_start = true` in the TOML.

Copied runtime prompts live in:

- `prompts/runtime/operating_style.md`
- `prompts/runtime/sandbox_environment.md`
- `prompts/runtime/artifact_discipline.md`

The active prompt embeds those sources and appends a `snippet_coding_agent` layer. Replay remains available as a library test model, not as the primary product interface.
