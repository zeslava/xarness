use std::path::Path;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
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
            description: "Read definitions with docs, attributes, names and source locations. When definition source is needed, request the exact definition named in the task, even if absent from the partial repo map. Otherwise use names from the map. Qualify with an enclosing scope only when known; do not invent one. Avoid reading an entire enclosing definition unless needed. Batch related definitions in one call. Overlapping source is returned once. Without `name`: outline, or full source for files up to 150 lines. Directory paths require `name`.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or directory path relative to the working directory" },
                    "name": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ],
                        "description": "Exact definition name or list of names. Scope-qualified names from the repo map are accepted; scope separators may be `.` or `::`."
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
        let path = super::resolve_path(root, &a.path)?;
        if path.is_dir() {
            let Some(names) = a.name else {
                bail!(
                    "{} is a directory; pass `name` to search it, or use list_dir",
                    a.path
                );
            };
            return search_dir(&path, root, &names.queries(), &a.path);
        }
        let Some(config) = repomap::languages().for_path(&a.path) else {
            bail!("unsupported language for {}; use read_file", a.path);
        };
        let src = std::fs::read_to_string(&path).with_context(|| format!("reading {}", a.path))?;
        let defs = repomap::defs(&mut TagsContext::new(), config, &src);
        match a.name {
            // An outline of a small file is about as long as the file and costs another round trip.
            None if src.lines().count() <= SMALL_FILE_LINES => Ok(numbered(&src)),
            None => Ok(outline(&defs)),
            Some(names) => source(
                &src,
                &defs,
                &names.queries(),
                &a.path,
                &mut MAX_LINES.clone(),
            ),
        }
    }
}

fn search_dir(dir: &Path, root: &Path, queries: &[&str], dir_arg: &str) -> Result<String> {
    let langs = repomap::languages();
    let mut ctx = TagsContext::new();
    let mut out = String::new();
    let mut budget = MAX_LINES;
    let mut seen: Vec<&str> = Vec::new();
    let files = WalkBuilder::new(dir)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()));
    for entry in files {
        let path = super::display(root, entry.path());
        let Some(config) = langs.for_path(&path) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let defs = repomap::defs(&mut ctx, config, &src);
        let hits: Vec<&str> = queries
            .iter()
            .copied()
            .filter(|q| defs.iter().any(|d| matches(d, q)))
            .collect();
        if hits.is_empty() {
            continue;
        }
        seen.extend(&hits);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&source(&src, &defs, &hits, &path, &mut budget)?);
        if budget == 0 {
            return Ok(out);
        }
    }
    let missing: Vec<String> = queries
        .iter()
        .filter(|q| !seen.contains(q))
        .map(|q| format!("`{q}`"))
        .collect();
    if out.is_empty() {
        bail!("no definition {} under {dir_arg}", missing.join(", "));
    }
    if !missing.is_empty() {
        out.push_str(&format!(
            "\n[no definition {} under {dir_arg}]\n",
            missing.join(", ")
        ));
    }
    Ok(out)
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
            format!("{indent}{}-{} {}\n", d.start, d.end, d.declaration())
        })
        .collect()
}

