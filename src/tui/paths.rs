//! Path mentions are input conveniences: no file contents enter the conversation.
use std::ops::Range;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::commands::Completion;

#[derive(Debug, PartialEq)]
struct Mention {
    range: Range<usize>,
    prefix: String,
}

fn byte_at(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i)
}

fn mention_ranges(input: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut consumed = 0;
    input.char_indices().filter_map(move |(start, ch)| {
        if start < consumed
            || ch != '@'
            || (start > 0 && !input[..start].ends_with(char::is_whitespace))
        {
            return None;
        }
        let body = start + 1;
        let quoted = input[body..].starts_with('"');
        let mut end = input.len();
        if quoted {
            let mut escaped = false;
            for (i, c) in input[body + 1..].char_indices() {
                if c == '"' && !escaped {
                    end = body + 1 + i + 1;
                    break;
                }
                escaped = c == '\\' && !escaped;
            }
        } else if let Some(i) = input[body..].find(char::is_whitespace) {
            end = body + i;
        }
        consumed = end;
        Some(start..end)
    })
}

fn mention(input: &str, cursor: usize) -> Option<Mention> {
    if input.starts_with('/') {
        return None;
    }
    let cursor = byte_at(input, cursor);
    let range = mention_ranges(input).find(|r| cursor > r.start && cursor <= r.end)?;
    let raw = &input[range.start + 1..cursor];
    let prefix = if let Some(raw) = raw.strip_prefix('"') {
        let raw = raw.strip_suffix('"').unwrap_or(raw);
        serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()?
    } else {
        raw.to_string()
    };
    Some(Mention { range, prefix })
}

/// Give the model unambiguous paths while keeping mention syntax in the input and log.
/// Use the file tools' lexical resolver: paths need not exist, and symlinks are not followed.
pub(super) fn resolve_mentions(input: &str, root: &Path) -> String {
    let mut out = String::with_capacity(input.len());
    let mut copied = 0;
    for range in mention_ranges(input) {
        let raw = &input[range.start + 1..range.end];
        let path = if raw.starts_with('"') {
            let Ok(path) = serde_json::from_str::<String>(raw) else {
                continue;
            };
            path
        } else {
            raw.to_string()
        };
        if path.is_empty() {
            continue;
        }
        let Ok(resolved) = crate::tools::resolve_path(root, &path) else {
            continue;
        };
        let mut resolved = resolved.to_string_lossy().into_owned();
        if path.ends_with('/') && !resolved.ends_with('/') {
            resolved.push('/');
        }
        out.push_str(&input[copied..range.start]);
        out.push_str(&quoted(&resolved));
        copied = range.end;
    }
    out.push_str(&input[copied..]);
    out
}

#[derive(Clone)]
struct Entry {
    name: String,
    directory: bool,
}

fn read_directory(path: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            Some(Entry {
                name: e.file_name().into_string().ok()?,
                directory: e.path().is_dir(),
            })
        })
        .collect();
    entries.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.name.cmp(&b.name)));
    entries
}

fn split(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map_or(("", prefix), |i| prefix.split_at(i + 1))
}

fn quoted(path: &str) -> String {
    if path
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        serde_json::to_string(path).unwrap()
    } else {
        path.into()
    }
}

type Scan = (u64, Vec<Entry>);

pub(super) struct Paths {
    root: PathBuf,
    tx: UnboundedSender<Scan>,
    rx: UnboundedReceiver<Scan>,
    generation: u64,
    directory: Option<PathBuf>,
    entries: Vec<Entry>,
    active: Option<Mention>,
    dismissed: Option<(String, usize)>,
}

