use std::path::Path;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::run::{self, TIMEOUT};
use crate::domain::ports::{Tool, ToolSpec};

/// A long-running server must not grow memory without bound; only the tail is ever shown.
const OUTPUT_CAP: usize = 1 << 20;
const TAIL_LINES: usize = 40;

/// Background commands started by `run`. Jobs outlive agent turns, so an aborted turn
/// leaves them running; they are killed when the registry is dropped on exit.
#[derive(Default)]
pub struct Jobs {
    list: Mutex<Vec<Job>>,
}

struct Job {
    command: String,
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    started: Instant,
    status: Option<ExitStatus>,
}

impl Job {
    fn poll(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            self.status = self.child.try_wait().ok().flatten();
        }
        self.status
    }

    fn kill(&mut self) {
        if self.poll().is_some() {
            return;
        }
        kill_group(&mut self.child);
        self.status = self.child.wait().ok();
    }

    fn state(&mut self) -> String {
        match self.poll() {
            None => format!("running {}s", self.started.elapsed().as_secs()),
            Some(s) => exit_label(s),
        }
    }
}

impl Jobs {
    /// Returns the 1-based job id.
    pub fn start(&self, command: &str, root: &Path) -> Result<usize> {
        let mut cmd = run::shell(command, root);
        // Its own process group, so a kill also reaches whatever the shell spawned.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().context("spawning sh")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let (output, _) = run::collect(stdout, OUTPUT_CAP);
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.push(Job {
            command: command.to_string(),
            child,
            output,
            started: Instant::now(),
            status: None,
        });
        Ok(list.len())
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        let list = self.list.get_mut().unwrap_or_else(|e| e.into_inner());
        for job in list.iter_mut() {
            job.kill();
        }
    }
}

#[cfg(unix)]
fn kill_group(child: &mut Child) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .status();
}

#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}

fn exit_label(s: ExitStatus) -> String {
    match s.code() {
        Some(c) => format!("exit {c}"),
        None => "killed by signal".into(),
    }
}

pub struct JobTool {
    pub jobs: Arc<Jobs>,
}

#[derive(Deserialize)]
struct Args {
    id: Option<usize>,
    #[serde(default)]
    kill: bool,
    wait: Option<u64>,
}

impl Tool for JobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job",
            description: "Inspect background commands started with `run` and `background: true`. Without `id`, lists all jobs. With `id`, shows the job's status and the last lines of its output; `wait` first waits up to that many seconds (at most 120) for it to exit, `kill` stops it.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Job id returned by `run`" },
                    "wait": { "type": "integer", "description": "Seconds to wait for the job to exit" },
                    "kill": { "type": "boolean", "description": "Stop the job and everything it spawned" }
                }
            }),
        }
    }

    // Jobs are only started in build mode, so they are only managed there too.
    fn mutating(&self) -> bool {
        true
    }

    fn run(&self, args: Value, _root: &Path) -> Result<String> {
        let a: Args = super::parse(args)?;
        let Some(id) = a.id else {
            let mut list = self.jobs.list.lock().unwrap_or_else(|e| e.into_inner());
            if list.is_empty() {
                return Ok("no jobs".into());
            }
            return Ok(list
                .iter_mut()
                .enumerate()
                .map(|(i, j)| format!("#{} {}: {}\n", i + 1, j.state(), j.command))
                .collect());
        };

        let lookup = || {
            let list = self.jobs.list.lock().unwrap_or_else(|e| e.into_inner());
            if id == 0 || id > list.len() {
                bail!("no job #{id}");
            }
            Ok(list)
        };
        if let Some(secs) = a.wait {
            let deadline = Instant::now() + Duration::from_secs(secs).min(TIMEOUT);
            // The lock is released between polls so other calls aren't blocked meanwhile.
            while lookup()?[id - 1].poll().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let mut list = lookup()?;
        let job = &mut list[id - 1];
        if a.kill {
            job.kill();
        }
        let bytes = job.output.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let text = String::from_utf8_lossy(&bytes);
        let output = match job.poll() {
            Some(s) if !s.success() && !a.kill => run::condense(&text),
            _ => run::tail(&text, TAIL_LINES),
        };
        Ok(format!("#{id} {}: {}\n{output}", job.state(), job.command))
    }
}
