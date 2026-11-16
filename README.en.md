# xarness

[Русский](README.md) | [English](README.en.md)

xarness is a simple harness with the essentials for working with code.

The goal is to get the most out of a model with as few tokens as possible.

Features:

- OpenAI-compatible Chat Completions API: use a local server such as llama.cpp or a remote API.
- File reading and search, symbol navigation, editing, and shell commands, including background jobs.
- A repository map and instructions from `AGENTS.md` in the model's context.
- Saved conversations, session resumption, and history compaction as the context fills up.
- Switching models and configured reasoning levels during a conversation.
- MCP servers, skills, and custom subagents.

## Installation

```bash
curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/zeslava/xarness/main/install.sh | sh
```

The script detects the OS and architecture (Linux x86_64/aarch64, macOS arm64), downloads the latest release, verifies its checksum if `shasum` or `sha256sum` is available, and installs the binary into `~/.local/bin`. Make sure this directory is in your `PATH`. To choose another location, set `INSTALL_DIR`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/zeslava/xarness/main/install.sh \
  | INSTALL_DIR=/usr/local/bin sh
```

Building from source requires Rust with edition 2024 support:

```bash
cargo install --path .
```

## Quick start

1. Start an OpenAI-compatible server with a model that supports tool calling. This example assumes the server is already available at `http://127.0.0.1:8080/v1`.
2. Create `~/.config/xarness/config.json`:

   ```json
   {
     "default": "local/my-model",
     "providers": {
       "local": {
         "base_url": "http://127.0.0.1:8080/v1",
         "models": [
           { "id": "my-model" }
         ]
       }
     }
   }
   ```

   `default` uses the format `{provider}/{model_id}`: `local` is the provider name (a key in `providers`), and `my-model` is an `id` in that provider's `models` list. Replace `my-model` in both places with the model identifier your server accepts. A local server without authentication does not need an API key.

3. Change to your project directory and start the agent:

   ```bash
   cd /path/to/project
   xarness
   ```

4. Enter a task, for example: `Find out why the test fails and fix it`. To explore the task first, use `/plan <task>`: the agent examines the code and proposes a plan; changes and command execution require approval during that turn.

When started without a configuration, xarness tries to create `~/.config/xarness/config.json` with an empty `providers` object and opens the interface with a setup hint. Add a provider and models, then restart the application.

The directory you launch from becomes the working root: built-in file writing tools are restricted to it, and reading files outside it requires approval. Shell commands run from this directory and also require approval; this is not OS-level process isolation. Ordinary file edits inside the project are applied without a separate prompt.

## Configuration

Both JSON and TOML are supported; all examples below use JSON. Settings are read at startup and merged in this order:

| Layer | JSON | TOML |
| --- | --- | --- |
| Global, on every OS | `~/.config/xarness/config.json` | `~/.config/xarness/config.toml` |
| Project settings | `./.xarness/config.json` | `./.xarness/config.toml` |
| Local project settings | `./.xarness/config.local.json` | `./.xarness/config.local.toml` |

Each layer uses only the first existing file: TOML takes precedence over JSON. Later layers override earlier ones by key. Objects are merged recursively; arrays are replaced entirely. For example, a new `models` array replaces the provider's entire model list.

Keep shared project settings in `.xarness/config.json` and personal endpoints and keys in `.xarness/config.local.json`. Add `.xarness/config.local.*` to your project's `.gitignore`. Restart xarness after changing settings.

### Provider and model

An example for a remote API; replace the URL and model identifier with those of your service:

```json
{
  "default": "remote/my-model",
  "auto_compact": 0.8,
  "providers": {
    "remote": {
      "kind": "openai",
      "base_url": "https://api.example.com/v1",
      "api_key_env": "XARNESS_API_KEY",
      "models": [
        {
          "id": "my-model",
          "name": "Main model",
          "context_window": 32768,
          "max_tokens": 4096,
          "temperature": 0.2
        }
      ]
    }
  }
}
```

Set `XARNESS_API_KEY` in your environment before starting the application. You can use `api_key` directly instead of `api_key_env`; if both are set, `api_key` takes precedence.

| Setting | Purpose |
| --- | --- |
| `default` | Startup model in `{provider}/{model_id}` format. If omitted, the first model of the first provider alphabetically with a nonempty `models` list is selected. |
| `providers.<name>.kind` | API type; currently `openai` is supported and is also the default. |
| `base_url` | API prefix. xarness appends `/chat/completions`, so do not include the full endpoint path. |
| `models` | Explicit list of available models; it is not fetched automatically from the server. |
| `id` / `name` | Model identifier sent to the API / optional display name. |
| `context_window` | Context size in tokens, used to fit history into the window. Set it to the server's actual context size. |
| `max_tokens` | Response token limit sent to the server. If omitted, the field is not sent. |
| `temperature` | Optional response randomness setting. |
| `auto_compact` | Fraction of the context window at which history is compacted after a turn; disabled if omitted. Can be set globally or per model, where it takes precedence. |

Model identifiers can contain `/`: for example, `remote/org/model` refers to provider `remote` and model `org/model`. Switch models with `/model remote/my-model`; `/model` lists available models.

If `context_window` is not set, xarness tries to fetch `n_ctx` from the llama.cpp server's `/props` endpoint before each turn. If that fails, history is sent without fitting it to a window size. For other servers, set the context size explicitly.

### Reasoning levels

Set `think` on a provider or a model. A model inherits the provider's setting unless it defines its own. Use `/think` to list available levels and `/think <level>` to select one.

For APIs with a `reasoning_effort` field, add `think` to the provider or model object:

```json
{
  "think": {
    "default": "low",
    "levels": ["low", "high"]
  }
}
```

