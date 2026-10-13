use std::fmt::Write;
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::ports::{Tool, ToolSpec};

pub struct Grep;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
}

const MAX_MATCHES: usize = 50;
const MAX_LINE_CHARS: usize = 200;

impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep",
            description: "Search file contents with a regex, respecting .gitignore. Returns `path:line:text`, at most 50 matches, lines longer than 200 chars are cut with …. Narrow the pattern or path if there are too many matches.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Rust regex syntax" },
                    "path": { "type": "string", "description": "File or directory, defaults to the working directory" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        false
    }

    fn run(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let re = Regex::new(&a.pattern)?;
        let base = super::resolve(root, a.path.as_deref().unwrap_or("."))?;
        let mut out = String::new();
        let mut count = 0;
        for entry in WalkBuilder::new(&base).build().filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let rel = super::display(root, entry.path());
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    count += 1;
                    if count <= MAX_MATCHES {
                        let _ =
                            writeln!(out, "{rel}:{}:{}", i + 1, super::clip(line, MAX_LINE_CHARS));
                    }
                }
            }
        }
        if count > MAX_MATCHES {
            let _ = writeln!(
                out,
                "[showing {MAX_MATCHES} of {count} matches; narrow the pattern or path]"
            );
        }
        Ok(if out.is_empty() {
            "no matches".into()
        } else {
            out
        })
    }
}
