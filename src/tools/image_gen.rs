//! `image_gen`: generate an image from a prompt and save it as a file, through the Images
//! API under the Codex endpoints, on the credentials the session already runs on. The
//! endpoint's shape follows nanocodex's client. It writes a file, so it is approved and
//! judged as a `write` of its path.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    BoxFuture, Image, Live, MAX_IMAGE_BYTES, Tool, floor_boundary, path_arg, string_arg,
    with_images,
};
use crate::client::{self, Provider};

pub const NAME: &str = "image_gen";

const PATH: &str = "/images/generations";
const MODEL: &str = "gpt-image-2";
const SIZES: [&str; 4] = ["auto", "1024x1024", "1536x1024", "1024x1536"];
const QUALITIES: [&str; 4] = ["auto", "low", "medium", "high"];
const BACKGROUNDS: [&str; 3] = ["auto", "transparent", "opaque"];
/// A high-quality image takes a minute or more to come back.
const TIMEOUT: Duration = Duration::from_secs(300);
/// The answer carries the image as base64, a third larger than the file.
const MAX_RESPONSE: usize = MAX_IMAGE_BYTES * 2;
const SUMMARY: usize = 200;
const TICK: Duration = Duration::from_millis(50);

const DESCRIPTION: &str = "Generate a PNG image from a text prompt and save it to a file, \
such as an icon, an illustration or a placeholder asset. Describe the subject, style, colours \
and composition in the prompt. The image comes back with the result. The user approves every \
image, as they do a `write` of its path.";

pub struct ImageGen {
    endpoint: String,
}

impl ImageGen {
    /// The Images API under the Codex endpoints the session's model calls.
    pub fn codex() -> Self {
        Self {
            endpoint: format!("{}{PATH}", client::base_url()),
        }
    }
}

/// A call's arguments, checked.
struct Request<'a> {
    prompt: &'a str,
    path: &'a Path,
    size: &'a str,
    quality: &'a str,
    background: &'a str,
}

impl Tool for ImageGen {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": DESCRIPTION,
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "What the image shows."
                    },
                    "path": {
                        "type": "string",
                        "description": "Absolute path the PNG is saved to."
                    },
                    "size": {
                        "type": "string",
                        "enum": SIZES,
                        "description": "Width x height; defaults to auto."
                    },
                    "quality": {"type": "string", "enum": QUALITIES},
                    "background": {
                        "type": "string",
                        "enum": BACKGROUNDS,
                        "description": "transparent for an icon or a sprite."
                    }
                },
                "required": ["prompt", "path"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let request = parse(args)?;
        // As with `write`, the approval line is where an overwrite has to show.
        let over = match std::fs::metadata(request.path) {
            Ok(meta) => format!("over {} bytes", meta.len()),
            Err(_) => "new file".to_string(),
        };
        let mut line = format!(
            "image_gen {} ({}, {over}): {:?}",
            request.path.display(),
            request.size,
            request.prompt
        );
        if line.len() > SUMMARY {
            line.truncate(floor_boundary(&line, SUMMARY));
            line.push_str("...");
        }
        Ok(line)
    }

    fn preview(&self, args: &Value) -> Option<String> {
        Some(parse(args).ok()?.prompt.to_string())
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NEVER,
            conversation: None,
        };
        self.execute_live(args, live)
    }

    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let (text, ok, images) = self.execute_images(args, live).await;
            (with_images(&text, &images), ok)
        })
    }

    fn execute_images<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool, Vec<Image>)> {
        Box::pin(async move {
            let fail = |why: String| (why, false, Vec::new());
            let Some(conversation) = live.conversation else {
                return fail("`image_gen` runs only inside a turn.".to_string());
            };
            if Provider::of(conversation.model) != Provider::Codex {
                return fail(format!(
                    "`image_gen` runs on the Codex backend, and `{}` is not a Codex model.",
                    conversation.model
                ));
            }
            let request = match parse(args) {
                Ok(request) => request,
                Err(e) => return fail(e),
            };
            let body = request_body(&request);
            let send = tokio::time::timeout(
                TIMEOUT,
                super::web::send(&self.endpoint, &body, "image request", MAX_RESPONSE),
            );
            tokio::pin!(send);
            let mut tick = tokio::time::interval(TICK);
            let bytes = loop {
                tokio::select! {
                    biased;
                    _ = tick.tick() => {
                        if live.cancel.load(Ordering::Relaxed) {
                            let why = "The user interrupted the turn; no image was saved.";
                            return fail(why.to_string());
                        }
                    }
                    out = &mut send => match out {
                        Ok(Ok(bytes)) => break bytes,
                        Ok(Err(e)) => return fail(e),
                        Err(_) => return fail(format!(
                            "the image request timed out after {}s",
                            TIMEOUT.as_secs()
                        )),
                    },
                }
            };
            match answer(&bytes).and_then(|(png, revised)| save(request.path, &png, revised)) {
                Ok((text, image)) => (text, true, image.into_iter().collect()),
                Err(e) => fail(e),
            }
        })
    }
}

