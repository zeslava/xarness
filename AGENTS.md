# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`xarness` — a terminal (ratatui) coding agent that talks to OpenAI-compatible chat-completions endpoints (e.g. local llama.cpp).

The working directory at launch becomes the sandbox root for all tools.

## Config

Three layers, each the first existing of its candidates, merged in order (later overrides earlier key by key; objects merge recursively, arrays such as `models` are replaced whole):

1. global: `~/.config/xarness/config.{toml,json}` on every OS; if no layer exists at all, a `config.json` with empty `providers` is written here (best effort) and the TUI starts with no model, prompting to configure a provider
2. project: `./.xarness/config.{toml,json}`
3. local: `./.xarness/config.local.{toml,json}` — gitignored, for keys and personal endpoints

The schema is defined in `src/config.rs`.

- Models are addressed as `provider/model`; split on the **first** `/` only, since model ids may contain `/`.
- `context_window` enables history trimming; without it, the provider is asked (llama.cpp `/props` → `n_ctx`, fetched each turn), and if that fails full history is sent.
- `auto_compact` (e.g. `0.8`, absent = off): after a turn whose server-reported usage reaches that fraction of the context window, history is compacted like `/compact`; queued messages are sent after it. A model's own `auto_compact` overrides the top-level one.
- `think.levels` maps a level name to raw JSON merged into the request body (`ChatRequest.extra`) — reasoning control is provider-specific, so it is pure config, not code. `think` may instead be a list of `reasoning_effort` values (`["low", "high"]`; also accepted as `levels` next to a `default`) or a preset name (`qwen`, see `think_preset`), and may be set on the provider, where models without their own inherit it.
- `mcp.<server>` configures an MCP server: `command`/`args`/`env` (stdio) or `url`/`headers`/`headers_env` (streamable HTTP), plus `enabled`, `approval` (`auto` | `always` | `never`) and an optional `tools` allowlist.
- `agents.<name>` holds a subagent's runtime settings: `enabled`, `model` (`provider/model`; absent = the caller's model), `think` (a level of that model) and `extra` (raw JSON merged over the level). The definitions themselves are files, see `subagents.rs`. `agents_parallel` (default `true`) runs consecutive `agent` calls in parallel; set it to `false` for a single-slot server.
- `skills.compat` (default `["agents"]`) adds other agents' skill dirs `.<name>/skills`; `skills.disabled` lists skill names to skip.

## Architecture

Hexagonal layout:

- `domain/` — `Message`, `Mode` (Plan/Build), and ports in `domain/ports.rs`: `LlmProvider` (returns a `BoxStream` of `LlmEvent`) and `Tool`. Domain must not import from `adapters/`.
- `adapters/` — `LlmProvider` implementations; `build_provider` dispatches on `ProviderKind`. `openai.rs` does SSE parsing, accumulates streamed tool-call deltas, and maps server "context exceeded" errors to the `ContextOverflow` error type. `mcp.rs` is the MCP client (`rmcp`): exposes each server tool as a `Tool` named `server__tool`; `mutating` = not annotated `readOnlyHint`, and with `approval = auto` mutating tools need approval.
- `tools/` — `Tool` impls registered in `ToolRegistry::new`. MCP tools are added later via `add_mcp`: servers connect in the background at startup (`tui::run`), so the tool list changes at most once per session. Tools with `mutating() == true` are hidden from the model and rejected at run time in Plan mode. All paths go through `tools::resolve`, which is lexical (no canonicalize, symlinks not followed), expands a leading `~` and rejects anything outside root; read-only tools (`read_file`, `grep`, `list_dir`, `read_symbol`) use `resolve_read`, which also allows the home directory; such a call outside root waits for approval (`ToolRegistry::needs_approval` checks the `path` argument).
- `agent.rs` — the agent loop, run as a spawned tokio task per user turn. Streams a request, executes tool calls, loops until the model returns no tool calls. Emits `AgentEvent`s tagged with a `run_id` over an unbounded channel.
- `tui/` — `mod.rs` event loop `select!`s over terminal events, agent events and a spinner ticker (only while running). `app.rs` owns state/history and handles keys and slash commands (`commands.rs`); `ui.rs` renders.