fn source(
    src: &str,
    defs: &[Def],
    queries: &[&str],
    path: &str,
    budget: &mut usize,
) -> Result<String> {
    let mut found: Vec<&Def> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for &query in queries {
        let mut matched = false;
        for d in defs.iter().filter(|d| matches(d, query)) {
            matched = true;
            if !found.iter().any(|f| f.line == d.line) {
                found.push(d);
            }
        }
        if !matched {
            missing.push(query);
        }
    }
    let not_found = (!missing.is_empty()).then(|| {
        let mut names: Vec<&str> = defs.iter().map(|d| d.qualified_name.as_str()).collect();
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
    let hash_comments = matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some("py" | "pyi" | "sh" | "bash")
    );
    let mut ranges: Vec<_> = found
        .iter()
        .map(|d| {
            let mut start = d.start;
            while start > 1 && is_preamble(lines[start - 2], hash_comments) {
                start -= 1;
            }
            (start, d.end)
        })
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some((_, last)) = merged.last_mut()
            && start <= *last + 1
        {
            *last = (*last).max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut out = String::new();
    for (start, end) in merged {
        if !out.is_empty() {
            out.push('\n');
        }
        let mut names: Vec<_> = found
            .iter()
            .filter(|d| start <= d.start && d.end <= end)
            .map(|d| d.qualified_name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        out.push_str(&format!("{path}:{start}-{end} [{}]\n", names.join(", ")));
        for n in start..=end {
            if *budget == 0 {
                out.push_str(&format!(
                    "[truncated; use read_file on {path} with offset={n} to continue]\n"
                ));
                return Ok(out);
            }
            let line = lines.get(n - 1).copied().unwrap_or("");
            out.push_str(&format!("{n:>5}\t{}\n", super::clip(line, MAX_LINE_CHARS)));
            *budget -= 1;
        }
    }
    if let Some(note) = not_found {
        out.push_str(&format!("\n[{note}]\n"));
    }
    Ok(out)
}

pub(super) fn matches(d: &Def, query: &str) -> bool {
    let query = query.replace("::", ".");
    let name = d.qualified_name.replace("::", ".");
    name == query || name.ends_with(&format!(".{query}")) || d.name == query
}

/// Doc comments, attributes and decorators directly above a definition belong to it.
fn is_preamble(line: &str, hash_comments: bool) -> bool {
    let t = line.trim_start();
    t.starts_with("#[")
        || t.starts_with("//")
        || t.starts_with('@')
        || hash_comments && t.starts_with('#')
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
        let out = source(SRC, &defs(), &["A::new"], "x.rs", &mut MAX_LINES.clone()).unwrap();
        assert_eq!(
            out,
            "x.rs:4-7 [A::new]\n    4\t    /// Makes one.\n    5\t    pub fn new() -> Self {\n    6\t        A\n    7\t    }\n"
        );
    }

    #[test]
    fn bare_name_returns_all_matches() {
        let out = source(SRC, &defs(), &["new"], "x.rs", &mut MAX_LINES.clone()).unwrap();
        assert!(out.contains("   11\t    fn new() {}"));
        assert!(out.contains("    5\t"));
    }

    #[test]
    fn overlapping_definitions_use_the_budget_once() {
        let expected = source(SRC, &defs(), &["A"], "x.rs", &mut 7).unwrap();
        let mut budget = 7;
        let out = source(SRC, &defs(), &["A::new", "A", "nope"], "x.rs", &mut budget).unwrap();
        let code = |s: &str| {
            s.lines()
                .filter(|line| line.contains('\t'))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(code(&out), code(&expected));
        assert!(out.starts_with("x.rs:1-1 [A]\n"));
        assert!(out.contains("x.rs:3-8 [A, A::new]\n"));
        assert_eq!(out.matches("pub fn new()").count(), 1);
        assert!(out.contains("[no definition `nope` in x.rs"));
        assert!(!out.contains("truncated"));
        assert_eq!(budget, 0);
    }

    #[test]
    fn repeated_and_equivalent_queries_are_not_missing() {
        let expected = source(SRC, &defs(), &["A::new"], "x.rs", &mut 4).unwrap();
        let out = source(SRC, &defs(), &["A::new", "A.new", "A::new"], "x.rs", &mut 4).unwrap();
        assert_eq!(out, expected);
    }

    #[test]
    fn qualified_reads_disambiguate_scopes_and_keep_source_verbatim() {
        let cases = [
            (
                "x.rs",
                "mod left {\nstruct A;\nimpl A {\nfn run() { left(); }\n}\n}\nmod right {\nstruct A;\nimpl A {\nfn run() { right(); }\n}\n}\n",
                "left::A::run",
                "right::A::run",
            ),
            (
                "x.go",
                "package p\ntype A struct {}\ntype B struct {}\nfunc (a *A) run(b B) { left() }\nfunc (b *B) run() { right() }\n",
                "A.run",
                "B.run",
            ),
            (
                "x.py",
                "class A:\n    def run(self):\n        left()\nclass B:\n    def run(self):\n        right()\n",
                "A.run",
                "B.run",
            ),
            (
                "x.js",
                "class A {\nrun() { left(); }\n}\nclass B {\nrun() { right(); }\n}\n",
                "A.run",
                "B.run",
            ),
            (
                "x.ts",
                "class A {\nrun() { left(); }\n}\nclass B {\nrun() { right(); }\n}\n",
                "A.run",
                "B.run",
            ),
        ];
        for (path, src, wanted, other) in cases {
            let defs = repomap::defs(
                &mut TagsContext::new(),
                repomap::languages().for_path(path).unwrap(),
                src,
            );
            let d = defs.iter().find(|d| d.qualified_name == wanted).unwrap();
            assert!(matches(d, wanted));
            assert!(matches(d, &wanted.replace("::", ".")));
            assert!(matches(d, &wanted.replace('.', "::")));
            assert!(!matches(d, other));
            assert!(!matches(d, &format!("wrong.{wanted}")));
            let out = source(src, &defs, &[wanted], path, &mut MAX_LINES.clone()).unwrap();
            assert!(out.contains(&format!("[{wanted}]")), "{out}");
            assert!(out.contains("left()"), "{out}");
            assert!(!out.contains("right()"), "{out}");
            for line in out.lines().filter_map(|line| line.split_once('\t')) {
                let n: usize = line.0.trim().parse().unwrap();
                assert_eq!(line.1, src.lines().nth(n - 1).unwrap());
            }
        }
    }

    #[test]
    fn outline_has_ranges() {
        let out = outline(&defs());
        assert!(out.contains("3-8 impl A\n"));
        assert!(out.contains("    5-7 A::new: pub fn new() -> Self\n"));
    }

    #[test]
    fn missing_name_lists_defined() {
        let err = source(SRC, &defs(), &["nope"], "x.rs", &mut MAX_LINES.clone()).unwrap_err();
        assert!(
            err.to_string()
                .contains("defined there: A, A::new, B, B::new")
        );
    }

    #[test]
    fn several_names_with_partial_miss() {
        let names: Names = serde_json::from_value(json!("A, nope")).unwrap();
        let out = source(
            SRC,
            &defs(),
            &names.queries(),
            "x.rs",
            &mut MAX_LINES.clone(),
        )
        .unwrap();
        assert!(out.starts_with("x.rs:1-1 [A]\n    1\tstruct A;\n"));
        assert!(out.contains("[no definition `nope` in x.rs"));
        let names: Names = serde_json::from_value(json!(["A", "B::new"])).unwrap();
        assert_eq!(names.queries(), ["A", "B::new"]);
    }
}
