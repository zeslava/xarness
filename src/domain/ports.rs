use std::path::Path;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::Value;

use super::message::{Message, ToolCall};

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

pub struct ChatRequest {
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// Provider-specific fields merged into the request body as-is.
    pub extra: Option<Value>,
}

#[derive(Debug)]
pub enum LlmEvent {
    TextDelta(String),
    /// The model is producing reasoning; its text isn't needed downstream.
    Reasoning,
    Done {
        tool_calls: Vec<ToolCall>,
        /// Exact prompt size as counted by the server, if it reports usage.
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        /// Prompt tokens served from the server's prefix cache.
        cached_tokens: Option<u32>,
        /// Output stopped at the max_tokens limit rather than naturally.
        truncated: bool,
    },
}

/// Returned (via `anyhow`) when the server rejects a prompt as too large.
#[derive(Debug)]
pub struct ContextOverflow {
    pub prompt_tokens: u32,
    pub context_window: u32,
}

impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prompt ({} tokens) exceeds context window ({} tokens)",
            self.prompt_tokens, self.context_window
        )
    }
}

impl std::error::Error for ContextOverflow {}

pub trait LlmProvider: Send + Sync {
    fn stream(&self, req: ChatRequest) -> BoxStream<'static, anyhow::Result<LlmEvent>>;
    /// Context size the server is actually running with, if it exposes one.
    fn context_window(&self) -> BoxFuture<'static, Option<u32>> {
        async { None }.boxed()
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Tools that change the filesystem are only available in build mode.
    fn mutating(&self) -> bool;
    /// Each call waits for the user's approval in the TUI.
    fn needs_approval(&self) -> bool {
        false
    }
    fn run(&self, args: Value, root: &Path) -> anyhow::Result<String>;
}
