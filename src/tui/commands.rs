pub enum Command {
    Plan,
    Build,
    Model(Option<String>),
    Think(Option<String>),
    Map,
    MapShow,
    Clear,
    Help,
    Quit,
    Unknown(String),
}

pub const HELP: &str = "/clear  /plan  /build  /model [provider/model]  /think [level]  /map [show]  /help  /quit\n\
Tab — complete command, Shift+Tab — toggle mode, Esc — interrupt, Up/Down/PgUp/PgDn/wheel — scroll, Ctrl+P/Ctrl+N — input history, Ctrl+C — interrupt or clear input, twice to quit\n\
Shift+Enter/Alt+Enter/Ctrl+J — new line, Ctrl+W/Alt+Backspace — delete word, Ctrl+U/Ctrl+K — delete to line start/end, \
Ctrl+A/Ctrl+E — line start/end, Ctrl+←→/Alt+B/Alt+F — move by word\n\
Approval: ←→/Tab + Enter; type a reason, pick no and Enter to decline with it; Esc interrupts";

pub const COMMANDS: &[(&str, &str)] = &[
    ("clear", "clear history"),
    ("plan", "read-only mode"),
    ("build", "mode with mutating tools"),
    ("model", "list or switch model"),
    ("think", "list or set reasoning level"),
    ("map", "rebuild repo map"),
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
/// only known at run time (config). Empty past the first argument.
pub fn complete(input: &str, args: impl Fn(&str) -> Vec<(String, String)>) -> Vec<Completion> {
    let Some(typed) = input.strip_prefix('/') else {
        return Vec::new();
    };
    let Some((name, arg)) = typed.split_once(char::is_whitespace) else {
        let commands = COMMANDS
            .iter()
            .map(|&(name, desc)| (name.to_string(), desc.to_string()));
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
            Some(other) => Command::Unknown(format!("map {other}")),
        },
        "clear" => Command::Clear,
        "help" => Command::Help,
        "quit" | "exit" | "q" => Command::Quit,
        other => Command::Unknown(other.to_string()),
    }
}
