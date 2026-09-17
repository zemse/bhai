//! Token counting per model: OpenAI's o200k_base where the model uses it, bytes/4
//! for anything else.

use serde_json::Value;

/// Counts the tokens a piece of text costs a model.
pub trait Tokenizer: Send + Sync {
    fn count(&self, text: &str) -> usize;
    /// True when counts are a bytes/4 guess rather than a real tokenizer.
    fn estimates(&self) -> bool {
        false
    }
}

/// OpenAI's o200k_base, loaded once on first use.
pub struct O200k;

impl Tokenizer for O200k {
    fn count(&self, text: &str) -> usize {
        tiktoken_rs::o200k_base_singleton().count_ordinary(text)
    }
}

/// Bytes/4, for models without a known tokenizer.
pub struct ByteEstimate;

impl Tokenizer for ByteEstimate {
    fn count(&self, text: &str) -> usize {
        text.len().div_ceil(4)
    }

    fn estimates(&self) -> bool {
        true
    }
}

/// The tokenizer for `model`: o200k_base where tiktoken-rs maps the name there, or the
/// name is an unknown `gpt-*`, else bytes/4.
pub fn for_model(model: &str) -> &'static dyn Tokenizer {
    use tiktoken_rs::tokenizer::{Tokenizer as Known, get_tokenizer};
    match get_tokenizer(model) {
        Some(Known::O200kBase) => &O200k,
        None if model.starts_with("gpt-") => &O200k,
        _ => &ByteEstimate,
    }
}

/// The text of a history item that the model reads, or `None` for encrypted reasoning.
pub fn item_text(item: &Value) -> Option<String> {
    let field = |key: &str| item.get(key).and_then(Value::as_str);
    match field("type") {
        Some("reasoning") => None,
        Some("message") => Some(
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
        ),
        Some("function_call") => Some(format!(
            "{}{}",
            field("name").unwrap_or_default(),
            field("arguments").unwrap_or_default()
        )),
        Some("function_call_output") => Some(match item.get("output") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }),
        _ => Some(item.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn o200k_counts_fixed_strings() {
        assert_eq!(O200k.count(""), 0);
        assert_eq!(O200k.count("hello world"), 2);
        assert_eq!(O200k.count("fn main() { println!(\"hi\"); }"), 9);
        assert_eq!(O200k.count("नमस्ते दुनिया"), 5);
        assert!(!O200k.estimates());
    }

    #[test]
    fn byte_estimate_is_bytes_over_four() {
        assert_eq!(ByteEstimate.count("hello world"), 3);
        assert_eq!(ByteEstimate.count(""), 0);
        assert!(ByteEstimate.estimates());
    }

    #[test]
    fn models_pick_their_tokenizer() {
        assert!(!for_model("gpt-5.4").estimates());
        assert!(!for_model("gpt-9-unknown").estimates());
        assert!(!for_model("o3").estimates());
        assert!(for_model("fake").estimates());
        assert!(for_model("claude-opus").estimates());
        assert!(
            for_model("gpt-3.5-turbo").estimates(),
            "cl100k is not o200k"
        );
    }

    #[test]
    fn item_text_reads_what_the_model_reads() {
        let message = json!({"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "a"}, {"type": "input_text", "text": "b"}]});
        assert_eq!(item_text(&message).unwrap(), "ab");
        let call = json!({"type": "function_call", "name": "bash", "arguments": "{}"});
        assert_eq!(item_text(&call).unwrap(), "bash{}");
        let output = json!({"type": "function_call_output", "output": "ok"});
        assert_eq!(item_text(&output).unwrap(), "ok");
        let reasoning = json!({"type": "reasoning", "encrypted_content": "xyz"});
        assert_eq!(item_text(&reasoning), None);
    }
}
