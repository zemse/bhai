**bhai** is an agent harness for running advanced long running workflows on systems.

> wip - this is a hobby project, try not to use it, until i remove this note

## feature set

- minimal tool set: bash, read, write, edit, web search.
- programable context compaction so very long sessions can sustain.
- customisable web search: multiple free and paid options pluggable.
- permission modes including explicit approval, bypass and auto mode.
- fuzzy multipass edits + openai's custom patch format.
- subagents with agent to agent support (only between parent child).
- prompt caching and optional compaction just before cache expiry.
- token accounting to clearly see what op consumed what.
- hooks for tool calls and skill invocations.
- live monitors with multiple progress tracks, custom metrics and rate-limited wake hooks.
- daemon for centralised session management.
- graph based memory.
- p2p remote over static IP or iroh relay.
- voice interface for hands free session management.
- server interface to programatically drive harness externally.
- x402 and crypto wallet, explorer integrations.
- secret hiding to prevent occurences in transcripts.

## use

```sh
$ cargo install bhai

$ bhai
```

<img src="https://raw.githubusercontent.com/zemse/bhai/main/assets/banner.png" alt="bhai running a task in the terminal" width="100%">


## monitors

for long-running work, the agent can proactively register a monitor. you approve its sampler command, working directory, timing and optional wake hooks once. the sampler runs every couple of seconds without model calls and prints one JSON snapshot, then exits:

```json
{
  "status": "running",
  "summary": "benchmarking matrix multiply",
  "tracks": [
    { "id": "newupdate", "current": 31, "total": 65 },
    { "id": "baseline", "current": 63, "total": 65 }
  ],
  "metrics": [{ "label": "RAM", "value": 2.1, "unit": "GB" }],
  "conditions": { "finished": false, "stalled": false }
}
```

live cards stay visible while the session works or sits idle. omit `total` for an unknown amount of work; optional `details` holds text lines. `done` or `failed` stops sampling and keeps the card. malformed output or timeouts keep the last valid snapshot marked stale, with retry backoff.

`/monitor` opens the background inspector; `/monitor <id>` opens one monitor. `/monitor pause|resume|stop|dismiss <id>` controls it. stopping the observer does not stop the task it watches. monitors last only for this session and do not restart when it is resumed. the debug server exposes the same live snapshots at `GET /monitors`.

optional hooks pair a named condition with a fixed approved prompt. they fire when the condition becomes true, optionally after `sustained_secs`, with `cooldown_secs` (default 60, minimum 30). `repeat` opts into repeated wakes while true; terminal snapshots fire each hook at most once. the session allows one monitor wake per 30 seconds and at most one queued monitor notification. routine samples never enter chat; a bounded snapshot reaches the agent when its next turn starts.

## openai and ollama support

inference runs on the codex cli's chatgpt-subscription credentials (`~/.codex/auth.json`), so log in with codex first, then run `bhai` in the directory you want to work in. or point it at a model on your own machine with `bhai --model ollama:<name>`.
