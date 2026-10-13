use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

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
            name: "edit_file",
            description: "Replace an exact string in a file. old_string must match exactly (including whitespace) and be unique unless replace_all is true.",
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

    fn run(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve(root, &a.path)?;
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", a.path))?;
        let updated = apply(&text, &a.old_string, &a.new_string, a.replace_all)?;
        std::fs::write(&path, updated).with_context(|| format!("writing {}", a.path))?;
        Ok(format!("edited {}", a.path))
    }
}

fn apply(text: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
    if old.is_empty() {
        bail!("old_string must not be empty");
    }
    match text.matches(old).count() {
        0 => bail!("old_string not found"),
        1 => Ok(text.replacen(old, new, 1)),
        _ if replace_all => Ok(text.replace(old, new)),
        n => bail!("old_string matches {n} times; add context or set replace_all"),
    }
}

#[cfg(test)]
mod tests {
    use super::apply;

    #[test]
    fn unique_match() {
        assert_eq!(apply("a b c", "b", "x", false).unwrap(), "a x c");
    }

    #[test]
    fn ambiguous_match_fails() {
        assert!(apply("a a", "a", "x", false).is_err());
        assert_eq!(apply("a a", "a", "x", true).unwrap(), "x x");
    }

    #[test]
    fn missing_match_fails() {
        assert!(apply("abc", "z", "x", false).is_err());
    }
}
