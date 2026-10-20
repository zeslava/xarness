use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::commands::{self, Command, Completion};
use crate::adapters::build_provider;
use crate::adapters::mcp;
use crate::agent::{self, AgentEvent, Answer, ContextCut, Run};
use crate::config::{Config, ModelConfig};
use crate::domain::message::{Message, Mode};
use crate::domain::ports::LlmProvider;
use crate::repomap;
use crate::tools::ToolRegistry;

pub enum Entry {
    User(String),
    Assistant(String),
    Tool { name: String, arguments: String },
    ToolOutput { text: String, is_error: bool },
    Info(String),
    Error(String),
}

/// What an active run is doing, for the live indicator at the bottom of the log.
#[derive(PartialEq)]
pub enum Phase {
    /// Request sent, nothing streamed yet (prompt processing).
    Waiting,
    Thinking,
    Writing,
    Running(String),
}

pub struct ContextUsage {
    pub used: u32,
    pub window: Option<u32>,
    pub cached: Option<u32>,
}

fn default_think(model: &ModelConfig) -> Option<String> {
    model.think.as_ref().and_then(|t| t.default.clone())
}

struct Active {
    provider_name: String,
    model: ModelConfig,
    provider: Arc<dyn LlmProvider>,
}

impl Active {
    fn new(config: &Config, provider_name: String, model: ModelConfig) -> Self {
        Self {
            provider: build_provider(&config.providers[&provider_name]),
            provider_name,
            model,
        }
    }
}

/// Answers to a tool call approval, in menu order.
pub const APPROVAL_OPTIONS: [&str; 3] = ["yes", "allow for session", "no"];

pub struct Approval {
    reply: oneshot::Sender<Answer>,
    pub name: String,
    pub arguments: String,
    /// Highlighted index into `APPROVAL_OPTIONS`.
    pub choice: usize,
}

const NO_PROVIDERS: &str = "no providers configured: add one to ~/.config/xarness/config.json \
     or ./.xarness/config.json, then restart";

pub struct App {
    config: Config,
    /// `None` until a provider with models is configured.
    active: Option<Active>,
    /// Selected key of `model.think.levels`.
    pub think: Option<String>,
    tools: Arc<ToolRegistry>,
    /// Contents of AGENTS.md in the working directory, read once at startup.
    instructions: Option<Arc<str>>,
    /// Per server: tool count or connection error; `None` while connecting.
    mcp: Option<Vec<(String, Result<usize, String>)>>,
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
    /// Wrapped log height from the last draw, to keep a scrolled view in place as it grows.
    pub log_total: usize,
    /// Lines added below the view since it was scrolled up.
    pub unseen: usize,
    /// Spinner frame counter, advanced only while a run is active.
    pub tick: usize,
    running: Option<JoinHandle<()>>,
    /// Current phase of the active run and when it began.
    pub phase: (Phase, Instant),
    /// Messages submitted while a run was active, sent together as one turn when it finishes.
    pub queued: VecDeque<String>,
    /// Reports of exited background jobs not yet sent to the model. They start a turn of their
    /// own when idle, otherwise they ride along with the next message.
    notices: Vec<String>,
    approval: Option<Approval>,
    /// Tools answered with "always": approved without asking until the app exits.
    pub always: BTreeSet<String>,
    run_id: u64,
    tx: UnboundedSender<(u64, AgentEvent)>,
    /// Set by the first Ctrl+C; the next key either quits (Ctrl+C) or disarms it.
    pub quit_armed: bool,
    pub quit: bool,
}

impl App {
    pub fn new(
        config: Config,
        tools: ToolRegistry,
        tx: UnboundedSender<(u64, AgentEvent)>,
    ) -> anyhow::Result<Self> {
        let active = config
            .default_model()?
            .map(|(name, model)| Active::new(&config, name, model));
        let log = match active {
            Some(_) => Vec::new(),
            None => vec![Entry::Info(NO_PROVIDERS.into())],
        };
        let instructions = std::fs::read_to_string(tools.root().join("AGENTS.md"))
            .ok()
            .map(Arc::from);
        let repo_map = repomap::build(tools.root()).map(|m| Arc::from(m.text));
        Ok(Self {
            think: active.as_ref().and_then(|a| default_think(&a.model)),
            active,
            config,
            tools: Arc::new(tools),
            mcp: None,
            instructions,
            repo_map,
            mode: Mode::Plan,
            history: Vec::new(),
            cut: ContextCut::default(),
            usage: None,
            log,
            input: String::new(),
            cursor: 0,
            sent: Vec::new(),
            recall: None,
            completion: 0,
            scroll: 0,
            log_height: 0,
            log_total: 0,
            unseen: 0,
            tick: 0,
            running: None,
            phase: (Phase::Waiting, Instant::now()),
            queued: VecDeque::new(),
            notices: Vec::new(),
            approval: None,
            always: BTreeSet::new(),
            run_id: 0,
            tx,
            quit_armed: false,
            quit: false,
        })
    }