The short form `"think": ["low", "high"]` is also accepted, without selecting a default level. For llama.cpp templates with an `enable_thinking` switch, the `"think": "qwen"` preset provides `on` and `off` levels.

You can also define arbitrary request fields explicitly. For example, instead of the preset, add the following `think` field to the `local` provider object:

```json
{
  "think": {
    "default": "off",
    "levels": {
      "on": {
        "chat_template_kwargs": { "enable_thinking": true }
      },
      "off": {
        "chat_template_kwargs": { "enable_thinking": false }
      }
    }
  }
}
```

The selected level's fields are merged into the request body as JSON. Support for these fields depends on your server and model.

### MCP

MCP adds tools from external servers. For each server, set either `command` for stdio or `url` for Streamable HTTP. The following are templates: replace the path, arguments, and URL with your MCP server's settings.

Add an `mcp` object at the top level of the configuration:

```json
{
  "mcp": {
    "local_tools": {
      "command": "/absolute/path/to/mcp-server",
      "args": [],
      "env": { "EXAMPLE_SETTING": "value" },
      "approval": "auto"
    },
    "remote_tools": {
      "url": "https://mcp.example.com/mcp",
      "headers_env": { "Authorization": "MCP_AUTHORIZATION" },
      "approval": "always"
    }
  }
}
```

In `headers_env`, specify the name of an environment variable containing the complete header value, such as `Bearer …`. Literal header values can be set through `headers`.

- `"approval": "auto"` (default) — require approval for tools the server has not marked as read-only.
- `"approval": "always"` — require approval for every call.
- `"approval": "never"` — do not request approval during an ordinary turn.
- `"tools": ["tool_name"]` — expose only the listed server tools; all are available if omitted.
- `"enabled": false` — disable the server.

Servers connect in the background at startup. `/mcp` shows their status; tools are exposed to the model as `server__tool`.

### Project instructions and skills

Put project instructions — validation commands, conventions, and constraints — in `AGENTS.md` in the launch directory. The file is read at startup and added to the system prompt. Parent directories and subdirectories are not searched.

Skills are reusable instructions stored in `<name>/SKILL.md` files. Directories are searched in this order of precedence:

1. `./.xarness/skills/`
2. `./.agents/skills/`
3. `~/.config/xarness/skills/`
4. `~/.agents/skills/`

If multiple skills share a name, the first one found wins. Example `.xarness/skills/review/SKILL.md`:

```markdown
---
name: review
description: Review changes for bugs and missing checks
---
Read git diff, identify potential bugs, and report file paths and reasons.
```

Users invoke a skill with `/review [task]`; the model can load it based on its description. `/skills` lists the catalog. The built-in `/plan` is available without any files; a custom skill named `plan` replaces it.

Configure additional compatibility directories and disabled skills as follows:

```json
{
  "skills": {
    "compat": ["agents", "claude"],
    "disabled": ["unused-skill"]
  }
}
```

This `compat` setting adds `.claude/skills` in both the project and home directories to the search path.

### Subagents

Define helpers for individual tasks in `./.xarness/agents/<name>.md` or `~/.config/xarness/agents/<name>.md`. Project definitions take precedence; there are no built-in subagents.

Example `.xarness/agents/reviewer.md`:

```markdown
---
name: reviewer
description: Reviews code for bugs without editing files
tools: [read_file, grep, list_dir, read_symbol]
---
Examine the specified code and return a list of issues with explanations and file paths.
```

The main agent invokes a helper through the `agent` tool. The subagent gets its own context and returns a final report. Add an `agents` object at the top level of the configuration to select the helper's model:

```json
{
  "agents": {
    "reviewer": {
      "model": "local/my-model",
      "enabled": true
    }
  }
}
```

If `model` is omitted, the calling agent's model is used. You can add `"think": "off"` to the `reviewer` object if that level is configured for the model. The optional `extra` object supplies JSON request fields that override the selected `think` level.

By default, consecutive subagent calls in a single response run in parallel; background calls are also available. For a server with a single slot, set `"agents_parallel": false` **at the top level of the configuration**. `/agents` opens a pane for running helpers; `/agents list` lists their definitions.

## Using the terminal interface

| Command | Action |
| --- | --- |
| `/model [provider/model_id]` | List models or switch the current model. |
| `/think [level]` | List or select a reasoning level. |
| `/plan <task>` | Explore the project and propose a plan. |
| `/compact [focus]` | Replace history with a summary; optionally specify what to preserve. |
| `/clear` | Clear history and start a new session. |
| `/resume [n]` | List saved sessions or load one. |
| `/map` / `/map show` | Rebuild / display the repository map. |
| `/mcp`, `/skills`, `/agents list` | Inspect connected extensions. |
| `/agents` | Open the subagent pane or cycle to the next helper. |
| `/stats` | Show statistics. |
| `/help`, `/quit` | Show help / quit. |

`Enter` sends a message; `Shift+Enter` / `Alt+Enter` / `Ctrl+J` insert a new line; `Tab` completes commands and arguments. `Esc` interrupts the current turn or closes the subagent pane. Background tasks continue after an interruption. `PgUp` / `PgDn` and the mouse wheel scroll the conversation; `↑` / `↓` browse input history. In an approval prompt, choose an option with the arrow keys or `Tab`, then press `Enter`.

Sessions are saved automatically in `./.xarness/sessions/`. To continue the latest session in the same project:

```bash
xarness --continue
# or xarness -c
```

Large tool results are shortened in the context, while full command output is saved in `./.xarness/tmp/`. Temporary files are cleared at startup and on `/clear`. The session and temporary directories each get their own `.gitignore`.
