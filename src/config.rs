use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub default: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Fraction of the context window (e.g. `0.8`) at which history is compacted after a turn;
    /// absent = off.
    pub auto_compact: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Other agents whose skill dirs are searched too, as `.<name>/skills` in the project and home.
    pub compat: Vec<String>,
    /// Skill names to skip.
    pub disabled: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            compat: vec!["agents".into(), "claude".into()],
            disabled: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    Openai,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    /// Inherited by models that set no `think` of their own.
    pub think: Option<ThinkConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: Option<String>,
    pub max_tokens: Option<u32>,
    /// Model context size in tokens; when set, old history is trimmed to fit.
    pub context_window: Option<u32>,
    pub temperature: Option<f32>,
    pub think: Option<ThinkConfig>,
    /// Overrides the top-level `auto_compact` for this model.
    pub auto_compact: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "ThinkSpec")]
pub struct ThinkConfig {
    /// Level used when the model is selected; absent = server default.
    pub default: Option<String>,
    /// Level name → fields merged into the request body as-is, since every
    /// server/model spells reasoning control differently.
    pub levels: IndexMap<String, Value>,
}

/// `think` is a full `{ default, levels }` table, the name of a built-in preset, or a list of
/// `reasoning_effort` values.
#[derive(Deserialize)]
#[serde(untagged)]
enum ThinkSpec {
    Preset(String),
    Efforts(Vec<String>),
    Full {
        default: Option<String>,
        levels: IndexMap<String, Value>,
    },
}

impl TryFrom<ThinkSpec> for ThinkConfig {
    type Error = String;

    fn try_from(spec: ThinkSpec) -> Result<Self, String> {
        Ok(match spec {
            ThinkSpec::Full { default, levels } => Self { default, levels },
            ThinkSpec::Preset(name) => Self {
                default: None,
                levels: think_preset(&name)
                    .ok_or_else(|| format!("unknown think preset `{name}`; available: qwen"))?,
            },
            ThinkSpec::Efforts(efforts) => Self {
                default: None,
                levels: efforts
                    .into_iter()
                    .map(|e| {
                        let patch = json!({ "reasoning_effort": e });
                        (e, patch)
                    })
                    .collect(),
            },
        })
    }
}

fn think_preset(name: &str) -> Option<IndexMap<String, Value>> {
    match name {
        // llama.cpp chat templates with a thinking switch (Qwen3 and alike).
        "qwen" => Some(
            [("on", true), ("off", false)]
                .into_iter()
                .map(|(l, on)| {
                    let patch = json!({ "chat_template_kwargs": { "enable_thinking": on } });
                    (l.to_string(), patch)
                })
                .collect(),
        ),
        _ => None,
    }
}

/// One MCP server: `command` (stdio) or `url` (streamable HTTP), checked when connecting.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Header name → environment variable holding its value, to keep secrets out of config.
    #[serde(default)]
    pub headers_env: BTreeMap<String, String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub approval: McpApproval,
    /// Exact names of the server's tools to expose; absent = all.
    pub tools: Option<Vec<String>>,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpApproval {
    /// Ask only for tools not annotated read-only.
    #[default]
    Auto,
    Always,
    Never,
}

impl ProviderConfig {
    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key.clone().or_else(|| {
            self.api_key_env
                .as_ref()
                .and_then(|v| std::env::var(v).ok())
        })
    }

    /// `m` with provider-wide settings filled in where it has none.
    fn model(&self, m: &ModelConfig) -> ModelConfig {
        let mut m = m.clone();
        if m.think.is_none() {
            m.think = self.think.clone();
        }
        m
    }
}

/// Written to the global layer on first run when no config exists anywhere.
const DEFAULT_CONFIG: &str = "{\n  \"providers\": {}\n}\n";

impl Config {
    /// Merges the global, project and local layers, later ones overriding earlier ones key by
    /// key. Objects merge recursively; anything else, arrays included, is replaced, so a layer
    /// that redefines a provider's `models` replaces the whole list.
    pub fn load() -> Result<Self> {
        // Not `dirs::config_dir()`: on macOS that is `~/Library/Application Support`.
        let global = dirs::home_dir().map(|h| h.join(".config/xarness"));
        let layers = [
            global
                .iter()
                .flat_map(|d| [d.join("config.toml"), d.join("config.json")])
                .collect(),
            [".xarness/config.toml", ".xarness/config.json"]
                .map(PathBuf::from)
                .to_vec(),
            [".xarness/config.local.toml", ".xarness/config.local.json"]
                .map(PathBuf::from)
                .to_vec(),
        ];
        let mut merged: Option<Value> = None;
        for path in layers
            .iter()
            .filter_map(|candidates: &Vec<PathBuf>| candidates.iter().find(|p| p.is_file()))
        {
            let layer = read_layer(path)?;
            match &mut merged {
                Some(m) => merge(m, layer),
                None => merged = Some(layer),
            }
        }
        let merged = merged.unwrap_or_else(|| {
            // Best effort: the template only helps the user get started, a failure to write it
            // must not stop the app from launching with no providers.
            if let Some(dir) = &global {
                let _ = std::fs::create_dir_all(dir)
                    .and_then(|()| std::fs::write(dir.join("config.json"), DEFAULT_CONFIG));
            }
            Value::Object(Default::default())
        });
        serde_json::from_value(merged).context("invalid config")
    }

    /// `spec` is `provider/model`; the model id itself may contain `/` (e.g. litellm's `openai/gpt-4o`).
    pub fn resolve_model(&self, spec: &str) -> Result<(String, ModelConfig)> {
        let (provider, model) = spec
            .split_once('/')
            .ok_or_else(|| anyhow!("expected provider/model, got `{spec}`"))?;
        let p = self
            .providers
            .get(provider)
            .ok_or_else(|| anyhow!("unknown provider `{provider}`"))?;
        let m = p
            .models
            .iter()
            .find(|m| m.id == model)
            .ok_or_else(|| anyhow!("unknown model `{model}` in provider `{provider}`"))?;
        Ok((provider.to_string(), p.model(m)))
    }

    /// `None` when no models are configured at all.
    pub fn default_model(&self) -> Result<Option<(String, ModelConfig)>> {
        if let Some(spec) = &self.default {
            return self.resolve_model(spec).map(Some);
        }
        Ok(self
            .providers
            .iter()
            .find_map(|(name, p)| p.models.first().map(|m| (name.clone(), p.model(m)))))
    }

    pub fn all_models(&self) -> impl Iterator<Item = String> + '_ {
        self.providers
            .iter()
            .flat_map(|(p, cfg)| cfg.models.iter().map(move |m| format!("{p}/{}", m.id)))
    }
}

fn read_layer(path: &Path) -> Result<Value> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = if path.extension().is_some_and(|e| e == "json") {
        serde_json::from_str(&raw).map_err(anyhow::Error::from)
    } else {
        toml::from_str(&raw).map_err(anyhow::Error::from)
    };
    parsed.with_context(|| format!("parsing {}", path.display()))
}

fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_overrides_by_key() {
        let mut base = json!({
            "default": "a/x",
            "providers": { "a": { "base_url": "http://a", "models": [{ "id": "x" }, { "id": "y" }] } }
        });
        merge(
            &mut base,
            json!({ "default": "a/z", "providers": { "a": { "models": [{ "id": "z" }] }, "b": { "base_url": "http://b" } } }),
        );
        assert_eq!(
            base,
            json!({
                "default": "a/z",
                "providers": {
                    "a": { "base_url": "http://a", "models": [{ "id": "z" }] },
                    "b": { "base_url": "http://b" }
                }
            })
        );
    }
}
