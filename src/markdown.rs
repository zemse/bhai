//! Markdown to styled, pre-wrapped lines for assistant text. Display only: the raw
//! text stays what is stored and copied.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// One styled character; lines are built from these so wrapping keeps styles.
type Cell = (char, Style);

const CODE: Style = Style::new().fg(Color::Green);
const DIM: Style = Style::new().fg(Color::DarkGray);
/// Code block lines are indented by this much.
const CODE_INDENT: &str = "  ";

/// Renders `text` into lines no wider than `width` chars.
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        width: width.max(1),
        ..Renderer::default()
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.flush();
    renderer.lines
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
                self.emit(rule.chars().map(|c| (c, DIM)).collect());
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
                    self.emit(Vec::new());
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
        }
    }

    /// Wraps and emits the pending inline text.
    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let cells = std::mem::take(&mut self.inline);
        let width = self.width.saturating_sub(self.prefix_width()).max(4);
        for line in wrap(&cells, width) {
            self.emit(line);
        }
    }

    fn code_block(&mut self) {
        let Some((lang, code)) = self.code.take() else {
            return;
        };
        if !lang.is_empty() {
            self.emit(lang.chars().map(|c| (c, DIM)).collect());
        }
        let width = self
            .width
            .saturating_sub(self.prefix_width() + CODE_INDENT.len())
            .max(4);
        let code = code.strip_suffix('\n').unwrap_or(&code);
        for line in code.split('\n') {
            let cells: Vec<Cell> = line.chars().map(|c| (c, CODE)).collect();
            // Long lines are split, never reflowed.
            let chunks: Vec<&[Cell]> = if cells.is_empty() {
                vec![&[]]
            } else {
                cells.chunks(width).collect()
            };
            for chunk in chunks {
                let mut row: Vec<Cell> = CODE_INDENT.chars().map(|c| (c, CODE)).collect();
                row.extend_from_slice(chunk);
                self.emit(row);
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
            for line in wrap(&cells, width) {
                self.emit(line);
            }
            if index == 0 {
                let total = widths.iter().sum::<usize>() + 2 * columns.saturating_sub(1);
                let rule = "─".repeat(total.min(width));
                self.emit(rule.chars().map(|c| (c, DIM)).collect());
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
    fn emit(&mut self, cells: Vec<Cell>) {
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
    }
}

/// Greedy word wrap over styled chars; a word longer than the line is hard-split.
fn wrap(cells: &[Cell], width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line: Vec<Cell> = Vec::new();
    let mut pos: usize = 0;
    for word in cells.split(|(c, _)| *c == ' ') {
        let space = pos.checked_sub(1).map(|i| cells[i]);
        pos += word.len() + 1;
        let mut word = word;
        while word.len() > width {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            out.push(word[..width].to_vec());
            word = &word[width..];
        }
        let extra = usize::from(!line.is_empty());
        if line.len() + extra + word.len() > width {
            out.push(std::mem::take(&mut line));
        } else if let Some(space) = space.filter(|_| extra == 1) {
            line.push(space);
        }
        line.extend_from_slice(word);
    }
    out.push(line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

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
        let lines = render("# Title\n\nbody", 40);
        assert_eq!(text(&lines), vec!["Title", "", "body"]);
        let style = style_of(&lines, "Title");
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style.fg, Some(Color::Magenta));
    }

    #[test]
    fn code_blocks_keep_their_lines_and_show_the_language() {
        let lines = render(
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
        assert_eq!(style_of(&lines, "fn main"), CODE);
    }

    #[test]
    fn an_unclosed_fence_renders_as_code() {
        let lines = render("text\n\n```py\nprint(1)\nx = [", 40);
        assert_eq!(
            text(&lines),
            vec!["text", "", "py", "  print(1)", "  x = ["]
        );
        assert_eq!(style_of(&lines, "x = ["), CODE);
    }

    #[test]
    fn long_code_lines_are_split_not_reflowed() {
        let lines = render("```\nabcdefghij klm\n```", 8);
        assert_eq!(text(&lines), vec!["  abcdef", "  ghij k", "  lm"]);
    }

    #[test]
    fn inline_styles_apply_to_their_spans() {
        let lines = render("a `code` **bold** *it* ~~no~~", 40);
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
        let lines = render("see [docs](https://x.io) or <https://y.io>", 60);
        assert_eq!(
            text(&lines),
            vec!["see docs (https://x.io) or https://y.io"]
        );
    }

    #[test]
    fn list_items_wrap_with_a_hanging_indent() {
        let lines = render(
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
        let lines = render("- outer\n  - inner\n- next", 40);
        assert_eq!(text(&lines), vec!["• outer", "  • inner", "• next"]);
    }

    #[test]
    fn block_quotes_get_a_bar() {
        let lines = render("> quoted words here\n>\n> more", 10);
        assert_eq!(
            text(&lines),
            vec!["│ quoted", "│ words", "│ here", "│", "│ more"]
        );
    }

    #[test]
    fn tables_render_as_aligned_rows() {
        let lines = render("| a | long |\n|---|---|\n| wide cell | b |", 40);
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
            for line in render(source, width) {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn soft_breaks_keep_the_author_lines() {
        assert_eq!(text(&render("one\ntwo", 40)), vec!["one", "two"]);
    }

    #[test]
    fn empty_text_renders_nothing() {
        assert!(render("", 40).is_empty());
    }
}