fn parse(args: &Value) -> Result<Request<'_>, String> {
    let prompt = string_arg(args, "prompt")
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| "missing required string field `prompt`.".to_string())?;
    let path = path_arg(args)?;
    let choice = |key: &str, allowed: &[&'static str]| match args.get(key) {
        None | Some(Value::Null) => Ok(allowed[0]),
        Some(value) => value
            .as_str()
            .and_then(|v| allowed.iter().copied().find(|a| *a == v))
            .ok_or_else(|| format!("`{key}` is one of {}.", allowed.join(", "))),
    };
    Ok(Request {
        prompt,
        path,
        size: choice("size", &SIZES)?,
        quality: choice("quality", &QUALITIES)?,
        background: choice("background", &BACKGROUNDS)?,
    })
}

fn request_body(request: &Request<'_>) -> Value {
    json!({
        "model": MODEL,
        "prompt": request.prompt,
        "n": 1,
        "size": request.size,
        "quality": request.quality,
        "background": request.background,
        "output_format": "png",
    })
}

/// The image's bytes in an Images API answer, and the prompt it was drawn from when the
/// API rewrote it.
fn answer(bytes: &[u8]) -> Result<(Vec<u8>, Option<String>), String> {
    let parsed: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("the image request's answer was not JSON: {e}"))?;
    let first = &parsed["data"][0];
    let data = first["b64_json"]
        .as_str()
        .ok_or("the image request's answer has no `b64_json` image.")?;
    let png = crate::clipboard::unbase64(data)
        .ok_or("the image request's answer is not valid base64.")?;
    let revised = first["revised_prompt"].as_str().map(str::to_string);
    Ok((png, revised))
}

