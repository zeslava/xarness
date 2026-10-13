mod edit;
mod grep;
mod ls;
mod read;
mod symbol;
mod write;

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::domain::message::Mode;
use crate::domain::ports::{Tool, ToolSpec};

pub struct ToolRegistry {
    root: PathBuf,
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            tools: vec![
                Box::new(read::ReadFile),
                Box::new(symbol::ReadSymbol),
                Box::new(ls::ListDir),
                Box::new(grep::Grep),
                Box::new(write::WriteFile),
                Box::new(edit::EditFile),
            ],
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn available(&self, mode: Mode) -> impl Iterator<Item = &dyn Tool> {
        self.tools
            .iter()
            .map(|t| t.as_ref())
            .filter(move |t| mode == Mode::Build || !t.mutating())
    }

    pub fn specs(&self, mode: Mode) -> Vec<ToolSpec> {
        self.available(mode).map(|t| t.spec()).collect()
    }

    pub fn run(&self, mode: Mode, name: &str, arguments: &str) -> Result<String> {
        let tool = self
            .available(mode)
            .find(|t| t.spec().name == name)
            .ok_or_else(|| anyhow!("tool `{name}` is not available in {} mode", mode.label()))?;
        let args: Value = if arguments.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(arguments).map_err(|e| anyhow!("invalid arguments: {e}"))?
        };
        tool.run(args, &self.root)
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
