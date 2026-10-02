//! A Chrome DevTools Protocol client over one websocket. A command goes out as a text
//! frame `{"id", "method", "params", "sessionId"}` and its answer comes back with the same
//! id, in any order; a frame without an id is an event. `sessionId` names the page a
//! command is for when the socket is the browser's, as with `flatten: true` attaching.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

const CLOSED: &str = "the browser closed the DevTools connection";

type Waiting = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

pub(crate) struct Cdp {
    out: mpsc::UnboundedSender<String>,
    /// `None` once the socket closed, so a call made after it fails rather than waits.
    pending: Arc<Mutex<Option<Waiting>>>,
    next: AtomicU64,
    tasks: [JoinHandle<()>; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    Event(Event),
}

/// The frame that sends `method` as command `id`.
pub(crate) fn command(id: u64, session: Option<&str>, method: &str, params: Value) -> String {
    let mut frame = json!({ "id": id, "method": method, "params": params });
    if let Some(session) = session {
        frame["sessionId"] = json!(session);
    }
    frame.to_string()
}

/// A frame from the browser, or `None` when it is neither an answer nor an event.
pub(crate) fn parse(text: &str) -> Option<Incoming> {
    let frame: Value = serde_json::from_str(text)
        .or_else(|_| serde_json::from_str(&without_lone_surrogates(text)))
        .ok()?;
    let session = frame
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(id) = frame.get("id").and_then(Value::as_u64) {
        let result = match frame.get("error") {
            Some(error) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_string)),
            None => Ok(frame.get("result").cloned().unwrap_or_else(|| json!({}))),
        };
        return Some(Incoming::Reply { id, result });
    }
    let method = frame.get("method")?.as_str()?.to_string();
    Some(Incoming::Event(Event {
        method,
        params: frame.get("params").cloned().unwrap_or_else(|| json!({})),
        session,
    }))
}

