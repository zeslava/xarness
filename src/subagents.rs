use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::{AgentConfig, ModelConfig};
use crate::domain::ports::{LlmProvider, ToolSpec};
use crate::repomap;
use crate::skills::{parse_frontmatter, split_frontmatter};
use crate::tools::clip;

pub const TOOL: &str = "agent";

/// A `<dir>/<name>.md` file: frontmatter (`name`, `description`, `tools`), then the system
/// prompt. Model settings come from `agents.<name>` in config.
pub struct Subagent {
    pub name: String,
    pub description: String,
    pub prompt: String,
    /// Tools it may use; `None` = all of the caller's. `agent` is never among them.
    pub tools: Option<Vec<String>>,
    /// `provider/model` from config, resolved into `target` by the TUI.
    pub model: Option<String>,
    pub target: Option<Target>,
    pub think: Option<String>,
    pub extra: Option<Value>,
    pub path: PathBuf,
}

pub struct Target {
    /// `provider/model`, for metrics.
    pub label: String,
    pub provider: Arc<dyn LlmProvider>,
    pub model: ModelConfig,
}

#[derive(Default)]
pub struct Subagents {
    pub list: Vec<Arc<Subagent>>,
    /// Definitions that failed to load, are shadowed, or name an unknown model.
    pub warnings: Vec<String>,
    /// Consecutive calls run in parallel rather than one by one.
    pub parallel: bool,
    /// Context of the directories subagents were sent to, built once per session so repeated
    /// calls send a byte-identical prompt.
    dirs: Mutex<HashMap<PathBuf, Arc<DirContext>>>,
}

/// What a subagent sent to `dir` sees instead of the caller's AGENTS.md and repo map.
pub struct DirContext {
    pub instructions: Option<Arc<str>>,
    pub repo_map: Option<Arc<str>>,
}

const DESCRIPTION_MAX: usize = 300;

impl Subagents {
    /// `resolve` turns a configured `provider/model` into a target.
    pub fn discover(
        root: &Path,
        cfg: &BTreeMap<String, AgentConfig>,
        resolve: impl Fn(&str) -> Result<Target>,
    ) -> Self {
        let mut dirs = vec![root.join(".xarness/agents")];
        if let Some(home) = dirs::home_dir() {
            let global = home.join(".config/xarness/agents");
            // Launched from $HOME, project and global dirs coincide.
            if !dirs.contains(&global) {
                dirs.push(global);
            }
        }
        Self::discover_in(&dirs, cfg, resolve)
    }

    /// Earlier dirs win on a name clash.
    fn discover_in(
        dirs: &[PathBuf],
        cfg: &BTreeMap<String, AgentConfig>,
        resolve: impl Fn(&str) -> Result<Target>,
    ) -> Self {
        let mut agents = Self::default();
        let mut found = Vec::new();
        for dir in dirs {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(|e| Some(e.ok()?.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "md") && p.is_file())
                .collect();
            paths.sort();
            for path in paths {
                match load(&path) {
                    Ok(a) => found.push(a),
                    Err(e) => agents.warnings.push(format!("{}: {e:#}", path.display())),
                }
            }
        }
        for mut a in found {
            if let Some(first) = agents.get(&a.name) {
                agents.warnings.push(format!(
                    "agent `{}` in {} is shadowed by {}",
                    a.name,
                    a.path.display(),
                    first.path.display()
                ));
                continue;
            }
            if let Some(c) = cfg.get(&a.name) {
                if !c.enabled {
                    continue;
                }
                a.model = c.model.clone();
                a.think = c.think.clone();
                a.extra = c.extra.clone();
            }
            if let Some(spec) = &a.model {
                match resolve(spec) {
                    Ok(t) => a.target = Some(t),
                    Err(e) => agents.warnings.push(format!(
                        "agent `{}`: {e:#}; it runs on the caller's model",
                        a.name
                    )),
                }
            }
            agents.list.push(Arc::new(a));
        }
        agents
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Subagent>> {
        self.list.iter().find(|a| a.name == name)
    }

    pub async fn dir_context(&self, dir: &Path) -> Arc<DirContext> {
        let cached = self
            .dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(dir)
            .cloned();
        if let Some(c) = cached {
            return c;
        }
        let root = dir.to_path_buf();
        let built = tokio::task::spawn_blocking(move || DirContext {
            instructions: fs::read_to_string(root.join("AGENTS.md"))
                .ok()
                .map(Arc::from),
            repo_map: repomap::build(&root).map(|m| Arc::from(m.text)),
        })
        .await
        .unwrap_or(DirContext {
            instructions: None,
            repo_map: None,
        });
        // A parallel call may have built it meanwhile; keep the first so prompts stay identical.
        let mut dirs = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(dirs.entry(dir.to_path_buf()).or_insert(Arc::new(built)))
    }

    /// The `agent` tool, with the agents listed in its description. Built once per session like
    /// the other specs, so the request prefix stays cacheable.
    pub fn spec(&self) -> Option<ToolSpec> {
        if self.list.is_empty() {
            return None;
        }
        let list: String = self
            .list
            .iter()
            .map(|a| format!("- {}: {}\n", a.name, a.short_description()))
            .collect();
        let parallel = if self.parallel {
            " Several calls in one response run in parallel."
        } else {
            ""
        };
        let mut parameters = json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": self.list.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
                },
                "task": { "type": "string", "description": "Complete instructions for the subagent" },
                "dir": {
                    "type": "string",
                    "description": "Directory to work in, for a task about one project: the \
                        subagent's paths are relative to it and it gets that project's \
                        AGENTS.md and repo map. Outside your working directory it is \
                        read-only and the call needs the user's approval. Omit to work in \
                        your directory."
                }
            },
            "required": ["agent", "task"]
        });
        // Running beside the caller needs a server that takes parallel requests too.
        if self.parallel {
            parameters["properties"]["background"] = json!({
                "type": "boolean",
                "description": "Return at once and keep working while the subagent runs; its \
                    report arrives later as a new message. Use it when you do not need the \
                    result to continue."
            });
        }
        Some(ToolSpec {
            name: TOOL.into(),
            description: format!(
                "Delegate a self-contained task to a subagent. It starts with a fresh context and \
                 none of this conversation, works with its own tools and returns only its final \
                 report, which keeps your context small. Write the task so it stands on its own: \
                 the goal, relevant paths and names, and what to report back.{parallel}\
                 \n\nAgents:\n{list}"
            ),
            parameters,
        })
    }
}

