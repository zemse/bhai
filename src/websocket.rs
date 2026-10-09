//! The Responses WebSocket: one socket per conversation, kept open between calls, on
//! which a call that extends the last one sends only the new items and names the last
//! response as its `previous_response_id`.
//!
//! The upgrade is `GET {base}/codex/responses` with `OpenAI-Beta: responses_websockets=
//! 2026-02-06`, as the Codex CLI sends it. Each request is one text frame holding the
//! HTTPS body plus `"type": "response.create"`, and each event comes back as one text
//! frame holding what the SSE `data:` line would. With `store: false` the backend keeps a
//! response only on the socket that made it, so a new socket starts with a full replay.

use serde_json::{Map, Value, json};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

pub(crate) const BETA: &str = "responses_websockets=2026-02-06";
/// The in-band code for a `previous_response_id` the socket no longer holds.
const STALE_CODE: &str = "previous_response_not_found";

/// One conversation's socket, if it has one open.
#[derive(Default)]
pub(crate) struct Socket {
    pub(crate) conn: Option<Conn>,
}

pub(crate) struct Conn {
    pub(crate) ws: WebSocketStream<reqwest::Upgraded>,
    /// What the backend holds for this socket's last completed response.
    pub(crate) last: Option<Last>,
}

/// A completed response, as the next request on the same socket can build on it.
pub(crate) struct Last {
    id: String,
    /// The request's fields other than `input`.
    rest: Map<String, Value>,
    /// The request's input followed by the response's output: what the backend has.
    input: Vec<Value>,
}

impl Last {
    /// `body` was answered with `output` as response `id`.
    pub(crate) fn after(id: String, body: &Value, output: &[Value]) -> Option<Self> {
        let mut rest = body.as_object()?.clone();
        let mut input = match rest.remove("input") {
            Some(Value::Array(input)) => input,
            _ => return None,
        };
        input.extend_from_slice(output);
        Some(Self { id, rest, input })
    }
}

/// The frame that sends `body`, and whether it is a delta on `last`. A delta needs every
/// field but `input` unchanged and the input to start with all `last` holds; anything
/// else, or nothing new after it, is sent whole.
pub(crate) fn frame(last: Option<&Last>, body: &Value) -> (String, bool) {
    let mut frame = body.clone();
    let tail = last.and_then(|last| {
        let fields = body.as_object()?;
        let input = fields.get("input")?.as_array()?;
        let same = fields.len() == last.rest.len() + 1
            && last.rest.iter().all(|(k, v)| fields.get(k) == Some(v));
        let extends = input.len() > last.input.len() && input.starts_with(&last.input);
        (same && extends).then(|| (last.id.clone(), input[last.input.len()..].to_vec()))
    });
    let delta = tail.is_some();
    if let Some(obj) = frame.as_object_mut() {
        obj.insert("type".to_string(), json!("response.create"));
        if let Some((id, tail)) = tail {
            obj.insert("previous_response_id".to_string(), json!(id));
            obj.insert("input".to_string(), Value::Array(tail));
        }
    }
    (frame.to_string(), delta)
}

/// Whether `event` says the socket no longer has the response a delta named.
pub(crate) fn stale(event: &Value) -> bool {
    let error = match event.get("type").and_then(Value::as_str) {
        Some("error") => event.get("error").unwrap_or(event),
        Some("response.failed") => event.pointer("/response/error").unwrap_or(&Value::Null),
        _ => return false,
    };
    ["code", "type"]
        .iter()
        .any(|key| error.get(key).and_then(Value::as_str) == Some(STALE_CODE))
}

/// Why a socket could not be opened.
pub(crate) enum Refusal {
    /// The backend answered but would not upgrade: no point asking again this session.
    Refused(String),
    /// The network or the backend failed this once; this call goes over HTTPS.
    Failed(String),
    Interrupted,
}

/// A fresh `Sec-WebSocket-Key` and the `Sec-WebSocket-Accept` that answers it.
pub(crate) fn key() -> (String, String) {
    let key = generate_key();
    let accept = derive_accept_key(key.as_bytes());
    (key, accept)
}

