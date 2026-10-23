use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::config::ModelConfig;
use crate::domain::message::{Message, Mode, ToolCall};
use crate::domain::ports::{ChatRequest, ContextOverflow, LlmEvent, LlmProvider};
use crate::metrics;
use crate::tools::{SKILL_TOOL, ToolRegistry};

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
    /// Replaces the whole history (`/compact`).
    Compacted(Vec<Message>),
    /// Context usage after a model response, as reported by the server.
    Usage {
        used: u32,
        window: Option<u32>,
        cached: Option<u32>,
    },
    /// A request finished, with its tool calls run.
    Metrics(metrics::Request),
    /// A tool call waits for the user's answer.
    Approve {
        name: String,
        arguments: String,
        reply: oneshot::Sender<Answer>,
    },
    Error(String),
    Finished,
}

pub enum Answer {
    Yes,
    /// Declined, optionally with what the model should do instead.
    No(Option<String>),
}

pub struct Run {
    pub provider: Arc<dyn LlmProvider>,
    pub model: ModelConfig,
    pub mode: Mode,
    pub history: Vec<Message>,
    pub cut: ContextCut,
    pub tools: Arc<ToolRegistry>,
    pub instructions: Option<Arc<str>>,
    /// Skill catalog, built at startup only, like the repo map.
    pub skills: Option<Arc<str>>,
    pub repo_map: Option<Arc<str>>,
    pub extra: Option<Value>,
}

