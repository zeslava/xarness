use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::{AgentConfig, ModelConfig};
use crate::domain::ports::{LlmProvider, ToolSpec};
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
    /// `None` for built-ins.
    pub path: Option<PathBuf>,
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
}

const DESCRIPTION_MAX: usize = 300;

const EXPLORE_DESCRIPTION: &str = "Read-only codebase search: finds where something is defined, \
     used or handled and explains how the pieces fit together. Give it a specific question; it \
     returns paths, symbols and a short summary instead of whole files.";
const EXPLORE_PROMPT: &str = "You explore a codebase to answer the question in the task. Search \
     and read with the tools, then reply with what you found: the relevant paths and symbols and a \
     short explanation of how they fit together, quoting code verbatim only where it matters. Do \
     not paste whole files. If you could not find something, say so and list where you looked.";

fn explore() -> Subagent {
    Subagent {
        name: "explore".into(),
        description: EXPLORE_DESCRIPTION.into(),
        prompt: EXPLORE_PROMPT.into(),
        tools: Some(
            ["read_file", "read_symbol", "list_dir", "grep"]
                .map(String::from)
                .to_vec(),
        ),
        model: None,
        target: None,
        think: None,
        extra: None,
        path: None,
    }
}

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

    /// Earlier dirs win on a name clash; built-ins come last.
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
        found.push(explore());
        for mut a in found {
            if let Some(first) = agents.get(&a.name) {
                let place = |p: &Option<PathBuf>| match p {
                    Some(p) => p.display().to_string(),
                    None => "built-in".into(),
                };
                agents.warnings.push(format!(
                    "agent `{}` in {} is shadowed by {}",
                    a.name,
                    place(&a.path),
                    place(&first.path)
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
        Some(ToolSpec {
            name: TOOL.into(),
            description: format!(
                "Delegate a self-contained task to a subagent. It starts with a fresh context and \
                 none of this conversation, works with its own tools and returns only its final \
                 report, which keeps your context small. Write the task so it stands on its own: \
                 the goal, relevant paths and names, and what to report back.{parallel}\
                 \n\nAgents:\n{list}"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": self.list.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
                    },
                    "task": { "type": "string", "description": "Complete instructions for the subagent" }
                },
                "required": ["agent", "task"]
            }),
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
        path: Some(path.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_shadow_builtins_and_config_applies() {
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
        // The second `review` is shadowed and `p/m` is unknown; disabled `explore`s are skipped.
        assert_eq!(agents.warnings.len(), 2);
    }
}
