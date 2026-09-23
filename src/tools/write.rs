//! Write a whole file, creating parent directories, after the user approves it.

use std::path::Path;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, path_arg, string_arg};

pub const NAME: &str = "write";

pub struct Write;

impl Tool for Write {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Create or overwrite a file with the given content, creating \
        missing parent directories. The user approves every write. Prefer `edit` for changes to an \
        existing file.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the file."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full new content of the file."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let (path, content) = parse(args)?;
        // Creating a file and replacing one are the same call, and the approval line is
        // where that difference has to be visible.
        let over = match std::fs::metadata(path) {
            Ok(meta) => format!("over {} bytes", meta.len()),
            Err(_) => "new file".to_string(),
        };
        Ok(format!(
            "write {} ({} bytes, {over})",
            path.display(),
            content.len()
        ))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match parse(args).and_then(|(path, content)| write(path, content)) {
                Ok(output) => (output, true),
                Err(e) => (e, false),
            }
        })
    }
}

fn parse(args: &Value) -> Result<(&Path, &str), String> {
    let path = path_arg(args)?;
    let content = string_arg(args, "content")
        .ok_or_else(|| "missing required string field `content`.".to_string())?;
    Ok((path, content))
}

fn write(path: &Path, content: &str) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, content)
        .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    Ok(format!(
        "Wrote {} bytes to {}.",
        content.len(),
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_missing_parent_directories() {
        let path = super::super::temp_dir().join("a/b/c.txt");
        let out = write(&path, "hello").unwrap();
        assert!(out.contains("5 bytes"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn describes_the_path_and_size() {
        let dir = super::super::temp_dir();
        let fresh = dir.join("x.txt");
        let args = json!({"path": fresh, "content": "abc"});
        assert_eq!(
            Write.describe(&args).unwrap(),
            format!("write {} (3 bytes, new file)", fresh.display())
        );
        // The one gate that matters has to say a create from an overwrite.
        std::fs::write(&fresh, "0123456789").unwrap();
        assert_eq!(
            Write.describe(&args).unwrap(),
            format!("write {} (3 bytes, over 10 bytes)", fresh.display())
        );
        let _ = std::fs::remove_dir_all(dir);
        assert!(
            Write
                .describe(&json!({"path": "x.txt", "content": ""}))
                .is_err()
        );
        assert!(Write.describe(&json!({"path": "/tmp/x.txt"})).is_err());
    }
}
