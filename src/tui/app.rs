use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::clipboard::Clipboard;
use super::commands::{self, Command, Completion};
use crate::adapters::build_provider;
use crate::adapters::mcp;
use crate::agent::{self, AgentEvent, Answer, ContextCut, Run};
use crate::config::{Config, ModelConfig};
use crate::domain::message::{Message, Mode};
use crate::domain::ports::LlmProvider;
use crate::metrics;
use crate::repomap;
use crate::session::{self, Session};
use crate::skills::{Skill, Skills};
use crate::tools::ToolRegistry;

const WHEEL_LINES: u16 = 3;
const FLASH_TTL: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy)]
pub enum FlashKind {
    Info,
    Ok,
    Error,
}

/// Short-lived notification shown at the right of the status line.
pub struct Flash {
    pub text: String,
    pub kind: FlashKind,
}

/// Mouse selection in the log, in content coordinates (wrapped line from the top, column),
/// so it stays on the same text while the log scrolls.
#[derive(Clone, Copy)]
pub struct Selection {
    anchor: (usize, u16),
    head: (usize, u16),
    dragged: bool,
}

impl Selection {
    /// Start and end, both inclusive, in reading order.
    pub fn range(&self) -> ((usize, u16), (usize, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

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

/// Skills available as `/name`; built-in commands take precedence.
fn is_command(skill: &Skill) -> bool {
    skill.user_invocable && !commands::is_builtin(&skill.name)
}

fn default_think(model: &ModelConfig) -> Option<String> {
    model.think.as_ref().and_then(|t| t.default.clone())
}

/// The log of a restored session. Tool outputs are left out, as successful ones are live.
fn history_log(history: &[Message]) -> Vec<Entry> {
    let mut log = Vec::new();
    for m in history {
        match m {
            Message::User(t) => log.push(Entry::User(t.clone())),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if !content.is_empty() {
                    log.push(Entry::Assistant(content.clone()));
                }
                log.extend(tool_calls.iter().map(|c| Entry::Tool {
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                }));
            }
            Message::System(_) | Message::Tool { .. } => {}
        }
    }
    log
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
    skills: Arc<Skills>,
    /// Built at startup only, so the system prompt prefix stays cacheable.
    skill_catalog: Option<Arc<str>>,
    /// Per server: tool count or connection error; `None` while connecting.
    mcp: Option<Vec<(String, Result<usize, String>)>>,
    /// Built at startup, on `/map` and on compaction only, so the system prompt prefix stays
    /// cacheable.
    repo_map: Option<Arc<str>>,
    /// Maps are built in the background and arrive through the event loop.
    map_tx: UnboundedSender<Option<repomap::RepoMap>>,
    /// Shown one at a time, each for `FLASH_TTL`; the front one is on screen since `flash_at`.
    flashes: VecDeque<Flash>,
    flash_at: Instant,
    pub mode: Mode,
    history: Vec<Message>,
    /// `None` after a failed write, until `/clear` or `/resume` starts over.
    session: Option<Session>,
    /// Requests of the current session, for `/stats`; not persisted.
    metrics: Vec<metrics::Request>,
    cut: ContextCut,
    pub usage: Option<ContextUsage>,
    pub log: Vec<Entry>,
    pub input: String,
    /// Cursor position in chars, not bytes.
    pub cursor: usize,
    /// Submitted inputs, slash commands included; survives `/clear` like shell history.
    sent: Vec<String>,
    /// While browsing `sent` with Up/Down or Ctrl+P/Ctrl+N: how many entries back from the newest (0 = newest).
    recall: Option<usize>,
    /// Selected row of the slash-command completion popup.
    pub completion: usize,
    /// Lines scrolled up from the bottom of the log.
    pub scroll: u16,
    /// Log viewport from the last draw, for page scrolling and mapping the mouse.
    pub log_area: Rect,
    /// Wrapped log height from the last draw, to keep a scrolled view in place as it grows.
    pub log_total: usize,
    /// Lines added below the view since it was scrolled up.
    pub unseen: usize,
    pub selection: Option<Selection>,
    /// Set when a drag ends; the next draw copies the selection, as it has the rendered log.
    pub copy: bool,
    pub clipboard: Clipboard,
    /// Spinner frame counter, advanced only while a run is active.
    pub tick: usize,
    running: Option<JoinHandle<()>>,
    /// The active run is a compaction; its `Finished` must not trigger another one.
    compacting: bool,
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
        skills: Arc<Skills>,
        tx: UnboundedSender<(u64, AgentEvent)>,
        map_tx: UnboundedSender<Option<repomap::RepoMap>>,
        resume: bool,
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
        let session = Session::new(tools.root());
        let mut app = Self {
            think: active.as_ref().and_then(|a| default_think(&a.model)),
            active,
            config,
            tools: Arc::new(tools),
            mcp: None,
            instructions,
            skill_catalog: skills.catalog().map(Arc::from),
            skills,
            repo_map: None,
            map_tx,
            flashes: VecDeque::new(),
            flash_at: Instant::now(),
            mode: Mode::Plan,
            history: Vec::new(),
            metrics: Vec::new(),
            session: Some(session),
            cut: ContextCut::default(),
            usage: None,
            log,
            input: String::new(),
            cursor: 0,
            sent: Vec::new(),
            recall: None,
            completion: 0,
            scroll: 0,
            log_area: Rect::default(),
            log_total: 0,
            unseen: 0,
            selection: None,
            copy: false,
            clipboard: Clipboard::new(),
            tick: 0,
            running: None,
            compacting: false,
            phase: (Phase::Waiting, Instant::now()),
            queued: VecDeque::new(),
            notices: Vec::new(),
            approval: None,
            always: BTreeSet::new(),
            run_id: 0,
            tx,
            quit_armed: false,
            quit: false,
        };
        app.build_map();
        if resume {
            match session::list(app.tools.root()).into_iter().next() {
                Some(s) => app.resume(s.path),
                None => app.log.push(Entry::Info("no saved sessions".into())),
            }
        }
        Ok(app)
    }

    fn resume(&mut self, path: PathBuf) {
        match Session::open(path) {
            Ok((session, history)) => {
                self.log = history_log(&history);
                self.log.push(Entry::Info("session resumed".into()));
                self.history = history;
                self.metrics.clear();
                self.session = Some(session);
                self.cut = ContextCut::default();
                self.usage = None;
                self.scroll = 0;
            }
            Err(e) => self
                .log
                .push(Entry::Error(format!("can't load session: {e}"))),
        }
    }

    /// Appends new history to the session file; `rewrite` after history was replaced.
    fn persist(&mut self, rewrite: bool) {
        let Some(session) = &mut self.session else {
            return;
        };
        let res = if rewrite {
            session.rewrite(&self.history)
        } else {
            session.save(&self.history)
        };
        if let Err(e) = res {
            self.session = None;
            self.push_log(Entry::Error(format!("session not saved: {e}")));
        }
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
            Event::Mouse(m) => self.on_mouse(m),
            // Content coordinates depend on the wrap width.
            Event::Resize(..) => self.selection = None,
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let area = self.log_area;
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_add(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = area.contains(Position::new(m.column, m.row)).then(|| {
                    let at = self.log_pos(m.column, m.row);
                    Selection {
                        anchor: at,
                        head: at,
                        dragged: false,
                    }
                });
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selection.is_some() => {
                // Past the edges the log scrolls, so a selection can span more than a screen.
                if m.row < area.y {
                    self.scroll = self.scroll.saturating_add(1);
                } else if m.row >= area.bottom() {
                    self.scroll = self.scroll.saturating_sub(1);
                }
                let head = self.log_pos(m.column, m.row);
                if let Some(s) = &mut self.selection {
                    s.head = head;
                    s.dragged = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match self.selection {
                Some(s) if s.dragged => self.copy = true,
                // A plain click clears the selection.
                _ => self.selection = None,
            },
            _ => {}
        }
    }

    /// Screen cell to content coordinates, clamped into the log viewport.
    fn log_pos(&self, col: u16, row: u16) -> (usize, u16) {
        let area = self.log_area;
        let row = row.clamp(area.y, area.bottom().saturating_sub(1)) - area.y;
        let col = col.clamp(area.x, area.right().saturating_sub(1)) - area.x;
        let max_scroll = self.log_total.saturating_sub(area.height as usize);
        let offset = max_scroll.saturating_sub(self.scroll as usize);
        (offset + row as usize, col)
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
        let key = latin_shortcut(key);
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
            // Left/Right move the input cursor once a reason is being typed.
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
            // A recalled slash command shows completions too; Up/Down keep browsing history.
            let browsing = self.recall.is_some();
            match key.code {
                KeyCode::Up if !browsing => {
                    self.completion = (self.completion + n - 1) % n;
                    return;
                }
                KeyCode::Down if !browsing => {
                    self.completion = (self.completion + 1) % n;
                    return;
                }
                // Arguments are cycled rather than inserted: inserting one would filter the
                // list down to it. Enter takes the selected one.
                KeyCode::Tab if self.input.contains(char::is_whitespace) => {
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
            KeyCode::Char('p') if ctrl => self.recall_older(),
            KeyCode::Char('n') if ctrl => self.recall_newer(),
            KeyCode::Up => self.recall_older(),
            KeyCode::Down => self.recall_newer(),
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
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(self.page()),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page()),
            _ => {}
        }
    }

    pub fn completions(&self) -> Vec<Completion> {
        let skills = self
            .user_skills()
            .map(|s| (s.name.clone(), s.short_description(60)))
            .collect();
        commands::complete(&self.input, skills, |name| self.arg_candidates(name))
    }

    fn user_skills(&self) -> impl Iterator<Item = &Skill> {
        self.skills.list.iter().filter(|s| is_command(s))
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
            "resume" => session::list(self.tools.root())
                .into_iter()
                .enumerate()
                .map(|(i, s)| ((i + 1).to_string(), format!("{}  {}", s.age(), s.title)))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn page(&self) -> u16 {
        self.log_area.height.saturating_sub(1).max(1)
    }

    fn recall_older(&mut self) {
        match self.recall {
            Some(n) if n + 1 < self.sent.len() => self.show_recalled(n + 1),
            Some(_) => {}
            // Don't clobber text being typed.
            None if self.input.is_empty() && !self.sent.is_empty() => self.show_recalled(0),
            None => {}
        }
    }

    fn recall_newer(&mut self) {
        match self.recall {
            Some(0) => {
                self.recall = None;
                self.input.clear();
                self.cursor = 0;
            }
            Some(n) => self.show_recalled(n - 1),
            None => {}
        }
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
        self.send_as(text.clone(), text);
    }

    /// `shown` goes to the log, `text` to the model.
    fn send_as(&mut self, shown: String, text: String) {
        self.push_log(Entry::User(shown));
        let content = match self.take_notices() {
            Some(notices) => format!("{notices}\n\n{text}"),
            None => text,
        };
        self.history.push(Message::User(content));
        self.persist(false);
        self.start_run();
    }

    fn send_notices(&mut self) {
        if let Some(notices) = self.take_notices() {
            self.history.push(Message::User(notices));
            self.persist(false);
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
        self.compacting = false;
        self.phase = (Phase::Waiting, Instant::now());
    }

    fn start_compact(&mut self, focus: Option<String>) {
        let Some(run) = self.new_run() else {
            self.log.push(Entry::Error(NO_PROVIDERS.into()));
            return;
        };
        self.running = Some(tokio::spawn(agent::compact(
            run,
            focus,
            self.tx.clone(),
            self.run_id,
        )));
        self.compacting = true;
        self.phase = (Phase::Waiting, Instant::now());
        // Compaction rewrites the history, so the next request misses the KV cache anyway: a fresh
        // map costs nothing extra then. Started now, it is ready before the next turn; the
        // compaction request itself already took the old one.
        self.build_map();
    }

    /// Context fill after the last response, if it crossed `auto_compact`.
    fn auto_compact_due(&self) -> Option<f32> {
        let threshold = self
            .active
            .as_ref()
            .and_then(|a| a.model.auto_compact)
            .or(self.config.auto_compact)?;
        let usage = self.usage.as_ref()?;
        let fill = usage.used as f32 / usage.window? as f32;
        (fill >= threshold).then_some(fill)
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
            skills: self.skill_catalog.clone(),
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
            Command::Map => self.build_map(),
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
            Command::Skills => {
                let msg = self.skills_status();
                self.log.push(Entry::Info(msg));
            }
            Command::Clear => {
                self.interrupt();
                self.history.clear();
                self.session = Some(Session::new(self.tools.root()));
                self.metrics.clear();
                self.cut = ContextCut::default();
                self.usage = None;
                self.log.clear();
            }
            Command::Resume(None) => {
                let list = session::list(self.tools.root());
                let msg = if list.is_empty() {
                    "no saved sessions".into()
                } else {
                    list.iter()
                        .enumerate()
                        .map(|(i, s)| format!("{:>2}  {:>9}  {}", i + 1, s.age(), s.title))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.log.push(Entry::Info(msg));
            }
            Command::Stats => {
                let msg = metrics::summary(&self.metrics);
                self.log.push(Entry::Info(msg));
            }
            Command::Resume(Some(arg)) => {
                let list = session::list(self.tools.root());
                let picked = arg
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| list.into_iter().nth(i));
                if self.is_running() {
                    self.log.push(Entry::Error(
                        "a run is active; wait for it or interrupt (Esc)".into(),
                    ));
                } else if let Some(s) = picked {
                    self.resume(s.path);
                } else {
                    self.log.push(Entry::Error(format!(
                        "no session `{arg}`; /resume lists them"
                    )));
                }
            }
            Command::Compact(focus) => {
                if self.is_running() {
                    self.log.push(Entry::Error(
                        "a run is active; wait for it or interrupt (Esc)".into(),
                    ));
                } else if self.history.is_empty() {
                    self.log.push(Entry::Info("nothing to compact".into()));
                } else {
                    self.start_compact(focus);
                }
            }
            Command::Help => self.log.push(Entry::Info(commands::HELP.into())),
            Command::Quit => self.quit = true,
            Command::Other { name, arg } => {
                let skills = Arc::clone(&self.skills);
                match skills.get(&name).filter(|s| is_command(s)) {
                    Some(skill) => self.invoke_skill(skill, arg),
                    None => {
                        let name = match arg {
                            Some(a) => format!("{name} {a}"),
                            None => name,
                        };
                        self.log
                            .push(Entry::Error(format!("unknown command /{name}; /help")))
                    }
                }
            }
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
                self.persist(false);
                self.cut = cut;
            }
            AgentEvent::Compacted(messages) => {
                self.history = messages;
                self.persist(true);
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
            AgentEvent::Metrics(mut stats) => {
                stats.model = self.model_label();
                self.metrics.push(stats);
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
                // Queued messages and notices wait for the compaction's own `Finished`.
                if !std::mem::take(&mut self.compacting)
                    && let Some(fill) = self.auto_compact_due()
                {
                    self.push_log(Entry::Info(format!(
                        "context {:.0}% full, compacting",
                        fill * 100.0
                    )));
                    self.start_compact(None);
                } else if !self.queued.is_empty() {
                    let next = self.queued.drain(..).collect::<Vec<_>>().join("\n\n");
                    self.send(next);
                } else {
                    self.send_notices();
                }
            }
        }
    }

    pub fn on_mcp(&mut self, servers: mcp::Connected) {
        let status: Vec<_> = servers
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
        let total = status.len();
        let ok = status.iter().filter(|(_, r)| r.is_ok()).count();
        let tools: usize = status.iter().filter_map(|(_, r)| r.as_ref().ok()).sum();
        self.mcp = Some(status);
        if total == 0 {
            return;
        }
        if ok == total {
            self.notify(FlashKind::Info, format!("mcp: {total} · {tools} tools"));
        } else {
            self.notify(
                FlashKind::Error,
                format!("mcp: {ok}/{total} · {tools} tools ✗"),
            );
        }
    }

    fn build_map(&self) {
        let root = self.tools.root().to_path_buf();
        let tx = self.map_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(repomap::build(&root));
        });
    }

    pub fn on_repo_map(&mut self, map: Option<repomap::RepoMap>) {
        let msg = match &map {
            Some(m) => format!("map: {} files", m.files),
            None => "map: no files".into(),
        };
        self.repo_map = map.map(|m| Arc::from(m.text));
        self.notify(FlashKind::Info, msg);
    }

    pub fn notify(&mut self, kind: FlashKind, text: impl Into<String>) {
        if self.flashes.is_empty() {
            self.flash_at = Instant::now();
        }
        self.flashes.push_back(Flash {
            text: text.into(),
            kind,
        });
    }

    pub fn flash(&self) -> Option<&Flash> {
        self.flashes.front()
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if !self.flashes.is_empty() && self.flash_at.elapsed() >= FLASH_TTL {
            self.flashes.pop_front();
            self.flash_at = Instant::now();
        }
    }

    fn invoke_skill(&mut self, skill: &Skill, arg: Option<String>) {
        if self.active.is_none() {
            self.log.push(Entry::Error(NO_PROVIDERS.into()));
            return;
        }
        if self.is_running() {
            self.log.push(Entry::Error(
                "a run is active; wait for it or interrupt (Esc)".into(),
            ));
            return;
        }
        let body = match skill.body() {
            Ok(b) => b,
            Err(e) => {
                self.log.push(Entry::Error(format!("{e:#}")));
                return;
            }
        };
        let mut text = format!("[skill `{}` invoked by the user]\n\n{body}", skill.name);
        let mut shown = format!("/{}", skill.name);
        if let Some(a) = arg {
            text.push_str(&format!("\n\nArguments: {a}"));
            shown.push_str(&format!(" {a}"));
        }
        self.send_as(shown, text);
    }

    fn skills_status(&self) -> String {
        let mut lines: Vec<String> = self
            .skills
            .list
            .iter()
            .map(|s| {
                let mut flags = Vec::new();
                if !s.model_invocable {
                    flags.push("user only".to_string());
                }
                if !s.user_invocable {
                    flags.push("model only".to_string());
                } else if commands::is_builtin(&s.name) {
                    flags.push(format!("/{} is built in", s.name));
                }
                let flags = match flags.is_empty() {
                    true => String::new(),
                    false => format!(" ({})", flags.join(", ")),
                };
                format!(
                    "{}{flags}: {}\n  {}",
                    s.name,
                    s.short_description(100),
                    s.dir.display()
                )
            })
            .collect();
        if lines.is_empty() {
            lines.push("skills: none found".into());
        }
        lines.extend(self.skills.warnings.iter().map(|w| format!("! {w}")));
        lines.join("\n")
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

/// With the kitty protocol a shortcut typed in a non-Latin layout arrives as that layout's char
/// (Ctrl+с, not Ctrl+c), and crossterm drops the base-layout key, so map Russian ЙЦУКЕН back.
fn latin_shortcut(mut key: KeyEvent) -> KeyEvent {
    const RU: &str = "йцукенгшщзфывапролдячсмитьбю";
    const EN: &str = "qwertyuiopasdfghjklzxcvbnm,.";
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && let KeyCode::Char(c) = key.code
        && let Some(i) = RU
            .chars()
            .position(|r| r == c.to_lowercase().next().unwrap_or(c))
    {
        key.code = KeyCode::Char(EN.as_bytes()[i] as char);
    }
    key
}
