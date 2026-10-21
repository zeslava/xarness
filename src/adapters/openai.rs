use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde_json::{Value, json};

use crate::domain::message::{Message, ToolCall};
use crate::domain::ports::{ChatRequest, ContextOverflow, LlmEvent, LlmProvider};

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl LlmProvider for OpenAiProvider {
    fn stream(&self, req: ChatRequest) -> BoxStream<'static, Result<LlmEvent>> {
        let (mut tx, rx) = mpsc::channel(64);
        let mut http = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&request_body(&req));
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        tokio::spawn(async move {
            if let Err(e) = pump(http, &mut tx).await {
                let _ = tx.send(Err(e)).await;
            }
        });
        rx.boxed()
    }

    /// llama.cpp serves the runtime `n_ctx` at `/props` on the server root, outside `/v1`.
    fn context_window(&self) -> BoxFuture<'static, Option<u32>> {
        let root = self.base_url.trim_end_matches("/v1");
        let http = self
            .client
            .get(format!("{root}/props"))
            .timeout(std::time::Duration::from_secs(2));
        async move {
            let v: Value = http.send().await.ok()?.json().await.ok()?;
            let n = v.pointer("/default_generation_settings/n_ctx")?.as_u64()?;
            Some(n as u32)
        }
        .boxed()
    }
}

async fn pump(
    http: reqwest::RequestBuilder,
    tx: &mut mpsc::Sender<Result<LlmEvent>>,
) -> Result<()> {
    let resp = send_with_retry(http, tx).await?;
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut acc = DeltaAccumulator::default();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                return finish(acc, tx).await;
            }
            let v: Value = serde_json::from_str(data)?;
            if let Some(err) = v.get("error") {
                bail!("provider error: {err}");
            }
            if let Some(n) = v.pointer("/usage/prompt_tokens").and_then(Value::as_u64) {
                acc.prompt_tokens = Some(n as u32);
            }
            if let Some(n) = v
                .pointer("/usage/completion_tokens")
                .and_then(Value::as_u64)
            {
                acc.completion_tokens = Some(n as u32);
            }
            // llama.cpp reports cache hits in `timings.cache_n`, others in the OpenAI field.
            if let Some(n) = v
                .pointer("/usage/prompt_tokens_details/cached_tokens")
                .or_else(|| v.pointer("/timings/cache_n"))
                .and_then(Value::as_u64)
            {
                acc.cached_tokens = Some(n as u32);
            }
            if v.pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                == Some("length")
            {
                acc.truncated = true;
            }
            for ev in acc.push(&v) {
                tx.send(Ok(ev)).await?;
            }
        }
    }
    finish(acc, tx).await
}

const MAX_RETRIES: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Retries rate-limit errors (429 or a "too many tokens" body) and connect errors,
/// honoring `Retry-After` (seconds) or falling back to exponential backoff.
async fn send_with_retry(
    http: reqwest::RequestBuilder,
    tx: &mpsc::Sender<Result<LlmEvent>>,
) -> Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        let req = http
            .try_clone()
            .ok_or_else(|| anyhow!("request body is not cloneable"))?;
        let retry_after = match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                let retry_after = resp
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                let body = resp.text().await.unwrap_or_default();
                if let Some(e) = context_overflow(&body) {
                    return Err(e.into());
                }
                let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
                    || body.to_lowercase().contains("too many tokens");
                if !rate_limited || attempt >= MAX_RETRIES {
                    bail!("HTTP {status}: {body}");
                }
                retry_after
            }
            // Transient socket failures, e.g. EADDRNOTAVAIL when ephemeral ports run out.
            Err(e) if e.is_connect() && attempt < MAX_RETRIES => None,
            Err(e) => return Err(e.into()),
        };
        let delay = retry_after
            .unwrap_or(Duration::from_secs(1 << attempt))
            .min(MAX_BACKOFF);
        tokio::time::sleep(delay).await;
        // The run was aborted while we waited.
        if tx.is_closed() {
            bail!("aborted");
        }
        attempt += 1;
    }
}

async fn finish(mut acc: DeltaAccumulator, tx: &mut mpsc::Sender<Result<LlmEvent>>) -> Result<()> {
    if let Some(ev) = acc.flush_think() {
        tx.send(Ok(ev)).await?;
    }
    let prompt_tokens = acc.prompt_tokens;
    let completion_tokens = acc.completion_tokens;
    let cached_tokens = acc.cached_tokens;
    let truncated = acc.truncated;
    tx.send(Ok(LlmEvent::Done {
        tool_calls: acc.finish(),
        prompt_tokens,
        completion_tokens,
        cached_tokens,
        truncated,
    }))
    .await
    .map_err(|e| anyhow!(e))
}

