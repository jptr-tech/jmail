//! Markdown composer widget: a `TextView` with live inline styling (headings,
//! bold, italic, code, quotes, links) and dimmed syntax markers, plus a
//! rendered HTML preview.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::pango;
use webkit6::prelude::*;

use jmail_core::markdown::{marker_ranges, render_html, style_ranges, StyleKind};

/// An editor with a live-styled source view and an HTML preview.
#[derive(Clone)]
pub struct MarkdownEditor {
    pub container: gtk::Box,
    pub stack: gtk::Stack,
    pub view: gtk::TextView,
    pub preview: webkit6::WebView,
    buffer: gtk::TextBuffer,
    marker: gtk::TextTag,
    headings: [gtk::TextTag; 6],
    bold: gtk::TextTag,
    italic: gtk::TextTag,
    strike: gtk::TextTag,
    code: gtk::TextTag,
    code_block: gtk::TextTag,
    quote: gtk::TextTag,
    link: gtk::TextTag,
    updating: Rc<Cell<bool>>,
}

impl MarkdownEditor {
    pub fn new() -> Self {
        let table = gtk::TextTagTable::new();

        let marker = tag(&table, "md-marker");
        marker.set_foreground(Some("#8b8b8b"));

        let headings = std::array::from_fn(|index| {
            let heading = tag(&table, &format!("md-h{}", index + 1));
            heading.set_weight(700);
            heading.set_scale([1.7, 1.45, 1.25, 1.15, 1.08, 1.0][index]);
            heading
        });

        let bold = tag(&table, "md-bold");
        bold.set_weight(700);

        let italic = tag(&table, "md-italic");
        italic.set_style(pango::Style::Italic);

        let strike = tag(&table, "md-strike");
        strike.set_strikethrough(true);

        let code = tag(&table, "md-code");
        code.set_family(Some("monospace"));
        code.set_foreground(Some("#c7254e"));

        let code_block = tag(&table, "md-code-block");
        code_block.set_family(Some("monospace"));

        let quote = tag(&table, "md-quote");
        quote.set_style(pango::Style::Italic);
        quote.set_foreground(Some("#7a7a7a"));

        let link = tag(&table, "md-link");
        link.set_foreground(Some("#1c71d8"));
        link.set_underline(pango::Underline::Single);

        let buffer = gtk::TextBuffer::new(Some(&table));
        buffer.set_enable_undo(true);

        let view = gtk::TextView::with_buffer(&buffer);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_top_margin(10);
        view.set_bottom_margin(10);
        view.set_left_margin(12);
        view.set_right_margin(12);
        view.set_monospace(false);

        let editor_scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .min_content_height(160)
            .build();

        let preview = webkit6::WebView::new();
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&preview) {
            settings.set_enable_javascript(false);
            settings.set_enable_javascript_markup(false);
        }

        let stack = gtk::Stack::new();
        stack.add_named(&editor_scroll, Some("edit"));
        stack.add_named(&preview, Some("preview"));
        stack.set_visible_child_name("edit");

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.append(&stack);
        container.set_vexpand(true);

