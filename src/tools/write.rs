use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::ports::{Tool, ToolSpec};

pub struct WriteFile;

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file",
            description: "Create or overwrite a file with the given content. Parent directories are created.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn run(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let path = super::resolve(root, &a.path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &a.content).with_context(|| format!("writing {}", a.path))?;
        Ok(format!("wrote {} bytes to {}", a.content.len(), a.path))
    }
}
