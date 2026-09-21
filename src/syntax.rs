//! Syntax colours for fenced code blocks. A lexer only as far as reading needs: what is
//! a comment, what is a string, what is a number, what is a keyword. It parses nothing
//! and is never right about a language it does not know, so a block that does not name
//! one is left alone.

use ratatui::style::{Color, Style};

/// Code with nothing said about it: the body of a block, and every block whose fence
/// names no language.
pub const PLAIN: Style = Style::new().fg(Color::Gray);
pub(crate) const COMMENT: Style = Style::new().fg(Color::DarkGray);
pub(crate) const STRING: Style = Style::new().fg(Color::Green);
pub(crate) const NUMBER: Style = Style::new().fg(Color::Yellow);
pub(crate) const KEYWORD: Style = Style::new().fg(Color::Magenta);
/// A name that starts upper case: a type in most of these languages, a constant or an
/// environment variable in the rest, and worth picking out either way.
pub(crate) const NAME: Style = Style::new().fg(Color::Cyan);

/// What a language spells its comments, strings and keywords with.
struct Syntax {
    line: &'static [&'static str],
    block: &'static [(&'static str, &'static str)],
    /// Each delimiter and whether a string in it may cross a newline. Longest first, so
    /// `"""` is found before `"`.
    strings: &'static [(&'static str, bool)],
    keywords: &'static [&'static str],
}

const C_BLOCK: &[(&str, &str)] = &[("/*", "*/")];

/// The language a fence names, or `None` when it names one this does not know. An
/// unknown language is left plain rather than guessed at: a block of log output would
/// come out painted in another language's rules.
fn syntax(lang: &str) -> Option<&'static Syntax> {
    // Only the word before any attributes, which fences carry in several dialects.
    let name = lang
        .split([' ', ',', '{', ':'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match name.as_str() {
        "rust" | "rs" => &RUST,
        "python" | "py" => &PYTHON,
        "javascript" | "js" | "jsx" | "mjs" | "cjs" | "typescript" | "ts" | "tsx" => &JS,
        "go" | "golang" => &GO,
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "kotlin" | "kt" | "swift" => &CLIKE,
        "ruby" | "rb" => &RUBY,
        "sh" | "bash" | "zsh" | "shell" | "console" => &SHELL,
        "json" | "jsonc" => &JSON,
        "toml" => &TOML,
        "yaml" | "yml" => &YAML,
        "sql" => &SQL,
        "css" | "scss" => &CSS,
        _ => return None,
    })
}

// A lifetime opens no string in Rust, so `'` is not a delimiter here: `&'a str` would
// otherwise paint the rest of the line.
static RUST: Syntax = Syntax {
    line: &["//"],
    block: C_BLOCK,
    strings: &[("\"", true)],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true",
        "type", "unsafe", "use", "where", "while", "bool", "char", "f32", "f64", "i8", "i16",
        "i32", "i64", "isize", "str", "u8", "u16", "u32", "u64", "usize",
    ],
};

static PYTHON: Syntax = Syntax {
    line: &["#"],
    block: &[],
    strings: &[("\"\"\"", true), ("'''", true), ("\"", false), ("'", false)],
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True",
        "try", "while", "with", "yield",
    ],
};

static JS: Syntax = Syntax {
    line: &["//"],
    block: C_BLOCK,
    strings: &[("`", true), ("\"", false), ("'", false)],
    keywords: &[
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "function",
        "from",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "let",
        "new",
        "null",
        "of",
        "readonly",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
        "boolean",
        "number",
        "string",
        "any",
        "unknown",
        "never",
    ],
};

static GO: Syntax = Syntax {
    line: &["//"],
    block: C_BLOCK,
    strings: &[("`", true), ("\"", false), ("'", false)],
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "false",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "nil",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "true",
        "type",
        "var",
        "bool",
        "byte",
        "error",
        "float32",
        "float64",
        "int",
        "int32",
        "int64",
        "rune",
        "string",
        "uint",
    ],
};

static CLIKE: Syntax = Syntax {
    line: &["//"],
    block: C_BLOCK,
    strings: &[("\"", false), ("'", false)],
    keywords: &[
        "auto",
        "bool",
        "break",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "extern",
        "false",
        "final",
        "float",
        "for",
        "func",
        "if",
        "implements",
        "import",
        "inline",
        "int",
        "interface",
        "let",
        "long",
        "namespace",
        "new",
        "null",
        "nullptr",
        "override",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "typename",
        "union",
        "unsigned",
        "using",
        "var",
        "virtual",
        "void",
        "volatile",
        "while",
    ],
};

static RUBY: Syntax = Syntax {
    line: &["#"],
    block: &[],
    strings: &[("\"", false), ("'", false)],
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "do", "else", "elsif", "end",
        "ensure", "false", "for", "if", "in", "module", "next", "nil", "not", "or", "raise",
        "redo", "require", "rescue", "retry", "return", "self", "super", "then", "true", "unless",
        "until", "when", "while", "yield",
    ],
};

