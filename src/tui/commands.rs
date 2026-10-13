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

pub const HELP: &str = "/plan  /build  /model [provider/model]  /think [level]  /map [show]  /clear  /help  /quit\n\
Tab — complete command, Shift+Tab — toggle mode, Esc — interrupt, Up/Down/PgUp/PgDn/wheel — scroll, Ctrl+P/Ctrl+N — input history, Ctrl+C — quit";

pub const COMMANDS: &[(&str, &str)] = &[
    ("plan", "read-only mode"),
    ("build", "mode with mutating tools"),
    ("model", "list or switch model"),
    ("think", "list or set reasoning level"),
    ("map", "rebuild repo map"),
    ("clear", "clear history"),
    ("help", "show help"),
    ("quit", "exit"),
];

/// Commands matching a `/name` prefix being typed: exact, then prefix, then fuzzy
/// (subsequence) matches. Empty once arguments start, so the popup gets out of the way.
pub fn complete(input: &str) -> Vec<(&'static str, &'static str)> {
    let Some(typed) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut found: Vec<(u8, (&str, &str))> = COMMANDS
        .iter()
        .filter_map(|&(name, desc)| {
            let rank = if name == typed {
                0
            } else if name.starts_with(typed) {
                1
            } else if is_subsequence(typed, name) {
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
