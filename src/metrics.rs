use std::collections::BTreeMap;

/// One model request of the current session, for `/stats`.
#[derive(Debug, Default)]
pub struct Request {
    /// `provider/model`, filled in by the TUI.
    pub model: String,
    pub compact: bool,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
    /// Our char-based estimate of the prompt; `context` adds up to it.
    pub estimated: usize,
    pub first_token_ms: Option<u64>,
    pub duration_ms: u64,
    pub tools: Vec<ToolRun>,
    pub tools_ms: u64,
    /// Messages dropped from the front of the history to fit the context.
    pub dropped: usize,
    /// Tool outputs cut to fit the context.
    pub cut_outputs: usize,
    /// Sent again after the server rejected the prompt as too large.
    pub retried: bool,
    /// Estimated prompt tokens per part: system parts, roles, `tool:<name>` outputs.
    pub context: BTreeMap<String, usize>,
}

#[derive(Debug, Default)]
pub struct ToolRun {
    pub name: String,
    pub error: bool,
    pub bytes: usize,
}

#[derive(Default)]
struct ModelStats {
    requests: usize,
    compacts: usize,
    prompt: u64,
    cached: u64,
    completion: u64,
    first_token_ms: u64,
    timed: usize,
    decode_ms: u64,
    tool_calls: usize,
    tool_errors: usize,
    trims: usize,
    retries: usize,
}

#[derive(Default)]
struct ToolStats {
    calls: usize,
    errors: usize,
    bytes: usize,
}

pub fn summary(reqs: &[Request]) -> String {
    if reqs.is_empty() {
        return "no metrics yet".into();
    }
    let mut models: BTreeMap<&str, ModelStats> = BTreeMap::new();
    let mut tools: BTreeMap<&str, ToolStats> = BTreeMap::new();
    for r in reqs {
        let m = models.entry(r.model.as_str()).or_default();
        m.requests += 1;
        m.compacts += r.compact as usize;
        m.prompt += r.prompt_tokens.unwrap_or(0) as u64;
        m.cached += r.cached_tokens.unwrap_or(0) as u64;
        m.completion += r.completion_tokens.unwrap_or(0) as u64;
        if let Some(t) = r.first_token_ms {
            m.first_token_ms += t;
            m.timed += 1;
            if r.completion_tokens.is_some() {
                m.decode_ms += r.duration_ms.saturating_sub(t);
            }
        }
        m.tool_calls += r.tools.len();
        m.tool_errors += r.tools.iter().filter(|t| t.error).count();
        m.trims += (r.dropped > 0 || r.cut_outputs > 0) as usize;
        m.retries += r.retried as usize;
        for t in &r.tools {
            let s = tools.entry(t.name.as_str()).or_default();
            s.calls += 1;
            s.errors += t.error as usize;
            s.bytes += t.bytes;
        }
    }

    let mut out = Vec::new();
    for (model, m) in &models {
        let model = if model.is_empty() { "?" } else { model };
        let compacts = match m.compacts {
            0 => String::new(),
            n => format!(" ({n} compact)"),
        };
        let mut line = format!(
            "{model}: {} requests{compacts}, prompt {} (cached {}%), completion {}",
            m.requests,
            k(m.prompt as usize),
            percent(m.cached, m.prompt),
            k(m.completion as usize),
        );
        if m.timed > 0 {
            line.push_str(&format!(
                ", first token {:.1}s avg",
                m.first_token_ms as f64 / m.timed as f64 / 1000.0
            ));
        }
        if m.decode_ms > 0 {
            line.push_str(&format!(
                ", {:.0} tok/s",
                m.completion as f64 / (m.decode_ms as f64 / 1000.0)
            ));
        }
        out.push(line);
        out.push(format!(
            "  {} tool calls, {} errors ({}%), {} trims, {} overflow retries",
            m.tool_calls,
            m.tool_errors,
            percent(m.tool_errors as u64, m.tool_calls as u64),
            m.trims,
            m.retries
        ));
    }

    if !tools.is_empty() {
        let mut list: Vec<_> = tools.into_iter().collect();
        list.sort_by_key(|(_, s)| std::cmp::Reverse(s.calls));
        let list: Vec<String> = list
            .into_iter()
            .map(|(name, s)| {
                let errors = match s.errors {
                    0 => String::new(),
                    n => format!(", {n} err"),
                };
                format!("{name} {}{errors}, ~{}", s.calls, k(s.bytes / 3))
            })
            .collect();
        out.push(format!("tools: {}", list.join(" · ")));
    }

    let context = scaled_context(&reqs[reqs.len() - 1]);
    let total: f64 = context.values().sum();
    let mut parts: Vec<_> = context.into_iter().collect();
    parts.sort_by(|a, b| b.1.total_cmp(&a.1));
    let parts: Vec<String> = parts
        .into_iter()
        .filter(|(_, n)| *n >= 1.0)
        .map(|(name, n)| format!("{name} {}", k(n as usize)))
        .collect();
    out.push(format!(
        "context of the last request (~{}): {}",
        k(total as usize),
        parts.join(" · ")
    ));
    out.join("\n")
}

/// Estimates corrected by the server's count of the same prompt, when it reported one.
fn scaled_context(r: &Request) -> BTreeMap<String, f64> {
    let scale = match r.prompt_tokens {
        Some(p) if r.estimated > 0 => p as f64 / r.estimated as f64,
        _ => 1.0,
    };
    r.context
        .iter()
        .map(|(k, &n)| (k.clone(), n as f64 * scale))
        .collect()
}

fn percent(part: u64, total: u64) -> u64 {
    (part * 100).checked_div(total).unwrap_or(0)
}

fn k(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}
