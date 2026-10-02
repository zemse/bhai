//! Images on their way to the model: decoded under a cap, scaled down to a pixel budget,
//! and cached by content, so the same image is the same bytes in every request.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{Mutex, MutexGuard};

use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, Limits};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::tools::{Image, MAX_IMAGE_BYTES};

/// The longest side the model is sent. High detail fits an image in 2048 square.
const MAX_SIDE: u32 = 2048;
/// The most pixels the model is sent. High detail then cuts the short side to 768, so a
/// 16:9 screenshot at this size loses nothing more on the backend.
const MAX_PIXELS: u64 = 2048 * 768;
/// A side past this is refused before decoding.
const MAX_DIMENSION: u32 = 16_384;
/// The most a decode may allocate, so a small file that inflates to gigabytes is refused.
/// A 6K RGBA screenshot needs about 80 MiB.
const MAX_DECODE: u64 = 128 << 20;
const CACHE_ENTRIES: usize = 32;
const CACHE_BYTES: usize = 64 << 20;

/// An image ready to send, and a line for the model when it was scaled.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub image: Image,
    pub note: Option<String>,
}

/// Prepare every image a tool brought in. Those that cannot be sent become lines saying
/// why, beside the scaling notes.
pub fn prepare_all(images: Vec<Image>) -> (Vec<Image>, Vec<String>) {
    let mut ready = Vec::with_capacity(images.len());
    let mut lines = Vec::new();
    for image in images {
        match prepare_image(&image) {
            Ok(Prepared { image, note }) => {
                ready.push(image);
                lines.extend(note);
            }
            Err(why) => lines.push(why),
        }
    }
    (ready, lines)
}

/// `image` as the model is sent it.
pub fn prepare_image(image: &Image) -> Result<Prepared, String> {
    decode_base64(&image.data)
        .ok_or_else(|| format!("[image {}: not valid base64]", image.mime))
        .and_then(|bytes| prepare(&bytes))
}

/// The image `bytes` hold, as the model is sent it: the bytes themselves when they fit
/// the budget, else scaled down and encoded again.
pub fn prepare(bytes: &[u8]) -> Result<Prepared, String> {
    let key: [u8; 32] = Sha256::digest(bytes).into();
    if let Some(found) = lock().get(&key) {
        return Ok(found);
    }
    let prepared = convert(bytes)?;
    lock().put(key, prepared.clone());
    Ok(prepared)
}

fn convert(bytes: &[u8]) -> Result<Prepared, String> {
    let format = image::guess_format(bytes)
        .map_err(|_| "[image: not a PNG, JPEG, GIF or WebP]".to_string())?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return Err("[image: not a PNG, JPEG, GIF or WebP]".to_string()),
    };
    let fail = |e: &dyn std::fmt::Display| format!("[image {mime}: could not be decoded: {e}]");
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| fail(&e))?;
    let (width, height) = (decoded.width(), decoded.height());
    let (to_width, to_height) = fit(width, height);
    if (to_width, to_height) == (width, height) && bytes.len() <= MAX_IMAGE_BYTES {
        let image = Image::new(mime, &crate::clipboard::base64(bytes))?;
        return Ok(Prepared { image, note: None });
    }
    let scaled = decoded.resize_exact(to_width, to_height, FilterType::CatmullRom);
    // A photo stays a JPEG; the rest become PNG, which keeps transparency.
    let mut out = Cursor::new(Vec::new());
    let sent = match format {
        ImageFormat::Jpeg => {
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
            scaled
                .to_rgb8()
                .write_with_encoder(encoder)
                .map(|()| "image/jpeg")
        }
        _ => scaled
            .write_to(&mut out, ImageFormat::Png)
            .map(|()| "image/png"),
    }
    .map_err(|e| format!("[image {mime}: could not be encoded: {e}]"))?;
    let out = out.into_inner();
    if out.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "[image {mime}: {} bytes once scaled, past the {MAX_IMAGE_BYTES} the model is sent]",
            out.len()
        ));
    }
    let image = Image::new(sent, &crate::clipboard::base64(&out))?;
    let note = match (to_width, to_height) == (width, height) {
        true => None,
        false => Some(format!(
            "[image scaled from {width}x{height} to {to_width}x{to_height}]"
        )),
    };
    Ok(Prepared { image, note })
}

/// The size `width` x `height` is sent at: within `MAX_SIDE` and `MAX_PIXELS`, in the
/// same proportions, never larger.
fn fit(width: u32, height: u32) -> (u32, u32) {
    let pixels = u64::from(width) * u64::from(height);
    let by_side = f64::from(MAX_SIDE) / f64::from(width.max(height).max(1));
    let by_pixels = (MAX_PIXELS as f64 / pixels.max(1) as f64).sqrt();
    let scale = by_side.min(by_pixels);
    if scale >= 1.0 {
        return (width, height);
    }
    let side = |n: u32| ((f64::from(n) * scale).floor() as u32).max(1);
    (side(width), side(height))
}

