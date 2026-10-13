use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::commands::{self, Command};
use crate::adapters::build_provider;
use crate::agent::{self, AgentEvent, ContextCut, Run};
use crate::config::{Config, ModelConfig};
use crate::domain::message::{Message, Mode};
use crate::domain::ports::LlmProvider;
use crate::repomap;
use crate::tools::ToolRegistry;

pub enum Entry {
    User(String),
    Assistant(String),
    Thinking,
    Tool { name: String, arguments: String },
    ToolOutput { text: String, is_error: bool },
    Info(String),
    Error(String),
}

pub struct ContextUsage {
    pub used: u32,
    pub window: Option<u32>,
    pub cached: Option<u32>,
}

fn default_think(model: &ModelConfig) -> Option<String> {
    model.think.as_ref().and_then(|t| t.default.clone())
}

pub struct App {
    config: Config,
    pub provider_name: String,
    pub model: ModelConfig,
    /// Selected key of `model.think.levels`.
    pub think: Option<String>,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    /// Contents of AGENTS.md in the working directory, read once at startup.
    instructions: Option<Arc<str>>,
    /// Built at startup and on `/map` only, so the system prompt prefix stays cacheable.
    repo_map: Option<Arc<str>>,
    pub mode: Mode,
    history: Vec<Message>,
    cut: ContextCut,
    pub usage: Option<ContextUsage>,
    pub log: Vec<Entry>,
    pub input: String,
    /// Cursor position in chars, not bytes.
    pub cursor: usize,
    /// Submitted inputs, slash commands included; survives `/clear` like shell history.
    sent: Vec<String>,
    /// While browsing `sent` with Ctrl+P/Ctrl+N: how many entries back from the newest (0 = newest).
    recall: Option<usize>,
    /// Selected row of the slash-command completion popup.
    pub completion: usize,
    /// Lines scrolled up from the bottom of the log.
    pub scroll: u16,
    /// Log viewport height from the last draw, for page scrolling.
    pub log_height: u16,
    /// Spinner frame counter, advanced only while a run is active.
    pub tick: usize,
    running: Option<JoinHandle<()>>,
    run_id: u64,
    tx: UnboundedSender<(u64, AgentEvent)>,
    pub quit: bool,
}

