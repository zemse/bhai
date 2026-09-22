//! Syntax colours for fenced code blocks, off syntect's default grammars and themes.
//! A fence that names no language, or names one syntect has no grammar for, is left
//! plain rather than guessed at: syntect will happily identify a block of log output as
//! some language by its first line, and painting it in that language's rules reads
//! worse than not painting it at all.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as Highlighted, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::palette;

/// Code with nothing said about it: the body of a block whose fence names no language,
/// and the indent every code line carries.
pub const PLAIN: Style = Style::new().fg(Color::Gray);

/// The theme when the config names none. Dark, like the rest of what bhai draws.
pub const DEFAULT_THEME: &str = "base16-ocean.dark";

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<ThemeSet> = OnceLock::new();
static CHOSEN: OnceLock<String> = OnceLock::new();

/// Name the theme for this process. Called once at startup; later calls do nothing,
/// since a theme that changed mid-session would repaint only what is redrawn.
pub fn set_theme(name: &str) {
    let _ = CHOSEN.set(name.to_string());
}

/// The theme names available, sorted, for the config error that lists them.
pub fn themes() -> Vec<String> {
    let mut names: Vec<String> = theme_set().themes.keys().cloned().collect();
    names.sort();
    names
}

fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEMES.get_or_init(ThemeSet::load_defaults)
}

fn theme() -> Option<&'static Theme> {
    let set = theme_set();
    let chosen = CHOSEN.get().map(String::as_str).unwrap_or(DEFAULT_THEME);
    set.themes
        .get(chosen)
        .or_else(|| set.themes.get(DEFAULT_THEME))
}

/// Every char of `code` with the style it is drawn in, newlines included, so the caller
/// can cut it back into the lines the code has.
pub fn highlight(code: &str, lang: &str) -> Vec<(char, Style)> {
    let plain = || code.chars().map(|c| (c, PLAIN)).collect();
    // Only the word before any attributes, which fences carry in several dialects.
    let name = lang
        .split([' ', ',', '{', ':'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if name.is_empty() {
        return plain();
    }
    let syntaxes = syntaxes();
    let (Some(syntax), Some(theme)) = (syntaxes.find_syntax_by_token(&name), theme()) else {
        return plain();
    };
    let mut lines = HighlightLines::new(syntax, theme);
    let mut out: Vec<(char, Style)> = Vec::with_capacity(code.len());
    for line in LinesWithEndings::from(code) {
        // A line syntect cannot take is drawn plain; the rest of the block still gets
        // its colours, which is better than dropping them all for one bad line.
        let Ok(regions) = lines.highlight_line(line, syntaxes) else {
            out.extend(line.chars().map(|c| (c, PLAIN)));
            continue;
        };
        for (highlighted, text) in regions {
            let style = convert(highlighted);
            out.extend(text.chars().map(|c| (c, style)));
        }
    }
    out
}

/// A syntect style as ratatui draws it. The theme's background is dropped: bhai draws
/// code on the terminal's own background, so a theme's would show as a block of colour
/// behind every block.
fn convert(highlighted: Highlighted) -> Style {
    let fg = highlighted.foreground;
    let mut style = Style::new().fg(palette::rgb(fg.r, fg.g, fg.b));
    if highlighted.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if highlighted.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if highlighted.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style the first occurrence of `word` is drawn in.
    fn style_of(painted: &[(char, Style)], word: &str) -> Style {
        let text: String = painted.iter().map(|(c, _)| c).collect();
        let at = text
            .find(word)
            .unwrap_or_else(|| panic!("no {word} in {text}"));
        let index = text[..at].chars().count();
        painted[index].1
    }

    #[test]
    fn a_known_language_is_coloured_by_its_grammar() {
        let painted = highlight("fn main() { let x = 1; }\n", "rust");
        assert_eq!(painted.iter().filter(|(c, _)| *c == '\n').count(), 1);
        // A keyword and a literal are told apart, and neither is left plain.
        let keyword = style_of(&painted, "fn");
        let number = style_of(&painted, "1");
        assert_ne!(keyword, number);
        assert_ne!(keyword, PLAIN);
    }

    #[test]
    fn an_alias_finds_the_same_grammar_as_the_name() {
        let code = "def f():\n    return 1\n";
        assert_eq!(highlight(code, "py"), highlight(code, "python"));
    }

    #[test]
    fn attributes_after_the_language_are_ignored() {
        let code = "fn main() {}\n";
        assert_eq!(highlight(code, "rust,no_run"), highlight(code, "rust"));
        assert_eq!(
            highlight(code, "rust {.line-numbers}"),
            highlight(code, "rust")
        );
    }

    #[test]
    fn a_fence_naming_nothing_or_nothing_known_stays_plain() {
        for lang in ["", "   ", "not-a-language"] {
            let painted = highlight("fn main() {}\n", lang);
            assert!(painted.iter().all(|(_, style)| *style == PLAIN), "{lang}");
        }
    }

    #[test]
    fn every_char_of_the_code_comes_back_in_order() {
        let code = "SELECT 1 -- a comment\nFROM t\n";
        for lang in ["sql", "", "nonsense"] {
            let painted = highlight(code, lang);
            let text: String = painted.iter().map(|(c, _)| c).collect();
            assert_eq!(text, code, "{lang}");
        }
    }

    #[test]
    fn the_default_theme_is_one_syntect_ships() {
        assert!(
            themes().iter().any(|t| t == DEFAULT_THEME),
            "{:?}",
            themes()
        );
    }
}
