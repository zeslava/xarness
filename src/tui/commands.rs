pub enum Command {
    Plan,
    Build,
    Model(Option<String>),
    Think(Option<String>),
    Clear,
    Help,
    Quit,
    Unknown(String),
}

pub const HELP: &str = "/plan  /build  /model [provider/model]  /think [level]  /clear  /help  /quit\n\
Shift+Tab — toggle mode, Esc — interrupt, Up/Down/PgUp/PgDn — scroll, Ctrl+C — quit";

pub fn parse(input: &str) -> Command {
    let mut parts = input.trim_start_matches('/').splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
    match name {
        "plan" => Command::Plan,
        "build" => Command::Build,
        "model" => Command::Model(arg.map(str::to_string)),
        "think" => Command::Think(arg.map(str::to_string)),
        "clear" => Command::Clear,
        "help" => Command::Help,
        "quit" | "exit" | "q" => Command::Quit,
        other => Command::Unknown(other.to_string()),
    }
}