impl App {
    pub fn new(
        config: Config,
        tools: ToolRegistry,
        tx: UnboundedSender<(u64, AgentEvent)>,
    ) -> anyhow::Result<Self> {
        let (provider_name, model) = config.default_model()?;
        let provider = build_provider(&config.providers[&provider_name]);
        let instructions = std::fs::read_to_string(tools.root().join("AGENTS.md"))
            .ok()
            .map(Arc::from);
        let repo_map = repomap::build(tools.root()).map(|m| Arc::from(m.text));
        Ok(Self {
            config,
            provider_name,
            think: default_think(&model),
            model,
            provider,
            tools: Arc::new(tools),
            instructions,
            repo_map,
            mode: Mode::Plan,
            history: Vec::new(),
            cut: ContextCut::default(),
            usage: None,
            log: Vec::new(),
            input: String::new(),
            cursor: 0,
            sent: Vec::new(),
            recall: None,
            completion: 0,
            scroll: 0,
            log_height: 0,
            tick: 0,
            running: None,
            run_id: 0,
            tx,
            quit: false,
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn model_label(&self) -> String {
        format!(
            "{}/{}",
            self.provider_name,
            self.model.name.as_deref().unwrap_or(&self.model.id)
        )
    }

    pub fn on_term_event(&mut self, ev: Event) {
        if let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
        {
            self.on_key(key);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !ctrl
            && matches!(
                key.code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
            )
        {
            // Editing a recalled entry turns it into ordinary input.
            self.recall = None;
            self.completion = 0;
        }
        let completions = self.completions();
        if !completions.is_empty() {
            let n = completions.len();
            let selected = completions[self.completion.min(n - 1)].0;
            match key.code {
                KeyCode::Up => {
                    self.completion = (self.completion + n - 1) % n;
                    return;
                }
                KeyCode::Down => {
                    self.completion = (self.completion + 1) % n;
                    return;
                }
                KeyCode::Tab => {
                    self.input = format!("/{selected} ");
                    self.cursor = self.input.chars().count();
                    return;
                }
                KeyCode::Enter => self.input = format!("/{selected}"),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('p') if ctrl => match self.recall {
                Some(n) if n + 1 < self.sent.len() => self.show_recalled(n + 1),
                Some(_) => {}
                // Don't clobber text being typed.
                None if self.input.is_empty() && !self.sent.is_empty() => self.show_recalled(0),
                None => {}
            },
            KeyCode::Char('n') if ctrl => match self.recall {
                Some(0) => {
                    self.recall = None;
                    self.input.clear();
                    self.cursor = 0;
                }
                Some(n) => self.show_recalled(n - 1),
                None => {}
            },
            KeyCode::Esc => self.interrupt(),
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab => self.set_mode(self.mode.toggle()),
            KeyCode::Char(c) => {
                let at = self.byte_pos();
                self.input.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_pos();
                self.input.remove(at);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let at = self.byte_pos();
                self.input.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            // The terminal sends the mouse wheel as Up/Down (alternate scroll mode).
            KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(self.page()),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page()),
            _ => {}
        }
    }

    pub fn completions(&self) -> Vec<(&'static str, &'static str)> {
        commands::complete(&self.input)
    }

    fn page(&self) -> u16 {
        self.log_height.saturating_sub(1).max(1)
    }

    fn show_recalled(&mut self, n: usize) {
        self.recall = Some(n);
        self.input = self.sent[self.sent.len() - 1 - n].clone();
        self.cursor = self.input.chars().count();
    }

    fn byte_pos(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map_or(self.input.len(), |(i, _)| i)
    }

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input).trim().to_string();
        self.cursor = 0;
        if text.is_empty() {
            return;
        }
        self.recall = None;
        if self.sent.last() != Some(&text) {
            self.sent.push(text.clone());
        }
        self.scroll = 0;
        if text.starts_with('/') {
            self.command(commands::parse(&text));
            return;
        }
        if self.is_running() {
            self.log
                .push(Entry::Error("agent is busy; Esc to interrupt".into()));
            self.input = text;
            self.cursor = self.input.chars().count();
            return;
        }
        self.log.push(Entry::User(text.clone()));
        self.history.push(Message::User(text));
        self.start_run();
    }

    fn start_run(&mut self) {
        self.run_id += 1;
        let run = Run {
            provider: self.provider.clone(),
            model: self.model.clone(),
            mode: self.mode,
            history: self.history.clone(),
            cut: self.cut.clone(),
            tools: self.tools.clone(),
            instructions: self.instructions.clone(),
            repo_map: self.repo_map.clone(),
            extra: self.think_patch(),
        };
        self.running = Some(tokio::spawn(agent::run(run, self.tx.clone(), self.run_id)));
    }

    fn think_patch(&self) -> Option<Value> {
        let level = self.think.as_ref()?;
        self.model.think.as_ref()?.levels.get(level).cloned()
    }

    fn interrupt(&mut self) {
        if let Some(h) = self.running.take() {
            h.abort();
            // Invalidate events already queued by the aborted run.
            self.run_id += 1;
            self.log.push(Entry::Info("interrupted".into()));
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Plan => self.set_mode(Mode::Plan),
            Command::Build => self.set_mode(Mode::Build),
            Command::Model(None) => {
                let current = format!("{}/{}", self.provider_name, self.model.id);
                let list = self
                    .config
                    .all_models()
                    .map(|m| {
                        let mark = if m == current { "*" } else { " " };
                        format!("{mark} {m}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.log.push(Entry::Info(list));
            }
            Command::Model(Some(spec)) => match self.config.resolve_model(&spec) {
                Ok((provider, model)) => {
                    self.provider = build_provider(&self.config.providers[&provider]);
                    self.provider_name = provider;
                    self.think = default_think(&model);
                    self.model = model;
                    self.cut = ContextCut::default();
                    self.usage = None;
                    self.log
                        .push(Entry::Info(format!("model: {}", self.model_label())));
                }
                Err(e) => self.log.push(Entry::Error(e.to_string())),
            },
            Command::Think(arg) => {
                let Some(think) = &self.model.think else {
                    self.log.push(Entry::Error(
                        "no think levels configured for this model".into(),
                    ));
                    return;
                };
                match arg {
                    None => {
                        let list = think
                            .levels
                            .keys()
                            .map(|l| {
                                let mark = if Some(l) == self.think.as_ref() {
                                    "*"
                                } else {
                                    " "
                                };
                                format!("{mark} {l}")
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.log.push(Entry::Info(list));
                    }
                    Some(level) if think.levels.contains_key(&level) => {
                        self.log.push(Entry::Info(format!("think: {level}")));
                        self.think = Some(level);
                    }
                    Some(level) => {
                        let available = think.levels.keys().cloned().collect::<Vec<_>>().join(", ");
                        self.log.push(Entry::Error(format!(
                            "unknown think level `{level}`; available: {available}"
                        )));
                    }
                }
            }
            Command::Map => {
                let map = repomap::build(self.tools.root());
                let msg = match &map {
                    Some(m) => format!(
                        "repo map rebuilt: {} files, ~{} tokens",
                        m.files,
                        m.text.len() / 3
                    ),
                    None => "repo map: no files".into(),
                };
                self.repo_map = map.map(|m| Arc::from(m.text));
                self.log.push(Entry::Info(msg));
            }
            Command::MapShow => {
                let msg = match &self.repo_map {
                    Some(m) => m.to_string(),
                    None => "repo map: none".into(),
                };
                self.log.push(Entry::Info(msg));
            }
            Command::Clear => {
                self.interrupt();
                self.history.clear();
                self.cut = ContextCut::default();
                self.usage = None;
                self.log.clear();
            }
            Command::Help => self.log.push(Entry::Info(commands::HELP.into())),
            Command::Quit => self.quit = true,
            Command::Unknown(name) => self
                .log
                .push(Entry::Error(format!("unknown command /{name}; /help"))),
        }
    }

    pub fn on_agent_event(&mut self, run_id: u64, ev: AgentEvent) {
        if run_id != self.run_id {
            return;
        }
        match ev {
            AgentEvent::TextDelta(t) => match self.log.last_mut() {
                Some(Entry::Assistant(s)) => s.push_str(&t),
                _ => self.log.push(Entry::Assistant(t)),
            },
            AgentEvent::Thinking => {
                if !matches!(self.log.last(), Some(Entry::Thinking)) {
                    self.log.push(Entry::Thinking)
                }
            }
            AgentEvent::ToolStart { name, arguments } => {
                self.log.push(Entry::Tool { name, arguments })
            }
            // Successful tool output is noise in the log; only errors are shown.
            AgentEvent::ToolResult { output, is_error } => {
                if is_error {
                    self.log.push(Entry::ToolOutput {
                        text: output,
                        is_error,
                    })
                }
            }
            AgentEvent::Commit { messages, cut } => {
                self.history.extend(messages);
                self.cut = cut;
            }
            AgentEvent::Usage {
                used,
                window,
                cached,
            } => {
                self.usage = Some(ContextUsage {
                    used,
                    window,
                    cached,
                })
            }
            AgentEvent::Error(e) => self.log.push(Entry::Error(e)),
            AgentEvent::Finished => self.running = None,
        }
    }
}
