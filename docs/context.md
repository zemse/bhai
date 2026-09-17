# Tokens and the cache

## Where the context goes

`/context` writes the whole profile to `.bhai/debug/context-<timestamp>.json` and `.md`
and prints the largest items in the transcript. The report breaks the request down by
category (system prompt, instructions, the skills, identities and mcp listings, each tool
schema, then every history item) and again item by item, with a per-call usage log, a
table of every transcript entry and its tokens, and one for the children that ran.

Each row takes its number from the best method available and says which it used: exact,
from the usage difference between consecutive calls; tokenized, with the model's own
tokenizer; or estimated at bytes/4, scaled by how far that estimate was off last call.

## Badges

Hovering an entry in the transcript shows its token badge: what the call read and wrote,
how often the entry has been resent, how much of it was cached, and what its children
spent. Click an entry to pin its badge, `ctrl+t` to show them all. The badges, `/context`
and `GET /state` all read the same attribution.

The status bar carries the session totals, the cache hit rate of the last call, and the
rate-limit headroom the backend reports in its response headers (`5h 42% · wk 17%`,
coloured by how close each window is to its limit, hover for the reset times).

## The cache guard

The prompt cache only holds if each request of a conversation is an append-only extension
of the one before it, so every request is checked against the last one before it is sent.
A break names the field that changed (the instructions, the tool list, an item that was
edited or dropped) and shows up in red in the status bar; the checks are logged to
`.bhai/debug/cache.jsonl`. A deliberate non-append-only change, compaction being the only
one, resets the guard instead of tripping it.

The guard only proves the request was well formed. What the backend actually served is
watched separately, and a call that came back with less cached than the last one left
shows `cache miss N%` in the status bar.

- `--strict-cache` refuses to send a request that would break the cache, rather than
  paying for the miss. Children inherit it
- `--cache-check` is a non-interactive check: it builds the same prefix a session would,
  pads it past the backend's cache minimum, sends a few calls on it and reports whether
  each was served from cache. It exits non-zero if not
- `--profile` logs the response headers of every call to `.bhai/debug/headers.jsonl`

The cache key is the session id, and `<session>-<identity>` for children, so children of
one identity share a prefix with each other and not with the parent.
