use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::ModelConfig;
use crate::domain::message::{Message, Mode};
use crate::domain::ports::{ChatRequest, ContextOverflow, LlmEvent, LlmProvider};
use crate::tools::ToolRegistry;

pub enum AgentEvent {
    TextDelta(String),
    Thinking,
    ToolStart {
        name: String,
        arguments: String,
    },
    ToolResult {
        output: String,
        is_error: bool,
    },
    /// Assistant message plus its tool results, appended to history atomically so an
    /// interrupted run never leaves tool calls without results.
    Commit {
        messages: Vec<Message>,
        cut: ContextCut,
    },
    /// Context usage after a model response, as reported by the server.
    Usage {
        used: u32,
        window: Option<u32>,
        cached: Option<u32>,
    },
    Error(String),
    Finished,
}

pub struct Run {
    pub provider: Arc<dyn LlmProvider>,
    pub model: ModelConfig,
    pub mode: Mode,
    pub history: Vec<Message>,
    pub cut: ContextCut,
    pub tools: Arc<ToolRegistry>,
    pub instructions: Option<Arc<str>>,
    pub repo_map: Option<Arc<str>>,
    pub extra: Option<Value>,
}

pub async fn run(mut r: Run, tx: UnboundedSender<(u64, AgentEvent)>, run_id: u64) {
    let send = |ev| {
        let _ = tx.send((run_id, ev));
    };
    let mut context_window = r.model.context_window;
    // Server-reported prompt size / our char-based estimate; corrects the estimator per model.
    let mut scale = 1.0_f64;
    let mut retried = false;
    'turn: loop {
        let system = system_prompt(
            r.mode,
            &r.tools,
            r.instructions.as_deref(),
            r.repo_map.as_deref(),
        );
        let tools = r.tools.specs(r.mode);
        let fixed = estimate_tokens(&system)
            + tools
                .iter()
                .map(|t| {
                    estimate_tokens(t.name)
                        + estimate_tokens(t.description)
                        + estimate_tokens(&t.parameters.to_string())
                })
                .sum::<usize>();
        let history = elide_old_outputs(&r.history);
        let history = match context_window {
            Some(ctx) => {
                let reserve = r.model.max_tokens.unwrap_or(DEFAULT_OUTPUT_RESERVE);
                let budget = (ctx.saturating_sub(reserve) as f64 / scale) as usize;
                fit_context(&history, budget.saturating_sub(fixed), &mut r.cut)
            }
            None => history,
        };
        let estimated = fixed + history.iter().map(message_tokens).sum::<usize>();
        let mut messages = vec![Message::System(system)];
        messages.extend(history);
        let req = ChatRequest {
            model: r.model.id.clone(),
            max_tokens: r.model.max_tokens,
            temperature: r.model.temperature,
            messages,
            tools,
            extra: r.extra.clone(),
        };

        let mut stream = r.provider.stream(req);
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut truncated = false;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(LlmEvent::TextDelta(t)) => {
                    text.push_str(&t);
                    send(AgentEvent::TextDelta(t));
                }
                Ok(LlmEvent::Reasoning) => send(AgentEvent::Thinking),
                Ok(LlmEvent::Done {
                    tool_calls,
                    prompt_tokens,
                    completion_tokens,
                    cached_tokens,
                    truncated: t,
                }) => {
                    // A cut-off tool call likely has incomplete arguments; don't run it.
                    calls = if t { Vec::new() } else { tool_calls };
                    truncated = t;
                    if let Some(n) = prompt_tokens {
                        scale = n as f64 / estimated as f64;
                        send(AgentEvent::Usage {
                            used: n + completion_tokens.unwrap_or(0),
                            window: context_window,
                            cached: cached_tokens,
                        });
                    }
                }
                Err(e) => {
                    if let Some(o) = e.downcast_ref::<ContextOverflow>()
                        && !retried
                        && text.is_empty()
                    {
                        scale = o.prompt_tokens as f64 / estimated as f64;
                        context_window = Some(
                            context_window.map_or(o.context_window, |c| c.min(o.context_window)),
                        );
                        retried = true;
                        continue 'turn;
                    }
                    send(AgentEvent::Error(format!("{e:#}")));
                    send(AgentEvent::Finished);
                    return;
                }
            }
        }

        retried = false;
        let mut batch = vec![Message::Assistant {
            content: text,
            tool_calls: calls.clone(),
        }];
        for c in &calls {
            send(AgentEvent::ToolStart {
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            });
            let (output, is_error) = match r.tools.run(r.mode, &c.name, &c.arguments) {
                Ok(out) => (out, false),
                Err(e) => (format!("error: {e:#}"), true),
            };
            let output = cap_output(output);
            send(AgentEvent::ToolResult {
                output: output.clone(),
                is_error,
            });
            batch.push(Message::Tool {
                call_id: c.id.clone(),
                content: output,
            });
        }
        r.history.extend(batch.iter().cloned());
        send(AgentEvent::Commit {
            messages: batch,
            cut: r.cut.clone(),
        });

        if calls.is_empty() {
            if truncated {
                send(AgentEvent::Error(
                    "response cut off at max_tokens (reasoning may have used it up)".into(),
                ));
            }
            send(AgentEvent::Finished);
            return;
        }
    }
}

