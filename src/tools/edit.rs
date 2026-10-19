use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use futures::FutureExt;
use futures::future::{self, BoxFuture};

use crate::domain::ports::{Tool, ToolSpec};

pub struct EditFile;

#[derive(Deserialize)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace an exact string in a file. old_string must match exactly (including whitespace) and be unique unless replace_all is true. Differences in trailing whitespace or indentation are tolerated; on a miss the closest matching lines are shown.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn run(&self, args: Value, root: &Path) -> BoxFuture<'static, Result<String>> {
        future::ready(self.call(args, root)).boxed()
    }
}

impl EditFile {
    fn call(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve(root, &a.path)?;
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", a.path))?;
        let (updated, at) = apply(&text, &a.old_string, &a.new_string, a.replace_all)?;
        std::fs::write(&path, &updated).with_context(|| format!("writing {}", a.path))?;
        Ok(match at {
            At::Lines(first, last) => format!(
                "edited {} (lines {first}-{last}):\n{}",
                a.path,
                excerpt(&updated, first, last)
            ),
            At::Count(n) => format!("edited {} ({n} replacements)", a.path),
        })
    }
}

enum At {
    Lines(usize, usize),
    Count(usize),
}

/// The edited lines with a little context, so the model can check the result without a re-read.
fn excerpt(text: &str, first: usize, last: usize) -> String {
    let from = first.saturating_sub(CONTEXT_LINES).max(1);
    let to = (last + CONTEXT_LINES).min(from + MAX_SHOWN_LINES - 1);
    let mut out = String::new();
    for (i, l) in text.lines().enumerate().take(to).skip(from - 1) {
        out.push_str(&format!(
            "{:>5}\t{}\n",
            i + 1,
            super::clip(l, MAX_LINE_CHARS)
        ));
    }
    out
}

fn apply(text: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, At)> {
    if old.is_empty() {
        bail!("old_string must not be empty");
    }
    let starts: Vec<usize> = text.match_indices(old).map(|(i, _)| i).collect();
    match starts.len() {
        0 => {}
        1 => {
            let (i, first) = (starts[0], line_of(text, starts[0]));
            let last = first + new.trim_end_matches('\n').matches('\n').count();
            let updated = format!("{}{new}{}", &text[..i], &text[i + old.len()..]);
            return Ok((updated, At::Lines(first, last)));
        }
        n if replace_all => return Ok((text.replace(old, new), At::Count(n))),
        n => {
            let lines: Vec<usize> = starts.iter().map(|&i| line_of(text, i)).collect();
            bail!("{}", ambiguous(n, &lines));
        }
    }

    // Small models often get trailing whitespace or indentation wrong; retry on whole lines,
    // still requiring a unique match.
    let file: Vec<&str> = text.split_inclusive('\n').collect();
    let olds: Vec<&str> = old.lines().collect();
    let strategies: [fn(&str, &str) -> bool; 2] = [
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    for (reindent, eq) in strategies.into_iter().enumerate() {
        let found: Vec<usize> = windows(file.len(), olds.len())
            .filter(|&k| (0..olds.len()).all(|j| eq(file[k + j], olds[j])))
            .collect();
        match found[..] {
            [] => continue,
            [k] => return Ok(replace_lines(&file, k, &olds, new, reindent == 1)),
            _ => {
                let lines: Vec<usize> = found.iter().map(|k| k + 1).collect();
                bail!("{}", ambiguous(found.len(), &lines));
            }
        }
    }
    match closest(&file, &olds) {
        Some(k) => {
            let shown = olds.len().clamp(1, MAX_SHOWN_LINES);
            let end = (k + shown).min(file.len());
            let mut msg = format!(
                "old_string not found; closest match at lines {}-{end}:\n",
                k + 1
            );
            for (i, l) in file[k..end].iter().enumerate() {
                let l = l.trim_end_matches(['\n', '\r']);
                msg.push_str(&format!(
                    "{:>5}\t{}\n",
                    k + i + 1,
                    super::clip(l, MAX_LINE_CHARS)
                ));
            }
            bail!("{msg}")
        }
        None => bail!("old_string not found"),
    }
}

const MAX_SHOWN_LINES: usize = 40;
const CONTEXT_LINES: usize = 3;
const MAX_LINE_CHARS: usize = 500;

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

fn ambiguous(n: usize, lines: &[usize]) -> String {
    let listed: Vec<String> = lines.iter().take(10).map(|l| l.to_string()).collect();
    format!(
        "old_string matches {n} times (lines {}); add context or set replace_all",
        listed.join(", ")
    )
}

fn windows(n: usize, m: usize) -> impl Iterator<Item = usize> {
    (0..(n + 1).saturating_sub(m)).filter(move |_| m > 0)
}

/// Replaces `file[k..k + olds.len()]` with `new`, keeping the file's line endings. With `reindent`,
/// shifts `new` by the indentation difference between `old_string` and the matched lines.
fn replace_lines(
    file: &[&str],
    k: usize,
    olds: &[&str],
    new: &str,
    reindent: bool,
) -> (String, At) {
    let m = olds.len();
    let eol = if file[k].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let shift = reindent
        .then(|| indent_shift(&file[k..k + m], olds))
        .flatten();
    let lines: Vec<String> = new
        .lines()
        .map(|l| match &shift {
            Some(_) if l.trim().is_empty() => l.to_string(),
            Some(Shift::Add(p)) => format!("{p}{l}"),
            Some(Shift::Strip(p)) => l.strip_prefix(p.as_str()).unwrap_or(l).to_string(),
            None => l.to_string(),
        })
        .collect();
    let mut out: String = file[..k].concat();
    if !lines.is_empty() {
        out.push_str(&lines.join(eol));
        if file[k + m - 1].ends_with('\n') {
            out.push_str(eol);
        }
    }
    out.push_str(&file[k + m..].concat());
    (out, At::Lines(k + 1, k + lines.len().max(1)))
}

enum Shift {
    Add(String),
    Strip(String),
}

fn indent_shift(matched: &[&str], olds: &[&str]) -> Option<Shift> {
    let indent = |s: &str| s[..s.len() - s.trim_start().len()].to_string();
    let j = olds.iter().position(|l| !l.trim().is_empty())?;
    let (have, want) = (indent(olds[j]), indent(matched[j]));
    if let Some(p) = want.strip_prefix(have.as_str()) {
        (!p.is_empty()).then(|| Shift::Add(p.to_string()))
    } else {
        have.strip_prefix(want.as_str())
            .map(|p| Shift::Strip(p.to_string()))
    }
}

/// Start of the window sharing the most non-trivial trimmed lines with `old_string`.
fn closest(file: &[&str], olds: &[&str]) -> Option<usize> {
    let m = olds.len().min(file.len());
    windows(file.len(), m)
        .map(|k| {
            let score = (0..m)
                .filter(|&j| olds[j].trim().len() > 2 && file[k + j].trim() == olds[j].trim())
                .count();
            (score, k)
        })
        .filter(|&(score, _)| score > 0)
        .max_by_key(|&(score, k)| (score, std::cmp::Reverse(k)))
        .map(|(_, k)| k)
}

#[cfg(test)]
mod tests {
    use super::apply;

    #[test]
    fn unique_match() {
        assert_eq!(apply("a b c", "b", "x", false).unwrap().0, "a x c");
    }

    #[test]
    fn ambiguous_match_fails() {
        assert!(apply("a a", "a", "x", false).is_err());
        assert_eq!(apply("a a", "a", "x", true).unwrap().0, "x x");
    }

    #[test]
    fn missing_match_fails() {
        assert!(apply("abc", "z", "x", false).is_err());
    }
}