pub async fn run(mut r: Run, tx: UnboundedSender<(u64, AgentEvent)>, run_id: u64) {
    let send = |ev| {
        let _ = tx.send((run_id, ev));
    };
    let mut context_window = match r.model.context_window {
        Some(c) => Some(c),
        None => r.provider.context_window().await,
    };
    // Server-reported prompt size / our char-based estimate; corrects the estimator per model.
    let mut scale = 1.0_f64;
    let mut retried = false;
    'turn: loop {
        let system = system_prompt(
            r.mode,
            &r.tools,
            r.instructions.as_deref(),
            r.skills.as_deref(),
            r.repo_map.as_deref(),
        );
        let tools = r.tools.specs(r.mode);
        let fixed = estimate_tokens(&system)
            + tools
                .iter()
                .map(|t| {
                    estimate_tokens(&t.name)
                        + estimate_tokens(&t.description)
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
        let part = |s: Option<&str>| s.map_or(0, estimate_tokens);
        let parts = [
            ("agents_md", part(r.instructions.as_deref())),
            ("skills", part(r.skills.as_deref())),
            ("repomap", part(r.repo_map.as_deref())),
        ];
        let base = estimate_tokens(&system).saturating_sub(parts.iter().map(|p| p.1).sum());
        let mut stats = metrics::Request {
            estimated,
            dropped: r.cut.start,
            cut_outputs: r.cut.truncated.len(),
            retried,
            context: breakdown(
                &[
                    &parts[..],
                    &[
                        ("system", base),
                        ("tool_specs", fixed - estimate_tokens(&system)),
                    ],
                ]
                .concat(),
                &history,
            ),
            ..Default::default()
        };
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

        let started = Instant::now();
        let mut stream = r.provider.stream(req);
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        let mut truncated = false;
        while let Some(ev) = stream.next().await {
            if stats.first_token_ms.is_none()
                && matches!(ev, Ok(LlmEvent::TextDelta(_) | LlmEvent::ReasoningDelta(_)))
            {
                stats.first_token_ms = Some(started.elapsed().as_millis() as u64);
            }
            match ev {
                Ok(LlmEvent::TextDelta(t)) => {
                    text.push_str(&t);
                    send(AgentEvent::TextDelta(t));
                }
                Ok(LlmEvent::ReasoningDelta(t)) => {
                    reasoning.push_str(&t);
                    send(AgentEvent::Thinking);
                }
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
                    stats.prompt_tokens = prompt_tokens;
                    stats.completion_tokens = completion_tokens;
                    stats.cached_tokens = cached_tokens;
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

        stats.duration_ms = started.elapsed().as_millis() as u64;
        let tools_started = Instant::now();
        retried = false;
        let mut batch = vec![Message::Assistant {
            content: text,
            reasoning,
            tool_calls: calls.clone(),
        }];
        for c in &calls {
            send(AgentEvent::ToolStart {
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            });
            let result = if c.arguments.contains(ELIDED_ARG) {
                // Models imitate the elided calls they see in history; running such a call would
                // write the mark into a file instead of the content.
                Err(anyhow::anyhow!(
                    "`{ELIDED_ARG}` is a placeholder from elided history, not real content; \
                     pass the actual text"
                ))
            } else {
                let answer = if r.tools.needs_approval(&c.name, &c.arguments) {
                    let (reply, answer) = oneshot::channel();
                    send(AgentEvent::Approve {
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                        reply,
                    });
                    answer.await.unwrap_or(Answer::No(None))
                } else {
                    Answer::Yes
                };
                match answer {
                    Answer::Yes => r.tools.run(r.mode, &c.name, &c.arguments).await,
                    Answer::No(None) => Err(anyhow::anyhow!("the user declined this call")),
                    Answer::No(Some(reason)) => Err(anyhow::anyhow!(
                        "the user declined this call and said: {reason}"
                    )),
                }
            };
            let (output, is_error) = match result {
                Ok(out) => (out, false),
                Err(e) => (format!("error: {e:#}"), true),
            };
            let output = cap_output(output);
            stats.tools.push(metrics::ToolRun {
                name: c.name.clone(),
                error: is_error,
                bytes: output.len(),
            });
            send(AgentEvent::ToolResult {
                output: output.clone(),
                is_error,
            });
            batch.push(Message::Tool {
                call_id: c.id.clone(),
                content: output,
            });
        }
        stats.tools_ms = tools_started.elapsed().as_millis() as u64;
        send(AgentEvent::Metrics(stats));
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

const COMPACT_PROMPT: &str = "You summarize a conversation between a user and a coding agent \
so the agent can continue the work from the summary alone. Keep: the user's goals and constraints, \
decisions made and why, files read or changed and what changed in them, commands run and their \
outcomes, errors and unresolved problems, the current state and next steps. Be specific: exact \
paths, identifiers, values. Omit pleasantries. Write in the language the user writes in.";
const COMPACT_REQUEST: &str = "Summarize the conversation so far.";

/// Summarizes the history with the model and sends it back as a request/summary pair, so roles
/// keep alternating once the next user message is appended. On failure the history is kept.
pub async fn compact(
    r: Run,
    focus: Option<String>,
    tx: UnboundedSender<(u64, AgentEvent)>,
    run_id: u64,
) {
    let send = |ev| {
        let _ = tx.send((run_id, ev));
    };
    let context_window = match r.model.context_window {
        Some(c) => Some(c),
        None => r.provider.context_window().await,
    };
    let request = Message::User(match &focus {
        Some(f) => format!("{COMPACT_REQUEST} Focus on: {f}"),
        None => COMPACT_REQUEST.into(),
    });
    let history = elide_old_outputs(&r.history);
    let history = match context_window {
        Some(ctx) => {
            let reserve = r.model.max_tokens.unwrap_or(DEFAULT_OUTPUT_RESERVE);
            let fixed = estimate_tokens(COMPACT_PROMPT) + message_tokens(&request);
            let budget = (ctx.saturating_sub(reserve) as usize).saturating_sub(fixed);
            fit_context(&history, budget, &mut ContextCut::default())
        }
        None => history,
    };
    let mut stats = metrics::Request {
        compact: true,
        estimated: estimate_tokens(COMPACT_PROMPT)
            + history.iter().map(message_tokens).sum::<usize>()
            + message_tokens(&request),
        context: breakdown(&[("system", estimate_tokens(COMPACT_PROMPT))], &history),
        ..Default::default()
    };
    let mut messages = vec![Message::System(COMPACT_PROMPT.into())];
    messages.extend(history);
    messages.push(request);
    let req = ChatRequest {
        model: r.model.id.clone(),
        max_tokens: r.model.max_tokens,
        temperature: r.model.temperature,
        messages,
        tools: Vec::new(),
        extra: r.extra.clone(),
    };

    let started = Instant::now();
    let mut stream = r.provider.stream(req);
    let mut summary = String::new();
    let mut truncated = false;
    while let Some(ev) = stream.next().await {
        if stats.first_token_ms.is_none()
            && matches!(ev, Ok(LlmEvent::TextDelta(_) | LlmEvent::ReasoningDelta(_)))
        {
            stats.first_token_ms = Some(started.elapsed().as_millis() as u64);
        }
        match ev {
            Ok(LlmEvent::TextDelta(t)) => {
                summary.push_str(&t);
                send(AgentEvent::TextDelta(t));
            }
            Ok(LlmEvent::ReasoningDelta(_)) => send(AgentEvent::Thinking),
            Ok(LlmEvent::Done {
                truncated: t,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                ..
            }) => {
                truncated = t;
                stats.prompt_tokens = prompt_tokens;
                stats.completion_tokens = completion_tokens;
                stats.cached_tokens = cached_tokens;
            }
            Err(e) => {
                send(AgentEvent::Error(format!("compaction failed: {e:#}")));
                send(AgentEvent::Finished);
                return;
            }
        }
    }
    stats.duration_ms = started.elapsed().as_millis() as u64;
    send(AgentEvent::Metrics(stats));
    if truncated || summary.trim().is_empty() {
        send(AgentEvent::Error(
            "compaction failed: summary empty or cut off at max_tokens; history kept".into(),
        ));
    } else {
        send(AgentEvent::Compacted(vec![
            Message::User(COMPACT_REQUEST.into()),
            Message::Assistant {
                content: summary,
                reasoning: String::new(),
                tool_calls: Vec::new(),
            },
        ]));
    }
    send(AgentEvent::Finished);
}

const DEFAULT_OUTPUT_RESERVE: u32 = 2048;
/// Initial guess before the server reports real counts; ~3 chars/token errs on the safe side.
const CHARS_PER_TOKEN: usize = 3;

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
            reasoning,
            tool_calls,
        } => {
            estimate_tokens(content)
                + estimate_tokens(reasoning)
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
const TRIM_TARGET: f64 = 0.4;
/// Target when tool outputs have to be cut. These are usually what the model has just read in
/// the current turn, so the headroom is small: cutting more makes it re-read the same files.
const TRUNCATE_TARGET: f64 = 0.85;
/// Outputs are first cut down to this many bytes (head + tail), and dropped entirely only if
/// that is not enough. Outputs already this short are never cut.
const MIN_KEEP: usize = 600;
const LABEL_ARGS_MAX: usize = 120;

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
            let label = call_label(&out, i);
            // The message may have been elided since the cut was made.
            if let Some(Message::Tool { content, .. }) = out.get_mut(i)
                && keep < content.len()
            {
                *content = shorten(content, keep, &label);
            }
        }
        out
    }
}

/// Names the call that produced tool output `i`, so a cut output still tells the model what it
/// already ran.
fn call_label(messages: &[Message], i: usize) -> String {
    let Some(Message::Tool { call_id, .. }) = messages.get(i) else {
        return "tool".into();
    };
    let call = messages[..i].iter().rev().find_map(|m| match m {
        Message::Assistant { tool_calls, .. } => tool_calls.iter().find(|c| &c.id == call_id),
        _ => None,
    });
    let Some(c) = call else {
        return "tool".into();
    };
    let mut args = c.arguments.as_str();
    if args.len() > LABEL_ARGS_MAX {
        let mut end = LABEL_ARGS_MAX;
        while !args.is_char_boundary(end) {
            end -= 1;
        }
        args = &args[..end];
    }
    format!("{} {args}", c.name)
}

fn cut_marker(label: &str, shown: usize, total: usize) -> String {
    format!("\n…[{label}: output cut to fit context, {shown} of {total} bytes shown]\n")
}

/// Keeps about `keep` bytes split between the head and the tail of `content`.
fn shorten(content: &str, keep: usize, label: &str) -> String {
    let mut head = keep / 2;
    while !content.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = content.len() - (keep - keep / 2);
    while !content.is_char_boundary(tail) {
        tail += 1;
    }
    let tail = tail.max(head);
    let shown = head + content.len() - tail;
    let mut s = String::from(&content[..head]);
    s.push_str(&cut_marker(label, shown, content.len()));
    s.push_str(&content[tail..]);
    s
}

/// Estimated tokens per part of a request: the given system parts, then the history by role,
/// with tool outputs keyed by tool name. Adds up to the `message_tokens` total.
fn breakdown(system: &[(&str, usize)], history: &[Message]) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = system
        .iter()
        .filter(|p| p.1 > 0)
        .map(|&(k, n)| (k.to_string(), n))
        .collect();
    let mut add = |k: &str, n: usize| *out.entry(k.to_string()).or_default() += n;
    let mut names: HashMap<&str, &str> = HashMap::new();
    for m in history {
        match m {
            Message::System(c) => add("system", 4 + estimate_tokens(c)),
            Message::User(c) => add("user", 4 + estimate_tokens(c)),
            Message::Tool { call_id, content } => {
                let name = names.get(call_id.as_str()).copied().unwrap_or("?");
                add(&format!("tool:{name}"), 4 + estimate_tokens(content));
            }
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                let reasoning = estimate_tokens(reasoning);
                if reasoning > 1 {
                    add("reasoning", reasoning);
                    add("assistant", 4 + estimate_tokens(content));
                } else {
                    add("assistant", 4 + estimate_tokens(content) + reasoning);
                }
                for c in tool_calls {
                    names.insert(&c.id, &c.name);
                    add(
                        "tool_args",
                        estimate_tokens(&c.name) + estimate_tokens(&c.arguments),
                    );
                }
            }
        }
    }
    out
}

