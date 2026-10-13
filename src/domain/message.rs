#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Build,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Plan => "PLAN",
            Mode::Build => "BUILD",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Mode::Plan => Mode::Build,
            Mode::Build => Mode::Plan,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
    },
}
