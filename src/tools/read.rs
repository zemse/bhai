//! Read a file as numbered lines. Needs no approval.

use std::path::Path;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, path_arg, truncate};

pub const NAME: &str = "read";

/// Lines returned when the call gives no limit.
const DEFAULT_LIMIT: usize = 2000;

pub struct Read;

impl Tool for Read {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Read a text file and return its lines numbered from 1. Use \
        `offset` and `limit` to page through large files. Runs without approval.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the file."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start from, 1-based. Defaults to 1."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum lines to return. Defaults to 2000."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let path = path_arg(args)?;
        let (offset, limit) = range(args)?;
        Ok(match limit {
            Some(limit) => format!("read {} (lines {offset}+{limit})", path.display()),
            None if offset > 1 => format!("read {} (from line {offset})", path.display()),
            None => format!("read {}", path.display()),
        })
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let result = path_arg(args).and_then(|path| {
                let (offset, limit) = range(args)?;
                read(path, offset, limit.unwrap_or(DEFAULT_LIMIT))
            });
            match result {
                Ok(output) => (truncate(&output), true),
                Err(e) => (e, false),
            }
        })
    }
}

/// The 1-based offset and the optional limit.
fn range(args: &Value) -> Result<(usize, Option<usize>), String> {
    let number = |key: &str| match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| Some(n as usize))
            .ok_or_else(|| format!("`{key}` must be a positive integer.")),
    };
    Ok((number("offset")?.unwrap_or(1), number("limit")?))
}

fn read(path: &Path, offset: usize, limit: usize) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    if total == 0 {
        return Ok("(empty file)".to_string());
    }
    if offset > total {
        return Err(format!(
            "offset {offset} is past the end; the file has {total} lines."
        ));
    }

    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    let last = (offset - 1 + limit).min(total);
    if last < total {
        out.push_str(&format!(
            "[showing lines {offset}-{last} of {total}; continue with offset {}]\n",
            last + 1
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(lines: usize) -> std::path::PathBuf {
        let path = super::super::temp_dir().join("f.txt");
        let body: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn returns_numbered_lines() {
        let out = read(&file(3), 1, DEFAULT_LIMIT).unwrap();
        assert_eq!(out, "     1\tline 1\n     2\tline 2\n     3\tline 3\n");
    }

    #[test]
    fn offset_and_limit_select_a_window() {
        let out = read(&file(10), 4, 2).unwrap();
        assert!(out.starts_with("     4\tline 4\n     5\tline 5\n"), "{out}");
        assert!(!out.contains("line 6"), "{out}");
        assert!(
            out.contains("lines 4-5 of 10; continue with offset 6"),
            "{out}"
        );
    }

    #[test]
    fn offset_past_the_end_is_an_error() {
        let err = read(&file(2), 5, 1).unwrap_err();
        assert!(err.contains("has 2 lines"), "{err}");
    }

    #[test]
    fn rejects_bad_ranges() {
        assert!(range(&json!({"offset": 0})).is_err());
        assert!(range(&json!({"limit": "5"})).is_err());
        assert_eq!(range(&json!({"limit": 5})).unwrap(), (1, Some(5)));
    }

    #[tokio::test]
    async fn a_missing_file_fails() {
        let path = super::super::temp_dir().join("missing");
        let args = json!({"path": path.to_str().unwrap()});
        let (out, ok) = Read.execute(&args).await;
        assert!(!ok);
        assert!(out.contains("Could not read"), "{out}");
    }
}