fn total_tokens(h: &[Message]) -> usize {
    h.iter().map(message_tokens).sum()
}

/// Keeps `cut` while its view fits `budget`; otherwise re-trims.
fn fit_context(history: &[Message], budget: usize, cut: &mut ContextCut) -> Vec<Message> {
    let view = cut.apply(history);
    if total_tokens(&view) <= budget {
        return view;
    }
    *cut = trim(history, budget);
    cut.apply(history)
}

/// Drops whole turns from the front down to `TRIM_TARGET` of `budget` (a turn starts at a user
/// message, so tool calls never lose their results). If the remaining turns still don't fit
/// `budget` — e.g. one long agentic run — tool outputs are cut oldest-first down to
/// `TRUNCATE_TARGET`: first to `MIN_KEEP` bytes each, then dropped entirely.
fn trim(history: &[Message], budget: usize) -> ContextCut {
    let target = (budget as f64 * TRIM_TARGET) as usize;
    let mut start = 0;
    while total_tokens(&history[start..]) > target {
        match history[start + 1..]
            .iter()
            .position(|m| matches!(m, Message::User(_)))
        {
            Some(i) => start += i + 1,
            None => break,
        }
    }

    let view = &history[start..];
    let total = total_tokens(view);
    if total <= budget {
        return ContextCut {
            start,
            truncated: Vec::new(),
        };
    }
    let mut excess = total - (budget as f64 * TRUNCATE_TARGET) as usize;
    let pinned = skill_calls(view);
    let mut keeps: Vec<Option<usize>> = vec![None; view.len()];
    for floor in [MIN_KEEP, 0] {
        for (i, m) in view.iter().enumerate() {
            if excess == 0 {
                break;
            }
            let Message::Tool { call_id, content } = m else {
                continue;
            };
            if content.len() <= MIN_KEEP || pinned.contains(call_id.as_str()) {
                continue;
            }
            let cur = match keeps[i] {
                Some(k) => k,
                None => {
                    let marker = cut_marker(&call_label(view, i), content.len(), content.len());
                    excess += marker.len() / CHARS_PER_TOKEN + 1;
                    content.len()
                }
            };
            if cur <= floor {
                continue;
            }
            let cut = (excess * CHARS_PER_TOKEN).min(cur - floor);
            keeps[i] = Some(cur - cut);
            excess = excess.saturating_sub(cut / CHARS_PER_TOKEN);
        }
    }
    let truncated = keeps
        .into_iter()
        .enumerate()
        .filter_map(|(i, k)| Some((i, k?)))
        .collect();
    ContextCut { start, truncated }
}