        let editor = Self {
            container,
            stack,
            view,
            preview,
            buffer,
            marker,
            headings,
            bold,
            italic,
            strike,
            code,
            code_block,
            quote,
            link,
            updating: Rc::new(Cell::new(false)),
        };
        editor.install_live_styling();
        editor
    }

    pub fn text(&self) -> String {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), true)
            .to_string()
    }

    pub fn set_text(&self, text: &str) {
        self.buffer.set_text(text);
        self.restyle();
        // GTK leaves the insert mark at the end after `set_text`; put it at the
        // top so replies start above the quoted text.
        self.place_cursor_at_start();
    }

    /// Move the cursor (and scroll) to the start of the text.
    pub fn place_cursor_at_start(&self) {
        self.buffer.place_cursor(&self.buffer.start_iter());
        self.scroll_cursor(0.0);
    }

    /// Move the cursor (and scroll) to the end of the text.
    pub fn place_cursor_at_end(&self) {
        self.buffer.place_cursor(&self.buffer.end_iter());
        self.scroll_cursor(1.0);
    }

    fn scroll_cursor(&self, yalign: f64) {
        self.view.scroll_to_mark(
            &self.buffer.get_insert(),
            0.0,
            true,
            0.0,
            yalign,
        );
    }

    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty()
    }

    /// Move keyboard focus into the editor.
    pub fn focus(&self) {
        self.view.grab_focus();
    }

    /// Toggle between the source editor and the rendered preview.
    pub fn set_preview(&self, enabled: bool) {
        if enabled {
            let html = format!(
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
                 <style>{}</style></head><body>{}</body></html>",
                preview_css(),
                render_html(&self.text())
            );
            self.preview.load_html(&html, None);
            self.stack.set_visible_child_name("preview");
        } else {
            self.stack.set_visible_child_name("edit");
        }
    }

    fn install_live_styling(&self) {
        let weak = self.clone();
        self.buffer.connect_changed(move |_| weak.restyle());
        let weak = self.clone();
        self.buffer
            .connect_cursor_position_notify(move |_| weak.restyle());
        self.restyle();
    }

    fn tag_for(&self, kind: StyleKind) -> &gtk::TextTag {
        match kind {
            StyleKind::Heading(level) => {
                let index = (level.clamp(1, 6) - 1) as usize;
                &self.headings[index]
            }
            StyleKind::Bold => &self.bold,
            StyleKind::Italic => &self.italic,
            StyleKind::Strikethrough => &self.strike,
            StyleKind::Code => &self.code,
            StyleKind::CodeBlock => &self.code_block,
            StyleKind::Quote => &self.quote,
            StyleKind::Link => &self.link,
        }
    }

    fn restyle(&self) {
        if self.updating.replace(true) {
            return;
        }
        let text = self.text();
        let map = byte_to_char(&text);
        let to_char = |byte: usize| map.get(byte).copied().unwrap_or(map.len().saturating_sub(1));

        self.buffer
            .remove_all_tags(&self.buffer.start_iter(), &self.buffer.end_iter());

        // The cursor's line shows its raw markers; everywhere else they are dim.
        let cursor = self.buffer.cursor_position().max(0) as i32;
        let mut line_start = self.buffer.iter_at_offset(cursor);
        while !line_start.starts_line() {
            if !line_start.backward_char() {
                break;
            }
        }
        let active_start = line_start.offset() as usize;
        let mut line_end = self.buffer.iter_at_offset(cursor);
        line_end.forward_to_line_end();
        let active_end = line_end.offset() as usize;

        for (start, end) in marker_ranges(&text) {
            let (start, end) = (to_char(start), to_char(end));
            // Reveal markers on the line the cursor is on.
            if start >= active_start && start <= active_end {
                continue;
            }
            self.apply(self.marker.clone(), start, end);
        }

        for range in style_ranges(&text) {
            let tag = self.tag_for(range.kind).clone();
            self.apply(tag, to_char(range.start), to_char(range.end));
        }

        self.updating.set(false);
    }

    fn apply(&self, tag: gtk::TextTag, start: usize, end: usize) {
        if end <= start {
            return;
        }
        let buffer = &self.buffer;
        let start_iter = buffer.iter_at_offset(start as i32);
        let end_iter = buffer.iter_at_offset(end as i32);
        buffer.apply_tag(&tag, &start_iter, &end_iter);
    }
}

fn tag(table: &gtk::TextTagTable, name: &str) -> gtk::TextTag {
    let tag = gtk::TextTag::new(Some(name));
    table.add(&tag);
    tag
}

/// Map every byte offset in `text` to a character offset.
fn byte_to_char(text: &str) -> Vec<usize> {
    let mut map = vec![0usize; text.len() + 1];
    let mut chars = 0usize;
    for (byte, _) in text.char_indices() {
        map[byte] = chars;
        chars += 1;
    }
    map[text.len()] = chars;
    // Continuation bytes inherit the preceding boundary's char offset.
    let mut last = 0usize;
    for byte in 0..=text.len() {
        if text.is_char_boundary(byte) {
            last = map[byte];
        } else {
            map[byte] = last;
        }
    }
    map
}

fn preview_css() -> &'static str {
    "body{font-family:sans-serif;margin:16px;line-height:1.5;color:#222;background:#fff}\
     code,pre{font-family:monospace;background:#f4f4f4;border-radius:4px}\
     pre{padding:8px;overflow:auto}\
     blockquote{border-left:3px solid #999;margin-left:0;padding-left:12px;color:#666}\
     a{color:#1c71d8}\
     @media (prefers-color-scheme: dark){\
       body{color:#eee;background:#1e1e1e}\
       code,pre{background:#2a2a2a}\
       blockquote{color:#aaa;border-color:#666}\
       a{color:#78aeed}\
     }"
}

#[cfg(test)]
mod tests {
    use super::byte_to_char;

    #[test]
    fn maps_ascii_bytes_to_chars() {
        assert_eq!(byte_to_char("ab"), vec![0, 1, 2]);
    }

    #[test]
    fn maps_multibyte_text() {
        // "é" is two bytes, so "!" starts at byte 2 / char 1.
        let map = byte_to_char("é!");
        assert_eq!(map[0], 0);
        assert_eq!(map[2], 1);
        assert_eq!(map[3], 2);
    }
}
