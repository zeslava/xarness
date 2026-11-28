//! Lazy, workspace-scoped stdio LSP connections. The reader never waits for a client request,
//! so servers can ask for configuration or register watchers even during initialize.
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobMatcher};
use lsp_types::{
    Position, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};

use crate::config::{LspServerConfig, validate_lsp};

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 32 * 1024 * 1024;
const LOG_BYTES: usize = 16 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

type LaunchResult = std::result::Result<Arc<Connection>, String>;
enum State {
    Starting(watch::Receiver<Option<LaunchResult>>),
    Ready(Arc<Connection>),
    Failed(String),
}
struct Slot {
    state: Mutex<State>,
}

pub struct Manager {
    servers: BTreeMap<String, LspServerConfig>,
    slots: Mutex<BTreeMap<(String, PathBuf), Arc<Slot>>>,
}

impl Manager {
    pub fn new(servers: BTreeMap<String, LspServerConfig>) -> Result<Self> {
        validate_lsp(&servers)?;
        Ok(Self {
            servers,
            slots: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn enabled(&self) -> bool {
        self.servers.values().any(|c| c.enabled)
    }

    pub async fn connection(&self, root: &Path, path: &Path) -> Result<Arc<Connection>> {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let (name, cfg) = self
            .servers
            .iter()
            .find(|(_, c)| c.enabled && c.filetypes.contains_key(ext))
            .ok_or_else(|| anyhow!("LSP unavailable: no enabled server for extension `{ext}`"))?;
        let mut rx = {
            let mut slots = lock(&self.slots);
            let key = (name.clone(), root.to_path_buf());
            if let Some(slot) = slots.get(&key) {
                match &*lock(&slot.state) {
                    State::Ready(c) if c.alive.load(Ordering::Acquire) => return Ok(c.clone()),
                    State::Starting(rx) => Some(rx.clone()),
                    _ => None,
                }
            } else {
                None
            }
            .unwrap_or_else(|| {
                let (tx, rx) = watch::channel(None);
                let slot = Arc::new(Slot {
                    state: Mutex::new(State::Starting(rx.clone())),
                });
                slots.insert(key, slot.clone());
                let (name, cfg, root) = (name.clone(), cfg.clone(), root.to_path_buf());
                // Initialization survives cancellation of any one tool waiting for it.
                tokio::spawn(async move {
                    let result =
                        match tokio::time::timeout(TIMEOUT, Connection::start(name, cfg, root))
                            .await
                        {
                            Ok(result) => {
                                result.map_err(|e| format!("LSP connection error: {e:#}"))
                            }
                            Err(_) => Err("LSP initialization timeout (30s)".into()),
                        };
                    *lock(&slot.state) = match &result {
                        Ok(c) => State::Ready(c.clone()),
                        Err(e) => State::Failed(e.clone()),
                    };
                    let _ = tx.send(Some(result));
                });
                rx
            })
        };
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result.map_err(anyhow::Error::msg);
            }
            rx.changed().await.context("LSP startup task stopped")?;
        }
    }

    pub fn status(&self) -> String {
        if self.servers.is_empty() {
            return "lsp: no servers configured".into();
        }
        let slots = lock(&self.slots);
        let mut lines = Vec::new();
        for (name, cfg) in &self.servers {
            if !cfg.enabled {
                lines.push(format!("lsp {name}: disabled"));
                continue;
            }
            let mut found = false;
            for ((server, root), slot) in slots.iter().filter(|((s, _), _)| s == name) {
                found = true;
                let status = match &*lock(&slot.state) {
                    State::Starting(_) => "initializing".into(),
                    State::Failed(e) => format!("failed; last error: {e}"),
                    State::Ready(c) => c.status(),
                };
                lines.push(format!("lsp {server} [{}]: {status}", root.display()));
            }
            if !found {
                lines.push(format!("lsp {name}: idle (starts on first request)"));
            }
        }
        lines.join("\n")
    }

    pub async fn shutdown(&self) {
        let receivers: Vec<_> = lock(&self.slots)
            .values()
            .filter_map(|slot| match &*lock(&slot.state) {
                State::Ready(c) => Some(Ok(c.clone())),
                State::Starting(rx) => Some(Err(rx.clone())),
                State::Failed(_) => None,
            })
            .collect();
        futures::future::join_all(receivers.into_iter().map(|state| async move {
            let connection = match state {
                Ok(c) => Some(c),
                Err(mut rx) => {
                    let _ = tokio::time::timeout(TIMEOUT, rx.changed()).await;
                    rx.borrow().as_ref().and_then(|r| r.as_ref().ok()).cloned()
                }
            };
            if let Some(c) = connection {
                c.close_documents().await;
                let _ = c
                    .request_with_timeout("shutdown", Value::Null, Duration::from_secs(2))
                    .await;
                let _ = c.notify("exit", Value::Null);
                // Give the exit notification time to reach the process before forced cleanup.
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    while c.alive.load(Ordering::Acquire) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
                let _ = c.stop.send(());
            }
        }))
        .await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

/// Public tool coordinates are one-based Unicode scalar counts; the protocol is zero-based.
pub fn to_position(text: &str, line: usize, column: usize, encoding: Encoding) -> Result<Position> {
    if line == 0 || column == 0 {
        bail!("line and column must be at least 1");
    }
    let source = text
        .split('\n')
        .nth(line - 1)
        .ok_or_else(|| anyhow!("line {line} is outside the file"))?
        .trim_end_matches('\r');
    if column - 1 > source.chars().count() {
        bail!("column {column} is outside line {line}");
    }
    let prefix: String = source.chars().take(column - 1).collect();
    Ok(Position::new(
        u32::try_from(line - 1)?,
        u32::try_from(match encoding {
            Encoding::Utf8 => prefix.len(),
            Encoding::Utf16 => prefix.encode_utf16().count(),
        })?,
    ))
}

pub fn from_position(text: &str, position: Position, encoding: Encoding) -> Result<(usize, usize)> {
    let line = text
        .split('\n')
        .nth(position.line as usize)
        .ok_or_else(|| anyhow!("server returned an invalid line"))?
        .trim_end_matches('\r');
    let mut units = 0;
    let mut chars = 0;
    for c in line.chars() {
        if units == position.character as usize {
            break;
        }
        units += match encoding {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
        };
        chars += 1;
    }
    if units != position.character as usize {
        bail!("server returned an invalid character boundary");
    }
    Ok((position.line as usize + 1, chars + 1))
}

struct Document {
    text: String,
    version: i32,
}
#[derive(Default)]
struct Info {
    capabilities: Option<ServerCapabilities>,
    last_error: Option<String>,
    stderr: String,
    progress: HashMap<String, String>,
    expects_server_status: bool,
    server_status: Option<ServerStatus>,
    registrations: HashMap<String, String>,
}
#[derive(serde::Deserialize)]
struct ServerStatus {
    health: String,
    quiescent: bool,
    message: Option<String>,
}

#[derive(Debug)]
struct ServerError(Value);

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LSP server error: {}", self.0)
    }
}
impl std::error::Error for ServerError {}
struct WatchPattern {
    base: PathBuf,
    matcher: GlobMatcher,
    kind: u32,
}

pub struct Connection {
    pub name: String,
    root: PathBuf,
    config: LspServerConfig,
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    sequence: AtomicU64,
    alive: AtomicBool,
    info: Mutex<Info>,
    activity: watch::Sender<()>,
    encoding: Mutex<Encoding>,
    documents: AsyncMutex<HashMap<PathBuf, Document>>,
    watchers: Mutex<HashMap<String, Vec<WatchPattern>>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    stop: mpsc::UnboundedSender<()>,
}

impl Connection {
    async fn start(name: String, config: LspServerConfig, root: PathBuf) -> Result<Arc<Self>> {
        let mut child = tokio::process::Command::new(&config.command)
            .args(&config.args)
            .envs(&config.env)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{}`", config.command))?;
        let (mut stdin, stdout, mut stderr) = (
            child.stdin.take().unwrap(),
            child.stdout.take().unwrap(),
            child.stderr.take().unwrap(),
        );
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel();
        let (stop, mut stop_rx) = mpsc::unbounded_channel();
        let c = Arc::new(Self {
            name,
            root,
            config,
            outgoing,
            pending: Mutex::default(),
            sequence: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            info: Mutex::default(),
            activity: watch::channel(()).0,
            encoding: Mutex::new(Encoding::Utf16),
            documents: AsyncMutex::default(),
            watchers: Mutex::default(),
            watcher: Mutex::default(),
            stop,
        });
        let weak = Arc::downgrade(&c);
        let mut reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let message = read_frame(&mut reader).await?;
                let Some(c) = weak.upgrade() else {
                    return Ok::<_, anyhow::Error>(());
                };
                c.incoming(message)?;
            }
        });
        let mut writer = tokio::spawn(async move {
            while let Some(message) = outgoing_rx.recv().await {
                write_frame(&mut stdin, &message).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let weak = Arc::downgrade(&c);
        let log = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let Some(c) = weak.upgrade() else {
                    break;
                };
                let mut info = lock(&c.info);
                info.stderr.push_str(&String::from_utf8_lossy(&buf[..n]));
                if info.stderr.len() > LOG_BYTES {
                    let mut cut = info.stderr.len() - LOG_BYTES;
                    while !info.stderr.is_char_boundary(cut) {
                        cut += 1;
                    }
                    info.stderr.drain(..cut);
                }
            }
        });
        let weak = Arc::downgrade(&c);
        tokio::spawn(async move {
            let error = tokio::select! {
                result = &mut reader => format!("LSP reader stopped: {}", task_error(result)),
                result = &mut writer => format!("LSP writer stopped: {}", task_error(result)),
                result = child.wait() => format!("LSP process exited: {result:?}"),
                _ = stop_rx.recv() => "LSP connection closed".into(),
            };
            if let Some(c) = weak.upgrade() {
                c.disconnected(error);
            }
            reader.abort();
            writer.abort();
            log.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
        });
        let root_uri = file_uri(&c.root)?;
        // The manager times the entire initialization, including process startup.
        let result = c.begin_request("initialize", json!({
            "processId": std::process::id(), "clientInfo": {"name":"xarness", "version":env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri, "workspaceFolders": [{"uri": root_uri, "name": c.root.file_name().and_then(|s| s.to_str()).unwrap_or("workspace")}],
            "initializationOptions": c.config.initialization_options,
            "capabilities": {
                "general": {"positionEncodings": ["utf-16", "utf-8"]},
                "workspace": {"configuration":true, "workspaceFolders":true, "didChangeWatchedFiles":{"dynamicRegistration":true,"relativePatternSupport":true}},
                "window": {"workDoneProgress":true},
                "experimental": {"serverStatusNotification":true},
                "textDocument": {"definition":{"linkSupport":true},"references":{},"synchronization":{"didSave":true}}
            }
        }))?.response().await?;
        let capabilities: ServerCapabilities =
            serde_json::from_value(result["capabilities"].clone())
                .context("invalid LSP capabilities")?;
        let encoding = match capabilities.position_encoding.as_ref().map(|e| e.as_str()) {
            None | Some("utf-16") => Encoding::Utf16,
            Some("utf-8") => Encoding::Utf8,
            Some(other) => bail!("unsupported LSP position encoding: {other}"),
        };
        *lock(&c.encoding) = encoding;
        {
            let mut info = lock(&c.info);
            info.capabilities = Some(capabilities);
            info.expects_server_status =
                result["serverInfo"]["name"].as_str() == Some("rust-analyzer");
        }
        c.notify("initialized", json!({}))?;
        c.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": c.config.settings}),
        )?;
        Ok(c)
    }

    pub fn encoding(&self) -> Encoding {
        *lock(&self.encoding)
    }

    pub fn supports(&self, method: &str) -> bool {
        let info = lock(&self.info);
        if info.registrations.values().any(|m| m == method) {
            return true;
        }
        let Some(c) = &info.capabilities else {
            return false;
        };
        match method {
            "textDocument/definition" => c
                .definition_provider
                .as_ref()
                .is_some_and(|v| !matches!(v, lsp_types::OneOf::Left(false))),
            "textDocument/references" => c
                .references_provider
                .as_ref()
                .is_some_and(|v| !matches!(v, lsp_types::OneOf::Left(false))),
            _ => false,
        }
    }

    fn status(&self) -> String {
        let capabilities = format!(
            "definition={}, references={}, encoding={:?}",
            self.supports("textDocument/definition"),
            self.supports("textDocument/references"),
            self.encoding()
        );
        let info = lock(&self.info);
        let mut status = format!(
            "{}; {capabilities}",
            if self.alive.load(Ordering::Acquire) {
                "connected"
            } else {
                "disconnected"
            }
        );
        if let Some(e) = &info.last_error {
            status.push_str(&format!("; last error: {e}"));
        }
        if let Some(server) = &info.server_status {
            status.push_str(&format!(
                "; health={}, quiescent={}",
                server.health, server.quiescent
            ));
            if let Some(message) = &server.message {
                status.push_str(&format!("; {message}"));
            }
        }
        if !info.progress.is_empty() {
            let mut progress: Vec<_> = info.progress.values().cloned().collect();
            progress.sort();
            status.push_str(&format!("; server progress: {}", progress.join("; ")));
        }
        if !info.stderr.is_empty() {
            status.push_str(&format!(
                "\nstderr (last {LOG_BYTES} bytes): {}",
                info.stderr.trim_end()
            ));
        }
        status
    }

    pub fn indexing(&self) -> Option<String> {
        let info = lock(&self.info);
        let mut progress: Vec<_> = info.progress.values().cloned().collect();
        progress.sort();
        if let Some(server) = &info.server_status {
            if !server.quiescent {
                progress.push("workspace loading".into());
            }
            if server.health != "ok" {
                progress.push(format!(
                    "health={}: {}",
                    server.health,
                    server.message.as_deref().unwrap_or("no details")
                ));
            }
        }
        (!progress.is_empty()).then(|| format!("server progress: {}", progress.join("; ")))
    }

    async fn wait_ready(&self) -> Result<()> {
        // Subscribe before checking state: a notification between the check and await must
        // wake every concurrent navigation caller, even if another caller is cancelled.
        let mut activity = self.activity.subscribe();
        loop {
            if !self.alive.load(Ordering::Acquire) {
                bail!("LSP connection error: server is disconnected");
            }
            {
                let info = lock(&self.info);
                let ready = match &info.server_status {
                    Some(status) => {
                        if status.quiescent && status.health == "error" {
                            bail!(
                                "LSP workspace error: {}",
                                status
                                    .message
                                    .as_deref()
                                    .unwrap_or("server is not functional")
                            );
                        }
                        status.quiescent
                    }
                    None => !info.expects_server_status,
                };
                if ready && info.progress.is_empty() {
                    return Ok(());
                }
            }
            activity
                .changed()
                .await
                .context("LSP readiness channel closed")?;
        }
    }

    fn disconnected(&self, error: String) {
        self.alive.store(false, Ordering::Release);
        lock(&self.info).last_error = Some(error.clone());
        self.activity.send_replace(());
        lock(&self.watcher).take();
        for (_, sender) in lock(&self.pending).drain() {
            let _ = sender.send(Err(anyhow!("LSP connection error: {error}")));
        }
    }

    fn send(&self, value: Value) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("LSP connection error: server is disconnected");
        }
        self.outgoing
            .send(value)
            .map_err(|_| anyhow!("LSP connection error: writer closed"))
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0", "method": method, "params": params}))
    }

    #[cfg(test)]
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, TIMEOUT).await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.begin_request(method, params)?.wait(timeout).await
    }

    fn begin_request(&self, method: &str, params: Value) -> Result<PendingRequest<'_>> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let guard = PendingRequest {
            connection: self,
            id,
            completed: false,
            method: method.into(),
            receiver: Some(rx),
        };
        self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
        Ok(guard)
    }

    fn incoming(self: &Arc<Self>, message: Value) -> Result<()> {
        if let Some(method) = message["method"].as_str() {
            if let Some(id) = message.get("id") {
                let response = match self.server_request(method, &message["params"]) {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(e) => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":e.to_string()}})
                    }
                };
                self.send(response)?;
            } else if method == "$/progress" {
                let p = &message["params"];
                let key = p["token"].to_string();
                let value = &p["value"];
                let mut info = lock(&self.info);
                match value["kind"].as_str() {
                    Some("begin") => {
                        info.progress
                            .insert(key, value["title"].as_str().unwrap_or("working").into());
                    }
                    Some("report") => {
                        if let Some(text) = value["message"].as_str() {
                            info.progress.insert(key, text.into());
                        }
                    }
                    Some("end") => {
                        info.progress.remove(&key);
                    }
                    _ => {}
                }
                drop(info);
                self.activity.send_replace(());
            } else if method == "experimental/serverStatus" {
                let status = serde_json::from_value(message["params"].clone())
                    .context("invalid LSP server status")?;
                lock(&self.info).server_status = Some(status);
                self.activity.send_replace(());
            }
        } else if let Some(id) = message["id"].as_u64()
            && let Some(tx) = lock(&self.pending).remove(&id)
        {
            let result = if let Some(error) = message.get("error") {
                Err(ServerError(error.clone()).into())
            } else if let Some(result) = message.get("result") {
                Ok(result.clone())
            } else {
                Err(anyhow!(
                    "LSP protocol error: response has neither result nor error"
                ))
            };
            let _ = tx.send(result);
        }
        Ok(())
    }

    fn server_request(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value> {
        match method {
            "workspace/configuration" => Ok(Value::Array(
                params["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|item| {
                        let section = item["section"].as_str().unwrap_or_default();
                        if section.is_empty() {
                            return self.config.settings.clone();
                        }
                        if let Some(v) = self.config.settings.get(section) {
                            return v.clone();
                        }
                        section
                            .split('.')
                            .fold(&self.config.settings, |v, part| &v[part])
                            .clone()
                    })
                    .collect(),
            )),
            "workspace/workspaceFolders" => Ok(
                json!([{"uri":file_uri(&self.root)?,"name":self.root.file_name().and_then(|s| s.to_str()).unwrap_or("workspace")}]),
            ),
            "workspace/applyEdit" => {
                Ok(json!({"applied":false,"failureReason":"xarness LSP tools are read-only"}))
            }
            "window/workDoneProgress/create" => Ok(Value::Null),
            "client/registerCapability" => {
                for r in params["registrations"].as_array().into_iter().flatten() {
                    let id = r["id"].as_str().context("registration without id")?;
                    let method = r["method"]
                        .as_str()
                        .context("registration without method")?;
                    if method == "workspace/didChangeWatchedFiles" {
                        self.register_watchers(id, &r["registerOptions"])?;
                    }
                    lock(&self.info)
                        .registrations
                        .insert(id.into(), method.into());
                }
                Ok(Value::Null)
            }
            "client/unregisterCapability" => {
                // The misspelling is part of the LSP wire format.
                for r in params["unregisterations"].as_array().into_iter().flatten() {
                    if let Some(id) = r["id"].as_str() {
                        lock(&self.watchers).remove(id);
                        lock(&self.info).registrations.remove(id);
                    }
                }
                Ok(Value::Null)
            }
            _ => bail!("unsupported client request: {method}"),
        }
    }

    fn register_watchers(self: &Arc<Self>, id: &str, options: &Value) -> Result<()> {
        let mut patterns = Vec::new();
        for w in options["watchers"]
            .as_array()
            .context("watch registration without watchers")?
        {
            let pattern = &w["globPattern"];
            let (base, glob) = if let Some(glob) = pattern.as_str() {
                (self.root.clone(), glob)
            } else {
                let base = pattern["baseUri"]
                    .as_str()
                    .or_else(|| pattern["baseUri"]["uri"].as_str())
                    .context("watcher without baseUri")?;
                (
                    Url::parse(base)?
                        .to_file_path()
                        .map_err(|_| anyhow!("non-file watch URI"))?,
                    pattern["pattern"]
                        .as_str()
                        .context("watcher without pattern")?,
                )
            };
            // Do not watch or read external directories as a side effect of a server request.
            if !base.starts_with(&self.root) {
                bail!("watcher base is outside the workspace");
            }
            patterns.push(WatchPattern {
                base,
                matcher: Glob::new(glob)?.compile_matcher(),
                kind: w["kind"].as_u64().unwrap_or(7) as u32,
            });
        }
        let mut watcher = lock(&self.watcher);
        if watcher.is_none() {
            let weak = Arc::downgrade(self);
            let mut native = RecommendedWatcher::new(
                move |event: notify::Result<Event>| {
                    if let Some(c) = weak.upgrade() {
                        match event {
                            Ok(event) => c.file_event(event),
                            Err(e) => lock(&c.info).last_error = Some(format!("LSP watcher: {e}")),
                        }
                    }
                },
                notify::Config::default().with_follow_symlinks(false),
            )?;
            native.watch(&self.root, RecursiveMode::Recursive)?;
            *watcher = Some(native);
        }
        lock(&self.watchers).insert(id.into(), patterns);
        Ok(())
    }

    fn file_event(&self, event: Event) {
        use notify::event::{ModifyKind, RenameMode};
        let events: Vec<_> = match event.kind {
            EventKind::Create(_) => event.paths.into_iter().map(|p| (p, 1)).collect(),
            EventKind::Remove(_) => event.paths.into_iter().map(|p| (p, 3)).collect(),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => event
                .paths
                .into_iter()
                .enumerate()
                .map(|(i, p)| (p, if i == 0 { 3 } else { 1 }))
                .collect(),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                event.paths.into_iter().map(|p| (p, 3)).collect()
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                event.paths.into_iter().map(|p| (p, 1)).collect()
            }
            EventKind::Modify(ModifyKind::Name(_)) => event
                .paths
                .into_iter()
                .map(|p| {
                    let kind = if p.exists() { 1 } else { 3 };
                    (p, kind)
                })
                .collect(),
            EventKind::Modify(_) => event.paths.into_iter().map(|p| (p, 2)).collect(),
            _ => return,
        };
        let patterns = lock(&self.watchers);
        let changes: Vec<_> = events
            .into_iter()
            .filter(|(p, kind)| {
                patterns.values().flatten().any(|w| {
                    let bit = 1 << (kind - 1);
                    w.kind & bit != 0
                        && p.strip_prefix(&w.base).is_ok_and(|relative| {
                            w.matcher.is_match(relative) || w.matcher.is_match(p)
                        })
                })
            })
            .filter_map(|(p, kind)| file_uri(&p).ok().map(|uri| json!({"uri":uri,"type":kind})))
            .collect();
        if !changes.is_empty() {
            let _ = self.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes": changes}),
            );
        }
    }

    /// Serialize document updates and the query enqueueing, so a parallel caller cannot
    /// replace the snapshot between a didChange and its corresponding semantic request.
    pub async fn query(
        &self,
        path: &Path,
        text: String,
        position: Position,
        method: &str,
    ) -> Result<Value> {
        if !self.supports(method) {
            bail!(
                "LSP unsupported capability: {} does not support {method}",
                self.name
            );
        }
        let mut activity = self.activity.subscribe();
        let result = tokio::time::timeout(TIMEOUT, async {
            loop {
                activity.borrow_and_update();
                self.wait_ready().await?;
                if activity.has_changed().unwrap_or(false) {
                    continue;
                }
                let result = self.query_once(path, text.clone(), position, method).await;
                if result
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<ServerError>())
                    .is_some_and(|e| e.0["code"] == -32801 || e.0["code"] == -32802)
                {
                    // A workspace snapshot can change after the readiness check.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                let result = result?;
                if activity.has_changed().unwrap_or(false) {
                    // Even a nonempty response can be incomplete if loading began while
                    // it was running. Repeat after the reported work settles.
                    continue;
                }
                return Ok(result);
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(anyhow!(
                "LSP navigation timeout (30s): {}",
                self.indexing().unwrap_or_else(|| {
                    "waiting for workspace readiness or navigation response".into()
                })
            ))
        });
        if let Err(e) = &result {
            lock(&self.info).last_error = Some(e.to_string());
        }
        result
    }

    async fn query_once(
        &self,
        path: &Path,
        text: String,
        position: Position,
        method: &str,
    ) -> Result<Value> {
        let mut docs = self.documents.lock().await;
        let current = std::fs::read_to_string(path)
            .with_context(|| format!("checking {} before LSP request", path.display()))?;
        if current != text {
            bail!(
                "file changed before LSP request; retry navigation for {}",
                path.display()
            );
        }
        let paths: Vec<_> = docs
            .keys()
            .filter(|p| p.as_path() != path)
            .cloned()
            .collect();
        for open in paths {
            match std::fs::read_to_string(&open) {
                Ok(text) => self.sync_document(&mut docs, &open, text)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if self.open_close() {
                        self.notify(
                            "textDocument/didClose",
                            json!({"textDocument":{"uri":file_uri(&open)?}}),
                        )?;
                    }
                    docs.remove(&open);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("synchronizing {}", open.display()));
                }
            }
        }
        self.sync_document(&mut docs, path, text)?;
        let mut params = json!({"textDocument":{"uri":file_uri(path)?},"position":position});
        if method == "textDocument/references" {
            params["context"] = json!({"includeDeclaration":false});
        }
        let request = self.begin_request(method, params)?;
        drop(docs);
        request.wait(TIMEOUT).await
    }

    fn open_close(&self) -> bool {
        match lock(&self.info)
            .capabilities
            .as_ref()
            .and_then(|c| c.text_document_sync.as_ref())
        {
            Some(TextDocumentSyncCapability::Kind(_)) => true,
            Some(TextDocumentSyncCapability::Options(o)) => o.open_close.unwrap_or(false),
            None => false,
        }
    }

    async fn close_documents(&self) {
        let mut docs = self.documents.lock().await;
        if self.open_close() {
            for path in docs.keys() {
                if let Ok(uri) = file_uri(path) {
                    let _ =
                        self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
                }
            }
        }
        docs.clear();
    }

    fn sync_document(
        &self,
        docs: &mut HashMap<PathBuf, Document>,
        path: &Path,
        text: String,
    ) -> Result<()> {
        let uri = file_uri(path)?;
        let (open_close, change, save) = {
            let info = lock(&self.info);
            match info
                .capabilities
                .as_ref()
                .and_then(|c| c.text_document_sync.as_ref())
            {
                Some(TextDocumentSyncCapability::Kind(kind)) => (true, *kind, None),
                Some(TextDocumentSyncCapability::Options(options)) => (
                    options.open_close.unwrap_or(false),
                    options.change.unwrap_or(TextDocumentSyncKind::NONE),
                    options.save.clone(),
                ),
                None => (false, TextDocumentSyncKind::NONE, None),
            }
        };
        let mut changed = false;
        if let Some(doc) = docs.get_mut(path) {
            if doc.text != text {
                changed = true;
                doc.version = doc
                    .version
                    .checked_add(1)
                    .context("LSP document version exhausted")?;
                if change != TextDocumentSyncKind::NONE {
                    let content = if change == TextDocumentSyncKind::INCREMENTAL {
                        let last_line = doc.text.split('\n').count();
                        let last_column = doc
                            .text
                            .split('\n')
                            .next_back()
                            .unwrap_or_default()
                            .trim_end_matches('\r')
                            .chars()
                            .count()
                            + 1;
                        json!({"range":{"start":Position::new(0,0),"end":to_position(&doc.text,last_line,last_column,self.encoding())?},"text":text})
                    } else {
                        json!({"text":text})
                    };
                    self.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":doc.version},"contentChanges":[content]}))?;
                }
                doc.text = text;
            }
        } else {
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if open_close {
                self.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":self.config.filetypes.get(ext).map(String::as_str).unwrap_or("plaintext"),"version":1,"text":text}}))?;
            }
            docs.insert(path.to_path_buf(), Document { text, version: 1 });
        }
        if changed && let Some(save) = save {
            let include = matches!(&save, lsp_types::TextDocumentSyncSaveOptions::SaveOptions(o) if o.include_text == Some(true));
            if !matches!(
                save,
                lsp_types::TextDocumentSyncSaveOptions::Supported(false)
            ) {
                let mut params = json!({"textDocument":{"uri":uri}});
                if include {
                    params["text"] = json!(docs[path].text);
                }
                self.notify("textDocument/didSave", params)?;
            }
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

struct PendingRequest<'a> {
    connection: &'a Connection,
    id: u64,
    completed: bool,
    method: String,
    receiver: Option<oneshot::Receiver<Result<Value>>>,
}
impl PendingRequest<'_> {
    async fn response(&mut self) -> Result<Value> {
        match self.receiver.take().unwrap().await {
            Ok(result) => {
                self.completed = true;
                result
            }
            Err(_) => Err(anyhow!("LSP connection error: response channel closed")),
        }
    }

    async fn wait(mut self, timeout: Duration) -> Result<Value> {
        let result = match tokio::time::timeout(timeout, self.response()).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "LSP request timeout: {} ({}s)",
                self.method,
                timeout.as_secs()
            )),
        };
        if let Err(e) = &result {
            lock(&self.connection.info).last_error = Some(e.to_string());
        }
        result
    }
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        lock(&self.connection.pending).remove(&self.id);
        if !self.completed {
            let _ = self
                .connection
                .notify("$/cancelRequest", json!({"id":self.id}));
        }
    }
}

fn task_error(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> String {
    match result {
        Ok(Ok(())) => "EOF".into(),
        Ok(Err(e)) => e.to_string(),
        Err(e) => e.to_string(),
    }
}

pub fn file_uri(path: &Path) -> Result<Url> {
    Url::from_file_path(path)
        .map_err(|_| anyhow!("cannot convert {} to a file URI", path.display()))
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("unexpected EOF in LSP headers");
        }
        header_bytes += n;
        if header_bytes > 8192 {
            bail!("LSP headers too large");
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').context("invalid LSP header")?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                bail!("duplicate LSP Content-Length");
            }
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing LSP Content-Length")?;
    if length > MAX_FRAME {
        bail!("LSP message exceeds {MAX_FRAME} bytes");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
#[path = "lsp_tests.rs"]
pub(crate) mod tests;