/// Tool outputs and long string arguments of tool calls from finished turns (followed by a later
/// user message) longer than this are replaced with a mark. Reasoning is kept only on the latest
/// assistant message of the current turn: with a small context window, every step's thinking
/// piling up within one turn fills it fast. This changes the previous step on each new step, so
/// the server re-processes the prompt from there; outputs and arguments change only when a new
/// turn starts.
const ELIDE_MIN_BYTES: usize = 200;
const ELIDED_MARK: &str = "[output from an earlier turn elided; re-run the tool if needed]";
const ELIDED_ARG: &str = "[argument from an earlier turn elided]";

fn elide_old_outputs(history: &[Message]) -> Vec<Message> {
    let last_user = history
        .iter()
        .rposition(|m| matches!(m, Message::User(_)))
        .unwrap_or(0);
    let last_assistant = history
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }))
        .filter(|&i| i > last_user);
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
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
            } => Message::Assistant {
                content: content.clone(),
                reasoning: if Some(i) == last_assistant {
                    reasoning.clone()
                } else {
                    String::new()
                },
                tool_calls: if i < last_user {
                    tool_calls.iter().map(elide_arguments).collect()
                } else {
                    tool_calls.clone()
                },
            },
            _ => m.clone(),
        })
        .collect()
}

/// Long string arguments of a finished turn's call (`edit_file`'s `old_string`/`new_string`,
/// `write_file`'s `content`) are replaced with `ELIDED_ARG`; short ones such as `path`,
/// `command` or `pattern` stay, so the model still sees what it did and where.
fn elide_arguments(call: &ToolCall) -> ToolCall {
    let mut call = call.clone();
    if call.arguments.len() <= ELIDE_MIN_BYTES || call.name == SKILL_TOOL {
        return call;
    }
    let Ok(Value::Object(mut args)) = serde_json::from_str::<Value>(&call.arguments) else {
        return call;
    };
    for v in args.values_mut() {
        if v.as_str().is_some_and(|s| s.len() > ELIDE_MIN_BYTES) {
            *v = ELIDED_ARG.into();
        }
    }
    call.arguments = Value::Object(args).to_string();
    call
}

