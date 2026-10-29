use std::io::Read;
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use super::jobs::Jobs;
use futures::FutureExt;
use futures::future::{self, BoxFuture};

use crate::domain::ports::{Tool, ToolSpec};

pub struct RunCommand {
    pub jobs: Arc<Jobs>,
}

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    background: bool,
}

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_LINES: usize = 60;
const OK_TAIL_LINES: usize = 10;
/// Successful output up to this size is kept whole whatever its line count: a diff or a log is
/// read for its content, while build noise worth cutting is usually longer.
const OK_WHOLE_BYTES: usize = 16_000;
const MAX_LINE_CHARS: usize = 300;

impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run".into(),
            description: "Run a shell command in the working directory, e.g. the build, test or lint commands from the project instructions. Run it after editing code to check your changes. Output is condensed: on failure the error lines (at most 60), on success output up to 16 KB whole and the last lines of longer output. Times out after 120s. Set `background` for dev servers, watchers, or long builds and tests: it returns at once, and the result arrives as a message when the command exits.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Passed to `sh -c`" },
                    "background": { "type": "boolean", "description": "Start without waiting and return a job id" }
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

    fn run(&self, args: Value, root: &Path) -> BoxFuture<'static, Result<String>> {
        future::ready(self.call(args, root)).boxed()
    }
}

impl RunCommand {
    fn call(&self, args: Value, root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        if a.background {
            let id = self.jobs.start(&a.command, root)?;
            return Ok(format!(
                "started job #{id}. Its result arrives as a new message when it exits. Do not wait or poll for it: continue with work that does not depend on it, or end your turn now."
            ));
        }
        let mut child = shell(&a.command, root).spawn().context("spawning sh")?;

        // A background grandchild may keep the pipe open past a timeout kill, so the reader is
        // not joined then.
        let stdout = child.stdout.take().context("no stdout")?;
        let (output, reader) = collect(stdout, usize::MAX);

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
            Some(s) if s.success() => Ok(format!("ok\n{}", success_output(&text))),
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

pub(super) fn shell(command: &str, root: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        // Merged in the shell so stdout and stderr keep their relative order.
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    cmd
}

/// Reads on a thread so a full pipe can't stall the child. Past `cap` bytes the oldest output
/// is dropped.
pub(super) fn collect(
    mut stdout: ChildStdout,
    cap: usize,
) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let output = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
                out.extend_from_slice(&buf[..n]);
                if out.len() > cap {
                    let excess = out.len() - cap;
                    out.drain(..excess);
                }
            }
        })
    };
    (output, reader)
}

/// Short output is kept whole, blank lines included (a diff or file listing needs them); long
/// output, usually build noise, is cut to its tail with a marker so the model knows to narrow it.
fn success_output(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_LINES || text.len() <= OK_WHOLE_BYTES {
        return clipped(&lines);
    }
    format!(
        "{}[showing last {OK_TAIL_LINES} of {} lines; to read it all, redirect the command to a \
         file and read that with read_file in parts]",
        tail(text, OK_TAIL_LINES),
        lines.len()
    )
}

pub(super) fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    clipped(&lines[lines.len().saturating_sub(n)..])
}

/// Short output is kept whole; long output is cut to the lines that point at a problem, falling
/// back to the tail when none do.
pub(super) fn condense(text: &str) -> String {
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