/// `text` with each `\uXXXX` escape of an unpaired surrogate made `�`. Chrome sends a
/// page's lone surrogate that way, and serde_json refuses the whole frame over it.
fn without_lone_surrogates(text: &str) -> String {
    fn unit(rest: &str) -> Option<u16> {
        let hex = rest.strip_prefix("\\u")?.get(..4)?;
        u16::from_str_radix(hex, 16).ok()
    }
    let high = |u: u16| (0xD800..0xDC00).contains(&u);
    let low = |u: u16| (0xDC00..0xE000).contains(&u);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match unit(rest) {
            Some(u) if high(u) && unit(&rest[6..]).is_some_and(low) => {
                out.push_str(&rest[..12]);
                rest = &rest[12..];
            }
            Some(u) if high(u) || low(u) => {
                out.push_str("\\ufffd");
                rest = &rest[6..];
            }
            _ => {
                let len = rest[1..].chars().next().map_or(1, |c| 1 + c.len_utf8());
                out.push_str(&rest[..len]);
                rest = &rest[len..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl Cdp {
    /// A client on `ws`, and the events the browser sends on it.
    pub(crate) fn start<S>(ws: WebSocketStream<S>) -> (Arc<Cdp>, mpsc::UnboundedReceiver<Event>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let (events, received) = mpsc::unbounded_channel();
        let pending = Arc::new(Mutex::new(Some(Waiting::new())));
        let writer = tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let waiting = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let text = match &message {
                    Message::Text(text) => text.as_str(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                match parse(text) {
                    Some(Incoming::Reply { id, result }) => {
                        let waiter = lock(&waiting).as_mut().and_then(|w| w.remove(&id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(result);
                        }
                    }
                    Some(Incoming::Event(event)) => {
                        let _ = events.send(event);
                    }
                    None => {}
                }
            }
            // Dropping the senders fails every call still waiting.
            lock(&waiting).take();
        });
        let cdp = Cdp {
            out,
            pending,
            next: AtomicU64::new(1),
            tasks: [writer, reader],
        };
        (Arc::new(cdp), received)
    }

    /// Send `method` and wait for its answer: the result, or the error the browser gave.
    pub(crate) async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match lock(&self.pending).as_mut() {
            Some(waiting) => waiting.insert(id, tx),
            None => return Err(CLOSED.to_string()),
        };
        if self.out.send(command(id, session, method, params)).is_err() {
            return Err(CLOSED.to_string());
        }
        match rx.await {
            Ok(result) => result.map_err(|e| format!("{method}: {e}")),
            Err(_) => Err(CLOSED.to_string()),
        }
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn lock(pending: &Mutex<Option<Waiting>>) -> std::sync::MutexGuard<'_, Option<Waiting>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn a_command_carries_its_id_method_params_and_session() {
        let frame: Value = serde_json::from_str(&command(
            7,
            Some("S1"),
            "Page.navigate",
            json!({"url": "u"}),
        ))
        .unwrap();
        assert_eq!(
            frame,
            json!({"id": 7, "method": "Page.navigate", "params": {"url": "u"}, "sessionId": "S1"})
        );
        let frame: Value = serde_json::from_str(&command(1, None, "X.y", json!({}))).unwrap();
        assert!(frame.get("sessionId").is_none(), "{frame}");
    }

    #[test]
    fn answers_errors_and_events_are_told_apart() {
        assert_eq!(
            parse(r#"{"id":3,"result":{"a":1}}"#),
            Some(Incoming::Reply {
                id: 3,
                result: Ok(json!({"a": 1}))
            })
        );
        assert_eq!(
            parse(r#"{"id":4,"error":{"code":-32601,"message":"no such method"}}"#),
            Some(Incoming::Reply {
                id: 4,
                result: Err("no such method".into())
            })
        );
        assert_eq!(
            parse(r#"{"method":"Page.loadEventFired","params":{"timestamp":1},"sessionId":"S"}"#),
            Some(Incoming::Event(Event {
                method: "Page.loadEventFired".into(),
                params: json!({"timestamp": 1}),
                session: Some("S".into()),
            }))
        );
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"neither":true}"#), None);
    }

    #[test]
    fn a_lone_surrogate_from_the_page_does_not_lose_the_answer() {
        let text = |frame: &str| match parse(frame) {
            Some(Incoming::Reply { id: 9, result }) => {
                result.unwrap()["v"].as_str().unwrap().to_string()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(text(r#"{"id":9,"result":{"v":"x\ud800y"}}"#), "x\u{fffd}y");
        assert_eq!(text(r#"{"id":9,"result":{"v":"x\uDC00"}}"#), "x\u{fffd}");
        assert_eq!(
            text(r#"{"id":9,"result":{"v":"\ud800\ud800"}}"#),
            "\u{fffd}\u{fffd}"
        );
        assert_eq!(text(r#"{"id":9,"result":{"v":"😀\ud800"}}"#), "😀\u{fffd}");
        assert_eq!(
            text(r#"{"id":9,"result":{"v":"\\ud800 \"q\" é\ud800"}}"#),
            "\\ud800 \"q\" é\u{fffd}"
        );
    }

    /// A browser end that answers each command with what `answer` makes of it, `None`
    /// meaning no answer, and sends `events` first.
    async fn fake(
        events: Vec<Value>,
        answer: fn(&Value) -> Option<Value>,
    ) -> (Arc<Cdp>, mpsc::UnboundedReceiver<Event>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            for event in events {
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            let mut held = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(text.as_str()).unwrap();
                if command["method"] == "Test.close" {
                    break;
                }
                // As Chrome escapes a page's lone surrogate, which no `Value` can hold.
                if command["method"] == "Test.surrogate" {
                    let frame = format!(r#"{{"id":{},"result":{{"v":"x\ud800"}}}}"#, command["id"]);
                    ws.send(Message::Text(frame.into())).await.unwrap();
                    continue;
                }
                match answer(&command) {
                    Some(reply) => held.push(reply),
                    None => continue,
                }
                // Answered newest first, so the client must match by id.
                if command["method"] == "Test.flush" {
                    for reply in held.drain(..).rev() {
                        ws.send(Message::Text(reply.to_string().into()))
                            .await
                            .unwrap();
                    }
                }
            }
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/devtools"), stream)
            .await
            .unwrap();
        Cdp::start(ws)
    }

    #[tokio::test]
    async fn answers_reach_their_callers_in_any_order_and_events_arrive() {
        let event = json!({"method": "Target.targetCreated", "params": {"id": 1}});
        let (cdp, mut events) = fake(vec![event], |command| {
            Some(match command["method"].as_str() {
                Some("Bad.call") => {
                    json!({"id": command["id"], "error": {"message": "wrong"}})
                }
                _ => json!({"id": command["id"], "result": {"echo": command["params"]}}),
            })
        })
        .await;
        let first = cdp.call(Some("S"), "Echo.one", json!({"n": 1}));
        let bad = cdp.call(None, "Bad.call", json!({}));
        let flush = cdp.call(None, "Test.flush", json!({"n": 3}));
        let (first, bad, flush) = tokio::join!(first, bad, flush);
        assert_eq!(first.unwrap(), json!({"echo": {"n": 1}}));
        assert_eq!(bad.unwrap_err(), "Bad.call: wrong");
        assert_eq!(flush.unwrap(), json!({"echo": {"n": 3}}));
        let event = events.recv().await.unwrap();
        assert_eq!(event.method, "Target.targetCreated");
        assert_eq!(event.params, json!({"id": 1}));
    }

    #[tokio::test]
    async fn an_answer_with_a_lone_surrogate_reaches_its_caller() {
        let (cdp, _events) = fake(Vec::new(), |_| None).await;
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            cdp.call(None, "Test.surrogate", json!({})),
        )
        .await
        .expect("the answer was dropped");
        assert_eq!(answer.unwrap(), json!({"v": "x\u{fffd}"}));
    }

    #[tokio::test]
    async fn a_closed_socket_fails_the_calls_waiting_and_later_ones() {
        let (cdp, _events) = fake(Vec::new(), |_| None).await;
        let waiting = cdp.call(None, "Never.answered", json!({}));
        let close = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = cdp.call(None, "Test.close", json!({})).await;
        };
        let (waiting, ()) = tokio::join!(waiting, close);
        assert_eq!(waiting.unwrap_err(), CLOSED);
        assert_eq!(
            cdp.call(None, "After.close", json!({})).await.unwrap_err(),
            CLOSED
        );
    }
}