const DEFAULT_OUTPUT_RESERVE: u32 = 2048;
/// Initial guess before the server reports real counts; ~3 chars/token errs on the safe side.
const CHARS_PER_TOKEN: usize = 3;
const TRUNCATED_MARK: &str = "\n…[truncated to fit context]";

fn estimate_tokens(s: &str) -> usize {
    s.len() / CHARS_PER_TOKEN + 1
}

fn message_tokens(m: &Message) -> usize {
    // +4 for per-message role/framing overhead
    4 + match m {
        Message::System(c) | Message::User(c) | Message::Tool { content: c, .. } => {
            estimate_tokens(c)
        }
        Message::Assistant {
            content,
            tool_calls,
        } => {
            estimate_tokens(content)
                + tool_calls
                    .iter()
                    .map(|c| estimate_tokens(&c.name) + estimate_tokens(&c.arguments))
                    .sum::<usize>()
        }
    }
}

/// Fraction of the budget the history is trimmed down to once it overflows. The headroom lets
/// the following requests reuse the same prefix (and the server's KV cache) instead of each
/// shifting the front of the history a little.
const TRIM_TARGET: f64 = 0.6;

/// The part of the history that is sent: messages from `start`, with some tool outputs cut to a
/// byte length. History is append-only, so the same cut yields a byte-identical prefix.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct ContextCut {
    start: usize,
    /// (index relative to `start`, bytes kept)
    truncated: Vec<(usize, usize)>,
}

impl ContextCut {
    fn apply(&self, history: &[Message]) -> Vec<Message> {
        let mut out = history[self.start.min(history.len())..].to_vec();
        for &(i, keep) in &self.truncated {
            // The message may have been elided since the cut was made.
            if let Some(Message::Tool { content, .. }) = out.get_mut(i)
                && keep < content.len()
            {
                content.truncate(keep);
                content.push_str(TRUNCATED_MARK);
            }
        }
        out
    }
}

fn total_tokens(h: &[Message]) -> usize {
    h.iter().map(message_tokens).sum()
}

/// Keeps `cut` while its view fits `budget`; otherwise re-trims to `TRIM_TARGET` of it.
fn fit_context(history: &[Message], budget: usize, cut: &mut ContextCut) -> Vec<Message> {
    let view = cut.apply(history);
    if total_tokens(&view) <= budget {
        return view;
    }
    *cut = trim(history, (budget as f64 * TRIM_TARGET) as usize);
    cut.apply(history)
}

/// Drops whole turns from the front (a turn starts at a user message, so tool calls never lose
/// their results). If the remaining turns still don't fit — e.g. one long agentic run — tool
/// outputs are truncated oldest-first.
fn trim(history: &[Message], budget: usize) -> ContextCut {
    let mut start = 0;
    while total_tokens(&history[start..]) > budget {
        match history[start + 1..]
            .iter()
            .position(|m| matches!(m, Message::User(_)))
        {
            Some(i) => start += i + 1,
            None => break,
        }
    }

    let mut truncated = Vec::new();
    let mut excess = total_tokens(&history[start..]).saturating_sub(budget);
    for (i, m) in history[start..].iter().enumerate() {
        if excess == 0 {
            break;
        }
        let Message::Tool { content, .. } = m else {
            continue;
        };
        let cut = (excess * CHARS_PER_TOKEN + TRUNCATED_MARK.len()).min(content.len());
        let mut keep = content.len() - cut;
        while !content.is_char_boundary(keep) {
            keep -= 1;
        }
        truncated.push((i, keep));
        let removed = content.len() - keep;
        excess =
            excess.saturating_sub(removed.saturating_sub(TRUNCATED_MARK.len()) / CHARS_PER_TOKEN);
    }
    ContextCut { start, truncated }
}