/// Standard or URL-safe base64, padded or not; `None` for anything else.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut held) = (0u32, 0u32);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace()) {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
        }
    }
    Some(out)
}

/// What the backend says when it refuses an image in the request.
pub fn refused(error: &anyhow::Error) -> bool {
    crate::client::bad_request(error) && format!("{error:#}").to_ascii_lowercase().contains("image")
}

/// Swap images in `history` for a line saying the backend refused them, so the request
/// can be sent again. Those after the model's last item go first, since every earlier
/// one was in a request it answered; with none there, every image goes. Returns how many
/// were taken out.
pub fn repair(history: &mut [Value]) -> usize {
    let after = history
        .iter()
        .rposition(written_by_model)
        .map_or(0, |at| at + 1);
    match replace(&mut history[after..]) {
        0 => replace(history),
        n => n,
    }
}

fn written_by_model(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => item.get("role").and_then(Value::as_str) == Some("assistant"),
        Some(
            "function_call" | "custom_tool_call" | "tool_search_call" | "reasoning" | "compaction",
        ) => true,
        _ => false,
    }
}

fn replace(items: &mut [Value]) -> usize {
    let mut replaced = 0;
    for item in items {
        for key in ["output", "content"] {
            let Some(parts) = item.get_mut(key).and_then(Value::as_array_mut) else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("input_image") {
                    continue;
                }
                let url = part.get("image_url").and_then(Value::as_str);
                let mime = url
                    .and_then(|u| u.strip_prefix("data:"))
                    .and_then(|u| u.split_once(';'))
                    .map_or("", |(mime, _)| mime);
                let text = format!("[image {mime} removed: the backend refused it]");
                *part = json!({"type": "input_text", "text": text});
                replaced += 1;
            }
        }
    }
    replaced
}

/// Prepared images, newest last, bounded by count and by base64 bytes.
struct Cache {
    entries: VecDeque<([u8; 32], Prepared)>,
    bytes: usize,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    entries: VecDeque::new(),
    bytes: 0,
});

fn lock() -> MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

impl Cache {
    fn get(&mut self, key: &[u8; 32]) -> Option<Prepared> {
        let at = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(at)?;
        let found = entry.1.clone();
        self.entries.push_back(entry);
        Some(found)
    }

