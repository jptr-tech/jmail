//! Markdown handling for the composer.
//!
//! The composer edits markdown; [`render_html`] produces the HTML alternative
//! used for sending and for the live preview, [`render_text`] produces the
//! plain-text alternative, and [`style_ranges`] feeds the live formatting in
//! the editor.

use pulldown_cmark::{html, Event, Options, Parser, Tag, TagEnd};

fn options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

fn parser(markdown: &str) -> Parser<'_> {
    Parser::new_ext(markdown, options())
}

/// Render markdown to an HTML fragment (no surrounding document).
pub fn render_html(markdown: &str) -> String {
    let mut out = String::new();
    html::push_html(&mut out, parser(markdown));
    out
}

/// Render markdown to a readable plain-text alternative.
pub fn render_text(markdown: &str) -> String {
    let mut out = String::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut link_url: Option<String> = None;
    let mut in_code_block = false;

    for event in parser(markdown) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {}
                Tag::BlockQuote(_) => push_line_prefix(&mut out, "> "),
                Tag::CodeBlock(_) => {
                    in_code_block = true;
                    ensure_blank_line(&mut out);
                    out.push_str("    ");
                }
                Tag::List(start) => {
                    ensure_newline(&mut out);
                    list_stack.push(start);
                }
                Tag::Item => {
                    ensure_newline(&mut out);
                    match list_stack.last().copied().flatten() {
                        Some(mut number) => {
                            if let Some(slot) = list_stack.last_mut() {
                                *slot = Some(number);
                            }
                            out.push_str(&format!("{number}. "));
                            number += 1;
                            if let Some(slot) = list_stack.last_mut() {
                                *slot = Some(number);
                            }
                        }
                        None => out.push_str("* "),
                    }
                }
                Tag::Table(_) => {}
                Tag::TableHead | Tag::TableRow => ensure_newline(&mut out),
                Tag::TableCell => {
                    if !out.ends_with('|') && !out.ends_with('\n') && !out.is_empty() {
                        out.push_str(" | ");
                    }
                }
                Tag::Link { dest_url, .. } => link_url = Some(dest_url.to_string()),
                Tag::Image { dest_url, .. } => {
                    out.push_str(&format!("[image: {dest_url}]"));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("\n\n"),
                TagEnd::Heading(_) => out.push_str("\n\n"),
                TagEnd::BlockQuote(_) => ensure_newline(&mut out),
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    out.push('\n');
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    ensure_newline(&mut out);
                }
                TagEnd::Item => ensure_newline(&mut out),
                TagEnd::TableRow | TagEnd::TableHead => out.push('\n'),
                TagEnd::Link => {
                    if let Some(url) = link_url.take() {
                        out.push_str(&format!(" <{url}>"));
                    }
                }
                _ => {}
            },
            Event::Text(text) => out.push_str(&text),
            Event::Code(code) => {
                out.push('`');
                out.push_str(&code);
                out.push('`');
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push('\n'),
            Event::Rule => {
                ensure_blank_line(&mut out);
                out.push_str("---\n");
            }
            Event::TaskListMarker(done) => {
                out.push_str(if done { "[x] " } else { "[ ] " });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if !in_code_block {
                    out.push_str(&html);
                }
            }
            _ => {}
        }
    }

    // Collapse trailing whitespace and runs of blank lines.
    let mut lines: Vec<&str> = out.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    if !out.is_empty() {
        ensure_newline(out);
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
}

fn push_line_prefix(out: &mut String, prefix: &str) {
    ensure_newline(out);
    out.push_str(prefix);
}

/// The kind of markdown styling, surfaced to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Heading(u8),
    Bold,
    Italic,
    Strikethrough,
    Code,
    CodeBlock,
    Quote,
    Link,
}

/// A styled span of the markdown source, as **byte** offsets into the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleRange {
    pub start: usize,
    pub end: usize,
    pub kind: StyleKind,
}

