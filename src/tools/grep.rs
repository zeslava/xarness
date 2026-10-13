use std::fmt::Write;
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tree_sitter_tags::TagsContext;

use crate::domain::ports::{Tool, ToolSpec};
use crate::repomap::{self, Def};

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
            description: "Search file contents with a regex, respecting .gitignore. Returns `path:line:text`, followed by `[in Name]` when the line is inside a definition — pass that name to read_symbol instead of reading the whole file. At most 50 matches, lines longer than 200 chars are cut with …. Narrow the pattern or path if there are too many matches.",
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
            let mut shown = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    count += 1;
                    if count <= MAX_MATCHES {
                        shown.push((i + 1, line));
                    }
                }
            }
            if shown.is_empty() {
                continue;
            }
            // Parsed only for files with shown matches, so a broad search stays cheap.
            let defs = repomap::languages()
                .for_path(&rel)
                .map(|c| repomap::defs(&mut TagsContext::new(), c, &text))
                .unwrap_or_default();
            for (n, line) in shown {
                let _ = write!(out, "{rel}:{n}:{}", super::clip(line, MAX_LINE_CHARS));
                if let Some(sym) = enclosing(&defs, n) {
                    let _ = write!(out, "  [in {sym}]");
                }
                out.push('\n');
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

/// The innermost definition around `line`, named the way `read_symbol` accepts it.
fn enclosing(defs: &[Def], line: usize) -> Option<String> {
    let k = (0..defs.len())
        .filter(|&k| defs[k].start <= line && line <= defs[k].end)
        .min_by_key(|&k| defs[k].end - defs[k].start)?;
    let name = type_name(&defs[k]);
    Some(match repomap::container(defs, k) {
        Some(c) => format!("{}::{name}", type_name(&defs[c])),
        None => name.to_string(),
    })
}

/// For `impl Trait for Type` the tag name is the trait; the type is what tells impls apart.
fn type_name(d: &Def) -> &str {
    match d
        .text
        .strip_prefix("impl")
        .and_then(|t| t.rsplit_once(" for "))
    {
        Some((_, ty)) => {
            let end = ty
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(ty.len());
            &ty[..end]
        }
        None => &d.name,
    }
}
