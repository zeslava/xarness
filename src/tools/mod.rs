mod edit;
mod grep;
mod jobs;
mod ls;
mod read;
mod run;
mod skill;
mod symbol;
mod write;

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::domain::message::Mode;
use crate::domain::ports::{Tool, ToolSpec};
use crate::skills::Skills;

pub(crate) use skill::NAME as SKILL_TOOL;

/// Tools whose `path` goes through `resolve_read`.
const PATH_READERS: [&str; 4] = ["read_file", "read_symbol", "list_dir", "grep"];

pub struct ToolRegistry {
    root: PathBuf,
    tools: Vec<Arc<dyn Tool>>,
    jobs: Arc<jobs::Jobs>,
    /// Added once MCP servers connect, after startup.
    mcp: RwLock<Vec<Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new(root: PathBuf, skills: Arc<Skills>) -> Self {
        let jobs = Arc::new(jobs::Jobs::default());
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadFile),
            Arc::new(symbol::ReadSymbol),
            Arc::new(ls::ListDir),
            Arc::new(grep::Grep),
            Arc::new(write::WriteFile),
            Arc::new(edit::EditFile),
            Arc::new(run::RunCommand {
                jobs: Arc::clone(&jobs),
            }),
            Arc::new(jobs::JobTool {
                jobs: Arc::clone(&jobs),
            }),
        ];
        if !skills.list.is_empty() {
            tools.push(Arc::new(skill::SkillTool { skills }));
        }
        Self {
            root,
            tools,
            jobs,
            mcp: RwLock::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `tx` receives a report for each background job that exits.
    pub fn watch_jobs(&self, tx: UnboundedSender<String>) {
        self.jobs.notify(tx);
    }

    pub fn add_mcp(&self, tools: Vec<Arc<dyn Tool>>) {
        self.mcp
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(tools);
    }

    /// `allow` restricts the tools by name, for subagents.
    fn available(&self, mode: Mode, allow: Option<&[String]>) -> Vec<Arc<dyn Tool>> {
        let mcp = self.mcp.read().unwrap_or_else(PoisonError::into_inner);
        self.tools
            .iter()
            .chain(mcp.iter())
            .filter(|t| mode == Mode::Build || !t.mutating())
            .filter(|t| allow.is_none_or(|a| a.contains(&t.spec().name)))
            .cloned()
            .collect()
    }

    fn find(&self, mode: Mode, allow: Option<&[String]>, name: &str) -> Option<Arc<dyn Tool>> {
        self.available(mode, allow)
            .into_iter()
            .find(|t| t.spec().name == name)
    }

    pub fn specs(&self, mode: Mode, allow: Option<&[String]>) -> Vec<ToolSpec> {
        self.available(mode, allow)
            .iter()
            .map(|t| t.spec())
            .collect()
    }

    /// Unknown tools count as mutating.
    pub fn is_mutating(&self, name: &str) -> bool {
        self.find(Mode::Build, None, name)
            .is_none_or(|t| t.mutating())
    }

    /// Besides tools that always ask, a read-only tool asks when its `path` leaves the working
    /// directory (`resolve_read` lets it into the home directory).
    pub fn needs_approval(&self, name: &str, arguments: &str) -> bool {
        let Some(tool) = self.find(Mode::Build, None, name) else {
            return false;
        };
        tool.needs_approval()
            || (PATH_READERS.contains(&name)
                && serde_json::from_str::<Value>(arguments)
                    .ok()
                    .as_ref()
                    .and_then(|a| a.get("path")?.as_str())
                    .is_some_and(|p| !lexical(&self.root, p).starts_with(&self.root)))
    }

    pub async fn run(
        &self,
        mode: Mode,
        allow: Option<&[String]>,
        name: &str,
        arguments: &str,
    ) -> Result<String> {
        let tool = self.find(mode, allow, name).ok_or_else(|| match allow {
            Some(_) if self.find(mode, None, name).is_some() => {
                anyhow!("tool `{name}` is not available to this agent")
            }
            _ => anyhow!("tool `{name}` is not available in {} mode", mode.label()),
        })?;
        let args: Value = if arguments.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(arguments).map_err(|e| anyhow!("invalid arguments: {e}"))?
        };
        tool.run(args, &self.root).await
    }
}

pub(crate) fn parse<T: DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|e| anyhow!("invalid arguments: {e}"))
}

/// Lexical resolution: the target may not exist yet, so no canonicalize. Symlinks are not followed.
pub(crate) fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    let out = lexical(root, path);
    if !out.starts_with(root) {
        bail!(
            "path `{path}` is outside the working directory; only files inside it are accessible"
        );
    }
    Ok(out)
}

/// For read-only tools: the home directory is readable too, e.g. dependency sources in
/// `~/.cargo/registry` or `~/go/pkg/mod`. Writes stay inside the working directory.
pub(crate) fn resolve_read(root: &Path, path: &str) -> Result<PathBuf> {
    let out = lexical(root, path);
    let home = dirs::home_dir();
    if !out.starts_with(root) && !home.is_some_and(|h| out.starts_with(h)) {
        bail!("path `{path}` is outside the working directory and the home directory");
    }
    Ok(out)
}

/// Relative paths start at `root`; a leading `~` is the home directory.
fn lexical(root: &Path, path: &str) -> PathBuf {
    let (mut out, rest) = match (path.strip_prefix('~'), dirs::home_dir()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            (home, rest.trim_start_matches('/'))
        }
        _ => (root.to_path_buf(), path),
    };
    for c in Path::new(rest).components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out = PathBuf::from(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(p) => out.push(p),
        }
    }
    out
}

pub(crate) fn clip(line: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &line[..i]).into(),
        None => line.into(),
    }
}

pub(crate) fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
