//! Show the model a local image. Needs no approval, and is decided as a `read` of the file.

use std::io::Read as _;
use std::path::Path;

use serde_json::{Value, json};

use super::{BoxFuture, Image, Live, MAX_IMAGE_INPUT, Tool, path_arg, with_images};

pub const NAME: &str = "view_image";

pub struct ViewImage;

impl Tool for ViewImage {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Look at a local PNG, JPEG, GIF or WebP image, such as a \
        screenshot or a diagram. The image comes back with the result. Runs without approval.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the image."
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

    fn parallel(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        Ok(format!("view image {}", path_arg(args)?.display()))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match path_arg(args).and_then(load) {
                Ok((text, image)) => (with_images(&text, &[image]), true),
                Err(e) => (e, false),
            }
        })
    }

    fn execute_images<'a>(
        &'a self,
        args: &'a Value,
        _live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool, Vec<Image>)> {
        Box::pin(async move {
            match path_arg(args).and_then(load) {
                Ok((text, image)) => (text, true, vec![image]),
                Err(e) => (e, false, Vec::new()),
            }
        })
    }
}

/// The line that goes with the image, and the image.
pub(crate) fn load(path: &Path) -> Result<(String, Image), String> {
    let fail = |e: &dyn std::fmt::Display| format!("Could not read {}: {e}", path.display());
    let meta = std::fs::metadata(path).map_err(|e| fail(&e))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file.", path.display()));
    }
    let too_large = || {
        format!(
            "{} is larger than {MAX_IMAGE_INPUT} bytes; scale it down first.",
            path.display()
        )
    };
    if meta.len() > MAX_IMAGE_INPUT as u64 {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // Bounded again in case the file grew since the check.
    std::fs::File::open(path)
        .map_err(|e| fail(&e))?
        .take(MAX_IMAGE_INPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(&e))?;
    if bytes.len() > MAX_IMAGE_INPUT {
        return Err(too_large());
    }
    let image = image(&bytes)
        .map_err(|_| format!("{} is not a PNG, JPEG, GIF or WebP image.", path.display()))?;
    let text = format!("{} ({}, {} bytes)", path.display(), image.mime, bytes.len());
    Ok((text, image))
}

/// `bytes` as an image, typed by what they start with.
pub(crate) fn image(bytes: &[u8]) -> Result<Image, String> {
    let mime = sniff(bytes).ok_or("not a PNG, JPEG, GIF or WebP image")?;
    Image::new(mime, &crate::clipboard::base64(bytes))
}

/// The type the file's first bytes say it is; the extension is not trusted.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn live() -> Live<'static> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        Live {
            progress: &|_| {},
            cancel: &NEVER,
            conversation: None,
        }
    }

    #[tokio::test]
    async fn an_image_comes_back_beside_its_line() {
        let dir = super::super::temp_dir();
        let path = dir.join("shot.dat");
        std::fs::write(&path, PNG).unwrap();
        let args = json!({"path": path.to_str().unwrap()});
        let (text, ok, images) = ViewImage.execute_images(&args, live()).await;
        assert!(ok, "{text}");
        assert!(text.contains("image/png, 16 bytes"), "{text}");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].data, crate::clipboard::base64(PNG));

        // Where only text goes, the image is a placeholder.
        let (text, ok) = ViewImage.execute(&args).await;
        assert!(ok && text.ends_with("\n[image image/png]"), "{text}");
    }

    #[tokio::test]
    async fn what_is_not_an_image_is_refused() {
        let dir = super::super::temp_dir();
        let path = dir.join("notes.png");
        std::fs::write(&path, "plain text").unwrap();
        let args = json!({"path": path.to_str().unwrap()});
        let (text, ok, images) = ViewImage.execute_images(&args, live()).await;
        assert!(!ok && images.is_empty());
        assert!(text.contains("not a PNG, JPEG, GIF or WebP"), "{text}");

        let args = json!({"path": dir.to_str().unwrap()});
        let (text, ok, _) = ViewImage.execute_images(&args, live()).await;
        assert!(!ok && text.contains("not a regular file"), "{text}");
        assert!(ViewImage.describe(&json!({"path": "shot.png"})).is_err());
    }

    #[test]
    fn the_type_comes_from_the_bytes() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVE"), None);
        assert_eq!(sniff(b""), None);
    }
}