    fn put(&mut self, key: [u8; 32], prepared: Prepared) {
        if self.entries.iter().any(|(k, _)| *k == key) {
            return;
        }
        self.bytes += prepared.image.data.len();
        self.entries.push_back((key, prepared));
        while self.entries.len() > CACHE_ENTRIES
            || (self.bytes > CACHE_BYTES && self.entries.len() > 1)
        {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.image.data.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};

    fn encoded(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200, 30, 30, 128]));
        encoded(DynamicImage::ImageRgba8(image), ImageFormat::Png)
    }

    fn decoded(image: &Image) -> DynamicImage {
        let bytes = decode_base64(&image.data).unwrap();
        image::load_from_memory(&bytes).unwrap()
    }

    #[test]
    fn the_budget_bounds_the_side_and_the_pixels() {
        assert_eq!(fit(800, 600), (800, 600));
        assert_eq!(fit(2048, 768), (2048, 768));
        // 4K is bound by its pixels, a long strip by its side.
        let (w, h) = fit(3840, 2160);
        assert!(u64::from(w) * u64::from(h) <= MAX_PIXELS, "{w}x{h}");
        assert_eq!((w, h), (1672, 940));
        assert_eq!(fit(10_000, 100), (2048, 20));
        assert_eq!(fit(100_000, 1), (2048, 1));
    }

    #[test]
    fn an_image_within_the_budget_goes_as_it_came() {
        let bytes = png(40, 30);
        let Prepared { image, note } = prepare(&bytes).unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.data, crate::clipboard::base64(&bytes));
        assert_eq!(note, None);
    }

    #[test]
    fn a_large_screenshot_is_scaled_and_says_so() {
        let bytes = png(3000, 1500);
        let Prepared { image, note } = prepare(&bytes).unwrap();
        assert_eq!(image.mime, "image/png");
        let sent = decoded(&image);
        assert_eq!((sent.width(), sent.height()), fit(3000, 1500));
        assert!(sent.color().has_alpha());
        assert_eq!(
            note.as_deref(),
            Some("[image scaled from 3000x1500 to 1773x886]")
        );
    }

    #[test]
    fn a_large_photo_stays_a_jpeg() {
        let image = RgbImage::from_pixel(2600, 500, Rgb([10, 120, 240]));
        let bytes = encoded(DynamicImage::ImageRgb8(image), ImageFormat::Jpeg);
        let Prepared { image, note } = prepare(&bytes).unwrap();
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(decoded(&image).width(), 2048);
        assert!(note.is_some());
    }

    #[test]
    fn the_same_bytes_give_the_same_image_and_hit_the_cache() {
        let bytes = png(2100, 900);
        let first = prepare(&bytes).unwrap();
        let key: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(lock().get(&key), Some(first.clone()));
        assert_eq!(prepare(&bytes).unwrap(), first);
        // Converted afresh, it encodes to the same bytes, so a cold cache keeps the prefix.
        assert_eq!(convert(&bytes).unwrap(), first);
    }

    #[test]
    fn the_cache_drops_the_oldest_past_its_bounds() {
        let entry = |n: u8, size: usize| {
            let image = Image {
                mime: "image/png".to_string(),
                data: "A".repeat(size),
            };
            ([n; 32], Prepared { image, note: None })
        };
        let mut cache = Cache {
            entries: VecDeque::new(),
            bytes: 0,
        };
        for n in 0..=CACHE_ENTRIES as u8 {
            let (key, prepared) = entry(n, 4);
            cache.put(key, prepared);
        }
        assert_eq!(cache.entries.len(), CACHE_ENTRIES);
        assert!(cache.get(&[0; 32]).is_none());
        // Read, the oldest is the newest again and outlives the next one in.
        assert!(cache.get(&[1; 32]).is_some());
        let (key, prepared) = entry(200, 4);
        cache.put(key, prepared);
        assert!(cache.get(&[1; 32]).is_some() && cache.get(&[2; 32]).is_none());

        let (key, prepared) = entry(201, CACHE_BYTES);
        cache.put(key, prepared);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, CACHE_BYTES);
    }

    #[test]
    fn a_decompression_bomb_is_refused_before_it_is_decoded() {
        // Under a kilobyte of PNG that would decode to 16384 x 16384 RGBA, a gigabyte.
        let bytes = png(1, 1);
        let mut header = bytes.clone();
        // IHDR width and height sit at bytes 16..24; the CRC after them goes stale, and
        // the decoder must refuse on the size before it gets there.
        header[16..20].copy_from_slice(&16_384u32.to_be_bytes());
        header[20..24].copy_from_slice(&16_384u32.to_be_bytes());
        let why = prepare(&header).unwrap_err();
        assert!(why.contains("could not be decoded"), "{why}");

        let mut wide = bytes;
        wide[16..20].copy_from_slice(&40_000u32.to_be_bytes());
        assert!(prepare(&wide).is_err());
    }

    #[test]
    fn what_is_not_an_image_or_not_base64_is_a_line() {
        let why = prepare(b"plain text").unwrap_err();
        assert!(why.contains("not a PNG"), "{why}");
        let bad = Image::new("image/png", "!!!!").unwrap();
        let good = Image::new("image/png", &crate::clipboard::base64(&png(2, 2))).unwrap();
        let (ready, lines) = prepare_all(vec![bad, good.clone()]);
        assert_eq!(ready, [good]);
        assert_eq!(lines, ["[image image/png: not valid base64]"]);
    }

    #[test]
    fn base64_reads_back_what_was_written() {
        for bytes in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foobar",
            &[0xff, 0xef, 0xbf],
        ] {
            let text = crate::clipboard::base64(bytes);
            assert_eq!(decode_base64(&text).as_deref(), Some(bytes), "{text}");
        }
        assert_eq!(decode_base64("Zm9v\nYg").unwrap(), b"foob");
        assert_eq!(decode_base64("_-8").unwrap(), [0xff, 0xef]);
        assert!(decode_base64("Zm9v*").is_none());
    }

    #[test]
    fn a_refused_image_is_the_unanswered_one_then_any() {
        let image = || Image::new("image/png", "iVBORw0K").unwrap();
        let call = |id: &str| json!({"type": "function_call", "call_id": id, "name": "view_image"});
        let mut history = vec![
            call("c1"),
            crate::tools::function_output("c1", "old", &[image()]),
            json!({"type": "message", "role": "assistant", "content": []}),
            call("c2"),
            crate::tools::function_output("c2", "new", &[image()]),
        ];
        assert_eq!(repair(&mut history), 1);
        assert_eq!(crate::tools::output_images(&history[1]["output"]), 1);
        assert_eq!(
            crate::tools::output_text(&history[4]["output"]),
            "new\n[image image/png removed: the backend refused it]"
        );
        // Nothing new is left, so the older one goes, and then there is nothing.
        assert_eq!(repair(&mut history), 1);
        assert_eq!(crate::tools::output_images(&history[1]["output"]), 0);
        assert_eq!(repair(&mut history), 0);
    }

    #[test]
    fn only_a_refusal_that_names_an_image_is_one() {
        let bad = |msg: &str| anyhow::Error::from(crate::client::BadRequest(msg.to_string()));
        assert!(refused(&bad("400 Bad Request: Invalid image.")));
        assert!(!refused(&bad("400 Bad Request: Invalid value")));
        assert!(!refused(&anyhow::anyhow!("image server down")));
    }
}
