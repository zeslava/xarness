use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::FutureExt;
use futures::future::BoxFuture;
use http::{HeaderName, HeaderValue};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

use crate::config::{McpApproval, McpServerConfig};
use crate::domain::ports::{Tool, ToolSpec};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

type Service = RunningService<RoleClient, ()>;

/// Each enabled server with its tools, or why it failed to connect.
pub type Connected = Vec<(String, Result<Vec<Arc<dyn Tool>>>)>;

/// Connects to every enabled server concurrently; a server that fails is reported, not fatal.
pub async fn connect_all(servers: BTreeMap<String, McpServerConfig>, root: PathBuf) -> Connected {
    let tasks = servers
        .into_iter()
        .filter(|(_, cfg)| cfg.enabled)
        .map(|(name, cfg)| {
            let root = root.clone();
            async move {
                let res = tokio::time::timeout(CONNECT_TIMEOUT, connect(&name, &cfg, &root))
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow!("timed out after {}s", CONNECT_TIMEOUT.as_secs()))
                    });
                (name, res)
            }
        });
    futures::future::join_all(tasks).await
}

async fn connect(name: &str, cfg: &McpServerConfig, root: &Path) -> Result<Vec<Arc<dyn Tool>>> {
    let service = match (&cfg.command, &cfg.url) {
        (Some(command), None) => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&cfg.args).envs(&cfg.env).current_dir(root);
            // The server's stderr would draw over the TUI.
            let (transport, _) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("spawning `{command}`"))?;
            ().serve(transport).await?
        }
        (None, Some(url)) => {
            let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .custom_headers(headers(cfg)?);
            ().serve(StreamableHttpClientTransport::from_config(config))
                .await?
        }
        _ => bail!("set exactly one of `command` or `url`"),
    };
    let service = Arc::new(service);
    let tools = service.list_all_tools().await?;
    Ok(tools
        .into_iter()
        .filter(|t| {
            cfg.tools
                .as_ref()
                .is_none_or(|allow| allow.iter().any(|a| *a == t.name))
        })
        .map(|t| {
            let mutating = !t
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false);
            Arc::new(McpTool {
                service: Arc::clone(&service),
                name: tool_name(name, &t.name),
                remote: t.name.to_string(),
                description: t.description.as_deref().unwrap_or_default().to_string(),
                parameters: Value::Object((*t.input_schema).clone()),
                mutating,
                approval: needs_approval(cfg.approval, mutating),
            }) as Arc<dyn Tool>
        })
        .collect())
}

fn headers(cfg: &McpServerConfig) -> Result<HashMap<HeaderName, HeaderValue>> {
    let from_env = cfg.headers_env.iter().map(|(k, var)| {
        std::env::var(var)
            .map(|v| (k, v))
            .with_context(|| format!("header `{k}`: env var `{var}` is not set"))
    });
    cfg.headers
        .iter()
        .map(|(k, v)| Ok((k, v.clone())))
        .chain(from_env)
        .map(|kv| {
            let (k, v) = kv?;
            Ok((
                HeaderName::try_from(k.as_str()).with_context(|| format!("header `{k}`"))?,
                HeaderValue::try_from(v).with_context(|| format!("header `{k}`"))?,
            ))
        })
        .collect()
}

/// `server__tool`, restricted to what chat-completions function names allow.
fn tool_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn needs_approval(policy: McpApproval, mutating: bool) -> bool {
    match policy {
        McpApproval::Auto => mutating,
        McpApproval::Always => true,
        McpApproval::Never => false,
    }
}

struct McpTool {
    /// Shared by all tools of a server; the connection closes when the last one is dropped.
    service: Arc<Service>,
    name: String,
    remote: String,
    description: String,
    parameters: Value,
    mutating: bool,
    approval: bool,
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    fn mutating(&self) -> bool {
        self.mutating
    }

    fn needs_approval(&self) -> bool {
        self.approval
    }

    fn run(&self, args: Value, _root: &Path) -> BoxFuture<'static, Result<String>> {
        let service = Arc::clone(&self.service);
        let name = self.remote.clone();
        async move {
            let Value::Object(args) = args else {
                bail!("arguments must be a JSON object");
            };
            let result = service
                .call_tool(CallToolRequestParams::new(name).with_arguments(args))
                .await?;
            render(&serde_json::to_value(result)?)
        }
        .boxed()
    }
}

/// Flattens a `CallToolResult` to text; non-text content is only named, since the model
/// can't see it through a tool message anyway.
fn render(result: &Value) -> Result<String> {
    let blocks = result["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut text = blocks.iter().map(block_text).collect::<Vec<_>>().join("\n");
    if text.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        text = structured.to_string();
    }
    if result["isError"].as_bool() == Some(true) {
        bail!("{text}");
    }
    Ok(text)
}

fn block_text(block: &Value) -> String {
    let uri = |v: &Value| format!("[resource {}]", v["uri"].as_str().unwrap_or("?"));
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().unwrap_or_default().to_string(),
        Some("resource") => {
            let r = &block["resource"];
            r["text"].as_str().map_or_else(|| uri(r), str::to_string)
        }
        Some("resource_link") => uri(block),
        Some(other) => format!("[{other} omitted]"),
        None => block.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_name_is_sanitized_and_capped() {
        assert_eq!(tool_name("gh", "list.issues"), "gh__list_issues");
        assert_eq!(tool_name("s", &"x".repeat(100)).len(), 64);
    }

    #[test]
    fn approval_follows_policy() {
        assert!(needs_approval(McpApproval::Auto, true));
        assert!(!needs_approval(McpApproval::Auto, false));
        assert!(needs_approval(McpApproval::Always, false));
        assert!(!needs_approval(McpApproval::Never, true));
    }

    #[test]
    fn render_flattens_content() {
        let result = json!({ "content": [
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "", "mimeType": "image/png" },
            { "type": "resource", "resource": { "uri": "file:///a", "text": "body" } },
            { "type": "resource_link", "uri": "file:///b", "name": "b" },
        ]});
        assert_eq!(
            render(&result).unwrap(),
            "hello\n[image omitted]\nbody\n[resource file:///b]"
        );
    }

    #[test]
    fn render_errors_and_structured() {
        let err = json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true });
        assert_eq!(render(&err).unwrap_err().to_string(), "boom");
        let structured = json!({ "content": [], "structuredContent": { "n": 1 } });
        assert_eq!(render(&structured).unwrap(), r#"{"n":1}"#);
    }
}
