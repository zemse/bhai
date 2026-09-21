//! Markdown to styled, pre-wrapped lines for assistant text. Display only: the raw
//! text stays what is stored and copied.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::syntax::{self, PLAIN};
use crate::wrap::Join;

/// One styled character; lines are built from these so wrapping keeps styles.
type Cell = (char, Style);

const CODE: Style = Style::new().fg(Color::Green);
const DIM: Style = Style::new().fg(Color::DarkGray);
/// Code block lines are indented by this much.
const CODE_INDENT: &str = "  ";

/// Renders `text` into lines no wider than `width` chars, each with how it joins the one
/// above so a copy of a selection can undo the wrapping done here.
pub fn render(text: &str, width: usize) -> (Vec<Line<'static>>, Vec<Join>) {
    let mut renderer = Renderer {
        width: width.max(1),
        ..Renderer::default()
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.flush();
    (renderer.lines, renderer.joins)
}

/// A block that prefixes every line inside it: a list item or a block quote.
struct Container {
    first: Span<'static>,
    rest: Span<'static>,
    used: bool,
}

#[derive(Default)]
struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    /// How each line joins the one above it.
    joins: Vec<Join>,
    /// Inline content of the block being built.
    inline: Vec<Cell>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// Next number of each open list, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Destination and start in `inline` of each open link or image.
    links: Vec<(String, usize)>,
    /// Language and text of the open code block.
    code: Option<(String, String)>,
    table: Vec<Vec<String>>,
    row: Vec<String>,
    /// A blank line goes before the next block.
    gap: bool,
}