    pub fn approval(&self) -> Option<&Approval> {
        self.approval.as_ref()
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn model_label(&self) -> String {
        match &self.active {
            Some(a) => format!(
                "{}/{}",
                a.provider_name,
                a.model.name.as_deref().unwrap_or(&a.model.id)
            ),
            None => "no model".into(),
        }
    }

    pub fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => self.paste(&text),
            _ => {}
        }
    }

    fn paste(&mut self, text: &str) {
        // The input renderer assumes one cell per char, so tabs become spaces.
        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        self.insert(&text);
    }

    fn insert(&mut self, text: &str) {
        self.recall = None;
        self.completion = 0;
        let at = self.byte_pos();
        self.input.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// Removes chars `from..to` (char indices) and puts the cursor at `from`.
    fn delete(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        self.recall = None;
        self.completion = 0;
        let byte = |i: usize| {
            self.input
                .char_indices()
                .nth(i)
                .map_or(self.input.len(), |(b, _)| b)
        };
        let range = byte(from)..byte(to);
        self.input.replace_range(range, "");
        self.cursor = from;
    }

    /// Start of the word before the cursor, skipping whitespace first, as readline's Ctrl+W.
    fn word_back(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_forward(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Char bounds of the input line holding the cursor, newline excluded.
    fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        let start = chars[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1);
        let end = chars[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |i| self.cursor + i);
        (start, end)
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let quit_armed = std::mem::take(&mut self.quit_armed);
        // Shift+Enter needs the kitty keyboard protocol; Alt+Enter and Ctrl+J work everywhere.
        let newline = match key.code {
            KeyCode::Enter => alt || key.modifiers.contains(KeyModifiers::SHIFT),
            KeyCode::Char('j') => ctrl,
            _ => false,
        };
        if newline {
            return self.insert("\n");
        }
        if let Some(mut a) = self.approval.take() {
            let n = APPROVAL_OPTIONS.len();
            // No letter hotkeys: any letter could start a decline reason.
            match key.code {
                KeyCode::Enter => return self.answer(a),
                KeyCode::Esc => return self.interrupt(),
                _ => {}
            }
            // Left/Right move the input cursor once a reason is being typed; Up/Down stay
            // scrolling, since the terminal sends the mouse wheel as them.
            let empty = self.input.is_empty();
            let step = match key.code {
                KeyCode::Tab => Some(1),
                KeyCode::Right if empty => Some(1),
                KeyCode::BackTab => Some(n - 1),
                KeyCode::Left if empty => Some(n - 1),
                _ => None,
            };
            if let Some(step) = step {
                a.choice = (a.choice + step) % n;
            }
            self.approval = Some(a);
            if step.is_some() {
                return;
            }
            // Anything else edits the input, scrolls or quits as usual.
        }
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
        if !completions.is_empty() && self.approval.is_none() {
            let n = completions.len();
            let selected = &completions[self.completion.min(n - 1)].insert;
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
                    self.input = selected.clone();
                    self.cursor = self.input.chars().count();
                    return;
                }
                KeyCode::Enter => self.input = selected.clone(),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if quit_armed {
                    self.quit = true;
                } else if self.is_running() {
                    self.interrupt();
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    self.recall = None;
                }
                self.quit_armed = !self.quit;
            }
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
            KeyCode::Char('w') if ctrl => self.delete(self.word_back(), self.cursor),
            KeyCode::Backspace if alt || ctrl => self.delete(self.word_back(), self.cursor),
            KeyCode::Char('u') if ctrl => self.delete(self.line_bounds().0, self.cursor),
            KeyCode::Char('k') if ctrl => self.delete(self.cursor, self.line_bounds().1),
            KeyCode::Char('a') if ctrl => self.cursor = self.line_bounds().0,
            KeyCode::Char('e') if ctrl => self.cursor = self.line_bounds().1,
            KeyCode::Char('b') if alt => self.cursor = self.word_back(),
            KeyCode::Char('f') if alt => self.cursor = self.word_forward(),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_back(),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_forward(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if self.cursor > 0 => self.delete(self.cursor - 1, self.cursor),
            KeyCode::Delete => self.delete(self.cursor, self.cursor + 1),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = self.line_bounds().0,
            KeyCode::End => self.cursor = self.line_bounds().1,
            // The terminal sends the mouse wheel as Up/Down (alternate scroll mode).
            KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(self.page()),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page()),
            _ => {}
        }
    }

    pub fn completions(&self) -> Vec<Completion> {
        commands::complete(&self.input, |name| self.arg_candidates(name))
    }

    fn arg_candidates(&self, command: &str) -> Vec<(String, String)> {
        let mark = |current: bool| if current { "current" } else { "" }.to_string();
        match command {
            "think" => self
                .active
                .as_ref()
                .and_then(|a| a.model.think.as_ref())
                .map(|t| {
                    t.levels
                        .keys()
                        .map(|l| (l.clone(), mark(Some(l) == self.think.as_ref())))
                        .collect()
                })
                .unwrap_or_default(),
            "model" => {
                let current = self
                    .active
                    .as_ref()
                    .map(|a| format!("{}/{}", a.provider_name, a.model.id));
                self.config
                    .all_models()
                    .map(|m| {
                        let desc = mark(Some(&m) == current.as_ref());
                        (m, desc)
                    })
                    .collect()
            }
            "map" => vec![("show".into(), "print the current map".into())],
            _ => Vec::new(),
        }
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
        if self.active.is_none() {
            self.log.push(Entry::Error(NO_PROVIDERS.into()));
            self.input = text;
            self.cursor = self.input.chars().count();
            return;
        }
        if self.is_running() {
            self.queued.push_back(text);
            return;
        }
        self.send(text);
    }

    fn send(&mut self, text: String) {
        self.push_log(Entry::User(text.clone()));
        let content = match self.take_notices() {
            Some(notices) => format!("{notices}\n\n{text}"),
            None => text,
        };
        self.history.push(Message::User(content));
        self.start_run();
    }

    fn send_notices(&mut self) {
        if let Some(notices) = self.take_notices() {
            self.history.push(Message::User(notices));
            self.start_run();
        }
    }

    fn take_notices(&mut self) -> Option<String> {
        if self.notices.is_empty() {
            return None;
        }
        Some(
            self.notices
                .drain(..)
                .map(|r| {
                    format!(
                        "[automatic notice, not written by the user] Background job exited:\n{r}"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    pub fn on_job(&mut self, report: String) {
        let summary = report.lines().next().unwrap_or_default();
        self.push_log(Entry::Info(format!("job {summary}")));
        self.notices.push(report);
        if !self.is_running() && self.active.is_some() {
            self.send_notices();
        }
    }

    fn start_run(&mut self) {
        let Some(run) = self.new_run() else {
            return;
        };
        self.running = Some(tokio::spawn(agent::run(run, self.tx.clone(), self.run_id)));
        self.phase = (Phase::Waiting, Instant::now());
    }

    fn new_run(&mut self) -> Option<Run> {
        let active = self.active.as_ref()?;
        let run = Run {
            provider: active.provider.clone(),
            model: active.model.clone(),
            mode: self.mode,
            history: self.history.clone(),
            cut: self.cut.clone(),
            tools: self.tools.clone(),
            instructions: self.instructions.clone(),
            repo_map: self.repo_map.clone(),
            extra: self.think_patch(),
        };
        self.run_id += 1;
        Some(run)
    }

    fn set_phase(&mut self, phase: Phase) {
        if self.phase.0 != phase {
            self.phase = (phase, Instant::now());
        }
    }

    fn think_patch(&self) -> Option<Value> {
        let level = self.think.as_ref()?;
        self.active
            .as_ref()?
            .model
            .think
            .as_ref()?
            .levels
            .get(level)
            .cloned()
    }

    fn interrupt(&mut self) {
        self.approval = None;
        if let Some(h) = self.running.take() {
            h.abort();
            // Invalidate events already queued by the aborted run.
            self.run_id += 1;
            self.push_log(Entry::Info("interrupted".into()));
        }
        // Hand queued messages back for editing rather than running them after an interrupt.
        if !self.queued.is_empty() {
            let mut parts: Vec<String> = self.queued.drain(..).collect();
            if !self.input.is_empty() {
                parts.push(std::mem::take(&mut self.input));
            }
            self.input = parts.join("\n");
            self.cursor = self.input.chars().count();
        }
    }

    /// Declining sends the typed input along as the reason.
    fn answer(&mut self, a: Approval) {
        let answer = match a.choice {
            0 => Answer::Yes,
            1 => {
                self.always.insert(a.name);
                Answer::Yes
            }
            _ => {
                let reason = std::mem::take(&mut self.input).trim().to_string();
                self.cursor = 0;
                self.recall = None;
                Answer::No((!reason.is_empty()).then_some(reason))
            }
        };
        let _ = a.reply.send(answer);
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Plan => self.set_mode(Mode::Plan),
            Command::Build => self.set_mode(Mode::Build),
            Command::Model(None) => {
                let Some(active) = &self.active else {
                    self.log.push(Entry::Info(NO_PROVIDERS.into()));
                    return;
                };
                let current = format!("{}/{}", active.provider_name, active.model.id);
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
                    self.think = default_think(&model);
                    self.active = Some(Active::new(&self.config, provider, model));
                    self.cut = ContextCut::default();
                    self.usage = None;
                    self.log
                        .push(Entry::Info(format!("model: {}", self.model_label())));
                }
                Err(e) => self.log.push(Entry::Error(e.to_string())),
            },
            Command::Think(arg) => {
                let Some(think) = self.active.as_ref().and_then(|a| a.model.think.as_ref()) else {
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
                    Some(level) if think.levels.contains_key(&level) => self.think = Some(level),
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
            Command::Mcp => {
                let msg = self.mcp_status();
                self.log.push(Entry::Info(msg));
            }
            Command::Clear => {
                self.interrupt();
                self.history.clear();
                self.cut = ContextCut::default();
                self.usage = None;
                self.log.clear();
            }
            Command::Compact(focus) => {
                if self.is_running() {
                    self.log.push(Entry::Error(
                        "a run is active; wait for it or interrupt (Esc)".into(),
                    ));
                } else if self.history.is_empty() {
                    self.log.push(Entry::Info("nothing to compact".into()));
                } else if let Some(run) = self.new_run() {
                    self.running = Some(tokio::spawn(agent::compact(
                        run,
                        focus,
                        self.tx.clone(),
                        self.run_id,
                    )));
                    self.phase = (Phase::Waiting, Instant::now());
                } else {
                    self.log.push(Entry::Error(NO_PROVIDERS.into()));
                }
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
            AgentEvent::TextDelta(t) => {
                self.set_phase(Phase::Writing);
                match self.log.last_mut() {
                    Some(Entry::Assistant(s)) => s.push_str(&t),
                    _ => self.push_log(Entry::Assistant(t)),
                }
            }
            AgentEvent::Thinking => self.set_phase(Phase::Thinking),
            AgentEvent::ToolStart { name, arguments } => {
                self.set_phase(Phase::Running(name.clone()));
                self.push_log(Entry::Tool { name, arguments })
            }
            // Successful tool output is noise in the log; only errors are shown.
            AgentEvent::ToolResult { output, is_error } => {
                self.set_phase(Phase::Waiting);
                if is_error {
                    self.push_log(Entry::ToolOutput {
                        text: output,
                        is_error,
                    })
                }
            }
            AgentEvent::Commit { messages, cut } => {
                self.history.extend(messages);
                self.cut = cut;
            }
            AgentEvent::Compacted(messages) => {
                self.history = messages;
                self.cut = ContextCut::default();
                self.usage = None;
                self.push_log(Entry::Info("history compacted".into()));
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
            AgentEvent::Approve { name, reply, .. } if self.always.contains(&name) => {
                let _ = reply.send(Answer::Yes);
            }
            AgentEvent::Approve {
                name,
                arguments,
                reply,
            } => {
                self.scroll = 0;
                self.approval = Some(Approval {
                    reply,
                    name,
                    arguments,
                    choice: 0,
                });
            }
            AgentEvent::Error(e) => self.push_log(Entry::Error(e)),
            AgentEvent::Finished => {
                self.running = None;
                if !self.queued.is_empty() {
                    let next = self.queued.drain(..).collect::<Vec<_>>().join("\n\n");
                    self.send(next);
                } else {
                    self.send_notices();
                }
            }
        }
    }

    pub fn on_mcp(&mut self, servers: mcp::Connected) {
        let status = servers
            .into_iter()
            .map(|(name, res)| match res {
                Ok(tools) => {
                    let n = tools.len();
                    self.tools.add_mcp(tools);
                    (name, Ok(n))
                }
                Err(e) => (name, Err(format!("{e:#}"))),
            })
            .collect();
        self.mcp = Some(status);
        let msg = self.mcp_status();
        self.push_log(Entry::Info(msg));
    }

    fn mcp_status(&self) -> String {
        if self.config.mcp.is_empty() {
            return "mcp: no servers configured".into();
        }
        let Some(status) = &self.mcp else {
            return "mcp: connecting…".into();
        };
        if status.is_empty() {
            return "mcp: all servers disabled".into();
        }
        status
            .iter()
            .map(|(name, res)| match res {
                Ok(n) => format!("mcp {name}: {n} tools"),
                Err(e) => format!("mcp {name}: failed: {e}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn push_log(&mut self, entry: Entry) {
        self.log.push(entry);
    }
}