### Non-obvious invariants

- **History is committed atomically**: the agent sends `AgentEvent::Commit` with the assistant message plus all its tool results together, so an interrupted run (Esc aborts the task) never leaves dangling tool calls. The TUI ignores events whose `run_id` is stale.
- **Context fitting** (`agent.rs`): token counts are estimated at ~3 chars/token, then corrected by a `scale` factor from server-reported `prompt_tokens`. `fit_context` drops whole turns from the front (a turn starts at a `User` message), then truncates oldest tool outputs. On `ContextOverflow` with no text yet streamed, it recalibrates and retries the turn once.
- **Background jobs report back** (`tools/jobs.rs`): `run` with `background` starts a job whose watcher sends its report to the TUI when it exits (skipped if the model already saw the exit via `job`). The report becomes a `User` message marked as an automatic notice: it starts a turn when idle, otherwise it is sent with the next turn after `Finished`. It is never injected mid-turn.
- **Sessions** (`session.rs`): history is saved as JSONL in `./.xarness/sessions/<millis>.jsonl` (the dir gets a `*` `.gitignore`). `App::persist` appends whatever history gained since the last save — after the user message is pushed and on each `Commit` — and rewrites the file on `Compacted`; `/clear` starts a new file, created only on first save. `-c`/`--continue` loads the newest session, `/resume [n]` lists or loads one; the log is rebuilt from history. `reasoning` is not persisted. A user message whose log text differs from its content (paste placeholders, `/skill args`) is preceded by a `{"shown": …}` line, restored into the log on resume. A failed write disables saving until `/clear` or `/resume`.
- The system prompt is rebuilt each turn from the current `Mode`. `AGENTS.md` from the working directory (only there, no parent/subdir lookup) is read once in `App::new` and appended to it.
- `repomap.rs` builds a repo map (files + tree-sitter tag definitions, Rust/Go/JS/TS) appended to the system prompt. It is built only in `App::new`, on `/map` and when compaction starts (the next request misses the cache anyway), never per turn, so the prompt prefix stays byte-identical for the server's KV cache. Definitions are listed without line numbers, which go stale with every edit. Over budget it degrades by `Depth` (trait-impl methods → all members → paths only → truncated paths).
- **Subagents** (`subagents.rs`, `agent.rs`): definitions are `./.xarness/agents/<name>.md`, then `~/.config/xarness/agents/<name>.md`, then the built-in read-only `explore`; the first of a name wins. Frontmatter `name`, `description`, `tools` (allowlist; absent = all), body = system prompt. The main agent gets an `agent` tool (`{agent, task}`); its spec lists the agents and is static per session. A call runs `drive` recursively with a fresh history `[User(task)]`, inside the parent's task (Esc aborts both), and returns only the final text as the tool result, so the parent's atomic `Commit` holds. The subagent's sink forwards only tool start/result, approvals and metrics, tagged with its label; its text, `Commit` and `Usage` are dropped. With `agents_parallel`, consecutive `agent` calls in one response run in parallel (`call_groups`), other calls keep their order; approvals queue in the TUI. Subagents cannot start agents or background jobs (the report would reach the main agent after they finish) and after `MAX_SUBAGENT_STEPS` get one last request without tools that asks for a report of what they found. With llama.cpp `-np 1` a subagent's prompt evicts the parent's KV cache; `-np 2+` (or `extra.id_slot`) avoids it.
- `skills.rs` loads Agent Skills (`<dir>/<name>/SKILL.md`, YAML frontmatter parsed by a minimal subset parser) once at startup from `./.xarness/skills`, project compat dirs, `~/.config/xarness/skills`, then home compat dirs; the first skill of a name wins. Only the catalog (name + description) goes into the system prompt; the model loads a body with the `skill` tool, the user with `/<name> [args]`. `skill` reads extra files with the skill dir as the `resolve` root, so global skills don't widen the sandbox. `skill` outputs are never truncated by context fitting within their turn; once the turn is finished they are elided like other tool outputs.
