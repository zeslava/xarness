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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::domain::message::Access;
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
        // Built-in skills are for the user and have no files to read.
        if skills.list.iter().any(|s| s.builtin.is_none()) {
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

    /// The same tools working in `root`, for a subagent sent to another directory. MCP tools
    /// connected later are not added to it, which is fine for a registry that lives one call.
    pub fn rooted(&self, root: PathBuf) -> Self {
        Self {
            root,
            tools: self.tools.clone(),
            jobs: Arc::clone(&self.jobs),
            mcp: RwLock::new(
                self.mcp
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
            ),
        }
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
    fn available(&self, access: Access, allow: Option<&[String]>) -> Vec<Arc<dyn Tool>> {
        let mcp = self.mcp.read().unwrap_or_else(PoisonError::into_inner);
        self.tools
            .iter()
            .filter(|t| access != Access::ReadOnly || !t.mutating())
            .chain(mcp.iter())
            .filter(|t| allow.is_none_or(|a| a.contains(&t.spec().name)))
            .cloned()
            .collect()
    }

    fn find(&self, access: Access, allow: Option<&[String]>, name: &str) -> Option<Arc<dyn Tool>> {
        self.available(access, allow)
            .into_iter()
            .find(|t| t.spec().name == name)
    }

    pub fn specs(&self, access: Access, allow: Option<&[String]>) -> Vec<ToolSpec> {
        self.available(access, allow)
            .iter()
            .map(|t| t.spec())
            .collect()
    }

    /// Unknown tools count as mutating.
    pub fn is_mutating(&self, name: &str) -> bool {
        self.find(Access::Full, None, name)
            .is_none_or(|t| t.mutating())
    }

    /// Besides tools that always ask, a read-only tool asks when its `path` leaves the working
    /// directory, and a mutating tool asks unless access is full (only MCP tools are offered
    /// to a read-only run).
    pub fn needs_approval(&self, access: Access, name: &str, arguments: &str) -> bool {
        let Some(tool) = self.find(Access::Full, None, name) else {
            return false;
        };
        tool.needs_approval()
            || (access != Access::Full && tool.mutating())
            || (PATH_READERS.contains(&name)
                && serde_json::from_str::<Value>(arguments)
                    .ok()
                    .as_ref()
                    .and_then(|a| a.get("path")?.as_str())
                    .is_some_and(|p| !lexical(&self.root, p).starts_with(&self.root)))
    }

    pub async fn run(
        &self,
        access: Access,
        allow: Option<&[String]>,
        name: &str,
        arguments: &str,
    ) -> Result<String> {
        let tool = self.find(access, allow, name).ok_or_else(|| match allow {
            Some(_) if self.find(access, None, name).is_some() => {
                anyhow!("tool `{name}` is not available to this agent")
            }
            _ if self.find(Access::Full, None, name).is_some() => {
                anyhow!("tool `{name}` is not available to a read-only agent")
            }
            _ => anyhow!("unknown tool `{name}`"),
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

/// For read-only tools: any path is readable, e.g. dependency sources in `~/.cargo/registry` or
/// `~/go/pkg/mod`; outside the working directory the call waits for approval
/// (`ToolRegistry::needs_approval`). Writes stay inside the working directory.
pub(crate) fn resolve_read(root: &Path, path: &str) -> Result<PathBuf> {
    Ok(lexical(root, path))
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

fn spill_dir(root: &Path) -> PathBuf {
    root.join(".xarness").join("tmp")
}

/// Saves the full text of an output that is shown cut, so the model can read the rest in parts
/// instead of running the command again. Returns the path relative to `root`; `None` if the
/// write fails.
pub(crate) fn spill(root: &Path, text: &str) -> Option<String> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = spill_dir(root);
    std::fs::create_dir_all(&dir).ok()?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "*\n").ok()?;
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = dir.join(format!(
        "{millis}-{}.txt",
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, text).ok()?;
    Some(display(root, &path))
}

/// Spilled outputs only matter to the conversation that produced them.
pub(crate) fn clear_spills(root: &Path) {
    let _ = std::fs::remove_dir_all(spill_dir(root));
}

pub(crate) fn spilled_note(path: &str) -> String {
    format!(
        "full output saved to {path}: read it in parts with read_file or search it with grep \
         instead of repeating the call"
    )
}

/// Where a command's output that was shown cut can be read whole.
fn command_note(root: &Path, text: &str) -> String {
    match spill(root, text) {
        Some(p) => spilled_note(&p),
        None => "to read it all, redirect the command to a file and read that with read_file \
                 in parts"
            .into(),
    }
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
