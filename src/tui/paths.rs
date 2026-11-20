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

fn mention(input: &str, cursor: usize) -> Option<Mention> {
    if input.starts_with('/') {
        return None;
    }
    let cursor = byte_at(input, cursor);
    for (start, ch) in input.char_indices() {
        if ch != '@' || (start > 0 && !input[..start].ends_with(char::is_whitespace)) {
            continue;
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
        if cursor < body || cursor > end {
            continue;
        }
        let raw = &input[body..cursor];
        let prefix = if quoted {
            let raw = raw.strip_prefix('"')?;
            let raw = raw.strip_suffix('"').unwrap_or(raw);
            serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()?
        } else {
            raw.to_string()
        };
        return Some(Mention {
            range: start..end,
            prefix,
        });
    }
    None
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
