use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tree_sitter_tags::TagsContext;

use futures::FutureExt;
use futures::future::{self, BoxFuture};

use crate::domain::ports::{Tool, ToolSpec};
use crate::repomap::{self, Def};

pub struct ReadSymbol;

#[derive(Deserialize)]
struct Args {
    path: String,
    name: Option<Names>,
}

/// Small models send either a list, one comma- or space-separated string, or a list stringified
/// as JSON (`["a", "b"]`); identifiers never contain whitespace, commas, brackets or quotes.
#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    One(String),
    Many(Vec<String>),
}

impl Names {
    fn queries(&self) -> Vec<&str> {
        let sep = |c: char| c == ',' || c.is_whitespace();
        let parts: Vec<&str> = match self {
            Names::One(s) => s.split(sep).collect(),
            Names::Many(v) => v.iter().flat_map(|s| s.split(sep)).collect(),
        };
        parts
            .into_iter()
            .map(|s| s.trim_matches(|c: char| "[]\"'`".contains(c)))
            .filter(|s| !s.is_empty())
            .collect()
    }
}

const MAX_LINES: usize = 400;
const MAX_LINE_CHARS: usize = 500;
const MAX_NAMES_LISTED: usize = 100;
const SMALL_FILE_LINES: usize = 150;

impl Tool for ReadSymbol {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_symbol".into(),
            description: "Read code by definition instead of by line range; much cheaper than read_file for Rust, Go, JS and TS. With `name`: returns the full source of those functions/methods/types (with its doc comments and attributes), line-numbered. The repo map in the system prompt already lists definition names, so pass `name` directly instead of asking for an outline first. Without `name`: returns the file's outline (every definition with its line range), or the whole file if it is under 150 lines.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the working directory" },
                    "name": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ],
                        "description": "One or more definition names, e.g. `run` or [\"Command\", \"parse\"]; qualify with the type to disambiguate: `App::run` or `App.run`"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        false
    }

    fn run(&self, args: Value, root: &Path) -> BoxFuture<'static, Result<String>> {
        future::ready(self.call(args, root)).boxed()
    }
}

impl ReadSymbol {
    fn call(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve_read(root, &a.path)?;
        let Some(config) = repomap::languages().for_path(&a.path) else {
            bail!("unsupported language for {}; use read_file", a.path);
        };
        let src = std::fs::read_to_string(&path).with_context(|| format!("reading {}", a.path))?;
        let defs = repomap::defs(&mut TagsContext::new(), config, &src);
        match a.name {
            // An outline of a small file is about as long as the file and costs another round trip.
            None if src.lines().count() <= SMALL_FILE_LINES => Ok(numbered(&src)),
            None => Ok(outline(&defs)),
            Some(names) => source(&src, &defs, &names.queries(), &a.path),
        }
    }
}

fn numbered(src: &str) -> String {
    src.lines()
        .enumerate()
        .map(|(i, l)| format!("{:>5}\t{}\n", i + 1, super::clip(l, MAX_LINE_CHARS)))
        .collect()
}

fn outline(defs: &[Def]) -> String {
    if defs.is_empty() {
        return "(no definitions)".into();
    }
    defs.iter()
        .map(|d| {
            let indent = if d.parent.is_some() { "    " } else { "" };
            format!("{indent}{}-{} {}\n", d.start, d.end, d.text)
        })
        .collect()
}

fn source(src: &str, defs: &[Def], queries: &[&str], path: &str) -> Result<String> {
    let mut found: Vec<&Def> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for &query in queries {
        let before = found.len();
        for d in defs.iter().filter(|d| matches(d, query)) {
            if !found.iter().any(|f| f.start == d.start) {
                found.push(d);
            }
        }
        if found.len() == before {
            missing.push(query);
        }
    }
    let not_found = (!missing.is_empty()).then(|| {
        let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names.truncate(MAX_NAMES_LISTED);
        format!(
            "no definition {} in {path}; defined there: {}",
            missing
                .iter()
                .map(|q| format!("`{q}`"))
                .collect::<Vec<_>>()
                .join(", "),
            names.join(", ")
        )
    });
    if found.is_empty() {
        bail!(not_found.unwrap_or_else(|| "no name given".into()));
    }

    let lines: Vec<&str> = src.lines().collect();
    let mut out = String::new();
    let mut budget = MAX_LINES;
    for d in found {
        let mut start = d.start;
        while start > 1 && is_preamble(lines[start - 2]) {
            start -= 1;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        for n in start..=d.end {
            if budget == 0 {
                out.push_str(&format!(
                    "[truncated; use read_file on {path} with offset={n} to continue]\n"
                ));
                return Ok(out);
            }
            let line = lines.get(n - 1).copied().unwrap_or("");
            out.push_str(&format!("{n:>5}\t{}\n", super::clip(line, MAX_LINE_CHARS)));
            budget -= 1;
        }
    }
    if let Some(note) = not_found {
        out.push_str(&format!("\n[{note}]\n"));
    }
    Ok(out)
}

fn matches(d: &Def, query: &str) -> bool {
    let (qualifier, name) = match query.rsplit_once("::").or_else(|| query.rsplit_once('.')) {
        Some((q, n)) => (q.rsplit([':', '.']).next(), n),
        None => (None, query),
    };
    // Go methods have no enclosing node; the receiver is in their own header line.
    d.name == name
        && qualifier.is_none_or(|q| {
            d.parent.as_deref().is_some_and(|p| has_token(p, q)) || has_token(&d.text, q)
        })
}

/// Doc comments and attributes directly above a definition belong to it.
fn is_preamble(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("#[") || t.starts_with("//")
}

fn has_token(text: &str, token: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|t| t == token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "struct A;\n\nimpl A {\n    /// Makes one.\n    pub fn new() -> Self {\n        A\n    }\n}\n\nimpl B {\n    fn new() {}\n}\n";

    fn defs() -> Vec<Def> {
        let config = repomap::languages().for_path("x.rs").unwrap();
        repomap::defs(&mut TagsContext::new(), config, SRC)
    }

    #[test]
    fn qualified_name_picks_impl() {
        let out = source(SRC, &defs(), &["A::new"], "x.rs").unwrap();
        assert_eq!(
            out,
            "    4\t    /// Makes one.\n    5\t    pub fn new() -> Self {\n    6\t        A\n    7\t    }\n"
        );
    }

    #[test]
    fn bare_name_returns_all_matches() {
        let out = source(SRC, &defs(), &["new"], "x.rs").unwrap();
        assert!(out.contains("   11\t    fn new() {}"));
        assert!(out.contains("    5\t"));
    }

    #[test]
    fn outline_has_ranges() {
        let out = outline(&defs());
        assert!(out.contains("3-8 impl A\n"));
        assert!(out.contains("    5-7 pub fn new() -> Self\n"));
    }

    #[test]
    fn missing_name_lists_defined() {
        let err = source(SRC, &defs(), &["nope"], "x.rs").unwrap_err();
        assert!(err.to_string().contains("defined there: A, B, new"));
    }

    #[test]
    fn several_names_with_partial_miss() {
        let names: Names = serde_json::from_value(json!("A, nope")).unwrap();
        let out = source(SRC, &defs(), &names.queries(), "x.rs").unwrap();
        assert!(out.starts_with("    1\tstruct A;\n"));
        assert!(out.contains("[no definition `nope` in x.rs"));
        let names: Names = serde_json::from_value(json!(["A", "B::new"])).unwrap();
        assert_eq!(names.queries(), ["A", "B::new"]);
    }
}