/// Finish the upgrade of `resp`, which must be a 101 carrying `accept`.
pub(crate) async fn open(resp: reqwest::Response, accept: &str) -> Result<Conn, Refusal> {
    let status = resp.status().as_u16();
    if status != 101 {
        let why = format!("{} on upgrade", resp.status());
        // A 4xx other than a rate limit or a timeout is the backend saying no.
        return Err(
            match (400..500).contains(&status) && status != 429 && status != 408 {
                true => Refusal::Refused(why),
                false => Refusal::Failed(why),
            },
        );
    }
    let answered = resp
        .headers()
        .get("sec-websocket-accept")
        .and_then(|v| v.to_str().ok());
    if answered != Some(accept) {
        return Err(Refusal::Refused("a bad Sec-WebSocket-Accept".to_string()));
    }
    let upgraded = resp
        .upgrade()
        .await
        .map_err(|e| Refusal::Failed(format!("upgrade failed: {e}")))?;
    let ws = WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await;
    Ok(Conn { ws, last: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say(text: &str) -> Value {
        json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
    }

    fn body(input: &[Value]) -> Value {
        crate::client::request_body("gpt-5.5", "medium", "k", "be brief", &[], input)
    }

    fn sent(frame: &str) -> Value {
        serde_json::from_str(frame).unwrap()
    }

    #[test]
    fn a_first_request_goes_whole_as_a_response_create() {
        let (frame, delta) = frame(None, &body(&[say("hi")]));
        let frame = sent(&frame);
        assert!(!delta);
        assert_eq!(frame["type"], "response.create");
        assert_eq!(frame["input"], json!([say("hi")]));
        assert!(frame.get("previous_response_id").is_none());
        assert_eq!(frame["prompt_cache_key"], "k");
    }

    #[test]
    fn a_request_that_extends_the_last_sends_only_what_is_new() {
        let first = body(&[say("hi")]);
        let answer = json!({ "type": "message", "role": "assistant", "content": [] });
        let last = Last::after("resp_1".to_string(), &first, std::slice::from_ref(&answer));
        let next = body(&[say("hi"), answer, say("more")]);
        let (frame, delta) = frame(last.as_ref(), &next);
        let frame = sent(&frame);
        assert!(delta);
        assert_eq!(frame["previous_response_id"], "resp_1");
        assert_eq!(frame["input"], json!([say("more")]));
        assert_eq!(frame["instructions"], "be brief");
    }

    #[test]
    fn user_timestamps_preserve_the_server_side_assistant_prefix() {
        let user = crate::tools::timestamp_message(say("hi"));
        let first = body(std::slice::from_ref(&user));
        let answer = json!({"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "hello"}
        ]});
        let last = Last::after("resp_1".to_string(), &first, std::slice::from_ref(&answer));
        let more = crate::tools::timestamp_message(say("more"));
        let next = body(&[user, crate::tools::timestamp_message(answer), more.clone()]);
        let (frame, delta) = frame(last.as_ref(), &next);
        let frame = sent(&frame);
        assert!(delta);
        assert_eq!(frame["previous_response_id"], "resp_1");
        assert_eq!(frame["input"], json!(crate::tools::timed_input(&[more])));
    }

    #[test]
    fn anything_but_an_append_is_replayed_whole() {
        let first = body(&[say("hi")]);
        let answer = json!({ "type": "message", "role": "assistant", "content": [] });
        let last = Last::after("resp_1".to_string(), &first, std::slice::from_ref(&answer));
        let whole = |next: &Value| {
            let (frame, delta) = frame(last.as_ref(), next);
            let frame = sent(&frame);
            !delta && frame.get("previous_response_id").is_none() && frame["input"] == next["input"]
        };
        // The output was not kept as it came.
        assert!(whole(&body(&[say("hi"), say("more")])));
        // Nothing new after the output.
        assert!(whole(&body(&[say("hi"), answer.clone()])));
        // A compaction rewrote the history.
        assert!(whole(&body(&[say("summary"), say("more")])));
        // The instructions changed.
        let mut changed = body(&[say("hi"), answer.clone(), say("more")]);
        changed["instructions"] = json!("be thorough");
        assert!(whole(&changed));
        // A field the last request did not have.
        let mut added = body(&[say("hi"), answer, say("more")]);
        added["service_tier"] = json!("priority");
        assert!(whole(&added));
    }

    #[test]
    fn a_missing_previous_response_is_recognised_in_either_shape() {
        let error = json!({ "type": "error", "error": { "code": STALE_CODE, "message": "gone" } });
        assert!(stale(&error));
        let flat = json!({ "type": "error", "code": STALE_CODE });
        assert!(stale(&flat));
        let failed =
            json!({ "type": "response.failed", "response": { "error": { "code": STALE_CODE } } });
        assert!(stale(&failed));
        let other = json!({ "type": "error", "error": { "code": "server_error" } });
        assert!(!stale(&other));
        let delta = json!({ "type": "response.output_text.delta", "delta": STALE_CODE });
        assert!(!stale(&delta));
    }

    #[test]
    fn the_accept_key_is_the_one_rfc_6455_derives() {
        assert_eq!(
            derive_accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        let (key, accept) = key();
        assert_eq!(derive_accept_key(key.as_bytes()), accept);
    }
}