// A shell has no escape inside `'`, but the string ends at the newline either way, so
// the worst an apostrophe does is colour the rest of one line.
static SHELL: Syntax = Syntax {
    line: &["#"],
    block: &[],
    strings: &[("\"", false), ("'", false)],
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
        "in", "local", "read", "readonly", "return", "select", "set", "shift", "source", "then",
        "unset", "until", "while",
    ],
};

static JSON: Syntax = Syntax {
    line: &[],
    block: &[],
    strings: &[("\"", false)],
    keywords: &["true", "false", "null"],
};

static TOML: Syntax = Syntax {
    line: &["#"],
    block: &[],
    strings: &[("\"\"\"", true), ("'''", true), ("\"", false), ("'", false)],
    keywords: &["true", "false"],
};

static YAML: Syntax = Syntax {
    line: &["#"],
    block: &[],
    strings: &[("\"", false), ("'", false)],
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
};

static SQL: Syntax = Syntax {
    line: &["--"],
    block: C_BLOCK,
    strings: &[("'", false), ("\"", false)],
    keywords: &[
        "alter", "and", "as", "asc", "between", "by", "case", "create", "delete", "desc",
        "distinct", "drop", "else", "end", "exists", "from", "group", "having", "in", "index",
        "inner", "insert", "into", "is", "join", "left", "like", "limit", "not", "null", "offset",
        "on", "or", "order", "outer", "primary", "select", "set", "table", "then", "union",
        "update", "values", "when", "where", "with",
    ],
};

static CSS: Syntax = Syntax {
    line: &["//"],
    block: C_BLOCK,
    strings: &[("\"", false), ("'", false)],
    keywords: &[
        "and",
        "auto",
        "false",
        "important",
        "inherit",
        "initial",
        "media",
        "none",
        "not",
        "true",
        "unset",
    ],
};

/// Every char of `code` with the style it is read in. A language this does not know
/// comes back plain, which is what the block looked like before any of this.
pub fn highlight(code: &str, lang: &str) -> Vec<(char, Style)> {
    let chars: Vec<char> = code.chars().collect();
    let Some(syntax) = syntax(lang) else {
        return chars.into_iter().map(|c| (c, PLAIN)).collect();
    };
    let mut out: Vec<(char, Style)> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = comment(&chars, i, syntax) {
            paint(&mut out, &chars[i..end], COMMENT);
            i = end;
        } else if let Some(end) = string(&chars, i, syntax) {
            paint(&mut out, &chars[i..end], STRING);
            i = end;
        } else if chars[i].is_ascii_digit() {
            // A number runs through its suffix and its exponent: `1_000u64`, `0xff`,
            // `1.5e-3`. What it is exactly does not matter, only where it stops.
            let mut end = i;
            while end < chars.len()
                && (chars[end].is_alphanumeric()
                    || chars[end] == '_'
                    || chars[end] == '.'
                    || (matches!(chars[end], '+' | '-') && matches!(chars[end - 1], 'e' | 'E')))
            {
                end += 1;
            }
            paint(&mut out, &chars[i..end], NUMBER);
            i = end;
        } else if is_word_start(chars[i]) {
            let mut end = i;
            while end < chars.len() && is_word(chars[end]) {
                end += 1;
            }
            let word: String = chars[i..end].iter().collect();
            let style = if syntax.keywords.contains(&word.as_str()) {
                KEYWORD
            } else if word.starts_with(char::is_uppercase) {
                NAME
            } else {
                PLAIN
            };
            paint(&mut out, &chars[i..end], style);
            i = end;
        } else {
            out.push((chars[i], PLAIN));
            i += 1;
        }
    }
    out
}

/// Where the comment starting at `i` ends, or `None` if none starts there. An unclosed
/// block comment runs to the end, which is what the compiler would say too.
fn comment(chars: &[char], i: usize, syntax: &Syntax) -> Option<usize> {
    if syntax.line.iter().any(|mark| at(chars, i, mark)) {
        return Some(find(chars, i, "\n").unwrap_or(chars.len()));
    }
    let (open, close) = syntax.block.iter().find(|(open, _)| at(chars, i, open))?;
    let from = i + open.chars().count();
    Some(find(chars, from, close).map_or(chars.len(), |at| at + close.chars().count()))
}

/// Where the string starting at `i` ends, or `None` if none starts there. A string that
/// may not cross a newline stops at one rather than painting the rest of the block.
fn string(chars: &[char], i: usize, syntax: &Syntax) -> Option<usize> {
    let (open, multiline) = syntax.strings.iter().find(|(open, _)| at(chars, i, open))?;
    let len = open.chars().count();
    let mut j = i + len;
    while j < chars.len() {
        if chars[j] == '\\' {
            j += 2;
            continue;
        }
        if !multiline && chars[j] == '\n' {
            return Some(j);
        }
        if at(chars, j, open) {
            return Some(j + len);
        }
        j += 1;
    }
    Some(chars.len())
}

fn at(chars: &[char], i: usize, mark: &str) -> bool {
    mark.chars()
        .enumerate()
        .all(|(n, c)| chars.get(i + n) == Some(&c))
}

