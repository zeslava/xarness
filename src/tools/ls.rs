use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::ports::{Tool, ToolSpec};

pub struct ListDir;

#[derive(Deserialize)]
struct Args {
    #[serde(default = "dot")]
    path: String,
}

fn dot() -> String {
    ".".into()
}

impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir",
            description: "List entries of a directory. Directories end with `/`.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Defaults to the working directory" }
                }
            }),
        }
    }

    fn mutating(&self) -> bool {
        false
    }

    fn run(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve(root, &a.path)?;
        let mut entries: Vec<String> = std::fs::read_dir(&path)
            .with_context(|| format!("listing {}", a.path))?
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    name + "/"
                } else {
                    name
                }
            })
            .collect();
        entries.sort();
        Ok(if entries.is_empty() {
            "(empty)".into()
        } else {
            entries.join("\n")
        })
    }
}
