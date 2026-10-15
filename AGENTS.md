# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`xarness` — a terminal (ratatui) coding agent that talks to OpenAI-compatible chat-completions endpoints (e.g. local llama.cpp). Single binary crate, Rust edition 2024.

## Commands

```sh
cargo build
cargo run                      # launches the TUI in the current dir (needs a config, see below)
cargo test                     # unit tests
cargo test <name>              # single test by name substring
cargo clippy --all-targets
cargo fmt
```

The working directory at launch becomes the sandbox root for all tools.

## Config

Three layers, each the first existing of its candidates, merged in order (later overrides earlier key by key; objects merge recursively, arrays such as `models` are replaced whole):

1. global: `<config_dir>/xarness/config.{toml,json}` (`dirs::config_dir()`, `~/.config` on Linux)
2. project: `./.xarness/config.{toml,json}`, or the old `./.xarness.{toml,json}`
3. local: `./.xarness/config.local.{toml,json}` — gitignored, for keys and personal endpoints

See `.xarness/config.json` for an example.

- Models are addressed as `provider/model`; split on the **first** `/` only, since model ids may contain `/`.
- `context_window` enables history trimming; without it, the provider is asked (llama.cpp `/props` → `n_ctx`, fetched each turn), and if that fails full history is sent.
- `think.levels` maps a level name to raw JSON merged into the request body (`ChatRequest.extra`) — reasoning control is provider-specific, so it is pure config, not code.

## Architecture

Hexagonal layout:

- `domain/` — `Message`, `Mode` (Plan/Build), and ports in `domain/ports.rs`: `LlmProvider` (returns a `BoxStream` of `LlmEvent`) and `Tool`. Domain must not import from `adapters/`.
- `adapters/` — `LlmProvider` implementations; `build_provider` dispatches on `ProviderKind`. `openai.rs` does SSE parsing, accumulates streamed tool-call deltas, and maps server "context exceeded" errors to the `ContextOverflow` error type.
- `tools/` — `Tool` impls registered in `ToolRegistry::new`. Tools with `mutating() == true` are hidden from the model and rejected at run time in Plan mode. All paths go through `tools::resolve`, which is lexical (no canonicalize, symlinks not followed) and rejects anything outside root.
- `agent.rs` — the agent loop, run as a spawned tokio task per user turn. Streams a request, executes tool calls, loops until the model returns no tool calls. Emits `AgentEvent`s tagged with a `run_id` over an unbounded channel.
- `tui/` — `mod.rs` event loop `select!`s over terminal events, agent events and a spinner ticker (only while running). `app.rs` owns state/history and handles keys and slash commands (`commands.rs`); `ui.rs` renders.

### Non-obvious invariants

- **History is committed atomically**: the agent sends `AgentEvent::Commit` with the assistant message plus all its tool results together, so an interrupted run (Esc aborts the task) never leaves dangling tool calls. The TUI ignores events whose `run_id` is stale.
- **Context fitting** (`agent.rs`): token counts are estimated at ~3 chars/token, then corrected by a `scale` factor from server-reported `prompt_tokens`. `fit_context` drops whole turns from the front (a turn starts at a `User` message), then truncates oldest tool outputs. On `ContextOverflow` with no text yet streamed, it recalibrates and retries the turn once.
- The system prompt is rebuilt each turn from the current `Mode`. `AGENTS.md` from the working directory (only there, no parent/subdir lookup) is read once in `App::new` and appended to it.
- `repomap.rs` builds a repo map (files + tree-sitter tag definitions, Rust/Go/JS/TS) appended to the system prompt. It is built only in `App::new` and on `/map`, never per turn, so the prompt prefix stays byte-identical for the server's KV cache. Over budget it degrades by `Depth` (trait-impl methods → all members → paths only → truncated paths).
