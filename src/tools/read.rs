use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::ports::{Tool, ToolSpec};

pub struct ReadFile;

#[derive(Deserialize)]
struct Args {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

const DEFAULT_LIMIT: usize = 300;
const MAX_LINE_CHARS: usize = 500;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file",
            description: "Read a text file. Returns lines prefixed with 1-based line numbers; returns at most 300 lines by default, lines longer than 500 chars are cut with …. For large files, first use grep to find the relevant line numbers, then read just that range with offset/limit.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the working directory" },
                    "offset": { "type": "integer", "description": "1-based line to start from" },
                    "limit": { "type": "integer", "description": "Max lines to return (default 300)" }
                },
                "required": ["path"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        false
    }

    fn run(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve(root, &a.path)?;
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", a.path))?;
        let start = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(DEFAULT_LIMIT);
        let total = text.lines().count();
        let mut out: String = text
            .lines()
            .enumerate()
            .skip(start - 1)
            .take(limit)
            .map(|(i, l)| format!("{:>5}\t{}\n", i + 1, super::clip(l, MAX_LINE_CHARS)))
            .collect();
        let shown_end = (start - 1).saturating_add(limit).min(total);
        if shown_end < total {
            out.push_str(&format!(
                "[showing lines {start}–{shown_end} of {total}; use offset={} to continue]\n",
                shown_end + 1
            ));
        }
        if out.is_empty() {
            out.push_str("(empty)");
        }
        Ok(out)
    }
}