/// Tool outputs from finished turns (followed by a later user message) longer than this are
/// replaced with `ELIDED_MARK`. The view changes only when a new turn starts, so within a turn
/// the sent prefix stays byte-identical and the server's KV cache stays valid.
const ELIDE_MIN_BYTES: usize = 200;
const ELIDED_MARK: &str = "[output from an earlier turn elided; re-run the tool if needed]";

fn elide_old_outputs(history: &[Message]) -> Vec<Message> {
    let last_user = history
        .iter()
        .rposition(|m| matches!(m, Message::User(_)))
        .unwrap_or(0);
    history
        .iter()
        .enumerate()
        .map(|(i, m)| match m {
            Message::Tool { call_id, content }
                if i < last_user && content.len() > ELIDE_MIN_BYTES =>
            {
                Message::Tool {
                    call_id: call_id.clone(),
                    content: ELIDED_MARK.into(),
                }
            }
            _ => m.clone(),
        })
        .collect()
}

/// Backstop for tools that return more than their own limits should allow.
const MAX_TOOL_OUTPUT: usize = 16_000;

fn cap_output(mut s: String) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s;
    }
    let total = s.len();
    let mut keep = MAX_TOOL_OUTPUT;
    while !s.is_char_boundary(keep) {
        keep -= 1;
    }
    s.truncate(keep);
    s.push_str(&format!("\n…[output truncated: {keep} of {total} bytes]"));
    s
}

fn system_prompt(
    mode: Mode,
    tools: &ToolRegistry,
    instructions: Option<&str>,
    repo_map: Option<&str>,
) -> String {
    let cwd = tools.root().display();
    let task = match mode {
        Mode::Plan => {
            "You are in PLAN mode. Explore the code with the read-only tools and produce a concise, \
             concrete implementation plan. Do not attempt to modify files."
        }
        Mode::Build => {
            "You are in BUILD mode. Implement the requested changes using the tools. \
             Read files before editing them; prefer edit_file for targeted changes."
        }
    };
    let mut prompt = format!(
        "You are a coding agent working in `{cwd}`. All paths are relative to it.\n{task}\nBe concise. When showing code from files, quote it verbatim; put explanations \
         outside the code block."
    );
    if let Some(text) = instructions {
        prompt.push_str("\n\n# Project instructions (AGENTS.md)\n\n");
        prompt.push_str(text.trim());
    }
    if let Some(map) = repo_map {
        prompt.push_str(
            "\n\n# Repository map\n\nFiles with line counts and their definitions as `line: declaration`. \
             Consult it before searching: if a symbol you need is listed, go straight to read_symbol with \
             its name instead of grepping for it. Use grep only for what the map does not show \
             (usages, string literals, symbols missing from the map). It may be stale: if the line \
             does not match, fall back to grep.\n\n",
        );
        prompt.push_str(map);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(n: usize) -> [Message; 2] {
        [
            Message::User(format!("q{n}")),
            Message::Assistant {
                content: "a".repeat(300),
                tool_calls: Vec::new(),
            },
        ]
    }

    #[test]
    fn cut_is_stable_until_budget_overflows() {
        let budget = 1000;
        let mut cut = ContextCut::default();
        let mut history: Vec<Message> = (0..10).flat_map(turn).collect();
        let first = fit_context(&history, budget, &mut cut);
        assert!(total_tokens(&first) <= (budget as f64 * TRIM_TARGET) as usize);
        let kept = cut.clone();

        history.extend(turn(10));
        let second = fit_context(&history, budget, &mut cut);
        assert_eq!(cut, kept);
        assert_eq!(second.len(), first.len() + 2);

        history.extend((11..20).flat_map(turn));
        fit_context(&history, budget, &mut cut);
        assert_ne!(cut, kept);
    }
}