fn request_body(req: &ChatRequest) -> Value {
    let mut body = json!({
        "model": req.model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": req.messages.iter().map(message_json).collect::<Vec<_>>(),
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.description, "parameters": t.parameters },
                })
            })
            .collect();
    }
    if let Some(n) = req.max_tokens {
        body["max_tokens"] = n.into();
    }
    if let Some(t) = req.temperature {
        body["temperature"] = t.into();
    }
    if let Some(Value::Object(extra)) = &req.extra {
        for (k, v) in extra {
            body[k.as_str()] = v.clone();
        }
    }
    body
}

/// llama.cpp reports `exceed_context_size_error` with exact counts; other servers just get a plain error.
fn context_overflow(body: &str) -> Option<ContextOverflow> {
    let v: Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if err.get("type")?.as_str()? != "exceed_context_size_error" {
        return None;
    }
    Some(ContextOverflow {
        prompt_tokens: err.get("n_prompt_tokens")?.as_u64()? as u32,
        context_window: err.get("n_ctx")?.as_u64()? as u32,
    })
}

fn message_json(m: &Message) -> Value {
    match m {
        Message::System(c) => json!({ "role": "system", "content": c }),
        Message::User(c) => json!({ "role": "user", "content": c }),
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
        } => {
            let mut v = json!({
                "role": "assistant",
                "content": if content.is_empty() { Value::Null } else { content.as_str().into() },
            });
            if !reasoning.is_empty() {
                v["reasoning_content"] = reasoning.as_str().into();
            }
            if !tool_calls.is_empty() {
                v["tool_calls"] = tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments },
                        })
                    })
                    .collect();
            }
            v
        }
        Message::Tool { call_id, content } => {
            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
        }
    }
}

/// Tool call fragments arrive keyed by `index`; `id` and `name` usually only in the first fragment.
#[derive(Default)]
struct DeltaAccumulator {
    calls: BTreeMap<u64, ToolCall>,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    cached_tokens: Option<u32>,
    truncated: bool,
    in_think: bool,
    think_tail: String,
}

impl DeltaAccumulator {
    fn push(&mut self, chunk: &Value) -> Vec<LlmEvent> {
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return Vec::new();
        };
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = self.calls.entry(idx).or_insert_with(|| ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    entry.id = id.to_string();
                }
                if let Some(name) = c.pointer("/function/name").and_then(Value::as_str) {
                    entry.name.push_str(name);
                }
                if let Some(args) = c.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(args);
                }
            }
        }
        let text = |key| {
            delta
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        // llama.cpp/vLLM put thinking in `reasoning_content`, some servers use `reasoning`.
        let reasoning = text("reasoning_content").or_else(|| text("reasoning"));
        let mut events: Vec<_> = reasoning.map(LlmEvent::ReasoningDelta).into_iter().collect();
        if let Some(content) = text("content") {
            events.extend(self.split_think(&content));
        }
        events
    }

    /// Routes `<think>` blocks the server left in `content` to reasoning and drops stray
    /// `</think>` (the opening tag was in the prompt). A trailing partial tag is held back
    /// until the next chunk.
    fn split_think(&mut self, s: &str) -> Vec<LlmEvent> {
        const OPEN: &str = "<think>";
        const CLOSE: &str = "</think>";
        let mut buf = std::mem::take(&mut self.think_tail) + s;
        let mut events = Vec::new();
        let mut emit = |in_think: bool, t: &str| {
            if !t.is_empty() {
                events.push(if in_think {
                    LlmEvent::ReasoningDelta(t.to_string())
                } else {
                    LlmEvent::TextDelta(t.to_string())
                });
            }
        };
        loop {
            let open = (!self.in_think).then(|| buf.find(OPEN)).flatten();
            let close = buf.find(CLOSE);
            let tag = match (open, close) {
                (Some(o), Some(c)) if c < o => Some((c, CLOSE)),
                (Some(o), _) => Some((o, OPEN)),
                (None, Some(c)) => Some((c, CLOSE)),
                (None, None) => None,
            };
            let Some((i, tag)) = tag else { break };
            emit(self.in_think, &buf[..i]);
            self.in_think = tag == OPEN;
            buf.drain(..i + tag.len());
        }
        let partial = |tag: &str| (1..tag.len()).rev().find(|&k| buf.ends_with(&tag[..k]));
        let held = partial(CLOSE)
            .max((!self.in_think).then(|| partial(OPEN)).flatten())
            .unwrap_or(0);
        let tail = buf.split_off(buf.len() - held);
        emit(self.in_think, &buf);
        self.think_tail = tail;
        events
    }

    /// Text held back by `split_think` that never became a tag.
    fn flush_think(&mut self) -> Option<LlmEvent> {
        let t = std::mem::take(&mut self.think_tail);
        (!t.is_empty()).then(|| {
            if self.in_think {
                LlmEvent::ReasoningDelta(t)
            } else {
                LlmEvent::TextDelta(t)
            }
        })
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .map(|(i, mut c)| {
                if c.id.is_empty() {
                    c.id = format!("call_{i}");
                }
                c
            })
            .collect()
    }
}