impl Subagent {
    pub fn short_description(&self) -> String {
        let line = self
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        clip(&line, DESCRIPTION_MAX).into_owned()
    }

    pub fn allows(&self, tool: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|t| t.iter().any(|n| n == tool))
    }
}

fn load(path: &Path) -> Result<Subagent> {
    let text = fs::read_to_string(path).context("reading")?;
    let (front, body) = split_frontmatter(&text);
    let fields = parse_frontmatter(front.ok_or_else(|| anyhow!("no frontmatter"))?);
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let description = get("description")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| anyhow!("no description"))?;
    let name = match get("name") {
        Some(n) => n.to_string(),
        None => path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if name.is_empty() || name.contains(char::is_whitespace) {
        bail!("invalid name `{name}`");
    }
    // `a, b`, `a b` or a flow list `[a, b]`.
    let tools = get("tools").map(|t| {
        t.trim_matches(['[', ']'])
            .split([',', ' '])
            .map(|n| n.trim().trim_matches(['"', '\'']))
            .filter(|n| !n.is_empty())
            .map(String::from)
            .collect()
    });
    Ok(Subagent {
        name,
        description: description.to_string(),
        prompt: body.trim().to_string(),
        tools,
        model: None,
        target: None,
        think: None,
        extra: None,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earlier_dirs_shadow_later_and_config_applies() {
        let dir = std::env::temp_dir().join(format!("xarness-agents-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(
            a.join("review.md"),
            "---\ndescription: Reviews\ntools: [read_file, grep]\n---\nBe strict.\n",
        )
        .unwrap();
        fs::write(b.join("review.md"), "---\ndescription: Other\n---\n").unwrap();
        fs::write(b.join("explore.md"), "---\ndescription: Mine\n---\nLook.\n").unwrap();
        let cfg = serde_json::from_value(json!({
            "review": { "model": "p/m", "think": "off" },
            "explore": { "enabled": false }
        }))
        .unwrap();
        let agents = Subagents::discover_in(&[a, b], &cfg, |spec| bail!("no model `{spec}`"));
        fs::remove_dir_all(&dir).unwrap();

        let names: Vec<_> = agents.list.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["review"]);
        let review = &agents.list[0];
        assert_eq!(review.prompt, "Be strict.");
        assert_eq!(
            review.tools.as_deref(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert_eq!(review.model.as_deref(), Some("p/m"));
        assert!(review.allows("grep") && !review.allows("run"));
        assert!(review.target.is_none());
        // The second `review` is shadowed and `p/m` is unknown; disabled `explore` is skipped.
        assert_eq!(agents.warnings.len(), 2);
    }
}
