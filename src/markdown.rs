//! Markdown to styled, pre-wrapped lines for assistant text. Display only: the raw
//! text stays what is stored and copied.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::mermaid;
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
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
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
    /// Rows of the open table, each a row of cells, each cell its styled chars.
    table: Vec<Vec<Vec<Cell>>>,
    row: Vec<Vec<Cell>>,
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
                let cell = self.inline.drain(..).collect();
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
        // Only the word before any attributes, which fences carry in several dialects.
        let name = lang.split([' ', ',', '{', ':']).next().unwrap_or("").trim();
        if mermaid::is_mermaid(name) && self.diagram(&code) {
            self.gap = true;
            return;
        }
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

    /// A mermaid fence drawn as a diagram, or `false` when it is to be shown as source.
    /// A diagram is art rather than text: splitting a line of it to fit the view leaves
    /// something worse to read than the source it was drawn from, so one too wide for
    /// the view is not drawn at all.
    fn diagram(&mut self, source: &str) -> bool {
        let width = self
            .width
            .saturating_sub(self.prefix_width() + CODE_INDENT.len());
        let Some(lines) = mermaid::render(source) else {
            return false;
        };
        if lines.iter().any(|l| l.chars().count() > width) {
            return false;
        }
        for line in lines {
            let mut row: Vec<Cell> = CODE_INDENT.chars().map(|c| (c, PLAIN)).collect();
            row.extend(line.chars().map(|c| (c, PLAIN)));
            self.emit(row, Join::Newline);
        }
        true
    }

    /// Tables as a grid, columns padded to line up. A cell too long for its column wraps
    /// inside it rather than running into the next line of the view, so every line of a
    /// row keeps its columns and a reader can still tell which one they are reading.
    fn table(&mut self) {
        let rows = std::mem::take(&mut self.table);
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let natural: Vec<usize> = (0..columns)
            .map(|i| {
                rows.iter()
                    .filter_map(|row| row.get(i))
                    .map(Vec::len)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let width = self.width.saturating_sub(self.prefix_width()).max(4);
        let gaps = 2 * columns.saturating_sub(1);
        let widths = fit(&natural, width.saturating_sub(gaps));
        for (index, row) in rows.iter().enumerate() {
            let head = index == 0;
            // Each cell wrapped to its own column, so the row is as tall as the cell
            // that needed the most lines.
            let cells: Vec<Vec<Vec<Cell>>> = widths
                .iter()
                .enumerate()
                .map(|(i, w)| match row.get(i) {
                    Some(cell) => wrap(cell, *w).into_iter().map(|(line, _)| line).collect(),
                    None => vec![Vec::new()],
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for line in 0..height {
                let mut text: Vec<Cell> = Vec::new();
                for (i, w) in widths.iter().enumerate() {
                    if i > 0 {
                        text.extend([(' ', Style::new()); 2]);
                    }
                    let part = cells[i].get(line).map_or(&[][..], Vec::as_slice);
                    text.extend(part.iter().map(|(c, style)| match head {
                        true => (*c, style.bold()),
                        false => (*c, *style),
                    }));
                    let pad = w.saturating_sub(part.len());
                    text.extend(std::iter::repeat_n((' ', Style::new()), pad));
                }
                while text.last().is_some_and(|(c, _)| *c == ' ') {
                    text.pop();
                }
                // The line is a grid row, so it goes out as it was laid out: the wrap
                // would take the leading spaces off it and with them the columns a
                // continuation line sits under. Only a table squeezed into a view too
                // narrow to give every column a character can still be too wide, and
                // that one is past keeping its shape anyway.
                match text.len() > width {
                    true => {
                        for (line, _) in wrap(&text, width) {
                            self.emit(line, Join::Newline);
                        }
                    }
                    false => self.emit(text, Join::Newline),
                }
            }
            if head {
                let rule = "─".repeat((widths.iter().sum::<usize>() + gaps).min(width));
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

/// Column widths that fit `budget`. What has to go comes off the widest columns first,
/// so a column of short values keeps its natural width and a column of prose is the one
/// that wraps. Below one character a column it is past helping, and the caller wraps.
fn fit(natural: &[usize], budget: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= budget {
        return natural.to_vec();
    }
    let held = |cap: &usize| natural.iter().map(|w| (*w).min(*cap)).sum::<usize>() <= budget;
    let cap = (1..=natural.iter().copied().max().unwrap_or(1))
        .take_while(held)
        .last()
        .unwrap_or(1);
    natural.iter().map(|w| (*w).min(cap).max(1)).collect()
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
        // The block is coloured as the language the fence names, so the keyword and the
        // literal are told apart. Which colours exactly is the theme's business.
        let keyword = style_of(&lines, "fn");
        assert_ne!(keyword, syntax::PLAIN);
        assert_ne!(keyword, style_of(&lines, "1"));
    }

    #[test]
    fn an_unclosed_fence_renders_as_code() {
        let lines = lines("text\n\n```py\nprint(1)\nx = [", 40);
        assert_eq!(
            text(&lines),
            vec!["text", "", "py", "  print(1)", "  x = ["]
        );
    }

    #[test]
    fn a_task_list_shows_what_is_done() {
        let lines = lines("- [x] landed\n- [ ] still to do\n- plain", 40);
        assert_eq!(
            text(&lines),
            vec!["• [x] landed", "• [ ] still to do", "• plain"]
        );
    }

    #[test]
    fn a_footnote_keeps_its_reference_and_its_definition() {
        let lines = lines("a claim[^1]\n\n[^1]: the source", 40);
        let shown = text(&lines);
        assert!(shown.iter().any(|l| l.contains("[^1]")), "{shown:?}");
        assert!(shown.iter().any(|l| l.contains("the source")), "{shown:?}");
    }

    #[test]
    fn a_mermaid_fence_is_drawn_rather_than_shown() {
        let source = "```mermaid\nsequenceDiagram\n    A->>B: hello\n```";
        let lines = lines(source, 80);
        let drawn = text(&lines).join("\n");
        // The diagram, not its source: no fence language line, no `->>`.
        assert!(drawn.contains("hello"), "{drawn}");
        assert!(!drawn.contains("->>"), "{drawn}");
        assert!(!drawn.contains("sequenceDiagram"), "{drawn}");
        assert!(
            !text(&lines).iter().any(|l| l.trim() == "mermaid"),
            "{drawn}"
        );
    }

    #[test]
    fn a_diagram_too_wide_for_the_view_is_shown_as_its_source() {
        let source = "```mermaid\nsequenceDiagram\n    A->>B: hello\n```";
        let lines = lines(source, 12);
        let shown = text(&lines);
        // Splitting box-drawing art reads worse than the source it was drawn from.
        assert_eq!(shown[0], "mermaid");
        assert!(shown.iter().any(|l| l.contains("->>")), "{shown:?}");
    }

    #[test]
    fn a_mermaid_fence_that_does_not_parse_is_shown_as_its_source() {
        let lines = lines("```mermaid\nnot a diagram\n```", 80);
        assert_eq!(text(&lines), vec!["mermaid", "  not a diagram"]);
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

    /// A table wider than the view keeps its columns: the room comes off the widest
    /// column and what will not fit wraps inside it, under the column it belongs to,
    /// rather than running back to the left margin where it reads as a new row.
    #[test]
    fn a_table_too_wide_wraps_inside_its_columns() {
        let table = "| tool | what it does |\n|---|---|\n                     | read | opens a file and returns the lines asked for |\n                     | edit | replaces one exact string in a file |";
        assert_eq!(
            text(&lines(table, 32)),
            vec![
                "tool  what it does",
                "────────────────────────────────",
                "read  opens a file and returns",
                "      the lines asked for",
                "edit  replaces one exact string",
                "      in a file",
            ]
        );
    }

    #[test]
    fn a_table_fits_the_width_it_is_given() {
        let table = "| a very long heading indeed | and another one here |\n|---|---|\n                     | a cell with a good deal of text in it | short |";
        for width in [12, 20, 40, 80] {
            for line in lines(table, width) {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn a_cell_keeps_the_styles_inside_it() {
        let table = "| call | note |\n|---|---|\n| `cargo test` | runs them |";
        let rendered = lines(table, 40);
        assert_eq!(
            style_of(&rendered, "cargo test"),
            style_of(&lines("`x`", 40), "x")
        );
        // The header is bold, and the rule under it is not.
        assert!(
            style_of(&rendered, "call")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !style_of(&rendered, "runs")
                .add_modifier
                .contains(Modifier::BOLD)
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
