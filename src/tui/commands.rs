pub enum Command {
    Plan,
    Build,
    Model(Option<String>),
    Think(Option<String>),
    Map,
    MapShow,
    Mcp,
    Skills,
    Agents,
    Compact(Option<String>),
    Clear,
    Resume(Option<String>),
    Stats,
    Help,
    Quit,
    /// Not built in: a skill, or unknown.
    Other {
        name: String,
        arg: Option<String>,
    },
}

pub const HELP: &str = "/clear  /compact [focus]  /resume [n]  /plan  /build  /model [provider/model]  /think [level]  /map [show]  /mcp  /skills  /agents  /stats  /<skill> [args]  /help  /quit\n\
Tab — complete command or cycle its arguments, Shift+Tab — toggle mode, Esc — interrupt, PgUp/PgDn/wheel — scroll, Up/Down/Ctrl+P/Ctrl+N — input history, drag — select and copy (Shift+drag — native selection), Ctrl+C — interrupt or clear input, twice to quit\n\
Shift+Enter/Alt+Enter/Ctrl+J — new line, Ctrl+W/Alt+Backspace — delete word, Ctrl+U/Ctrl+K — delete to line start/end, \
Ctrl+A/Ctrl+E — line start/end, Ctrl+←→/Alt+B/Alt+F — move by word\n\
Approval: ←→/Tab + Enter; type a reason, pick no and Enter to decline with it; Esc interrupts";

pub const COMMANDS: &[(&str, &str)] = &[
    ("clear", "clear history"),
    ("compact", "replace history with a summary"),
    ("resume", "list or resume saved sessions"),
    ("plan", "read-only mode"),
    ("build", "mode with mutating tools"),
    ("model", "list or switch model"),
    ("think", "list or set reasoning level"),
    ("map", "rebuild repo map"),
    ("mcp", "MCP server status"),
    ("skills", "list skills"),
    ("agents", "list subagents"),
    ("stats", "request and context metrics"),
    ("help", "show help"),
    ("quit", "exit"),
];

pub struct Completion {
    pub label: String,
    pub desc: String,
    /// Replaces the whole input when picked.
    pub insert: String,
}

/// Completions for a `/name` or `/name arg` being typed: exact, then prefix, then fuzzy
/// (subsequence) matches. Argument candidates come from `args(name)`, since some are
/// only known at run time (config). `extra` are commands added at run time (skills). Empty past
/// the first argument.
pub fn complete(
    input: &str,
    extra: Vec<(String, String)>,
    args: impl Fn(&str) -> Vec<(String, String)>,
) -> Vec<Completion> {
    let Some(typed) = input.strip_prefix('/') else {
        return Vec::new();
    };
    let Some((name, arg)) = typed.split_once(char::is_whitespace) else {
        let commands = COMMANDS
            .iter()
            .map(|&(name, desc)| (name.to_string(), desc.to_string()))
            .chain(extra);
        return ranked(typed, commands)
            .into_iter()
            .map(|(name, desc)| Completion {
                label: format!("/{name}"),
                insert: format!("/{name} "),
                desc,
            })
            .collect();
    };
    let arg = arg.trim_start();
    if arg.contains(char::is_whitespace) {
        return Vec::new();
    }
    ranked(arg, args(name).into_iter())
        .into_iter()
        .map(|(value, desc)| Completion {
            insert: format!("/{name} {value}"),
            label: value,
            desc,
        })
        .collect()
}

fn ranked(
    typed: &str,
    candidates: impl Iterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut found: Vec<(u8, (String, String))> = candidates
        .filter_map(|(name, desc)| {
            let rank = if name == typed {
                0
            } else if name.starts_with(typed) {
                1
            } else if is_subsequence(typed, &name) {
                2
            } else {
                return None;
            };
            Some((rank, (name, desc)))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, c)| c).collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|c| hay.any(|h| h == c))
}

pub fn is_builtin(name: &str) -> bool {
    COMMANDS.iter().any(|&(n, _)| n == name) || matches!(name, "exit" | "q")
}

pub fn parse(input: &str) -> Command {
    let mut parts = input.trim_start_matches('/').splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
    match name {
        "plan" => Command::Plan,
        "build" => Command::Build,
        "model" => Command::Model(arg.map(str::to_string)),
        "think" => Command::Think(arg.map(str::to_string)),
        "map" => match arg {
            None => Command::Map,
            Some("show") => Command::MapShow,
            Some(other) => Command::Other {
                name: "map".into(),
                arg: Some(other.into()),
            },
        },
        "mcp" => Command::Mcp,
        "skills" => Command::Skills,
        "agents" => Command::Agents,
        "compact" => Command::Compact(arg.map(str::to_string)),
        "clear" => Command::Clear,
        "resume" => Command::Resume(arg.map(str::to_string)),
        "stats" => Command::Stats,
        "help" => Command::Help,
        "quit" | "exit" | "q" => Command::Quit,
        other => Command::Other {
            name: other.to_string(),
            arg: arg.map(str::to_string),
        },
    }
}