impl Renderer {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some((_, code)) => code.push_str(&text),
                None => self.push(&text, self.style()),
            },
            Event::Code(text) => self.push(&text, self.style().patch(CODE)),
            Event::InlineMath(text) | Event::DisplayMath(text) => self.push(&text, self.style()),
            Event::Html(text) | Event::InlineHtml(text) => {
                self.push(text.trim_end_matches('\n'), self.style())
            }
            Event::FootnoteReference(name) => self.push(&format!("[^{name}]"), self.style()),
            Event::SoftBreak | Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block();
                let rule = "─".repeat(self.width.saturating_sub(self.prefix_width()).min(40));
                self.emit(rule.chars().map(|c| (c, DIM)).collect(), Join::Newline);
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                self.push(if done { "[x] " } else { "[ ] " }, self.style())
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Emphasis => self.styles.push(Style::new().italic()),
            Tag::Strong => self.styles.push(Style::new().bold()),
            Tag::Strikethrough => self.styles.push(Style::new().crossed_out()),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.styles.push(Style::new().underlined());
                self.links.push((dest_url.to_string(), self.inline.len()));
            }
            Tag::Heading { level, .. } => {
                self.block();
                let color = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => Style::new().fg(Color::Magenta),
                    _ => Style::new(),
                };
                self.styles.push(color.bold());
            }
            Tag::CodeBlock(kind) => {
                self.block();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                // A nested list follows its item's text without a gap.
                if self.containers.is_empty() {
                    self.block();
                } else {
                    self.flush();
                    self.gap = false;
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                self.gap = false;
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "• ".to_string(),
                };
                let rest = " ".repeat(marker.chars().count());
                self.containers.push(Container {
                    first: Span::raw(marker),
                    rest: Span::raw(rest),
                    used: false,
                });
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.containers.push(Container {
                    first: Span::styled("│ ", DIM),
                    rest: Span::styled("│ ", DIM),
                    used: false,
                });
            }
            Tag::Table(_) => self.block(),
            Tag::Paragraph | Tag::HtmlBlock | Tag::FootnoteDefinition(_) => self.block(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let label: String = self.inline[start..].iter().map(|(c, _)| c).collect();
                    if !url.is_empty() && label != url {
                        self.push(&format!(" ({url})"), DIM);
                    }
                }
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.gap = true;
            }
            TagEnd::CodeBlock => self.code_block(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.gap = true;
            }
            TagEnd::Item => {
                self.flush();
                // An empty item still shows its marker.
                if self.containers.last().is_some_and(|c| !c.used) {
                    self.emit(Vec::new(), Join::Newline);
                }
                self.containers.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::TableCell => {
                let cell = self.inline.drain(..).map(|(c, _)| c).collect();
                self.row.push(cell);
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let row = std::mem::take(&mut self.row);
                self.table.push(row);
            }
            TagEnd::Table => self.table(),
            TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::FootnoteDefinition => {
                self.flush();
                self.gap = true;
            }
            _ => {}
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::new(), |style, patch| style.patch(*patch))
    }

    fn push(&mut self, text: &str, style: Style) {
        self.inline.extend(text.chars().map(|c| (c, style)));
    }

    /// Starts a block: pending text is flushed and the gap, if owed, drawn.
    fn block(&mut self) {
        self.flush();
        // A block right after a list marker shares its line.
        let fresh_item = self.containers.last().is_some_and(|c| !c.used);
        if std::mem::take(&mut self.gap) && !self.lines.is_empty() && !fresh_item {
            let prefix: Vec<Span> = self.containers.iter().map(|c| c.rest.clone()).collect();
            let blank = Line::from(prefix);
            let trimmed = blank.to_string().trim_end().to_string();
            self.lines.push(if trimmed.is_empty() {
                Line::from("")
            } else {
                Line::from(Span::styled(trimmed, DIM))
            });
            self.joins.push(Join::Newline);
        }
    }

    /// Wraps and emits the pending inline text.
    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let cells = std::mem::take(&mut self.inline);
        let width = self.width.saturating_sub(self.prefix_width()).max(4);
        for (line, join) in wrap(&cells, width) {
            self.emit(line, join);
        }
    }

    fn code_block(&mut self) {
        let Some((lang, code)) = self.code.take() else {
            return;
        };
        if !lang.is_empty() {
            let cells: Vec<Cell> = lang.chars().map(|c| (c, DIM)).collect();
            let width = self.width.saturating_sub(self.prefix_width()).max(4);
            for (line, join) in wrap(&cells, width) {
                self.emit(line, join);
            }
        }
        let width = self
            .width
            .saturating_sub(self.prefix_width() + CODE_INDENT.len())
            .max(4);
        let code = code.strip_suffix('\n').unwrap_or(&code);
        // Coloured as a whole, since a string or a comment can cross a line, then cut
        // back into the lines the code has.
        let painted = syntax::highlight(code, &lang);
        for line in split_lines(&painted) {
            let cells: Vec<Cell> = line.to_vec();
            // Long lines are split, never reflowed.
            let chunks: Vec<&[Cell]> = if cells.is_empty() {
                vec![&[]]
            } else {
                cells.chunks(width).collect()
            };
            for (index, chunk) in chunks.into_iter().enumerate() {
                let mut row: Vec<Cell> = CODE_INDENT.chars().map(|c| (c, PLAIN)).collect();
                row.extend_from_slice(chunk);
                // The line was split to fit, not reflowed, so nothing stands between.
                let join = match index {
                    0 => Join::Newline,
                    _ => Join::Split,
                };
                self.emit(row, join);
            }
        }
        self.gap = true;
    }

    /// Tables as plain rows with columns padded to line up.
    fn table(&mut self) {
        let rows = std::mem::take(&mut self.table);
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|i| {
                rows.iter()
                    .filter_map(|row| row.get(i))
                    .map(|cell| cell.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let width = self.width.saturating_sub(self.prefix_width()).max(4);
        for (index, row) in rows.iter().enumerate() {
            let text = widths
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    let cell = row.get(i).map_or("", String::as_str);
                    format!("{cell}{}", " ".repeat(w - cell.chars().count()))
                })
                .collect::<Vec<_>>()
                .join("  ");
            let style = if index == 0 {
                Style::new().bold()
            } else {
                Style::new()
            };
            let cells: Vec<Cell> = text.trim_end().chars().map(|c| (c, style)).collect();
            for (line, join) in wrap(&cells, width) {
                self.emit(line, join);
            }
            if index == 0 {
                let total = widths.iter().sum::<usize>() + 2 * columns.saturating_sub(1);
                let rule = "─".repeat(total.min(width));
                self.emit(rule.chars().map(|c| (c, DIM)).collect(), Join::Newline);
            }
        }
        self.gap = true;
    }

    fn prefix_width(&self) -> usize {
        self.containers
            .iter()
            .map(|c| c.rest.content.chars().count())
            .sum()
    }

    /// Pushes one screen line behind the container prefixes.
    fn emit(&mut self, cells: Vec<Cell>, join: Join) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for container in &mut self.containers {
            let prefix = if container.used {
                &container.rest
            } else {
                &container.first
            };
            spans.push(prefix.clone());
            container.used = true;
        }
        let mut run = String::new();
        let mut style = None;
        for (c, s) in cells {
            if style.is_some_and(|current| current != s) {
                spans.push(Span::styled(std::mem::take(&mut run), style.unwrap()));
            }
            style = Some(s);
            run.push(c);
        }
        if let Some(style) = style {
            spans.push(Span::styled(run, style));
        }
        self.lines.push(Line::from(spans));
        self.joins.push(join);
    }
}

