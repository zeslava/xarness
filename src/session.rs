use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::message::Message;

/// History persisted as JSONL in `.xarness/sessions/`, one file per session. The file is created
/// on the first save, so empty sessions leave nothing behind.
pub struct Session {
    path: PathBuf,
    /// How many history messages are already in the file.
    saved: usize,
}

pub struct Summary {
    pub path: PathBuf,
    pub modified: SystemTime,
    /// First line of the first user message.
    pub title: String,
}

impl Summary {
    pub fn age(&self) -> String {
        let s = self.modified.elapsed().unwrap_or_default().as_secs();
        match s {
            0..60 => "just now".into(),
            60..3600 => format!("{}m ago", s / 60),
            3600..86400 => format!("{}h ago", s / 3600),
            _ => format!("{}d ago", s / 86400),
        }
    }
}

fn dir(root: &Path) -> PathBuf {
    root.join(".xarness").join("sessions")
}

impl Session {
    pub fn new(root: &Path) -> Self {
        // Millis, since `/clear` can start several sessions within a second.
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            path: dir(root).join(format!("{millis}.jsonl")),
            saved: 0,
        }
    }

    pub fn open(path: PathBuf) -> io::Result<(Self, Vec<Message>)> {
        let mut history = Vec::new();
        for line in BufReader::new(File::open(&path)?).lines() {
            history.push(serde_json::from_str(&line?)?);
        }
        let session = Self {
            path,
            saved: history.len(),
        };
        Ok((session, history))
    }

    /// Appends the messages added to `history` since the last save.
    pub fn save(&mut self, history: &[Message]) -> io::Result<()> {
        let Some(new) = history.get(self.saved..).filter(|m| !m.is_empty()) else {
            return Ok(());
        };
        self.ensure_dir()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(&lines(new)?)?;
        self.saved = history.len();
        Ok(())
    }

    /// For when history was replaced rather than extended (`/compact`).
    pub fn rewrite(&mut self, history: &[Message]) -> io::Result<()> {
        self.ensure_dir()?;
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, lines(history)?)?;
        fs::rename(&tmp, &self.path)?;
        self.saved = history.len();
        Ok(())
    }

    fn ensure_dir(&self) -> io::Result<()> {
        let Some(dir) = self.path.parent() else {
            return Ok(());
        };
        fs::create_dir_all(dir)?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n")?;
        }
        Ok(())
    }
}

fn lines(messages: &[Message]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    for m in messages {
        serde_json::to_writer(&mut buf, m)?;
        buf.push(b'\n');
    }
    Ok(buf)
}

/// Newest first.
pub fn list(root: &Path) -> Vec<Summary> {
    let Ok(entries) = fs::read_dir(dir(root)) else {
        return Vec::new();
    };
    let mut found: Vec<Summary> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|path| {
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            let title = title(&path).unwrap_or_default();
            Some(Summary {
                path,
                modified,
                title,
            })
        })
        .collect();
    found.sort_by_key(|b| std::cmp::Reverse(b.modified));
    found
}

fn title(path: &Path) -> Option<String> {
    let text = BufReader::new(File::open(path).ok()?)
        .lines()
        .map_while(Result::ok)
        .find_map(|l| match serde_json::from_str(&l) {
            Ok(Message::User(t)) => Some(t),
            _ => None,
        })?;
    let line = text.lines().next().unwrap_or_default();
    Some(match line.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    })
}