/// Compute the content spans (excluding syntax markers) for live editor
/// formatting. Markers between spans are intentionally left out so the editor
/// can dim them.
pub fn style_ranges(markdown: &str) -> Vec<StyleRange> {
    let mut ranges = Vec::new();
    let mut active: Vec<StyleKind> = Vec::new();
    let mut link_depth = 0usize;

    for (event, range) in Parser::new_ext(markdown, options()).into_offset_iter() {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => active.push(StyleKind::Heading(level as u8)),
                Tag::Emphasis => active.push(StyleKind::Italic),
                Tag::Strong => active.push(StyleKind::Bold),
                Tag::Strikethrough => active.push(StyleKind::Strikethrough),
                Tag::BlockQuote(_) => active.push(StyleKind::Quote),
                Tag::CodeBlock(_) => active.push(StyleKind::CodeBlock),
                Tag::Link { .. } => {
                    link_depth += 1;
                    active.push(StyleKind::Link);
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_)
                | TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock => {
                    active.pop();
                }
                TagEnd::Link => {
                    link_depth = link_depth.saturating_sub(1);
                    active.pop();
                }
                _ => {}
            },
            Event::Text(_) | Event::Code(_) => {
                for kind in &active {
                    ranges.push(StyleRange {
                        start: range.start,
                        end: range.end,
                        kind: *kind,
                    });
                }
            }
            _ => {}
        }
    }
    ranges
}

/// Byte ranges of markdown syntax markers (`#`, `>`, list bullets, `**`, `*`,
/// `_`, `~~`, backticks) so the editor can dim them.
pub fn marker_ranges(markdown: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    for line in markdown.split_inclusive('\n') {
        let line_start = offset;
        let line_bytes = line.as_bytes();
        offset += line_bytes.len();

        let trimmed_start = line.len() - line.trim_start().len();
        let rest = &line[trimmed_start..];

        // Leading heading hashes.
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        if hashes > 0 && rest.as_bytes().get(hashes) == Some(&b' ') {
            ranges.push((line_start + trimmed_start, line_start + trimmed_start + hashes));
        }
        // Blockquote marker.
        if rest.starts_with('>') {
            ranges.push((line_start + trimmed_start, line_start + trimmed_start + 1));
        }
        // List bullets (`- `, `* `, `+ `) and ordered (`1. `).
        if rest.len() >= 2
            && matches!(&rest[..1], "-" | "*" | "+")
            && rest.as_bytes()[1] == b' '
        {
            ranges.push((line_start + trimmed_start, line_start + trimmed_start + 1));
        } else {
            let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0
                && rest.as_bytes().get(digits) == Some(&b'.')
                && rest.as_bytes().get(digits + 1) == Some(&b' ')
            {
                ranges.push((line_start + trimmed_start, line_start + trimmed_start + digits + 1));
            }
        }

        // Inline delimiters.
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let run = |ch: u8| bytes[i..].iter().take_while(|c| **c == ch).count();
            match b {
                b'*' | b'_' => {
                    let len = run(b).min(2);
                    if len >= 1 {
                        ranges.push((line_start + i, line_start + i + len));
                        i += len;
                        continue;
                    }
                }
                b'~' => {
                    let len = run(b).min(2);
                    if len == 2 {
                        ranges.push((line_start + i, line_start + i + 2));
                        i += 2;
                        continue;
                    }
                }
                b'`' => {
                    let len = run(b);
                    ranges.push((line_start + i, line_start + i + len));
                    i += len;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings_and_emphasis_to_html() {
        let html = render_html("# Title\n\nSome **bold** and *italic* text.");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>italic</em>"));
    }

    #[test]
    fn renders_lists_and_links_to_html() {
        let html = render_html("- one\n- two\n\n[link](https://example.org)");
        assert!(html.contains("<li>one</li>"));
        assert!(html.contains("<a href=\"https://example.org\">link</a>"));
    }

    #[test]
    fn renders_gfm_strikethrough_and_tables() {
        let html = render_html("~~gone~~\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(html.contains("<del>gone</del>"));
        assert!(html.contains("<table>"));
    }

    #[test]
    fn plain_text_keeps_readable_structure() {
        let text = render_text("# Title\n\n- one\n- two\n\n> quoted");
        assert!(text.contains("Title"));
        assert!(text.contains("* one"));
        assert!(text.contains("> quoted"));
        assert!(!text.contains("**"));
    }

    #[test]
    fn plain_text_appends_link_targets() {
        let text = render_text("See [the docs](https://example.org).");
        assert!(text.contains("the docs"));
        assert!(text.contains("<https://example.org>"));
    }

    #[test]
    fn style_ranges_cover_content_not_markers() {
        let ranges = style_ranges("Some **bold** text");
        let bold: Vec<_> = ranges
            .iter()
            .filter(|range| range.kind == StyleKind::Bold)
            .collect();
        assert_eq!(bold.len(), 1);
        let source = "Some **bold** text";
        assert_eq!(&source[bold[0].start..bold[0].end], "bold");
    }

    #[test]
    fn style_ranges_mark_headings() {
        let ranges = style_ranges("# Heading");
        assert!(ranges
            .iter()
            .any(|range| range.kind == StyleKind::Heading(1)));
    }
}
