//! Markdown to styled, pre-wrapped lines for assistant text. The raw text stays what is
//! stored, and every drawn char remembers the bytes of it that it came from, so a copy
//! of what is on screen is the markdown that drew it.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::mermaid;
use crate::syntax::{self, PLAIN};
use crate::wrap::Join;

/// One styled character and where it came from; lines are built from these so wrapping
/// keeps both.
type Cell = (char, Style, Option<Origin>, Option<usize>);

/// The bytes of the source a drawn char came from. Every char of an inline element
/// (`**bold**`, a link, `code`) has the whole element, so a copy keeps its delimiters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    pub start: usize,
    pub end: usize,
}

impl From<Range<usize>> for Origin {
    fn from(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

/// What `render` draws, row by row.
#[derive(Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// How each row joins the one above it.
    pub joins: Vec<Join>,
    /// Chars at the start of each row that are layout rather than text.
    pub margins: Vec<usize>,
    /// Where each char of each row came from, `None` for what the renderer adds.
    pub origins: Vec<Vec<Option<Origin>>>,
    /// Clickable code: its row, its chars and the code it copies.
    pub copies: Vec<(usize, Range<usize>, String)>,
}

/// The label over a code block that a click copies the block by.
pub const COPY_LABEL: &str = "[copy]";

const CODE: Style = Style::new().fg(Color::Green);
const DIM: Style = Style::new().fg(Color::DarkGray);
/// Code block lines are indented by this much.
const CODE_INDENT: &str = "  ";

/// Renders `text` into lines no wider than `width` chars, each with how it joins the one
/// above, how many chars in front of it are layout and where its chars came from, so a
/// copy of a selection can undo what was done here. Origins are offsets into
/// `wrap::readable(text)`, which is what is parsed.
pub fn render(text: &str, width: usize) -> Rendered {
    let text = &crate::wrap::readable(text);
    let mut renderer = Renderer {
        width: width.max(1),
        ..Renderer::default()
    };
    for (event, range) in Parser::new_ext(text, OPTIONS).into_offset_iter() {
        renderer.event(event, range);
    }
    renderer.flush();
    renderer.out
}

const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_FOOTNOTES);

/// A block that prefixes every line inside it: a list item or a block quote.
struct Container {
    first: Span<'static>,
    rest: Span<'static>,
    used: bool,
}

/// A fenced or indented code block while its text arrives.
struct Code {
    lang: String,
    text: String,
    /// Where each char of `text` came from.
    origins: Vec<Option<Origin>>,
    /// The whole block, fences and all.
    block: Origin,
}

#[derive(Default)]
struct Renderer {
    width: usize,
    out: Rendered,
    /// Inline content of the block being built.
    inline: Vec<Cell>,
    /// Payloads for inline code, indexed by its cells.
    click_codes: Vec<String>,
    styles: Vec<Style>,
    /// The open inline elements; the outermost is what their chars copy as.
    elements: Vec<Origin>,
    containers: Vec<Container>,
    /// Next number of each open list, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Destination and start in `inline` of each open link or image.
    links: Vec<(String, usize)>,
    /// The open code block.
    code: Option<Code>,
    /// Rows of the open table, each a row of cells, each cell its styled chars.
    table: Vec<Vec<Vec<Cell>>>,
    row: Vec<Vec<Cell>>,
    /// A blank line goes before the next block.
    gap: bool,
}

