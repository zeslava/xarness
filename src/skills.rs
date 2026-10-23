use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::SkillsConfig;
use crate::tools::clip;

/// One `<dir>/<name>/SKILL.md` (Agent Skills format): YAML frontmatter, then the instructions.
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    /// Listed in the system prompt and loadable via the `skill` tool.
    pub model_invocable: bool,
    /// Available as a `/name` command.
    pub user_invocable: bool,
}

#[derive(Default)]
pub struct Skills {
    pub list: Vec<Skill>,
    /// Skills that failed to load or are shadowed by an earlier one with the same name.
    pub warnings: Vec<String>,
}

const DESCRIPTION_MAX: usize = 200;
/// Bytes of catalog in the system prompt; over it only names are listed.
const CATALOG_BUDGET: usize = 6000;
const MAX_FILES: usize = 50;
const MAX_DEPTH: usize = 3;

impl Skills {
    pub fn discover(root: &Path, cfg: &SkillsConfig) -> Self {
        let dirs = search_dirs(root, dirs::home_dir().as_deref(), cfg);
        Self::discover_in(&dirs, cfg)
    }

    /// Earlier dirs win on a name clash.
    fn discover_in(dirs: &[PathBuf], cfg: &SkillsConfig) -> Self {
        let mut skills = Self::default();
        for dir in dirs {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            // `is_file` follows symlinks, so linked skill dirs are picked up too.
            let mut paths: Vec<PathBuf> = entries
                .filter_map(|e| Some(e.ok()?.path()))
                .filter(|p| p.join("SKILL.md").is_file())
                .collect();
            paths.sort();
            for path in paths {
                match load(&path) {
                    Ok(skill) if cfg.disabled.contains(&skill.name) => {}
                    Ok(skill) => match skills.get(&skill.name) {
                        Some(first) => skills.warnings.push(format!(
                            "skill `{}` in {} is shadowed by {}",
                            skill.name,
                            skill.dir.display(),
                            first.dir.display()
                        )),
                        None => skills.list.push(skill),
                    },
                    Err(e) => skills
                        .warnings
                        .push(format!("{}: {e:#}", path.join("SKILL.md").display())),
                }
            }
        }
        skills
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.list.iter().find(|s| s.name == name)
    }

    /// Model-invocable skills as `- name: description` lines, for the system prompt.
    pub fn catalog(&self) -> Option<String> {
        let skills: Vec<&Skill> = self.list.iter().filter(|s| s.model_invocable).collect();
        if skills.is_empty() {
            return None;
        }
        let full: String = skills
            .iter()
            .map(|s| format!("- {}: {}\n", s.name, s.short_description(DESCRIPTION_MAX)))
            .collect();
        if full.len() <= CATALOG_BUDGET {
            return Some(full);
        }
        Some(skills.iter().map(|s| format!("- {}\n", s.name)).collect())
    }
}

/// Project dirs before global ones, xarness's own before other agents' (`compat`).
fn search_dirs(root: &Path, home: Option<&Path>, cfg: &SkillsConfig) -> Vec<PathBuf> {
    let compat = |base: &Path| {
        cfg.compat
            .iter()
            .map(|c| base.join(format!(".{c}/skills")))
            .collect::<Vec<_>>()
    };
    let mut dirs = vec![root.join(".xarness/skills")];
    dirs.extend(compat(root));
    if let Some(home) = home {
        dirs.push(home.join(".config/xarness/skills"));
        dirs.extend(compat(home));
    }
    // Launched from $HOME, project and global dirs coincide.
    let mut unique = Vec::new();
    for d in dirs {
        if !unique.contains(&d) {
            unique.push(d);
        }
    }
    unique
}

fn load(dir: &Path) -> Result<Skill> {
    let text = fs::read_to_string(dir.join("SKILL.md")).context("reading")?;
    let fields = match split_frontmatter(&text).0 {
        Some(front) => parse_frontmatter(front),
        None => bail!("no frontmatter"),
    };
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let description = get("description")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| anyhow!("no description"))?;
    let name = match get("name") {
        Some(n) => n.to_string(),
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    // Used as a `/name` command.
    if name.is_empty() || name.contains(|c: char| c.is_whitespace() || c == '/') {
        bail!("invalid name `{name}`");
    }
    Ok(Skill {
        name,
        description: description.to_string(),
        dir: dir.to_path_buf(),
        model_invocable: get("disable-model-invocation") != Some("true"),
        user_invocable: get("user-invocable") != Some("false"),
    })
}

impl Skill {
    pub fn short_description(&self, max_chars: usize) -> String {
        let line = self
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        clip(&line, max_chars).into_owned()
    }

    /// The instructions with `{skill_dir}` filled in, headed by the skill's directory and files.
    pub fn body(&self) -> Result<String> {
        let text = fs::read_to_string(self.dir.join("SKILL.md"))
            .with_context(|| format!("reading {}", self.dir.join("SKILL.md").display()))?;
        let dir = self.dir.display().to_string();
        let mut out = format!("Base directory: {dir}\n");
        let files = self.files();
        if !files.is_empty() {
            out.push_str("Files (read them with the `skill` tool's `file` argument):\n");
            for f in files {
                out.push_str(&format!("- {f}\n"));
            }
        }
        out.push('\n');
        out.push_str(
            &split_frontmatter(&text)
                .1
                .trim()
                .replace("{skill_dir}", &dir),
        );
        Ok(out)
    }

    fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.dir, &self.dir, 0, &mut out);
        out.sort();
        out
    }
}

/// Depth-limited, since symlinks are followed and may loop.
fn walk(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            if depth < MAX_DEPTH {
                walk(base, &path, depth + 1, out);
            }
        } else if path != base.join("SKILL.md") {
            out.push(crate::tools::display(base, &path));
        }
    }
}

/// (frontmatter between `---` lines, the rest).
pub(crate) fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut pos = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&rest[..pos]), &rest[pos + line.len()..]);
        }
        pos += line.len();
    }
    (None, text)
}

/// Top-level `key: value` pairs of a YAML subset: plain, quoted and multi-line scalars, `|`/`>`
/// blocks (chomping is ignored, values are trimmed). Nested values come out as text, since
/// only scalar keys are read.
pub(crate) fn parse_frontmatter(front: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = front.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let start = i;
        while i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with([' ', '\t'])) {
            i += 1;
        }
        let more = &lines[start..i];
        let value = value.trim();
        let value = match value.chars().next() {
            Some(c @ ('|' | '>')) => block(more, c == '|'),
            _ => {
                let joined = std::iter::once(value)
                    .chain(more.iter().map(|l| l.trim()))
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                scalar(&joined)
            }
        };
        out.push((key.trim().to_string(), value));
    }
    out
}

fn block(lines: &[&str], literal: bool) -> String {
    let indent = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map_or(0, |l| l.len() - l.trim_start().len());
    let lines = lines
        .iter()
        .map(|l| l.get(indent..).unwrap_or("").trim_end());
    let text = if literal {
        lines.collect::<Vec<_>>().join("\n")
    } else {
        // Folded: line breaks become spaces, empty lines become line breaks.
        let mut out = String::new();
        let mut after_break = true;
        for l in lines {
            if l.is_empty() {
                out.push('\n');
                after_break = true;
                continue;
            }
            if !after_break {
                out.push(' ');
            }
            out.push_str(l);
            after_break = false;
        }
        out
    };
    text.trim().to_string()
}

fn scalar(s: &str) -> String {
    let mut out = String::new();
    if let Some(rest) = s.strip_prefix('"') {
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c) => out.push(c),
                    None => {}
                },
                c => out.push(c),
            }
        }
    } else if let Some(rest) = s.strip_prefix('\'') {
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\'' {
                out.push(c);
            } else if chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
            } else {
                break;
            }
        }
    } else {
        out.push_str(s.split(" #").next().unwrap_or(s).trim());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(front: &str) -> Vec<(String, String)> {
        parse_frontmatter(front)
    }

    #[test]
    fn parses_scalars() {
        let f = fields(
            "name: pdf # comment\n\
             description: \"Fill \\\"PDF\\\" forms\"\n\
             other: 'it''s'\n\
             metadata:\n  author: x\n",
        );
        assert_eq!(f[0], ("name".into(), "pdf".into()));
        assert_eq!(f[1], ("description".into(), "Fill \"PDF\" forms".into()));
        assert_eq!(f[2], ("other".into(), "it's".into()));
        assert_eq!(f[3].0, "metadata");
        assert_eq!(f.len(), 4);
    }

    #[test]
    fn parses_multiline() {
        let f = fields(
            "folded: >-\n  one\n  two\n\n  three\n\
             literal: |\n  a\n    b\n\
             plain: first\n  second\n",
        );
        assert_eq!(f[0].1, "one two\nthree");
        assert_eq!(f[1].1, "a\n  b");
        assert_eq!(f[2].1, "first second");
    }

    #[test]
    fn splits_frontmatter() {
        let (front, body) = split_frontmatter("---\nname: a\n---\n# Body\n");
        assert_eq!(front, Some("name: a\n"));
        assert_eq!(body, "# Body\n");
        assert_eq!(split_frontmatter("# no front").0, None);
    }

    #[test]
    fn earlier_dirs_shadow_later() {
        let base = std::env::temp_dir().join(format!("xarness-skills-{}", std::process::id()));
        let write = |dir: &str, name: &str, desc: &str| {
            let d = base.join(dir).join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("SKILL.md"),
                format!("---\ndescription: {desc}\n---\nbody"),
            )
            .unwrap();
        };
        write("project", "a", "project a");
        write("global", "a", "global a");
        write("global", "b", "global b");
        write("global", "off", "disabled");
        fs::create_dir_all(base.join("global/broken")).unwrap();
        fs::write(base.join("global/broken/SKILL.md"), "no frontmatter").unwrap();

        let cfg = SkillsConfig {
            disabled: vec!["off".into()],
            ..Default::default()
        };
        let skills = Skills::discover_in(&[base.join("project"), base.join("global")], &cfg);
        let _ = fs::remove_dir_all(&base);

        let names: Vec<_> = skills.list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(skills.get("a").unwrap().description, "project a");
        assert_eq!(skills.warnings.len(), 2);
    }
}
