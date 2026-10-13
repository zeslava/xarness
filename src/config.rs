use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub default: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThinkConfig {
    /// Level used when the model is selected; absent = server default.
    pub default: Option<String>,
    /// Level name → fields merged into the request body as-is, since every
    /// server/model spells reasoning control differently.
    pub levels: IndexMap<String, Value>,
}

impl ProviderConfig {
    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key.clone().or_else(|| {
            self.api_key_env
                .as_ref()
                .and_then(|v| std::env::var(v).ok())
        })
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::locate().ok_or_else(|| {
            anyhow!(
                "config not found: create ./.xarness.{{toml,json}} or ~/.config/xarness/config.{{toml,json}}"
            )
        })?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed = if path.extension().is_some_and(|e| e == "json") {
            serde_json::from_str(&raw).map_err(anyhow::Error::from)
        } else {
            toml::from_str(&raw).map_err(anyhow::Error::from)
        };
        parsed.with_context(|| format!("parsing {}", path.display()))
    }

    fn locate() -> Option<PathBuf> {
        let global = dirs::config_dir().map(|d| d.join("xarness"));
        [".xarness.toml", ".xarness.json"]
            .into_iter()
            .map(PathBuf::from)
            .chain(
                global
                    .into_iter()
                    .flat_map(|d| [d.join("config.toml"), d.join("config.json")]),
            )
            .find(|p| p.is_file())
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
        Ok((provider.to_string(), m.clone()))
    }

    pub fn default_model(&self) -> Result<(String, ModelConfig)> {
        if let Some(spec) = &self.default {
            return self.resolve_model(spec);
        }
        for (name, p) in &self.providers {
            if let Some(m) = p.models.first() {
                return Ok((name.clone(), m.clone()));
            }
        }
        bail!("no models configured")
    }

    pub fn all_models(&self) -> impl Iterator<Item = String> + '_ {
        self.providers
            .iter()
            .flat_map(|(p, cfg)| cfg.models.iter().map(move |m| format!("{p}/{}", m.id)))
    }
}
