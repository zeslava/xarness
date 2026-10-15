use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::ports::{Tool, ToolSpec};

pub struct RunCommand;

#[derive(Deserialize)]
struct Args {
    command: String,
}

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_LINES: usize = 60;
const OK_TAIL_LINES: usize = 10;
const MAX_LINE_CHARS: usize = 300;

impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run",
            description: "Run a shell command in the working directory, e.g. the build, test or lint commands from the project instructions. Run it after editing code to check your changes. Output is condensed: on failure the error lines (at most 60), on success the last lines. Times out after 120s.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Passed to `sh -c`" }
                },
                "required": ["command"]
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn run(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let mut child = Command::new("sh")
            .arg("-c")
            // Merged in the shell so stdout and stderr keep their relative order.
            .arg(format!("exec 2>&1\n{}", a.command))
            .current_dir(root)
            .env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("spawning sh")?;

        // Read on a thread so a full pipe can't stall the child; a background grandchild may keep
        // the pipe open past a timeout kill, so the thread is not joined then.
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut stdout = child.stdout.take().context("no stdout")?;
        let reader = {
            let output = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut buf = [0; 8192];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    output
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend_from_slice(&buf[..n]);
                }
            })
        };

        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                let _ = reader.join();
                break Some(status);
            }
            if start.elapsed() > TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        let bytes = output.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let text = String::from_utf8_lossy(&bytes);
        match status {
            Some(s) if s.success() => Ok(format!("ok\n{}", tail(&text, OK_TAIL_LINES))),
            Some(s) => bail!(
                "exit {}\n{}",
                s.code().map_or("signal".into(), |c| c.to_string()),
                condense(&text)
            ),
            None => bail!(
                "timed out after {}s\n{}",
                TIMEOUT.as_secs(),
                condense(&text)
            ),
        }
    }
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    clipped(&lines[lines.len().saturating_sub(n)..])
}

/// Short output is kept whole; long output is cut to the lines that point at a problem, falling
/// back to the tail when none do.
fn condense(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.dedup();
    if lines.len() <= MAX_LINES {
        return clipped(&lines);
    }
    let re = Regex::new(r"(?i)error|warning|panic|fail|:\d+(:\d+)?\b").expect("valid regex");
    let relevant: Vec<&str> = lines.iter().copied().filter(|l| re.is_match(l)).collect();
    let (shown, total) = if relevant.is_empty() {
        (&lines[lines.len() - MAX_LINES..], lines.len())
    } else {
        (&relevant[..relevant.len().min(MAX_LINES)], lines.len())
    };
    format!(
        "{}[showing {} of {total} lines]",
        clipped(shown),
        shown.len()
    )
}

fn clipped(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| format!("{}\n", super::clip(l, MAX_LINE_CHARS)))
        .collect()
}
