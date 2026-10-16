mod app;
mod commands;
mod markdown;
mod ui;

use std::io::Write;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::tools::ToolRegistry;
use app::App;

pub async fn run(config: Config, tools: ToolRegistry) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = App::new(config, tools, tx)?;
    let mut terminal = ratatui::init();
    set_alternate_scroll(true)?;
    let res = event_loop(&mut terminal, app, rx).await;
    let _ = set_alternate_scroll(false);
    ratatui::restore();
    res
}

/// Alternate scroll mode: the terminal sends the wheel as Up/Down keys. Unlike mouse capture,
/// this leaves native text selection working without Shift.
fn set_alternate_scroll(on: bool) -> std::io::Result<()> {
    let seq: &[u8] = if on { b"\x1b[?1007h" } else { b"\x1b[?1007l" };
    let mut out = std::io::stdout();
    out.write_all(seq)?;
    out.flush()
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut rx: mpsc::UnboundedReceiver<(u64, crate::agent::AgentEvent)>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    while !app.quit {
        terminal.draw(|f| ui::draw(f, &mut app))?;
        tokio::select! {
            Some(ev) = events.next() => app.on_term_event(ev?),
            Some((id, ev)) = rx.recv() => app.on_agent_event(id, ev),
            // Only animates the spinner, so no redraws while idle.
            _ = ticker.tick(), if app.is_running() => app.tick = app.tick.wrapping_add(1),
        }
    }
    Ok(())
}
