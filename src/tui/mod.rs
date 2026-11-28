mod app;
mod clipboard;
mod commands;
mod location;
mod log;
mod markdown;
mod paths;
mod render;
mod ui;

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::supports_keyboard_enhancement;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::adapters::mcp;
use crate::config::Config;
use crate::skills::Skills;
use crate::tools::ToolRegistry;
use app::App;

pub async fn run(
    config: Config,
    tools: ToolRegistry,
    skills: Arc<Skills>,
    resume: bool,
) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (mcp_tx, mcp_rx) = mpsc::unbounded_channel();
    let (job_tx, job_rx) = mpsc::unbounded_channel();
    let (map_tx, map_rx) = mpsc::unbounded_channel();
    let (background_tx, background_rx) = mpsc::unbounded_channel();
    tools.watch_jobs(job_tx);
    if !config.mcp.is_empty() {
        let servers = config.mcp.clone();
        let root = tools.root().to_path_buf();
        // In the background: servers started via npx/uvx can take seconds to come up.
        tokio::spawn(async move {
            let _ = mcp_tx.send(mcp::connect_all(servers, root).await);
        });
    }
    let lsp = tools.lsp_manager();
    let app = App::new(config, tools, skills, tx, background_tx, map_tx, resume)?;
    let mut terminal = ratatui::init();
    // Wheel and drag arrive as mouse events, apart from the arrow keys; selection is ours
    // to draw and copy (native one needs Shift).
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    // Without it a pasted newline arrives as Enter and submits the first line.
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
    // Kitty keyboard protocol, so Shift+Enter is distinguishable from Enter.
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let res = event_loop(
        &mut terminal,
        app,
        rx,
        background_rx,
        mcp_rx,
        job_rx,
        map_rx,
    )
    .await;
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(lsp) = lsp {
        lsp.shutdown().await;
    }
    res
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut rx: mpsc::UnboundedReceiver<(u64, crate::agent::AgentEvent)>,
    mut background_rx: mpsc::UnboundedReceiver<crate::agent::BackgroundEvent>,
    mut mcp_rx: mpsc::UnboundedReceiver<mcp::Connected>,
    mut job_rx: mpsc::UnboundedReceiver<String>,
    mut map_rx: mpsc::UnboundedReceiver<Option<crate::repomap::RepoMap>>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut frames = render::Frames::new(tokio::time::Instant::now());
    while !app.quit {
        let now = tokio::time::Instant::now();
        let animated = app.is_running() || app.has_background() || app.flash().is_some();
        if frames.due(now, animated) {
            let now = now.into_std();
            app.on_tick(now);
            terminal.draw(|f| ui::draw(f, &mut app, now))?;
            frames.drawn(tokio::time::Instant::now());
        }
        let animated = app.is_running() || app.has_background() || app.flash().is_some();
        let mut changed = true;
        tokio::select! {
            Some(ev) = events.next() => app.on_term_event(ev?),
            Some((id, ev)) = rx.recv() => {
                // Repeated reasoning deltas carry no new visible state.
                changed = id == app.run_id() && !matches!(
                    &ev, crate::agent::AgentEvent::Thinking
                        if app.phase.0 == app::Phase::Thinking
                );
                app.on_agent_event(id, ev);
            },
            Some(ev) = background_rx.recv() => app.on_background(ev),
            Some(servers) = mcp_rx.recv() => app.on_mcp(servers),
            Some(report) = job_rx.recv() => app.on_job(report),
            Some(map) = map_rx.recv() => app.on_repo_map(map),
            _ = app.paths.receive() => {},
            _ = tokio::time::sleep_until(frames.deadline()), if frames.pending(animated) => {
                changed = false;
            },
        }
        app.refresh_paths();
        if changed {
            frames.invalidate();
        }
    }
    Ok(())
}