impl Renderer {
    fn event(&mut self, event: Event, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag, range),
            Event::Text(text) => match &mut self.code {
                Some(code) => {
                    code.text.push_str(&text);
                    code.origins.extend(origins(&text, range));
                }
                None => self.push(&text, self.style(), range),
            },
            Event::Code(text) => {
                let start = self.inline.len();
                self.push(&text, self.style().patch(CODE), range);
                let code = self.click_codes.len();
                self.click_codes.push(text.to_string());
                for cell in &mut self.inline[start..] {
                    cell.3 = Some(code);
                }
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push(&text, self.style(), range)
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.push(text.trim_end_matches('\n'), self.style(), range)
            }
            Event::FootnoteReference(name) => self.push(&format!("[^{name}]"), self.style(), range),
            Event::SoftBreak | Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block();
                let rule = "─".repeat(self.width.saturating_sub(self.prefix_width()).min(40));
                let origin = Some(range.into());
                self.emit(
                    rule.chars().map(|c| (c, DIM, origin, None)).collect(),
                    Join::Newline,
                );
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                self.push(if done { "[x] " } else { "[ ] " }, self.style(), range)
            }
        }
    }

    fn start(&mut self, tag: Tag, range: Range<usize>) {
        match tag {
            Tag::Emphasis => {
                self.styles.push(Style::new().italic());
                self.elements.push(range.into());
            }
            Tag::Strong => {
                self.styles.push(Style::new().bold());
                self.elements.push(range.into());
            }
            Tag::Strikethrough => {
                self.styles.push(Style::new().crossed_out());
                self.elements.push(range.into());
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.styles.push(Style::new().underlined());
                self.links.push((dest_url.to_string(), self.inline.len()));
                self.elements.push(range.into());
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
                self.code = Some(Code {
                    lang,
                    text: String::new(),
                    origins: Vec::new(),
                    block: range.into(),
                });
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

    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
                self.elements.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let label: String = self.inline[start..].iter().map(|(c, ..)| c).collect();
                    if !url.is_empty() && label != url {
                        self.push(&format!(" ({url})"), DIM, range);
                    }
                }
                self.elements.pop();
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

    /// Text that came from `range` of the source, or from the inline element it sits in.
    fn push(&mut self, text: &str, style: Style, range: Range<usize>) {
        match self.elements.first() {
            Some(&element) => {
                let cells = text.chars().map(|c| (c, style, Some(element), None));
                self.inline.extend(cells);
            }
            None => {
                let cells = text.chars().zip(origins(text, range));
                self.inline
                    .extend(cells.map(|(c, origin)| (c, style, origin, None)));
            }
        }
    }

    /// Starts a block: pending text is flushed and the gap, if owed, drawn.
    fn block(&mut self) {
        self.flush();
        // A block right after a list marker shares its line.
        let fresh_item = self.containers.last().is_some_and(|c| !c.used);
        if std::mem::take(&mut self.gap) && !self.out.lines.is_empty() && !fresh_item {
            let prefix: Vec<Span> = self.containers.iter().map(|c| c.rest.clone()).collect();
            let blank = Line::from(prefix);
            let trimmed = blank.to_string().trim_end().to_string();
            self.out.origins.push(vec![None; trimmed.chars().count()]);
            self.out.lines.push(if trimmed.is_empty() {
                Line::from("")
            } else {
                Line::from(Span::styled(trimmed, DIM))
            });
            self.out.joins.push(Join::Newline);
            self.out.margins.push(0);
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
        let Some(Code {
            lang,
            text,
            origins,
            block,
        }) = self.code.take()
        else {
            return;
        };
        let code = text.strip_suffix('\n').unwrap_or(&text);
        self.code_header(&lang, code);
        // Only the word before any attributes, which fences carry in several dialects.
        let name = lang.split([' ', ',', '{', ':']).next().unwrap_or("").trim();
        let start = self.out.lines.len();
        if mermaid::is_mermaid(name) && self.diagram(code, block) {
            self.code_targets(start, code);
            self.gap = true;
            return;
        }
        let width = self
            .width
            .saturating_sub(self.prefix_width() + CODE_INDENT.len())
            .max(4);
        // Coloured as a whole, since a string or a comment can cross a line, then cut
        // back into the lines the code has.
        let painted: Vec<Cell> = syntax::highlight(code, &lang)
            .into_iter()
            .zip(origins)
            .map(|((c, style), origin)| (c, style, origin, None))
            .collect();
        for line in split_lines(&painted) {
            let cells: Vec<Cell> = line.to_vec();
            // Long lines are split, never reflowed.
            let chunks: Vec<&[Cell]> = if cells.is_empty() {
                vec![&[]]
            } else {
                cells.chunks(width).collect()
            };
            for (index, chunk) in chunks.into_iter().enumerate() {
                let mut row: Vec<Cell> = CODE_INDENT
                    .chars()
                    .map(|c| (c, PLAIN, None, None))
                    .collect();
                row.extend_from_slice(chunk);
                // The line was split to fit, not reflowed, so nothing stands between.
                let join = match index {
                    0 => Join::Newline,
                    _ => Join::Split,
                };
                self.emit_code(row, join);
            }
        }
        self.code_targets(start, code);
        self.gap = true;
    }

    fn code_targets(&mut self, start: usize, code: &str) {
        for row in start..self.out.lines.len() {
            let chars = self.out.lines[row].to_string().chars().count();
            self.out
                .copies
                .push((row, self.prefix_width()..chars, code.to_string()));
        }
    }

    /// The row over a code block: its language, if the fence names one, and the label a
    /// click copies the code by. A view too narrow for the label goes without.
    fn code_header(&mut self, lang: &str, code: &str) {
        let width = self.width.saturating_sub(self.prefix_width()).max(4);
        let mut rows = match lang.is_empty() {
            true => Vec::new(),
            false => wrap(
                &lang
                    .chars()
                    .map(|c| (c, DIM, None, None))
                    .collect::<Vec<_>>(),
                width,
            ),
        };
        let label = COPY_LABEL.chars().count();
        let last = rows.last().map_or(0, |(row, _)| cells_width(row));
        match rows.last_mut() {
            Some((row, _)) if last + 1 + label <= width => row.push((' ', DIM, None, None)),
            _ if label <= width => rows.push((Vec::new(), Join::Newline)),
            _ => {}
        }
        let at = rows.last().map(|(row, _)| row.len());
        if let (Some(at), Some((row, _))) = (at, rows.last_mut())
            && label <= width
        {
            row.extend(COPY_LABEL.chars().map(|c| (c, DIM, None, None)));
            let start = self.prefix_width() + at;
            let line = self.out.lines.len() + rows.len() - 1;
            self.out
                .copies
                .push((line, start..start + label, code.to_string()));
        }
        for (row, join) in rows {
            self.emit(row, join);
        }
    }

    /// A mermaid fence drawn as a diagram, or `false` when it is to be shown as source.
    /// A diagram is art rather than text: splitting a line of it to fit the view leaves
    /// something worse to read than the source it was drawn from, so one too wide for
    /// the view is not drawn at all. Each char of it copies as the whole fence.
    fn diagram(&mut self, source: &str, block: Origin) -> bool {
        let width = self
            .width
            .saturating_sub(self.prefix_width() + CODE_INDENT.len());
        let Some(lines) = mermaid::render(source) else {
            return false;
        };
        if lines.iter().any(|l| crate::wrap::width(l) > width) {
            return false;
        }
        for line in lines {
            let mut row: Vec<Cell> = CODE_INDENT
                .chars()
                .map(|c| (c, PLAIN, None, None))
                .collect();
            row.extend(line.chars().map(|c| (c, PLAIN, Some(block), None)));
            self.emit_code(row, Join::Newline);
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
                    .map(|cell| cells_width(cell))
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
                        text.extend([(' ', Style::new(), None, None); 2]);
                    }
                    let part = cells[i].get(line).map_or(&[][..], Vec::as_slice);
                    text.extend(part.iter().map(|&(c, style, origin, click)| match head {
                        true => (c, style.bold(), origin, click),
                        false => (c, style, origin, click),
                    }));
                    let pad = w.saturating_sub(cells_width(part));
                    text.extend(std::iter::repeat_n((' ', Style::new(), None, None), pad));
                }
                while text.last().is_some_and(|(c, ..)| *c == ' ') {
                    text.pop();
                }
                // The line is a grid row, so it goes out as it was laid out: the wrap
                // would take the leading spaces off it and with them the columns a
                // continuation line sits under. Only a table squeezed into a view too
                // narrow to give every column a character can still be too wide, and
                // that one is past keeping its shape anyway.
                match cells_width(&text) > width {
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
                self.emit(
                    rule.chars().map(|c| (c, DIM, None, None)).collect(),
                    Join::Newline,
                );
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
        let mut origins = Vec::with_capacity(cells.len());
        for container in &mut self.containers {
            let prefix = if container.used {
                &container.rest
            } else {
                &container.first
            };
            origins.extend(prefix.content.chars().map(|_| None));
            spans.push(prefix.clone());
            container.used = true;
        }
        let mut run = String::new();
        let mut style = None;
        let mut click_run: Option<(usize, usize)> = None;
        for (c, s, origin, click) in cells {
            let column = origins.len();
            if click_run.map(|(_, code)| code) != click {
                if let Some((start, code)) = click_run.take() {
                    self.out.copies.push((
                        self.out.lines.len(),
                        start..column,
                        self.click_codes[code].clone(),
                    ));
                }
                click_run = click.map(|code| (column, code));
            }
            origins.push(origin);
            if style.is_some_and(|current| current != s) {
                spans.push(Span::styled(std::mem::take(&mut run), style.unwrap()));
            }
            style = Some(s);
            run.push(c);
        }
        if let Some((start, code)) = click_run {
            self.out.copies.push((
                self.out.lines.len(),
                start..origins.len(),
                self.click_codes[code].clone(),
            ));
        }
        if let Some(style) = style {
            spans.push(Span::styled(run, style));
        }
        // A row the wrap continued sits under the text it continues, not under a marker.
        let margin = match join {
            Join::Newline => 0,
            _ => self.prefix_width(),
        };
        self.out.lines.push(Line::from(spans));
        self.out.joins.push(join);
        self.out.margins.push(margin);
        self.out.origins.push(origins);
    }

    /// A row of a code block: what a copy takes is the code, without the indent it is
    /// drawn behind or the containers it sits in.
    fn emit_code(&mut self, cells: Vec<Cell>, join: Join) {
        let margin = self.prefix_width() + CODE_INDENT.len();
        self.emit(cells, join);
        if let Some(last) = self.out.margins.last_mut() {
            *last = margin;
        }
    }
}

/// The cells of each line of a painted block, without the newlines between them.
fn split_lines(painted: &[Cell]) -> Vec<&[Cell]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, (c, ..)) in painted.iter().enumerate() {
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
    let mut line_width = 0;
    for word in cells.split(|(c, ..)| *c == ' ') {
        let space = pos.checked_sub(1).map(|i| cells[i]);
        pos += word.len() + 1;
        let mut word = word;
        let mut left = cells_width(word);
        while left > width {
            if !line.is_empty() {
                out.push((std::mem::take(&mut line), join));
                line_width = 0;
                join = Join::Space;
            }
            let split = cells_split(word, width);
            let taken = cells_width(&word[..split]);
            out.push((word[..split].to_vec(), join));
            join = Join::Split;
            word = &word[split..];
            left -= taken;
        }
        let extra = usize::from(!line.is_empty());
        if line_width + extra + left > width {
            out.push((std::mem::take(&mut line), join));
            line_width = 0;
            join = Join::Space;
        } else if let Some(space) = space.filter(|_| extra == 1) {
            line.push(space);
            line_width += 1;
        }
        line.extend_from_slice(word);
        line_width += left;
    }
    out.push((line, join));
    out
}

/// The columns these cells take on screen; one cell is one character, which may be two
/// columns wide.
fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|(c, ..)| char_width(*c)).sum()
}