/// The cells of each line of a painted block, without the newlines between them.
fn split_lines(painted: &[Cell]) -> Vec<&[Cell]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, (c, _)) in painted.iter().enumerate() {
        if *c == '\n' {
            lines.push(&painted[start..index]);
            start = index + 1;
        }
    }
    lines.push(&painted[start..]);
    lines
}

/// Greedy word wrap over styled chars, each line with how it joins the one above; a word
/// longer than the line is hard-split.
fn wrap(cells: &[Cell], width: usize) -> Vec<(Vec<Cell>, Join)> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line: Vec<Cell> = Vec::new();
    // The first line of a block stands under whatever the caller put above it.
    let mut join = Join::Newline;
    let mut pos: usize = 0;
    for word in cells.split(|(c, _)| *c == ' ') {
        let space = pos.checked_sub(1).map(|i| cells[i]);
        pos += word.len() + 1;
        let mut word = word;
        while word.len() > width {
            if !line.is_empty() {
                out.push((std::mem::take(&mut line), join));
                join = Join::Space;
            }
            out.push((word[..width].to_vec(), join));
            join = Join::Split;
            word = &word[width..];
        }
        let extra = usize::from(!line.is_empty());
        if line.len() + extra + word.len() > width {
            out.push((std::mem::take(&mut line), join));
            join = Join::Space;
        } else if let Some(space) = space.filter(|_| extra == 1) {
            line.push(space);
        }
        line.extend_from_slice(word);
    }
    out.push((line, join));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    /// The lines alone, for the tests that have nothing to say about the wrapping.
    fn lines(source: &str, width: usize) -> Vec<Line<'static>> {
        render(source, width).0
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    fn style_of(lines: &[Line], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("no span with {needle:?}"))
            .style
    }

    #[test]
    fn headings_are_bold_without_hashes() {
        let lines = lines("# Title\n\nbody", 40);
        assert_eq!(text(&lines), vec!["Title", "", "body"]);
        let style = style_of(&lines, "Title");
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style.fg, Some(Color::Magenta));
    }

    #[test]
    fn code_blocks_keep_their_lines_and_show_the_language() {
        let lines = lines(
            "before\n\n```rust\nfn main() {\n    let  x = 1;\n}\n```\nafter",
            40,
        );
        assert_eq!(
            text(&lines),
            vec![
                "before",
                "",
                "rust",
                "  fn main() {",
                "      let  x = 1;",
                "  }",
                "",
                "after"
            ]
        );
        assert_eq!(style_of(&lines, "rust"), DIM);
        // The block is coloured as the language the fence names.
        assert_eq!(style_of(&lines, "fn"), syntax::KEYWORD);
        assert_eq!(style_of(&lines, "main"), syntax::PLAIN);
        assert_eq!(style_of(&lines, "1"), syntax::NUMBER);
    }

    #[test]
    fn an_unclosed_fence_renders_as_code() {
        let lines = lines("text\n\n```py\nprint(1)\nx = [", 40);
        assert_eq!(
            text(&lines),
            vec!["text", "", "py", "  print(1)", "  x = ["]
        );
        assert_eq!(style_of(&lines, "x = ["), PLAIN);
    }

    #[test]
    fn a_fence_with_no_language_stays_plain() {
        let lines = lines("```\nit's fine # not a comment\n```", 40);
        for span in lines.iter().flat_map(|l| l.spans.iter()) {
            assert_eq!(span.style, PLAIN);
        }
    }

    #[test]
    fn long_code_lines_are_split_not_reflowed() {
        let lines = lines("```\nabcdefghij klm\n```", 8);
        assert_eq!(text(&lines), vec!["  abcdef", "  ghij k", "  lm"]);
    }

    #[test]
    fn a_long_language_tag_fits_the_width() {
        let lines = lines("```abcdefghijkl\nx\n```", 8);
        assert_eq!(text(&lines), vec!["abcdefgh", "ijkl", "  x"]);
    }

    #[test]
    fn inline_styles_apply_to_their_spans() {
        let lines = lines("a `code` **bold** *it* ~~no~~", 40);
        assert_eq!(text(&lines), vec!["a code bold it no"]);
        assert_eq!(style_of(&lines, "code"), CODE);
        assert!(
            style_of(&lines, "bold")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            style_of(&lines, "it")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            style_of(&lines, "no")
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
    }

    #[test]
    fn links_show_their_destination() {
        let lines = lines("see [docs](https://x.io) or <https://y.io>", 60);
        assert_eq!(
            text(&lines),
            vec!["see docs (https://x.io) or https://y.io"]
        );
    }

    #[test]
    fn list_items_wrap_with_a_hanging_indent() {
        let lines = lines(
            "- one two three four\n- five\n\n3. alpha beta\n4. gamma",
            12,
        );
        assert_eq!(
            text(&lines),
            vec![
                "• one two",
                "  three four",
                "• five",
                "",
                "3. alpha",
                "   beta",
                "4. gamma"
            ]
        );
    }

    #[test]
    fn nested_lists_indent_under_their_item() {
        let lines = lines("- outer\n  - inner\n- next", 40);
        assert_eq!(text(&lines), vec!["• outer", "  • inner", "• next"]);
    }

    #[test]
    fn block_quotes_get_a_bar() {
        let lines = lines("> quoted words here\n>\n> more", 10);
        assert_eq!(
            text(&lines),
            vec!["│ quoted", "│ words", "│ here", "│", "│ more"]
        );
    }

    #[test]
    fn tables_render_as_aligned_rows() {
        let lines = lines("| a | long |\n|---|---|\n| wide cell | b |", 40);
        assert_eq!(
            text(&lines),
            vec!["a          long", "───────────────", "wide cell  b"]
        );
    }

    #[test]
    fn every_line_fits_the_width() {
        let source = "# A heading that is long\n\nSome paragraph text that goes on and on.\n\n\
                      - a list item that also wraps a lot\n\n> a quote that wraps too\n\n\
                      ```\nsome code that is quite long indeed\n```";
        for width in [10, 17, 30] {
            for line in lines(source, width) {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn soft_breaks_keep_the_author_lines() {
        assert_eq!(text(&lines("one\ntwo", 40)), vec!["one", "two"]);
    }

    #[test]
    fn empty_text_renders_nothing() {
        assert!(lines("", 40).is_empty());
    }
}
