mod edit;
mod grep;
mod jobs;
mod ls;
mod read;
mod run;
mod symbol;
mod write;

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::domain::message::Mode;
use crate::domain::ports::{Tool, ToolSpec};

pub struct ToolRegistry {
    root: PathBuf,
    tools: Vec<Arc<dyn Tool>>,
    /// Added once MCP servers connect, after startup.
    mcp: RwLock<Vec<Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new(root: PathBuf) -> Self {
        let jobs = Arc::new(jobs::Jobs::default());
        Self {
            root,
            tools: vec![
                Arc::new(read::ReadFile),
                Arc::new(symbol::ReadSymbol),
                Arc::new(ls::ListDir),
                Arc::new(grep::Grep),
                Arc::new(write::WriteFile),
                Arc::new(edit::EditFile),
                Arc::new(run::RunCommand {
                    jobs: Arc::clone(&jobs),
                }),
                Arc::new(jobs::JobTool { jobs }),
            ],
            mcp: RwLock::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn add_mcp(&self, tools: Vec<Arc<dyn Tool>>) {
        self.mcp
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(tools);
    }

    fn available(&self, mode: Mode) -> Vec<Arc<dyn Tool>> {
        let mcp = self.mcp.read().unwrap_or_else(PoisonError::into_inner);
        self.tools
            .iter()
            .chain(mcp.iter())
            .filter(|t| mode == Mode::Build || !t.mutating())
            .cloned()
            .collect()
    }

    fn find(&self, mode: Mode, name: &str) -> Option<Arc<dyn Tool>> {
        self.available(mode)
            .into_iter()
            .find(|t| t.spec().name == name)
    }

    pub fn specs(&self, mode: Mode) -> Vec<ToolSpec> {
        self.available(mode).iter().map(|t| t.spec()).collect()
    }

    pub fn needs_approval(&self, name: &str) -> bool {
        self.find(Mode::Build, name)
            .is_some_and(|t| t.needs_approval())
    }

    pub async fn run(&self, mode: Mode, name: &str, arguments: &str) -> Result<String> {
        let tool = self
            .find(mode, name)
            .ok_or_else(|| anyhow!("tool `{name}` is not available in {} mode", mode.label()))?;
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
    let mut out = root.to_path_buf();
    for c in Path::new(path).components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out = PathBuf::from(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(p) => out.push(p),
        }
    }
    if !out.starts_with(root) {
        bail!("path `{path}` is outside the working directory");
    }
    Ok(out)
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