/// How many cells reach `columns` on screen, never leaving a two-column character
/// straddling the edge.
fn cells_split(cells: &[Cell], columns: usize) -> usize {
    let mut used = 0;
    for (i, (c, ..)) in cells.iter().enumerate() {
        let next = used + char_width(*c);
        if next > columns {
            return i;
        }
        used = next;
    }
    cells.len()
}

fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Where each char of `text` came from, given that it came from `range`. Text that is
/// the source byte for byte maps char by char; text the parser changed on the way, an
/// escape or an entity say, all maps to the whole range.
fn origins(text: &str, range: Range<usize>) -> Vec<Option<Origin>> {
    match text.len() == range.len() {
        true => text
            .char_indices()
            .map(|(at, c)| {
                let start = range.start + at;
                Some(Origin {
                    start,
                    end: start + c.len_utf8(),
                })
            })
            .collect(),
        false => vec![Some(range.into()); text.chars().count()],
    }
}

/// What a copy of `range` of `source` takes so that it is still markdown: a table it cuts
/// across rows of, or a code block it runs out of, whole, and the marker the first line
/// opens with (`## `, `- `, `> `) when the copy starts at that line's text.
pub fn excerpt(source: &str, range: Range<usize>) -> &str {
    let (mut lo, mut hi) = (range.start, range.end.min(source.len()));
    let mut wholes = Vec::new();
    for (event, block) in Parser::new_ext(source, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::Table(_)) => wholes.push((block, true)),
            Event::Start(Tag::CodeBlock(_)) => wholes.push((block, false)),
            _ => {}
        }
    }
    // Taking one in can reach another, a table and the code block after it say.
    loop {
        let before = (lo, hi);
        for (block, table) in &wholes {
            if lo >= block.end || hi <= block.start {
                continue;
            }
            let inside = lo >= block.start && hi <= block.end;
            let rows = *table && source[lo..hi].contains('\n');
            if !inside || rows {
                lo = lo.min(block.start);
                hi = hi.max(block.end);
            }
        }
        if (lo, hi) == before {
            break;
        }
    }
    let line = source[..lo].rfind('\n').map_or(0, |at| at + 1);
    if lo <= line + markup(&source[line..]) {
        lo = line;
    }
    while !source.is_char_boundary(lo) {
        lo -= 1;
    }
    while !source.is_char_boundary(hi) {
        hi += 1;
    }
    source[lo..hi].trim_end_matches('\n')
}

