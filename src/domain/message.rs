use serde::{Deserialize, Serialize};

/// What a run may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Full,
    /// A turn started by a read-only skill (`/plan`). Every tool stays in the request, so the
    /// prompt prefix (and the server's KV cache) is unchanged, but mutating calls wait for approval.
    Plan,
    /// Mutating built-in tools are hidden; mutating MCP tools ask. For subagents.
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        /// Sent back only within the current turn, as the models' chat templates expect.
        /// Not persisted: a restored turn is never the current one.
        #[serde(skip)]
        reasoning: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
    },
}