/// Ids of `skill` calls. Their outputs are instructions the model keeps following, so they are
/// never cut within their turn. Once it is finished they are elided like other outputs: a body
/// pulled along through every later turn costs thousands of tokens per request, and the model
/// can load it again.
fn skill_calls(history: &[Message]) -> HashSet<&str> {
    history
        .iter()
        .flat_map(|m| match m {
            Message::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
        .filter(|c| c.name == SKILL_TOOL)
        .map(|c| c.id.as_str())
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
    skills: Option<&str>,
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
             Read files before editing them; prefer edit_file for targeted changes. \
             Change only what the task requires: no unrelated refactoring, renaming, reformatting, \
             extra features, comments, tests or new files unless asked. If the request is ambiguous \
             or a broader change seems necessary, stop and ask instead of guessing."
        }
    };
    let mut prompt = format!(
        "You are a coding agent working in `{cwd}`. All paths are relative to it; read-only tools \
         also accept paths under the home directory (`~/…`), e.g. dependency sources.\n{task}\n\
         Do exactly what the user asked — nothing more. If you have ideas, improvements or noticed \
         other problems, mention them briefly at the end and ask whether to act on them; do not act \
         on them yourself. If the user asks a question, answer it without changing anything.\n\
         Ground every claim about the code in what you have read in this session; if you have not \
         read it, read or search first instead of guessing. Before building on an assumption, \
         state it in one sentence and verify it with a tool.\n\
         When the user says you are wrong, do not apologize or agree yet: re-check with tools, then \
         state what the code shows. If you were wrong, say so in one line and fix it; if you were \
         right, say so and cite the evidence.\n\
         Be concise. When showing code from files, quote it verbatim; put explanations \
         outside the code block."
    );
    if let Some(text) = instructions {
        prompt.push_str("\n\n# Project instructions (AGENTS.md)\n\n");
        prompt.push_str(text.trim());
    }
    if let Some(list) = skills {
        prompt.push_str(
            "\n\n# Skills\n\nInstructions for specific tasks. When a task matches a skill's \
             description, call `skill` with its name before starting and follow what it returns. \
             Skills may name other agents' tools (Bash, Read, Edit, …): use your equivalent tool.\n\n",
        );
        prompt.push_str(list);
    }
    if let Some(map) = repo_map {
        prompt.push_str(
            "\n\n# Repository map\n\nFiles with line counts and the declarations they define. \
             Consult it before searching: if a symbol you need is listed, go straight to read_symbol with \
             its name instead of grepping for it. Use grep only for what the map does not show \
             (usages, string literals, symbols missing from the map). It may not reflect edits made in \
             this session: if read_symbol does not find a name, fall back to grep.\n\n",
        );
        prompt.push_str(map);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ToolCall;

    fn turn(n: usize) -> [Message; 2] {
        [
            Message::User(format!("q{n}")),
            Message::Assistant {
                content: "a".repeat(300),
                reasoning: String::new(),
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

    #[test]
    fn skill_outputs_are_cut_only_in_finished_turns() {
        let call = |id: &str, name: &str| Message::Assistant {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
            }],
        };
        let output = |id: &str| Message::Tool {
            call_id: id.into(),
            content: "x".repeat(3000),
        };
        let history = vec![
            Message::User("q".into()),
            call("s", SKILL_TOOL),
            output("s"),
            call("r", "read_file"),
            output("r"),
            Message::User("next".into()),
        ];
        let elided = elide_old_outputs(&history);
        assert!(matches!(&elided[2], Message::Tool { content, .. } if content == ELIDED_MARK));
        assert!(matches!(&elided[4], Message::Tool { content, .. } if content == ELIDED_MARK));

        let cut = trim(&history[..5], 1100);
        assert_eq!(
            cut.truncated.iter().map(|&(i, _)| i).collect::<Vec<_>>(),
            [4]
        );
    }

    #[test]
    fn reasoning_kept_only_on_latest_step() {
        let step = |id: &str| Message::Assistant {
            content: String::new(),
            reasoning: "think".into(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        };
        let output = |id: &str| Message::Tool {
            call_id: id.into(),
            content: "x".into(),
        };
        let reasoning = |m: &Message| match m {
            Message::Assistant { reasoning, .. } => reasoning.clone(),
            _ => unreachable!(),
        };
        let mut history = vec![
            Message::User("q".into()),
            step("a"),
            output("a"),
            step("b"),
            output("b"),
        ];
        let elided = elide_old_outputs(&history);
        assert_eq!(reasoning(&elided[1]), "");
        assert_eq!(reasoning(&elided[3]), "think");

        history.push(Message::User("next".into()));
        let elided = elide_old_outputs(&history);
        assert_eq!(reasoning(&elided[3]), "");
    }

    #[test]
    fn long_arguments_elided_after_turn() {
        let args = serde_json::json!({ "path": "src/a.rs", "old_string": "x".repeat(300) });
        let mut history = vec![
            Message::User("q".into()),
            Message::Assistant {
                content: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: "e".into(),
                    name: "edit_file".into(),
                    arguments: args.to_string(),
                }],
            },
        ];
        let arguments = |h: &[Message]| match &elide_old_outputs(h)[1] {
            Message::Assistant { tool_calls, .. } => {
                serde_json::from_str::<Value>(&tool_calls[0].arguments).unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(arguments(&history), args);

        history.push(Message::User("next".into()));
        let elided = arguments(&history);
        assert_eq!(elided["path"], "src/a.rs");
        assert_eq!(elided["old_string"], ELIDED_ARG);
    }
}
