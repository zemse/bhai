//! Replace text in a file, after the user approves it. Tries an exact match first, then a
//! unique match that ignores indentation and trailing whitespace on each line.

use std::path::Path;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, path_arg, string_arg};

pub const NAME: &str = "edit";

pub struct Edit;

struct Args<'a> {
    path: &'a Path,
    old: &'a str,
    new: &'a str,
    replace_all: bool,
}

impl Tool for Edit {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Replace `old_string` with `new_string` in a file. `old_string` \
        must match exactly one place unless `replace_all` is set; include enough surrounding lines \
        to make it unique. The user approves every edit. Read the file first.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the file."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to replace."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement text."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every exact occurrence. Defaults to false."
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let args = parse(args)?;
        let lines = |s: &str| s.lines().count().max(1);
        Ok(format!(
            "edit {} ({} lines -> {} lines{})",
            args.path.display(),
            lines(args.old),
            lines(args.new),
            if args.replace_all {
                ", all matches"
            } else {
                ""
            }
        ))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match parse(args).and_then(|args| edit_file(&args)) {
                Ok(output) => (output, true),
                Err(e) => (e, false),
            }
        })
    }
}

fn parse(args: &Value) -> Result<Args<'_>, String> {
    let path = path_arg(args)?;
    let field = |key: &str| {
        string_arg(args, key).ok_or_else(|| format!("missing required string field `{key}`."))
    };
    let (old, new) = (field("old_string")?, field("new_string")?);
    if old.is_empty() {
        return Err("`old_string` is empty; use `write` to create a file.".to_string());
    }
    if old == new {
        return Err("`old_string` and `new_string` are identical.".to_string());
    }
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Args {
        path,
        old,
        new,
        replace_all,
    })
}

fn edit_file(args: &Args) -> Result<String, String> {
    let path = args.path;
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let (updated, note) = apply(&content, args.old, args.new, args.replace_all)?;
    std::fs::write(path, updated)
        .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    Ok(format!("Edited {}: {note}.", path.display()))
}

/// The edited content and a note on how it matched.
fn apply(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, String), String> {
    match content.matches(old).count() {
        0 => {}
        1 => return Ok((content.replacen(old, new, 1), "1 exact match".to_string())),
        n if replace_all => return Ok((content.replace(old, new), format!("{n} exact matches"))),
        n => {
            return Err(format!(
                "`old_string` matches {n} places. Add surrounding lines to make it unique, or \
set `replace_all`."
            ));
        }
    }

    let starts = fuzzy_matches(content, old);
    match starts.as_slice() {
        [] => Err(
            "`old_string` was not found, even ignoring indentation and trailing whitespace. \
Read the file again and copy the text exactly."
                .to_string(),
        ),
        [range] => Ok((
            format!("{}{new}{}", &content[..range.0], &content[range.1..]),
            "1 match ignoring whitespace".to_string(),
        )),
        many => Err(format!(
            "`old_string` was not found exactly and matches {} places ignoring whitespace. \
Copy the text exactly, with more surrounding lines.",
            many.len()
        )),
    }
}

/// Byte ranges of line runs that equal `old` line by line, ignoring leading and trailing
/// whitespace. A range ends after the last line's newline only if `old` ends with one.
fn fuzzy_matches(content: &str, old: &str) -> Vec<(usize, usize)> {
    let needle: Vec<&str> = old.lines().map(str::trim).collect();
    if needle.iter().all(|l| l.is_empty()) {
        return Vec::new();
    }
    // Each line's start, end without the newline, and end with it.
    let mut lines = Vec::new();
    let mut start = 0;
    for line in content.split_inclusive('\n') {
        let end = start + line.len();
        let bare = start + line.trim_end_matches(['\n', '\r']).len();
        lines.push((start, bare, end));
        start = end;
    }

    let mut found = Vec::new();
    for first in 0..lines.len().saturating_sub(needle.len() - 1) {
        let window = &lines[first..first + needle.len()];
        let same = window
            .iter()
            .zip(&needle)
            .all(|(&(s, e, _), want)| content[s..e].trim() == *want);
        if same {
            let last = window[window.len() - 1];
            let end = if old.ends_with('\n') { last.2 } else { last.1 };
            found.push((window[0].0, end));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_a_unique_exact_match() {
        let (out, note) = apply("a\nb\nc\n", "b\n", "B\n", false).unwrap();
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!(note, "1 exact match");
    }

    #[test]
    fn an_ambiguous_match_needs_replace_all() {
        let err = apply("x x", "x", "y", false).unwrap_err();
        assert!(err.contains("matches 2 places"), "{err}");
        let (out, _) = apply("x x", "x", "y", true).unwrap();
        assert_eq!(out, "y y");
    }

    #[test]
    fn a_missing_match_is_an_error() {
        let err = apply("a\nb\n", "zzz", "y", false).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn a_unique_fuzzy_match_is_applied() {
        let content = "fn f() {\n    let a = 1;  \n    let b = 2;\n}\n";
        let (out, note) =
            apply(content, "let a = 1;\nlet b = 2;", "    let c = 3;", false).unwrap();
        assert_eq!(out, "fn f() {\n    let c = 3;\n}\n");
        assert_eq!(note, "1 match ignoring whitespace");

        let (out, _) = apply(content, "  let a = 1;\n  let b = 2;\n", "", false).unwrap();
        assert_eq!(out, "fn f() {\n}\n");
    }

    #[test]
    fn an_ambiguous_fuzzy_match_is_refused() {
        let content = "  a\n  b\n\ta\n\tb\n";
        let err = apply(content, "a\nb", "c", false).unwrap_err();
        assert!(
            err.contains("matches 2 places ignoring whitespace"),
            "{err}"
        );
        let err = apply(content, "a\nb", "c", true).unwrap_err();
        assert!(err.contains("2 places"), "{err}");
    }

    #[test]
    fn edits_a_file_on_disk() {
        let path = super::super::temp_dir().join("f.rs");
        std::fs::write(&path, "hello world\n").unwrap();
        let args = json!({
            "path": path.to_str().unwrap(),
            "old_string": "world",
            "new_string": "bhai",
        });
        let args = parse(&args).unwrap();
        edit_file(&args).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello bhai\n");
    }

    #[test]
    fn describes_a_short_summary() {
        let args = json!({
            "path": "/tmp/f.rs",
            "old_string": "a\nb",
            "new_string": "c",
            "replace_all": true,
        });
        assert_eq!(
            Edit.describe(&args).unwrap(),
            "edit /tmp/f.rs (2 lines -> 1 lines, all matches)"
        );
        let relative = json!({"path": "f.rs", "old_string": "a", "new_string": "b"});
        assert!(Edit.describe(&relative).unwrap_err().contains("absolute"));
    }
}