/// The bytes of block markup a line opens with: quote marks, then a heading's `#`s or a
/// list's bullet and task box, each with the spaces after it.
fn markup(line: &str) -> usize {
    let bytes = line.as_bytes();
    let spaces = |mut at: usize| {
        while matches!(bytes.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        at
    };
    let mut at = spaces(0);
    let mut found = false;
    while bytes.get(at) == Some(&b'>') {
        found = true;
        at = spaces(at + 1);
    }
    let hashes = bytes[at..].iter().take_while(|b| **b == b'#').count();
    if (1..=6).contains(&hashes) && bytes.get(at + hashes) == Some(&b' ') {
        return spaces(at + hashes);
    }
    let digits = bytes[at..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let bullet = match bytes.get(at) {
        Some(b'-' | b'*' | b'+') => Some(at + 1),
        _ if digits > 0 && matches!(bytes.get(at + digits), Some(b'.' | b')')) => {
            Some(at + digits + 1)
        }
        _ => None,
    };
    if let Some(end) = bullet.filter(|end| bytes.get(*end) == Some(&b' ')) {
        let mut end = spaces(end);
        if bytes.get(end) == Some(&b'[')
            && bytes.get(end + 2) == Some(&b']')
            && matches!(bytes.get(end + 1), Some(b' ' | b'x' | b'X'))
        {
            end = spaces(end + 3);
        }
        return end;
    }
    if found { at } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn inline_code_targets_survive_wrapping_and_nested_markup() {
        let rendered = render("**before `one two three` and ``a ` b`` after**", 12);
        let mut seen = Vec::new();
        for (row, chars, code) in &rendered.copies {
            let line = rendered.lines[*row].to_string();
            let drawn: String = line.chars().skip(chars.start).take(chars.len()).collect();
            assert!(!drawn.is_empty());
            assert!(code.contains(&drawn), "{drawn:?} is not in {code:?}");
            seen.push(code.as_str());
        }
        assert!(seen.iter().filter(|code| **code == "one two three").count() > 1);
        assert!(seen.contains(&"a ` b"));
    }

    #[test]
    fn inline_code_in_tables_keeps_separate_targets() {
        let rendered = render("| a | b |\n|---|---|\n| `left` | `right` |", 30);
        let codes: Vec<_> = rendered
            .copies
            .iter()
            .map(|(_, _, code)| code.as_str())
            .collect();
        assert_eq!(codes, ["left", "right"]);
    }

    #[test]
    fn code_rows_are_clickable_without_a_copy_label() {
        let rendered = render("```\nabcdef\n\n  x\n```", 4);
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| !line.to_string().contains(COPY_LABEL))
        );
        assert_eq!(rendered.copies.len(), rendered.lines.len());
        assert!(
            rendered
                .copies
                .iter()
                .all(|(_, _, code)| code == "abcdef\n\n  x")
        );
    }

    /// The lines alone, for the tests that have nothing to say about the wrapping.
    fn lines(source: &str, width: usize) -> Vec<Line<'static>> {
        render(source, width).lines
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
                "rust [copy]",
                "  fn main() {",
                "      let  x = 1;",
                "  }",
                "",
                "after"
            ]
        );
        assert_eq!(style_of(&lines, "rust [copy]"), DIM);
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
            vec!["text", "", "py [copy]", "  print(1)", "  x = ["]
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
        // The diagram, not its source: no `->>`, and only the header names the fence.
        assert!(drawn.contains("hello"), "{drawn}");
        assert!(!drawn.contains("->>"), "{drawn}");
        assert!(!drawn.contains("sequenceDiagram"), "{drawn}");
        assert_eq!(text(&lines)[0], "mermaid [copy]");
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
        assert_eq!(text(&lines), vec!["mermaid [copy]", "  not a diagram"]);
    }

    #[test]
    fn a_fence_with_no_language_stays_plain() {
        let lines = lines("```\nit's fine # not a comment\n```", 40);
        // Under the header, which is dim like any fence's.
        for span in lines.iter().skip(1).flat_map(|l| l.spans.iter()) {
            assert_eq!(span.style, PLAIN);
        }
    }

    #[test]
    fn long_code_lines_are_split_not_reflowed() {
        let lines = lines("```\nabcdefghij klm\n```", 8);
        assert_eq!(text(&lines), vec!["[copy]", "  abcdef", "  ghij k", "  lm"]);
    }

    #[test]
    fn a_long_language_tag_fits_the_width() {
        let lines = lines("```abcdefghijkl\nx\n```", 8);
        // The label does not fit beside the tag's last row, so it takes one of its own.
        assert_eq!(text(&lines), vec!["abcdefgh", "ijkl", "[copy]", "  x"]);
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
