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

    fn needs_approval(&self, args: &Value) -> bool {
        let Ok(a) = super::parse::<Args>(args.clone()) else {
            return false;
        };
        let Some(skill) = self.skills.get(&a.name) else {
            return false;
        };
        skill.builtin.is_none()
            && a.file
                .is_some_and(|file| !super::lexical(&skill.dir, &file).starts_with(&skill.dir))
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
        if skill.builtin.is_some() {
            bail!("skill `{}` is built in and has no files", a.name);
        }
        // Files within the skill's directory need no approval, including global skills.
        // Leaving that directory is gated by needs_approval.
        let path = super::resolve_path(&skill.dir, &file)?;
        std::fs::read_to_string(&path).with_context(|| format!("reading {file}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Access;
    use crate::skills::Skill;
    use crate::tools::ToolRegistry;

    #[tokio::test]
    async fn files_outside_skill_dir_need_approval_and_can_be_read() {
        let base = std::env::temp_dir().join(format!("xarness-skill-{}", std::process::id()));
        let dir = base.join("skills/a");
        std::fs::create_dir_all(&dir).unwrap();
        let target = base.join("skills/file.txt");
        std::fs::write(&target, "external content").unwrap();
        let tools = ToolRegistry::new(
            base.join("project"),
            Arc::new(Skills {
                list: vec![Skill {
                    name: "a".into(),
                    description: "d".into(),
                    dir: dir.clone(),
                    model_invocable: true,
                    user_invocable: true,
                    read_only: false,
                    builtin: None,
                }],
                warnings: Vec::new(),
            }),
        );
        for file in [
            "SKILL.md".to_string(),
            "nested/../file.txt".to_string(),
            dir.join("file.txt").to_string_lossy().into_owned(),
        ] {
            let args = json!({"name": "a", "file": file}).to_string();
            assert!(!tools.needs_approval(Access::Full, NAME, &args), "{file}");
        }
        assert!(!tools.needs_approval(Access::Full, NAME, r#"{"name":"a"}"#));
        let args = json!({"name": "a", "file": "~/xarness-outside/file.txt"}).to_string();
        assert!(tools.needs_approval(Access::Full, NAME, &args));
        for file in [
            "../file.txt".to_string(),
            target.to_string_lossy().into_owned(),
        ] {
            let args = json!({"name": "a", "file": file}).to_string();
            assert!(tools.needs_approval(Access::Full, NAME, &args), "{file}");
            // The agent runs the tool only after approval; it also works in read-only runs.
            assert_eq!(
                tools
                    .run(Access::ReadOnly, None, NAME, &args)
                    .await
                    .unwrap(),
                "external content"
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }
}
