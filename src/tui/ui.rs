use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr;

use super::app::{APPROVAL_OPTIONS, App, Approval, Entry, Phase, Selection};
use super::markdown;
use crate::domain::message::Mode;

const TOOL_OUTPUT_PREVIEW: usize = 8;
const MAX_INPUT_ROWS: usize = 6;
/// Upper bound of the panel above the input, as a share of the screen height.
const PANEL_SHARE_PCT: usize = 40;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App) {
    let width = f.area().width.saturating_sub(2).max(1) as usize;
    let (rows, cur_row, cur_col) = input_lines(&app.input, app.cursor, width);
    let input_rows = rows.len().clamp(1, MAX_INPUT_ROWS);
    let accent = match app.mode {
        Mode::Plan => Color::Cyan,
        Mode::Build => Color::LightBlue,
    };
    let panel_cap = (f.area().height as usize * PANEL_SHARE_PCT / 100).max(1);
    let (panel, keep) = panel_lines(app, f.area().width as usize, panel_cap, accent);
    let [log_area, panel_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(panel.len().min(panel_cap) as u16),
        Constraint::Length(input_rows as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // A pending approval has the panel as its indicator.
    let live = app.is_running() && app.approval().is_none();
    let mut text = log_text(app);
    if live {
        // Room for the indicator, so at the bottom it doesn't cover the last line.
        text.lines.push(Line::default());
    }
    let log = Paragraph::new(text).wrap(Wrap { trim: false });
    let total = log.line_count(log_area.width);
    if app.scroll > 0 {
        // `scroll` counts from the bottom, so growth would push the viewed lines up.
        let grown = total as i64 - app.log_total as i64;
        app.scroll = (app.scroll as i64 + grown).clamp(0, u16::MAX as i64) as u16;
        app.unseen += grown.max(0) as usize;
    } else {
        app.unseen = 0;
    }
    app.log_total = total;
    let total = total as u16;
    let max_scroll = total.saturating_sub(log_area.height);
    // Clamp so scrolling past the top doesn't need as many presses to come back.
    app.scroll = app.scroll.min(max_scroll);
    app.log_area = log_area;
    let offset = max_scroll.saturating_sub(app.scroll);
    if std::mem::take(&mut app.copy)
        && let Some(sel) = app.selection
    {
        let text = selected_text(&log, log_area.width, sel);
        app.clipboard.copy(&text);
    }
    f.render_widget(log.scroll((offset, 0)), log_area);
    if let Some(sel) = app.selection {
        highlight(f.buffer_mut(), log_area, offset, sel);
    }
    if live {
        // Follows the text while it fits, then stays pinned to the bottom, scrolled or not.
        let row = Rect {
            y: log_area.y + total.min(log_area.height).saturating_sub(1),
            height: 1,
            ..log_area
        };
        f.render_widget(Clear, row);
        f.render_widget(live_line(app), row);
    }
    let skip = (keep as u16 + 1).saturating_sub(panel_area.height);
    f.render_widget(Paragraph::new(panel).scroll((skip, 0)), panel_area);

    let first = (cur_row + 1).saturating_sub(input_rows);
    let visible: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .skip(first)
        .take(input_rows)
        .map(|(i, r)| {
            if i == 0 && first == 0 {
                Line::from(vec![
                    Span::styled("> ", Style::new().fg(accent).bold()),
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
                .border_style(Style::new().fg(Color::Gray)),
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

fn highlight(buf: &mut Buffer, area: Rect, offset: u16, sel: Selection) {
    let ((l0, c0), (l1, c1)) = sel.range();
    for row in 0..area.height {
        let line = offset as usize + row as usize;
        if line < l0 || line > l1 {
            continue;
        }
        let from = if line == l0 { c0 } else { 0 };
        let to = if line == l1 { c1 } else { area.width - 1 };
        buf.set_style(
            Rect::new(area.x + from, area.y + row, to - from + 1, 1),
            Style::new().add_modifier(Modifier::REVERSED),
        );
    }
}

/// Renders only the selected wrapped lines off-screen and reads the cells back, so the text
/// matches what was highlighted even where it has scrolled out of view.
fn selected_text(log: &Paragraph, width: u16, sel: Selection) -> String {
    let ((l0, c0), (l1, c1)) = sel.range();
    let height = (l1 - l0 + 1).min(u16::MAX as usize) as u16;
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    log.clone().scroll((l0 as u16, 0)).render(area, &mut buf);
    let mut lines = Vec::new();
    for y in 0..height {
        let from = if y == 0 { c0 } else { 0 };
        let to = if y == height - 1 { c1 } else { width - 1 };
        let mut line = String::new();
        // Cells covered by a wide char hold a placeholder space.
        let mut hidden = 0;
        for x in 0..width {
            let sym = buf[(x, y)].symbol();
            if hidden > 0 {
                hidden -= 1;
                continue;
            }
            hidden = sym.width().saturating_sub(1);
            if (from..=to).contains(&x) {
                line.push_str(sym);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// The panel between the log and the input shows one thing at a time, by priority:
/// a pending approval, slash-command completions, queued messages. Returns its lines,
/// sized to the content, and the row to keep visible if they exceed `cap`.
fn panel_lines(app: &App, width: usize, cap: usize, accent: Color) -> (Vec<Line<'static>>, usize) {
    if let Some(a) = app.approval() {
        let lines = approval_lines(a, width, cap, accent);
        let last = lines.len().saturating_sub(1);
        return (lines, last);
    }
    let items = app.completions();
    if !items.is_empty() {
        let selected = app.completion.min(items.len() - 1);
        let name_width = items.iter().map(|c| c.label.len()).max().unwrap_or(0) + 1;
        let lines = items
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let line = Line::from(vec![
                    Span::raw(format!(" {:<name_width$}", c.label)),
                    Span::styled(format!(" {} ", c.desc), Style::new().dim()),
                ]);
                if i == selected { line.reversed() } else { line }
            })
            .collect();
        return (lines, selected);
    }
    // Newest last, nearest the input; overflow hides the oldest.
    let lines: Vec<Line> = app
        .queued
        .iter()
        .map(|m| {
            let first = m.lines().next().unwrap_or_default();
            let shown = truncate(first, width.saturating_sub(4));
            let shown = if m.contains('\n') && !shown.ends_with('…') {
                shown + "…"
            } else {
                shown
            };
            Line::styled(format!("⏳ {shown}"), Style::new().dim())
        })
        .collect();
    let last = lines.len().saturating_sub(1);
    (lines, last)
}

/// Title, a preview of what the call will do and the options on one row.
/// The preview is cut to fit `cap` so the options always stay visible.
fn approval_lines(a: &Approval, width: usize, cap: usize, accent: Color) -> Vec<Line<'static>> {
    let mut title = vec![Span::styled(
        format!("Allow {}?", display_name(&a.name)),
        Style::new().fg(Color::Magenta).bold(),
    )];
    // For `run` the summary is the command itself, already shown in full below.
    if a.name != "run" {
        title.push(Span::styled(
            format!(" {}", truncate(&tool_summary(&a.name, &a.arguments), width)),
            Style::new().dim(),
        ));
    }
    let mut lines = vec![Line::from(title)];
    let room = cap.saturating_sub(2);
    // Hard-wrapped rather than truncated: the whole command must be visible to approve it.
    let preview: Vec<String> = approval_preview(&a.name, &a.arguments)
        .iter()
        .flat_map(|l| {
            let chars: Vec<char> = l.chars().collect();
            let chunks: Vec<String> = chars
                .chunks(width.saturating_sub(4).max(1))
                .map(|c| c.iter().collect())
                .collect();
            if chunks.is_empty() {
                vec![String::new()]
            } else {
                chunks
            }
        })
        .collect();
    let shown = if preview.len() > room {
        room.saturating_sub(1)
    } else {
        preview.len()
    };
    let style = Style::new().dim();
    lines.extend(
        preview[..shown]
            .iter()
            .map(|l| Line::styled(format!("  │ {l}"), style)),
    );
    if shown < preview.len() && room > 0 {
        lines.push(Line::styled(
            format!("  │ … {} more lines", preview.len() - shown),
            style,
        ));
    }
    let mut options = Vec::new();
    for (i, label) in APPROVAL_OPTIONS.iter().enumerate() {
        let text = format!(" {label} ");
        options.push(if i == a.choice {
            Span::styled(text, Style::new().fg(accent).bold().reversed())
        } else {
            Span::raw(text)
        });
        options.push(Span::raw(" "));
    }
    lines.push(Line::from(options));
    lines
}

/// What a call needing approval will do, one string per line.
fn approval_preview(name: &str, arguments: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.lines().map(String::from).collect();
    };
    let str_arg = |k: &str| v.get(k).and_then(|x| x.as_str());
    let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
    match name {
        "run" if str_arg("command").is_some() => lines(str_arg("command").unwrap_or_default()),
        "edit_file" if str_arg("old_string").is_some() => {
            let mut out: Vec<String> = str_arg("old_string")
                .unwrap_or_default()
                .lines()
                .map(|l| format!("- {l}"))
                .collect();
            out.extend(
                str_arg("new_string")
                    .unwrap_or_default()
                    .lines()
                    .map(|l| format!("+ {l}")),
            );
            out
        }
        "write_file" if str_arg("content").is_some() => {
            lines(str_arg("content").unwrap_or_default())
        }
        _ => serde_json::to_string_pretty(&v)
            .map(|s| lines(&s))
            .unwrap_or_default(),
    }
}

/// Splits on newlines, then hard-wraps by chars so the cursor position is trivially computable.
/// Returns the rows and the cursor's (row, col).
fn input_lines(input: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let mut lines = Vec::new();
    let mut pos = (0, 0);
    let mut start = 0;
    for logical in input.split('\n') {
        let chars: Vec<char> = logical.chars().collect();
        if (start..=start + chars.len()).contains(&cursor) {
            let off = cursor - start;
            pos = (lines.len() + off / width, off % width);
        }
        lines.extend(chars.chunks(width).map(|c| c.iter().collect::<String>()));
        if chars.len().is_multiple_of(width) {
            lines.push(String::new());
        }
        start += chars.len() + 1;
    }
    (lines, pos.0, pos.1)
}

fn status_line(app: &App) -> Line<'static> {
    let (label, color) = match app.mode {
        Mode::Plan => (" PLAN ", Color::Cyan),
        Mode::Build => (" BUILD ", Color::LightBlue),
    };
    let mut spans = vec![Span::styled(
        label,
        Style::new().fg(Color::Black).bg(color).bold(),
    )];
    spans.push(Span::raw(format!(" {} ", app.model_label())));
    if let Some(think) = &app.think {
        spans.push(Span::styled(
            format!("· think {think} "),
            Style::new().fg(Color::DarkGray),
        ));
    }
    if let Some(u) = &app.usage {
        let mut s = format!("· ctx {}", kilo(u.used));
        let mut fg = Color::DarkGray;
        if let Some(w) = u.window.filter(|&w| w > 0) {
            let pct = u.used as u64 * 100 / w as u64;
            s += &format!("/{} {pct}%", kilo(w));
            fg = match pct {
                0..50 => Color::DarkGray,
                50..75 => Color::Gray,
                _ => Color::LightRed,
            };
        }
        if let Some(c) = u.cached {
            s += &format!(" · cache {}", kilo(c));
        }
        spans.push(Span::styled(s, Style::new().fg(fg)));
    }
    if !app.always.is_empty() {
        let tools = app.always.iter().cloned().collect::<Vec<_>>().join(", ");
        spans.push(Span::styled(
            format!(" · auto {tools}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    if app.unseen > 0 {
        spans.push(Span::styled(
            format!(" · ↓ {} new lines", app.unseen),
            Style::new().fg(Color::Yellow),
        ));
    }
    if app.quit_armed {
        spans.push(Span::styled(
            " · Ctrl+C again to quit",
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    Line::from(spans)
}

fn live_line(app: &App) -> Line<'static> {
    let (phase, started) = &app.phase;
    let label = match phase {
        Phase::Waiting => "waiting".to_string(),
        Phase::Thinking => "thinking".to_string(),
        Phase::Writing => "writing".to_string(),
        Phase::Running(name) => format!("running {name}"),
    };
    let secs = started.elapsed().as_secs();
    let elapsed = if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    };
    let icon = SPINNER[app.tick % SPINNER.len()];
    Line::from(vec![
        Span::styled(
            format!("{icon} {label} "),
            Style::new().fg(Color::Magenta).bold(),
        ),
        Span::styled(elapsed, Style::new().dim()),
    ])
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
    for entry in app.log.iter() {
        match entry {
            Entry::User(t) => {
                for (i, l) in t.lines().enumerate() {
                    let prefix = if i == 0 { "> " } else { "  " };
                    lines.push(Line::from(format!("{prefix}{l}")).bold().fg(Color::Blue));
                }
            }
            Entry::Assistant(t) => lines.extend(markdown::render(t).lines),
            Entry::Tool { name, arguments } => lines.push(Line::from(vec![
                Span::styled(
                    format!("⚙ {} ", display_name(name)),
                    Style::new().fg(Color::Magenta).bold(),
                ),
                Span::styled(
                    truncate(&tool_summary(name, arguments), 200),
                    Style::new().dim(),
                ),
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

/// Human-readable form of tool arguments; falls back to raw JSON for unknown
/// tools or arguments that don't parse.
fn tool_summary(name: &str, arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    let str_arg = |k: &str| v.get(k).and_then(|x| x.as_str());
    let path = str_arg("path");
    match (name, path) {
        ("read_file", Some(p)) => {
            let offset = v.get("offset").and_then(|x| x.as_u64());
            let limit = v.get("limit").and_then(|x| x.as_u64());
            match (offset, limit) {
                (Some(o), Some(l)) => format!("{p}:{o}-{}", o + l.saturating_sub(1)),
                (Some(o), None) => format!("{p}:{o}-"),
                (None, Some(l)) => format!("{p}:1-{l}"),
                (None, None) => p.to_string(),
            }
        }
        ("read_symbol", Some(p)) => match v.get("name") {
            Some(serde_json::Value::String(n)) => format!("{n} in {p}"),
            Some(serde_json::Value::Array(ns)) => {
                let ns: Vec<&str> = ns.iter().filter_map(|n| n.as_str()).collect();
                format!("{} in {p}", ns.join(", "))
            }
            // Small files come back whole, so "outline" would be misleading.
            _ => p.to_string(),
        },
        ("grep", _) => match str_arg("pattern") {
            Some(pat) => format!("/{pat}/ in {}", path.unwrap_or(".")),
            None => arguments.to_string(),
        },
        ("list_dir", _) => path.unwrap_or(".").to_string(),
        ("write_file", Some(p)) => match str_arg("content") {
            Some(c) => format!("{p} ({} lines)", c.lines().count()),
            None => p.to_string(),
        },
        ("edit_file", Some(p)) => {
            if v.get("replace_all").and_then(|x| x.as_bool()) == Some(true) {
                format!("{p} (replace all)")
            } else {
                p.to_string()
            }
        }
        // A span can't hold line breaks, so multi-line scripts are flattened.
        ("run", _) => match str_arg("command") {
            Some(c) if v.get("background").and_then(|x| x.as_bool()) == Some(true) => {
                format!("{} &", c.trim().replace('\n', " ↵ "))
            }
            Some(c) => c.trim().replace('\n', " ↵ "),
            None => arguments.to_string(),
        },
        (n, _) if n.contains("__") => mcp_summary(&v).unwrap_or_else(|| arguments.to_string()),
        _ => arguments.to_string(),
    }
}

/// MCP tools have arbitrary schemas: a lone argument is shown as its bare
/// value, several as `key=value` pairs.
fn mcp_summary(v: &serde_json::Value) -> Option<String> {
    let obj = v.as_object()?;
    let flat = |x: &serde_json::Value| match x {
        serde_json::Value::String(s) => s.trim().replace('\n', " ↵ "),
        other => other.to_string(),
    };
    Some(match obj.len() {
        1 => flat(obj.values().next()?),
        _ => obj
            .iter()
            .map(|(k, x)| format!("{k}={}", flat(x)))
            .collect::<Vec<_>>()
            .join("  "),
    })
}

/// `server__tool` → `server: tool`; built-in names are returned as is.
fn display_name(name: &str) -> String {
    match name.split_once("__") {
        Some((server, tool)) => format!("{server}: {tool}"),
        None => name.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}