fn find(chars: &[char], from: usize, mark: &str) -> Option<usize> {
    (from..chars.len()).find(|&i| at(chars, i, mark))
}

fn paint(out: &mut Vec<(char, Style)>, chars: &[char], style: Style) {
    out.extend(chars.iter().map(|&c| (c, style)));
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style the first `part` of `code` is drawn in.
    fn style_of(code: &str, lang: &str, part: &str) -> Style {
        let cells = highlight(code, lang);
        let text: String = cells.iter().map(|(c, _)| c).collect();
        let at = text.find(part).expect("part is in the code");
        let start = text[..at].chars().count();
        let styles: Vec<Style> = cells[start..start + part.chars().count()]
            .iter()
            .map(|(_, style)| *style)
            .collect();
        assert!(
            styles.iter().all(|style| *style == styles[0]),
            "{part:?} is drawn in more than one style"
        );
        styles[0]
    }

    #[test]
    fn rust_reads_as_rust() {
        let code = "// a note\nlet total: u32 = 42;\nlet s = \"text\";\nstruct Thing;";
        assert_eq!(style_of(code, "rust", "// a note"), COMMENT);
        assert_eq!(style_of(code, "rust", "let"), KEYWORD);
        assert_eq!(style_of(code, "rust", "42"), NUMBER);
        assert_eq!(style_of(code, "rust", "\"text\""), STRING);
        assert_eq!(style_of(code, "rust", "Thing"), NAME);
        assert_eq!(style_of(code, "rust", "total"), PLAIN);
    }

    #[test]
    fn a_lifetime_is_not_a_string() {
        // The `'a` used to open a string nothing closed, painting the rest of the line.
        let code = "fn f<'a>(s: &'a str) -> &'a str { s }";
        assert_eq!(style_of(code, "rs", "'a>"), PLAIN);
        // Still a keyword, so the lifetime before it swallowed nothing.
        assert_eq!(style_of(code, "rs", "str"), KEYWORD);
        assert_eq!(style_of(code, "rs", "fn"), KEYWORD);
    }

    #[test]
    fn an_unclosed_quote_stops_at_the_line() {
        let code = "print(\"open\nnext = 1";
        assert_eq!(style_of(code, "py", "next"), PLAIN);
        assert_eq!(style_of(code, "py", "1"), NUMBER);
    }

    #[test]
    fn a_docstring_crosses_lines() {
        let code = "def f():\n    \"\"\"says\n    what it does\n    \"\"\"\n    return 1";
        assert_eq!(style_of(code, "python", "what it does"), STRING);
        assert_eq!(style_of(code, "python", "return"), KEYWORD);
    }

    #[test]
    fn a_block_comment_crosses_lines_and_an_unclosed_one_runs_out() {
        let code = "/* one\n   two */ let x = 1;";
        assert_eq!(style_of(code, "rust", "/* one"), COMMENT);
        assert_eq!(style_of(code, "rust", "two */"), COMMENT);
        assert_eq!(style_of(code, "rust", "let"), KEYWORD);
        assert_eq!(style_of("/* never closed\nx = 1", "js", "x = 1"), COMMENT);
    }

    #[test]
    fn a_shell_variable_and_a_flag_stay_readable() {
        let code = "# build\nexport PATH=/usr/bin  # note\ncargo test -- --nocapture";
        assert_eq!(style_of(code, "bash", "export"), KEYWORD);
        assert_eq!(style_of(code, "bash", "PATH"), NAME);
        assert_eq!(style_of(code, "bash", "# note"), COMMENT);
        assert_eq!(style_of(code, "bash", "cargo"), PLAIN);
    }

    #[test]
    fn a_language_this_does_not_know_is_left_plain() {
        let code = "error: it's broken # not a comment\n42";
        for (_, style) in highlight(code, "") {
            assert_eq!(style, PLAIN);
        }
        for (_, style) in highlight(code, "brainfuck") {
            assert_eq!(style, PLAIN);
        }
    }

    #[test]
    fn a_fence_with_attributes_still_names_its_language() {
        assert_eq!(style_of("let x = 1;", "rust,ignore", "let"), KEYWORD);
        assert_eq!(style_of("let x = 1;", "RUST", "let"), KEYWORD);
        assert_eq!(style_of("const x = 1", "js {1,3}", "const"), KEYWORD);
    }

    #[test]
    fn every_char_survives_the_lexer() {
        let code = "fn main() {\n    let s = \"héllo\";\n} // ok\n";
        let back: String = highlight(code, "rust").iter().map(|(c, _)| c).collect();
        assert_eq!(back, code);
        let back: String = highlight(code, "nonesuch").iter().map(|(c, _)| c).collect();
        assert_eq!(back, code);
    }

    #[test]
    fn json_colours_its_values() {
        let code = "{\"name\": \"bhai\", \"n\": 3, \"ok\": true}";
        assert_eq!(style_of(code, "json", "\"bhai\""), STRING);
        assert_eq!(style_of(code, "json", "3"), NUMBER);
        assert_eq!(style_of(code, "json", "true"), KEYWORD);
    }
}
