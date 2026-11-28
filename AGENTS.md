# xarness

Rust terminal coding agent using ratatui and OpenAI-compatible APIs.
The launch directory is the tools' root.

## Architecture

- `src/domain/`: messages, access and ports; must not import adapters.
- `src/adapters/`: LLM providers and MCP client.
- `src/tools/`: built-in tools, path resolution and approval checks.
- `src/agent.rs`: model/tool loop; emits events to the TUI.
- `src/tui/`: application state, event loop and rendering.

## Preserve these invariants

- Commit assistant tool calls and their results atomically. Ignore stale
  `run_id` events; background agents use a separate event channel.
- Keep the system prompt and tool schemas stable for KV caching, except
  initial MCP discovery. Rebuild the main repomap only at startup, `/map`
  and compaction; subagent directory maps are cached for the session.
- Trim whole turns before truncating tool outputs. Preserve skill
  instructions within their active turn. Cut command outputs must name
  their full spill file; cut file reads must give a continuation offset.
- Deliver background job/subagent reports between turns, never during an
  active turn. Esc cancels foreground work; background work survives it
  but is stopped by `/clear` and `/resume`.
- Preserve `Full`/`Plan`/`ReadOnly` access and per-tool approval checks.
  Path resolution is lexical, not canonical; outside-root path-tool
  calls require approval. `/plan` is per-turn, not a separate user mode.
- Persist committed history, not reasoning or live subagent transcripts.
- Load project `AGENTS.md` only from the working directory at startup.
  Keep only skill names/descriptions in the prompt; load bodies on demand.

## Configuration

See `README.md` and `src/config.rs` for parameters and implementation.
Layers: global (`~/.config/xarness`) -> project (`.xarness/config.*`) ->
local (`.xarness/config.local.*`). Within each layer JSON wins over TOML.
Create and edit configuration in JSON; use TOML only when explicitly requested.
Objects merge recursively; arrays, including `models`, replace whole.
Split `provider/model` identifiers on the first slash only.
Reasoning controls are provider-specific JSON configured through `think`.

## Build checks in restricted sandboxes

Use `RUSTC_WRAPPER= CARGO_CACHE_RUSTC_INFO=0` when running Cargo in a
sandbox that blocks sccache's local socket. This prevents failed compiler
probes from poisoning `target/.rustc_info.json` for normal builds and LSP.
Keep sccache enabled for normal builds outside such sandboxes.