/// Write the image to `path`, creating parent directories, and return what the model is
/// told with the image itself; an image too large to send is saved all the same.
fn save(
    path: &Path,
    bytes: &[u8],
    revised: Option<String>,
) -> Result<(String, Option<Image>), String> {
    let image = super::view_image::image(bytes)
        .map_err(|_| "the image request answered with something that is not an image.")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    let mut text = format!(
        "Saved a {} image of {} bytes to {}.",
        image.mime,
        bytes.len(),
        path.display()
    );
    if let Some(revised) = revised.filter(|r| !r.is_empty()) {
        text.push_str(&format!("\nThe prompt as drawn: {revised}"));
    }
    let sent = Image::new(&image.mime, &image.data).ok();
    Ok((text, sent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Conversation;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn arguments_are_checked_and_defaulted() {
        let tool = ImageGen::codex();
        assert!(tool.needs_approval());
        let args = json!({"prompt": "a red fox", "path": "/tmp/fox.png"});
        assert_eq!(
            request_body(&parse(&args).unwrap()),
            json!({
                "model": MODEL,
                "prompt": "a red fox",
                "n": 1,
                "size": "auto",
                "quality": "auto",
                "background": "auto",
                "output_format": "png",
            })
        );
        let args = json!({"prompt": "icon", "path": "/tmp/i.png", "size": "1024x1024", "background": "transparent"});
        let body = request_body(&parse(&args).unwrap());
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["background"], "transparent");
        for (args, says) in [
            (json!({"path": "/tmp/x.png"}), "`prompt`"),
            (json!({"prompt": "  ", "path": "/tmp/x.png"}), "`prompt`"),
            (json!({"prompt": "x", "path": "x.png"}), "bsolute"),
            (
                json!({"prompt": "x", "path": "/tmp/x.png", "size": "4096x4096"}),
                "`size` is one of",
            ),
            (
                json!({"prompt": "x", "path": "/tmp/x.png", "quality": 3}),
                "`quality` is one of",
            ),
        ] {
            let err = tool.describe(&args).err().unwrap_or_default();
            assert!(err.contains(says), "{args}: {err}");
        }
    }

    #[test]
    fn the_approval_line_names_the_path_and_whether_it_overwrites() {
        let dir = super::super::temp_dir();
        let path = dir.join("logo.png");
        let args = json!({"prompt": "a logo", "path": path});
        let tool = ImageGen::codex();
        assert_eq!(
            tool.describe(&args).unwrap(),
            format!("image_gen {} (auto, new file): \"a logo\"", path.display())
        );
        std::fs::write(&path, "0123456789").unwrap();
        assert!(tool.describe(&args).unwrap().contains("over 10 bytes"));
        assert_eq!(tool.preview(&args).unwrap(), "a logo");
        let long = json!({"prompt": "x".repeat(400), "path": path});
        assert!(tool.describe(&long).unwrap().len() <= SUMMARY + 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_answer_is_saved_and_comes_back_as_an_image() {
        let dir = super::super::temp_dir();
        let path = dir.join("a/b/fox.png");
        let body = json!({"data": [{"b64_json": crate::clipboard::base64(PNG), "revised_prompt": "a red fox, flat"}]});
        let (png, revised) = answer(body.to_string().as_bytes()).unwrap();
        let (text, image) = save(&path, &png, revised).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        assert!(text.contains("image/png image of 16 bytes"), "{text}");
        assert!(
            text.ends_with("The prompt as drawn: a red fox, flat"),
            "{text}"
        );
        assert_eq!(image.unwrap().data, crate::clipboard::base64(PNG));

        let not_image = json!({"data": [{"b64_json": crate::clipboard::base64(b"hello")}]});
        let (bytes, _) = answer(not_image.to_string().as_bytes()).unwrap();
        let other = dir.join("other.png");
        assert!(
            save(&other, &bytes, None)
                .unwrap_err()
                .contains("not an image")
        );
        assert!(
            !other.exists(),
            "nothing is written for an answer that is not an image"
        );
        assert!(answer(br#"{"data": []}"#).unwrap_err().contains("b64_json"));
        assert!(
            answer(br#"{"data": [{"b64_json": "!!"}]}"#)
                .unwrap_err()
                .contains("base64")
        );
        assert!(answer(b"<html>").unwrap_err().contains("not JSON"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_call_outside_a_turn_or_on_ollama_sends_nothing() {
        let tool = ImageGen {
            endpoint: "http://127.0.0.1:9/never".to_string(),
        };
        let args = json!({"prompt": "x", "path": "/tmp/never.png"});
        let (out, ok) = tool.execute(&args).await;
        assert!(!ok && out.contains("only inside a turn"), "{out}");

        static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NOT_CANCELLED,
            conversation: Some(Conversation {
                id: "s",
                model: "ollama:qwen3",
                history: &[],
            }),
        };
        let (out, ok, images) = tool.execute_images(&args, live).await;
        assert!(
            !ok && images.is_empty() && out.contains("not a Codex model"),
            "{out}"
        );
    }
}
