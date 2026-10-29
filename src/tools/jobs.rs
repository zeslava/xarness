use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::run;
use futures::FutureExt;
use futures::future::BoxFuture;

use crate::domain::ports::{Tool, ToolSpec};

/// A long-running server must not grow memory without bound; only the tail is ever shown.
const OUTPUT_CAP: usize = 1 << 20;
const TAIL_LINES: usize = 40;
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// Background commands started by `run`. Jobs outlive agent turns, so an aborted turn
/// leaves them running; they are killed when the registry is dropped on exit.
#[derive(Default)]
pub struct Jobs {
    list: Mutex<Vec<Job>>,
    /// Receives the report of each job that exits, unless the model already saw it via `job`.
    notify: Mutex<Option<UnboundedSender<String>>>,
}

struct Job {
    command: String,
    /// Where a cut report saves the full output.
    root: PathBuf,
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    started: Instant,
    status: Option<ExitStatus>,
    /// The exit was shown to the model, either by `job` or by a notification.
    reported: bool,
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

    fn report(&mut self, id: usize, killed: bool) -> String {
        let bytes = self
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let text = String::from_utf8_lossy(&bytes);
        let output = match self.poll() {
            Some(s) if !s.success() && !killed => run::condense(&text, &self.root),
            _ => run::tail(&text, TAIL_LINES, &self.root),
        };
        if self.status.is_some() {
            self.reported = true;
        }
        format!("#{id} {}: {}\n{output}", self.state(), self.command)
    }
}

impl Jobs {
    pub fn notify(&self, tx: UnboundedSender<String>) {
        *self.notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// Returns the 1-based job id.
    pub fn start(self: &Arc<Self>, command: &str, root: &Path) -> Result<usize> {
        let mut cmd = run::shell(command, root);
        // Its own process group, so a kill also reaches whatever the shell spawned.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().context("spawning sh")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let (output, _) = run::collect(stdout, OUTPUT_CAP);
        let id = {
            let mut list = self.lock();
            list.push(Job {
                command: command.to_string(),
                root: root.to_path_buf(),
                child,
                output,
                started: Instant::now(),
                status: None,
                reported: false,
            });
            list.len()
        };
        tokio::spawn(watch(Arc::downgrade(self), id));
        Ok(id)
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Job>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: usize) -> Result<MutexGuard<'_, Vec<Job>>> {
        let list = self.lock();
        if id == 0 || id > list.len() {
            bail!("no job #{id}");
        }
        Ok(list)
    }
}

/// Weak, so a watcher doesn't keep the registry alive and its `Drop` still kills the jobs.
async fn watch(jobs: Weak<Jobs>, id: usize) {
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let Some(jobs) = jobs.upgrade() else { return };
        let report = {
            let mut list = jobs.lock();
            let job = &mut list[id - 1];
            if job.poll().is_none() {
                continue;
            }
            if job.reported {
                return;
            }
            job.report(id, false)
        };
        if let Some(tx) = &*jobs.notify.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = tx.send(report);
        }
        return;
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
}

impl Tool for JobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job".into(),
            description: "Inspect background jobs. Exits are reported automatically as a new message, so never poll: to wait for a job, end your turn. Without `id`, lists jobs. With `id`, shows status and output tail (e.g. a dev server's log); `kill` stops it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Job id returned by `run`" },
                    "kill": { "type": "boolean", "description": "Stop the job and everything it spawned" }
                }
            }),
        }
    }

    // Read-only runs cannot start jobs, so they do not manage them either.
    fn mutating(&self) -> bool {
        true
    }

    fn run(&self, args: Value, _root: &Path) -> BoxFuture<'static, Result<String>> {
        let jobs = Arc::clone(&self.jobs);
        async move { call(&jobs, args).await }.boxed()
    }
}

async fn call(jobs: &Jobs, args: Value) -> Result<String> {
    let a: Args = super::parse(args)?;
    let Some(id) = a.id else {
        let mut list = jobs.lock();
        if list.is_empty() {
            return Ok("no jobs".into());
        }
        return Ok(list
            .iter_mut()
            .enumerate()
            .map(|(i, j)| format!("#{} {}: {}\n", i + 1, j.state(), j.command))
            .collect());
    };

    let mut list = jobs.get(id)?;
    let job = &mut list[id - 1];
    if a.kill {
        job.kill();
    }
    let mut report = job.report(id, a.kill);
    if job.poll().is_none() {
        report.push_str(
            "\n(still running; its exit will be reported automatically, end your turn instead of polling)",
        );
    }
    Ok(report)
}
