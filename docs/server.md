# The debug server

`bhai --serve` binds a small HTTP server to `127.0.0.1:7878`, or the port given after the
flag, and exposes the running session. `--headless` runs it without the tui, for driving
a session from a script.

| endpoint | what it does |
| --- | --- |
| `GET /state` | the whole session: mode, transcript entries with their tokens, totals, queued prompts and the pending approval |
| `GET /events` | the same events the tui gets, as SSE |
| `GET /context` | the token profile as JSON |
| `POST /prompt` | `{"text": "..."}`; while a turn is running it answers `{"queued": <position>}` and the prompt starts when the queue reaches it |
| `POST /approve` | answer the pending approval; optional body `{"remember": "exact"}` or `"prefix"` |
| `POST /reject` | reject the pending approval |
| `POST /interrupt` | stop the running turn |
| `POST /mode` | `{"mode": "ask"}`, `"auto"` or `"bypass"` |

The tui and the server share one session, so either can answer an approval and exactly
one of them does.

## The local guard

Requests are refused unless the `Host` header is `127.0.0.1` or `localhost` and no
`Origin` header is present. Any web page could otherwise POST `/approve` to localhost,
and a rebound DNS name could drive the whole session. It stays bound to the loopback
address; there is no flag to expose it.