impl Paths {
    pub fn new(root: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            root,
            tx,
            rx,
            generation: 0,
            directory: None,
            entries: Vec::new(),
            active: None,
            dismissed: None,
        }
    }

    pub fn refresh(&mut self, input: &str, cursor: usize, enabled: bool) -> bool {
        let previous = self.active.take();
        if self
            .dismissed
            .as_ref()
            .is_some_and(|(s, c)| s != input || *c != cursor)
        {
            self.dismissed = None;
        }
        self.active = if enabled && self.dismissed.is_none() {
            mention(input, cursor)
        } else {
            None
        };
        let directory = self.active.as_ref().and_then(|m| {
            let (dir, _) = split(&m.prefix);
            crate::tools::resolve_path(&self.root, if dir.is_empty() { "." } else { dir }).ok()
        });
        let changed = previous != self.active;
        if directory != self.directory {
            self.directory = directory.clone();
            self.entries.clear();
            self.generation += 1;
            if let Some(dir) = directory {
                let tx = self.tx.clone();
                let generation = self.generation;
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send((generation, read_directory(&dir)));
                });
            }
        }
        changed
    }

    pub async fn receive(&mut self) {
        while let Some((generation, entries)) = self.rx.recv().await {
            if generation == self.generation {
                self.apply_scan(generation, entries);
                return;
            }
        }
    }

    fn apply_scan(&mut self, generation: u64, entries: Vec<Entry>) {
        if generation == self.generation {
            self.entries = entries;
        }
    }

    pub fn completions(&self) -> Vec<Completion> {
        let Some(m) = &self.active else {
            return Vec::new();
        };
        let (dir, name) = split(&m.prefix);
        self.entries
            .iter()
            .filter(|e| {
                e.name.starts_with(name) && (!e.name.starts_with('.') || name.starts_with('.'))
            })
            .take(100)
            .map(|e| {
                let path = format!("{dir}{}{}", e.name, if e.directory { "/" } else { "" });
                Completion {
                    label: path.clone(),
                    desc: if e.directory { "directory" } else { "file" }.into(),
                    insert: format!("@{}", quoted(&path)),
                }
            })
            .collect()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn dismiss(&mut self, input: &str, cursor: usize) {
        self.dismissed = Some((input.into(), cursor));
        self.active = None;
    }

    pub fn accept(&self, selected: usize, input: &mut String, cursor: &mut usize) -> bool {
        let items = self.completions();
        let Some(item) = items.get(selected.min(items.len().saturating_sub(1))) else {
            return false;
        };
        let m = self.active.as_ref().unwrap();
        let mut insert = item.insert.clone();
        if item.desc == "file" && !input[m.range.end..].starts_with(char::is_whitespace) {
            insert.push(' ');
        }
        *cursor = input[..m.range.start].chars().count() + insert.chars().count();
        if item.desc == "file" && input[m.range.end..].starts_with(char::is_whitespace) {
            *cursor += 1;
        }
        // Continue a quoted directory inside the closing quote.
        if item.desc == "directory" && insert.ends_with('"') {
            *cursor -= 1;
        }
        input.replace_range(m.range.clone(), &insert);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_at_cursor() {
        let s = "Прочти @src/main.rs и @../billing/";
        let m = mention(s, "Прочти @src/ma".chars().count()).unwrap();
        assert_eq!(&s[m.range], "@src/main.rs");
        assert_eq!(m.prefix, "src/ma");
        assert_eq!(mention(s, s.chars().count()).unwrap().prefix, "../billing/");
        assert!(mention("a@example.com", 13).is_none());
        assert!(mention("/plan @src", 10).is_none());
        assert_eq!(
            mention("@\"my service/src/\"", 18).unwrap().prefix,
            "my service/src/"
        );
        assert_eq!(mention("@\"my service", 12).unwrap().prefix, "my service");
    }

    #[test]
    fn mentions_resolve_for_model() {
        let root = Path::new("/work/team/project");
        assert_eq!(
            resolve_mentions("Прочти @../../file.rs и @../file.rs\n@src/../new.rs", root),
            "Прочти /work/file.rs и /work/team/file.rs\n/work/team/project/new.rs"
        );
        assert_eq!(
            resolve_mentions(r#"@"../my service/a\"b.rs" @/tmp/a/../b/"#, root),
            r#""/work/team/my service/a\"b.rs" /tmp/b/"#
        );
        assert_eq!(
            resolve_mentions(r#"@"my @ service/file.rs""#, root),
            r#""/work/team/project/my @ service/file.rs""#
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                resolve_mentions("@~/src/../new.rs", root),
                quoted(&home.join("new.rs").to_string_lossy())
            );
        }
        // Empty or malformed mentions and email addresses remain literal text.
        for input in [
            "a@example.com @",
            r#"@"""#,
            r#"@"unfinished"#,
            r#"@"bad\q""#,
        ] {
            assert_eq!(resolve_mentions(input, root), input);
        }
    }

    #[tokio::test]
    async fn parent_completions_preserve_depth_when_read() {
        let base =
            std::env::temp_dir().join(format!("xarness-parent-paths-{}", std::process::id()));
        let root = base.join("parent/project");
        std::fs::create_dir_all(&root).unwrap();
        for (directory, content) in [(&base, "grandparent"), (&base.join("parent"), "parent")] {
            std::fs::create_dir_all(directory.join("my service")).unwrap();
            std::fs::write(directory.join("my service/main.rs"), content).unwrap();
        }
        let mut paths = Paths::new(root.clone());
        let tools = crate::tools::ToolRegistry::new(
            root,
            std::sync::Arc::new(crate::skills::Skills::default()),
        );

        for (parents, content) in [("../", "parent"), ("../../", "grandparent")] {
            let mut input = format!("Прочти @{parents}my");
            let mut cursor = input.chars().count();
            paths.refresh(&input, cursor, true);
            paths.receive().await;
            assert_eq!(
                paths.completions()[0].label,
                format!("{parents}my service/")
            );
            assert!(paths.accept(0, &mut input, &mut cursor));
            assert_eq!(input, format!("Прочти @\"{parents}my service/\""));

            paths.refresh(&input, cursor, true);
            paths.receive().await;
            assert!(paths.accept(0, &mut input, &mut cursor));
            assert_eq!(input, format!("Прочти @\"{parents}my service/main.rs\" "));
            // Read the absolute path sent to the model after accepting the completion.
            let resolved = resolve_mentions(&input, tools.root());
            let path: String =
                serde_json::from_str(resolved.strip_prefix("Прочти ").unwrap().trim()).unwrap();
            let output = tools
                .run(
                    crate::domain::message::Access::Full,
                    None,
                    "read_file",
                    &serde_json::json!({ "path": path }).to_string(),
                )
                .await
                .unwrap();
            assert_eq!(output, format!("    1\t{content}\n"));
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn browsing_and_insertion() {
        let root = std::env::temp_dir().join(format!("xarness-paths-{}", std::process::id()));
        std::fs::create_dir_all(root.join("my service")).unwrap();
        std::fs::write(root.join("main.rs"), "").unwrap();
        std::fs::write(root.join(".hidden"), "").unwrap();
        let mut paths = Paths::new(root.clone());
        let mut input = "read @m then test".to_string();
        let mut cursor = 7;
        paths.refresh(&input, cursor, true);
        paths.receive().await;
        assert_eq!(paths.completions().len(), 2);
        assert_eq!(paths.completions()[0].desc, "directory");
        paths.accept(0, &mut input, &mut cursor);
        assert_eq!(input, "read @\"my service/\" then test");
        assert_eq!(input.chars().nth(cursor), Some('"'));
        paths.refresh("@.", 2, true);
        assert_eq!(paths.completions()[0].label, ".hidden");
        paths.refresh("@ma", 3, true);
        input = "@ma rest".into();
        cursor = 3;
        paths.accept(0, &mut input, &mut cursor);
        assert_eq!(input, "@main.rs rest");
        paths.dismiss("@ma", 3);
        paths.refresh("@ma", 3, true);
        assert!(paths.completions().is_empty());
        paths.refresh("@m", 2, true);
        paths.receive().await;
        assert_eq!(paths.completions().len(), 2);
        paths.apply_scan(paths.generation - 1, Vec::new());
        assert_eq!(paths.completions().len(), 2);
        let absolute = format!("@{}/", root.display());
        paths.refresh(&absolute, absolute.chars().count(), true);
        // This resolves to the same cached directory.
        assert_eq!(paths.completions().len(), 2);
        paths.refresh("@\"my service/\"", 14, true);
        paths.receive().await;
        assert!(paths.completions().is_empty());
        paths.refresh("@missing/", 9, true);
        paths.receive().await;
        assert!(paths.completions().is_empty());
        paths.refresh("@\"my service/../ma\"", 18, true);
        paths.receive().await;
        assert_eq!(paths.completions()[0].label, "my service/../main.rs");
        std::fs::remove_dir_all(root).unwrap();
    }
}
