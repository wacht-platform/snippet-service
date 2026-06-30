# Contributing to snippet

Thanks for your interest! Issues and pull requests are welcome.

## Getting started
- Build/run and configuration: see [README.md](README.md).
- `cargo build` compiles the single binary (TUI + `serve`); `cargo run` launches the TUI.
- Run `cargo fmt` and `cargo clippy` if you have them; `cargo build` must pass before a PR.

## Style
- Keep code comments lean and sparse; match the surrounding style.
- Verify a third-party API against its live docs before coding against it.
- Never log or expose the user's API key or the serve token.
- No AI/tool branding in commit messages.

## Licensing
By contributing, you agree that your contributions are licensed under the
project's license, **AGPL-3.0-or-later** (see [LICENSE](LICENSE)).
