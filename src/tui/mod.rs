mod app;
mod commands;
mod ui;

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
    let res = event_loop(&mut terminal, app, rx).await;
    ratatui::restore();
    res
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut rx: mpsc::UnboundedReceiver<(u64, crate::agent::AgentEvent)>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    while !app.quit {
        terminal.draw(|f| ui::draw(f, &app))?;
        tokio::select! {
            Some(ev) = events.next() => app.on_term_event(ev?),
            Some((id, ev)) = rx.recv() => app.on_agent_event(id, ev),
            // Only animates the spinner, so no redraws while idle.
            _ = ticker.tick(), if app.is_running() => app.tick = app.tick.wrapping_add(1),
        }
    }
    Ok(())
}
