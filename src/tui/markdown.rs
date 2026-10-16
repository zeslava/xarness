use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

pub fn render(md: &str) -> Text<'static> {
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut r = Renderer::default();
    for event in Parser::new_ext(md, opts) {
        r.event(event);
    }
    r.flush();
    Text::from(r.lines)
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// One entry per open list: the next number for ordered lists.
    lists: Vec<Option<u64>>,
    quote: usize,
    marker: Option<String>,
    in_code: bool,
    link: Option<(String, String)>,
    need_blank: bool,
}

impl Renderer {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) if self.in_code => {
                let mut parts = t.split('\n').peekable();
                while let Some(part) = parts.next() {
                    if !part.is_empty() {
                        self.push(part.to_string(), Style::new().fg(Color::LightBlue));
                    }
                    if parts.peek().is_some() {
                        self.flush_line(true);
                    }
                }
            }
            Event::Text(t) => {
                if let Some((_, text)) = &mut self.link {
                    text.push_str(&t);
                }
                self.push(t.into_string(), self.style());
            }
            Event::Code(t) => self.push(t.into_string(), self.style().fg(Color::LightBlue)),
            Event::Html(t) | Event::InlineHtml(t) => self.push(t.into_string(), self.style()),
            Event::SoftBreak => self.push(" ".into(), self.style()),
            Event::HardBreak => self.flush_line(true),
            Event::Rule => {
                self.flush();
                self.push("─".repeat(40), Style::new().add_modifier(Modifier::DIM));
                self.flush_block();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.push(mark.into(), self.style());
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush();
                let mut style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
                if level == HeadingLevel::H1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.push(lang.into_string(), Style::new().add_modifier(Modifier::DIM));
                    self.flush_line(false);
                }
                self.in_code = true;
            }
            Tag::List(start) => {
                self.flush_line(false);
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_line(false);
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "• ".into(),
                });
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(
                    Style::new()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.link = Some((dest_url.into_string(), String::new()));
            }
            Tag::Table(_) => self.flush(),
            Tag::TableHead => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::TableCell if self.spans.iter().any(|s| !s.content.trim().is_empty()) => {
                self.push(" │ ".into(), Style::new().add_modifier(Modifier::DIM));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush_block();
            }
            TagEnd::Paragraph | TagEnd::Table => self.flush_block(),
            TagEnd::BlockQuote(_) => {
                self.flush_block();
                self.quote -= 1;
            }
            TagEnd::CodeBlock => {
                self.in_code = false;
                self.flush_block();
            }
            TagEnd::List(_) => {
                self.flush_line(false);
                self.lists.pop();
                if self.lists.is_empty() {
                    self.need_blank = true;
                }
            }
            TagEnd::Item => self.flush_line(false),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, text)) = self.link.take()
                    && !url.is_empty()
                    && url != text
                {
                    self.push(
                        format!(" ({url})"),
                        Style::new().add_modifier(Modifier::DIM),
                    );
                }
            }
            TagEnd::TableHead => {
                self.styles.pop();
                self.flush_line(false);
            }
            TagEnd::TableRow => self.flush_line(false),
            _ => {}
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::new(), |acc, s| acc.patch(*s))
    }

    fn push(&mut self, text: String, style: Style) {
        if self.spans.is_empty() {
            self.begin_line();
        }
        self.spans.push(Span::styled(text, style));
    }

    fn begin_line(&mut self) {
        if self.need_blank && !self.lines.is_empty() {
            self.lines.push(Line::from(self.quote_prefix()));
        }
        self.need_blank = false;
        self.spans = self.quote_prefix();
        if let Some(depth) = self.lists.len().checked_sub(1) {
            self.spans.push(Span::raw("  ".repeat(depth)));
            let marker = self.marker.take().unwrap_or_else(|| "  ".into());
            self.spans
                .push(Span::styled(marker, Style::new().fg(Color::DarkGray)));
        }
        if self.in_code {
            self.spans.push(Span::raw("  "));
        }
    }

    fn quote_prefix(&self) -> Vec<Span<'static>> {
        let style = Style::new().fg(Color::DarkGray);
        (0..self.quote).map(|_| Span::styled("│ ", style)).collect()
    }

    /// `force` emits a line even with no content, for blank lines inside code blocks and
    /// hard breaks.
    fn flush_line(&mut self, force: bool) {
        if self.spans.is_empty() {
            if !force {
                return;
            }
            self.begin_line();
        }
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
    }

    fn flush(&mut self) {
        self.flush_line(false);
    }

    fn flush_block(&mut self) {
        self.flush();
        self.need_blank = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(md: &str) -> Vec<String> {
        render(md)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn blocks_are_separated_by_one_blank_line() {
        assert_eq!(
            plain("# T\n\ntext\n\n- a\n- b\n\nend"),
            ["T", "", "text", "", "• a", "• b", "", "end"]
        );
    }

    #[test]
    fn code_block_keeps_blank_lines() {
        assert_eq!(
            plain("```rs\nfn a() {}\n\nfn b() {}\n```"),
            ["rs", "  fn a() {}", "  ", "  fn b() {}"]
        );
    }

    #[test]
    fn unclosed_fence_renders_while_streaming() {
        assert_eq!(plain("```\nlet x"), ["  let x"]);
    }

    #[test]
    fn nested_and_ordered_lists() {
        assert_eq!(plain("1. a\n   - b\n2. c"), ["1. a", "  • b", "2. c"]);
    }

    #[test]
    fn link_shows_url_unless_autolink() {
        assert_eq!(plain("[x](http://a) <http://b>"), ["x (http://a) http://b"]);
    }
}
