use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use futures::FutureExt;
use futures::future::{self, BoxFuture};

use crate::domain::ports::{Tool, ToolSpec};
use crate::skills::Skills;

pub const NAME: &str = "skill";

pub struct SkillTool {
    pub skills: Arc<Skills>,
}

#[derive(Deserialize)]
struct Args {
    name: String,
    file: Option<String>,
}

impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Load a skill listed under \"Skills\" in the system prompt. Returns its instructions; follow them. With `file`, returns that file from the skill's directory instead.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name" },
                    "file": { "type": "string", "description": "Path relative to the skill's directory, as listed in its instructions" }
                },
                "required": ["name"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        false
    }

    fn run(&self, args: Value, _root: &Path) -> BoxFuture<'static, Result<String>> {
        future::ready(self.call(args)).boxed()
    }
}

impl SkillTool {
    fn call(&self, args: Value) -> Result<String> {
        let a: Args = super::parse(args)?;
        let skill = self
            .skills
            .get(&a.name)
            .ok_or_else(|| anyhow!("unknown skill `{}`", a.name))?;
        let Some(file) = a.file else {
            if !skill.model_invocable {
                bail!("skill `{}` can only be invoked by the user", a.name);
            }
            return skill.body();
        };
        // The skill's directory is the root here, so global skills stay readable without
        // widening the working-directory sandbox.
        let path = super::resolve(&skill.dir, &file)
            .map_err(|_| anyhow!("`{file}` is outside the skill's directory"))?;
        std::fs::read_to_string(&path).with_context(|| format!("reading {file}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;

    #[test]
    fn file_outside_skill_dir_is_rejected() {
        let tool = SkillTool {
            skills: Arc::new(Skills {
                list: vec![Skill {
                    name: "a".into(),
                    description: "d".into(),
                    dir: "/tmp/skills/a".into(),
                    model_invocable: true,
                    user_invocable: true,
                }],
                warnings: Vec::new(),
            }),
        };
        let err = tool
            .call(json!({ "name": "a", "file": "../b/SKILL.md" }))
            .unwrap_err();
        assert!(err.to_string().contains("outside"));
    }
}
