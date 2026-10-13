use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::app::{App, Entry};
use crate::domain::message::Mode;

const TOOL_OUTPUT_PREVIEW: usize = 8;
const MAX_INPUT_ROWS: usize = 6;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &App) {
    let width = f.area().width.saturating_sub(2).max(1) as usize;
    let input_rows = input_lines(&app.input, width)
        .len()
        .clamp(1, MAX_INPUT_ROWS);
    let [log_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_rows as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let log = Paragraph::new(log_text(app)).wrap(Wrap { trim: false });
    let total = log.line_count(log_area.width) as u16;
    let max_scroll = total.saturating_sub(log_area.height);
    let offset = max_scroll.saturating_sub(app.scroll);
    f.render_widget(log.scroll((offset, 0)), log_area);

    let rows = input_lines(&app.input, width);
    let (cur_row, cur_col) = (app.cursor / width, app.cursor % width);
    let first = (cur_row + 1).saturating_sub(input_rows);
    let border = match app.mode {
        Mode::Plan => Color::Cyan,
        Mode::Build => Color::Yellow,
    };
    let visible: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .skip(first)
        .take(input_rows)
        .map(|(i, r)| {
            if i == 0 && first == 0 {
                Line::from(vec![
                    Span::styled("> ", Style::new().fg(border).bold()),
                    Span::raw(r),
                ])
            } else {
                Line::from(r)
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(visible).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(border)),
        ),
        input_area,
    );
    let cursor_col = if first == 0 && cur_row == 0 {
        cur_col + 2
    } else {
        cur_col
    };
    f.set_cursor_position(Position::new(
        input_area.x + cursor_col as u16,
        input_area.y + 1 + (cur_row - first) as u16,
    ));
    f.render_widget(status_line(app), status_area);
}

/// Hard-wrap by chars so the cursor position is trivially computable.
fn input_lines(input: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut lines: Vec<String> = chars.chunks(width).map(|c| c.iter().collect()).collect();
    if chars.len().is_multiple_of(width) {
        lines.push(String::new());
    }
    lines
}

fn status_line(app: &App) -> Line<'static> {
    let (label, color) = match app.mode {
        Mode::Plan => (" PLAN ", Color::Cyan),
        Mode::Build => (" BUILD ", Color::Yellow),
    };
    let mut spans = vec![
        Span::styled(label, Style::new().fg(Color::Black).bg(color).bold()),
        Span::raw(format!(" {} ", app.model_label())),
    ];
    if let Some(u) = &app.usage {
        let mut s = format!("· ctx {}", kilo(u.used));
        let mut fg = Color::DarkGray;
        if let Some(w) = u.window.filter(|&w| w > 0) {
            let pct = u.used as u64 * 100 / w as u64;
            s += &format!("/{} {pct}%", kilo(w));
            fg = match pct {
                0..60 => Color::DarkGray,
                60..85 => Color::Yellow,
                _ => Color::Red,
            };
        }
        if let Some(c) = u.cached {
            s += &format!(" · cache {}", kilo(c));
        }
        spans.push(Span::styled(s, Style::new().fg(fg)));
    }
    Line::from(spans)
}

fn kilo(n: u32) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

fn log_text(app: &App) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();
    for (i, entry) in app.log.iter().enumerate() {
        match entry {
            Entry::User(t) => {
                for (i, l) in t.lines().enumerate() {
                    let prefix = if i == 0 { "> " } else { "  " };
                    lines.push(Line::from(format!("{prefix}{l}")).bold().fg(Color::Blue));
                }
            }
            Entry::Assistant(t) => lines.extend(t.lines().map(|l| Line::from(l.to_string()))),
            Entry::Thinking => {
                // Only the latest entry of an active run is still thinking.
                let active = app.is_running() && i + 1 == app.log.len();
                let icon = if active {
                    SPINNER[app.tick % SPINNER.len()]
                } else {
                    "✻"
                };
                lines.push(Line::styled(
                    format!("{icon} thinking"),
                    Style::new().fg(Color::Magenta).bold(),
                ));
            }
            Entry::Tool { name, arguments } => lines.push(Line::from(vec![
                Span::styled(format!("⚙ {name} "), Style::new().fg(Color::Magenta).bold()),
                Span::styled(truncate(arguments, 200), Style::new().dim()),
            ])),
            Entry::ToolOutput { text, is_error } => {
                let style = if *is_error {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new().add_modifier(Modifier::DIM)
                };
                let total = text.lines().count();
                for l in text.lines().take(TOOL_OUTPUT_PREVIEW) {
                    lines.push(Line::styled(format!("  │ {l}"), style));
                }
                if total > TOOL_OUTPUT_PREVIEW {
                    lines.push(Line::styled(
                        format!("  │ … {} more lines", total - TOOL_OUTPUT_PREVIEW),
                        style,
                    ));
                }
            }
            Entry::Info(t) => lines.extend(
                t.lines()
                    .map(|l| Line::styled(l.to_string(), Style::new().dim())),
            ),
            Entry::Error(t) => lines.extend(
                t.lines()
                    .map(|l| Line::styled(l.to_string(), Style::new().fg(Color::Red))),
            ),
        }
        lines.push(Line::default());
    }
    Text::from(lines)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}
