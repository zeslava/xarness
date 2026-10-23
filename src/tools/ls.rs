use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use futures::FutureExt;
use futures::future::{self, BoxFuture};

use crate::domain::ports::{Tool, ToolSpec};

pub struct ListDir;

const MAX_ENTRIES: usize = 100;

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
            name: "list_dir".into(),
            description: "List entries of a directory, at most 100. Directories end with `/`."
                .into(),
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

    fn run(&self, args: Value, root: &Path) -> BoxFuture<'static, Result<String>> {
        future::ready(self.call(args, root)).boxed()
    }
}

impl ListDir {
    fn call(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve_read(root, &a.path)?;
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
        if entries.is_empty() {
            return Ok("(empty)".into());
        }
        let total = entries.len();
        entries.truncate(MAX_ENTRIES);
        let mut out = entries.join("\n");
        if total > MAX_ENTRIES {
            out.push_str(&format!(
                "\n[showing {MAX_ENTRIES} of {total} entries; use grep or a narrower path]"
            ));
        }
        Ok(out)
    }
}
