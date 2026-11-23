//! Log mutations invalidate only the entries they expose for editing.
use std::cell::{OnceCell, RefCell, RefMut};

use super::{app::Entry, ui};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::Text,
    widgets::{Paragraph, Widget, Wrap},
};

struct CachedEntry {
    entry: Entry,
    text: OnceCell<Text<'static>>,
}

#[derive(Default)]
pub(super) struct Log {
    entries: Vec<CachedEntry>,
    cache: RefCell<Rendered>,
}

#[derive(Default)]
pub(super) struct Rendered {
    pub text: Text<'static>,
    pub total: usize,
    valid: bool,
    #[cfg(test)]
    builds: usize,
    #[cfg(test)]
    paints: usize,
    width: Option<u16>,
    viewport: Option<(Rect, u16, Buffer)>,
}

impl From<Vec<Entry>> for Log {
    fn from(entries: Vec<Entry>) -> Self {
        let mut log = Self::default();
        for entry in entries {
            log.push(entry);
        }
        log
    }
}

impl Log {
    fn invalidate(&mut self) {
        self.cache.get_mut().valid = false;
    }
    pub fn push(&mut self, entry: Entry) {
        self.invalidate();
        self.entries.push(CachedEntry {
            entry,
            text: OnceCell::new(),
        });
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        // Release both the old text and viewport immediately.
        *self.cache.get_mut() = Rendered::default();
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&Entry) -> bool) {
        let old = self.entries.len();
        self.entries.retain(|e| keep(&e.entry));
        if old != self.entries.len() {
            self.invalidate();
        }
    }
    pub fn last_mut(&mut self) -> Option<&mut Entry> {
        self.invalidate();
        self.entries.last_mut().map(|e| {
            e.text.take();
            &mut e.entry
        })
    }
    pub fn update_where(
        &mut self,
        mut matches: impl FnMut(&Entry) -> bool,
        mut update: impl FnMut(&mut Entry),
    ) {
        let mut changed = false;
        for cached in &mut self.entries {
            if matches(&cached.entry) {
                cached.text.take();
                update(&mut cached.entry);
                changed = true;
            }
        }
        if changed {
            self.invalidate();
        }
    }
    pub fn prepare(&self, width: u16) -> RefMut<'_, Rendered> {
        let mut cache = self.cache.borrow_mut();
        if !cache.valid {
            #[cfg(test)]
            {
                cache.builds += 1;
            }
            cache.text = Text::from(
                self.entries
                    .iter()
                    .flat_map(|e| {
                        e.text
                            .get_or_init(|| ui::log_text(std::slice::from_ref(&e.entry)))
                            .lines
                            .clone()
                    })
                    .collect::<Vec<_>>(),
            );
            cache.valid = true;
            cache.width = None;
            cache.viewport = None;
        }
        if cache.width != Some(width) {
            cache.total = Paragraph::new(cache.text.clone())
                .wrap(Wrap { trim: false })
                .line_count(width);
            cache.width = Some(width);
            cache.viewport = None;
        }
        cache
    }
}

impl Rendered {
    pub fn paint(&mut self, area: Rect, offset: u16, target: &mut Buffer) {
        if !self
            .viewport
            .as_ref()
            .is_some_and(|(a, o, _)| *a == area && *o == offset)
        {
            #[cfg(test)]
            {
                self.paints += 1;
            }
            let mut buffer = Buffer::empty(area);
            Paragraph::new(self.text.clone())
                .wrap(Wrap { trim: false })
                .scroll((offset, 0))
                .render(area, &mut buffer);
            self.viewport = Some((area, offset, buffer));
        }
        let buffer = &self.viewport.as_ref().unwrap().2;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                target[(x, y)] = buffer[(x, y)].clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::ToolStatus;
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn assert_matches_uncached(log: &Log, width: u16, height: u16, offset: u16) {
        let entries: Vec<_> = log.entries.iter().map(|e| e.entry.clone()).collect();
        let text = ui::log_text(&entries);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let mut cached = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut reference = Terminal::new(TestBackend::new(width, height)).unwrap();
        cached
            .draw(|f| {
                let mut rendered = log.prepare(width);
                assert_eq!(rendered.total, paragraph.line_count(width));
                rendered.paint(f.area(), offset, f.buffer_mut());
            })
            .unwrap();
        reference
            .draw(|f| f.render_widget(paragraph.clone().scroll((offset, 0)), f.area()))
            .unwrap();
        assert_eq!(cached.backend().buffer(), reference.backend().buffer());
    }

    #[test]
    fn cached_view_matches_full_render_after_mutations_and_resize() {
        let mut log = Log::from(vec![
            Entry::User("a long prompt that wraps onto several lines".into()),
            Entry::Assistant("**hello**\n\n```rust\nlet x = 1;\n```".into()),
        ]);
        for (width, height, offset) in [(40, 10, 0), (12, 4, 2), (40, 3, 1)] {
            assert_matches_uncached(&log, width, height, offset);
        }
        if let Some(Entry::Assistant(text)) = log.last_mut() {
            text.push_str("\nmore text");
        }
        assert_matches_uncached(&log, 20, 5, 1);
        log.push(Entry::Tool {
            name: "edit_file".into(),
            arguments: r#"{"old_string":"a\nb","new_string":"a\nc"}"#.into(),
            agent: None,
            status: ToolStatus::Running,
        });
        assert_matches_uncached(&log, 30, 12, 0);
        log.update_where(
            |e| matches!(e, Entry::Tool { .. }),
            |e| {
                if let Entry::Tool { status, .. } = e {
                    *status = ToolStatus::Done;
                }
            },
        );
        assert_matches_uncached(&log, 30, 12, 0);
        log.retain(|e| !matches!(e, Entry::Tool { .. }));
        assert_matches_uncached(&log, 30, 12, 0);
        log.clear();
        assert_matches_uncached(&log, 30, 12, 0);
        log = vec![Entry::Info("session resumed".into())].into();
        assert_matches_uncached(&log, 30, 12, 0);
    }

    #[test]
    fn unchanged_frames_reuse_text_and_viewport() {
        let mut log = Log::from(vec![
            Entry::Assistant("**finished**".into()),
            Entry::Assistant("stream".into()),
        ]);
        let area = Rect::new(2, 1, 25, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        for _ in 0..100 {
            log.prepare(area.width).paint(area, 0, &mut buffer);
        }
        assert_eq!(log.cache.borrow().builds, 1);
        assert_eq!(log.cache.borrow().paints, 1);
        if let Some(Entry::Assistant(text)) = log.last_mut() {
            text.push_str(" delta");
        }
        assert!(log.entries[0].text.get().is_some());
        assert!(log.entries[1].text.get().is_none());
        log.prepare(area.width).paint(area, 0, &mut buffer);
        assert_eq!(log.cache.borrow().builds, 2);
        assert_eq!(log.cache.borrow().paints, 2);
        // Geometry changes repaint, without re-parsing the entries.
        log.prepare(15)
            .paint(Rect::new(0, 0, 15, 3), 1, &mut buffer);
        assert_eq!(log.cache.borrow().builds, 2);
        assert_eq!(log.cache.borrow().paints, 3);
    }
}
