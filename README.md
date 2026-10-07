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

## goals

an explicit implementation task in chat can become a persistent goal without `/goal`. the agent adopts it with the `goal` tool and keeps opening turns until the objective is verified or genuinely blocked. follow-up requests steer the same objective. a reply resolving a blocker lets the agent resume it; unrelated questions do not. questions and analysis alone are not adopted as implementation tasks.

the saved specification holds only an objective, requirements and verification, limited to 1024 characters of text. the objective is at most 240 characters; each list has up to five items of 160 characters each. `goal` status `update` replaces it when follow-ups change the task, preserving unchanged constraints, state and progress. a developer context snapshot is appended only when the specification changes, kept verbatim through local and server compaction, and saved before the next model call. continuation turns use a short reminder instead of copying the specification again. completion is still the model's judgment, with instructions to check every requirement and cite verification results.

the plan belongs to the goal but stays outside its small specification. `update_plan` supports up to 500 steps, incremental `changes` by step name and `append` for newly discovered work. existing goal steps stay visible; unnecessary work is marked `skipped` with a reason instead of silently dropped. goal completion is refused while steps are pending or in progress, and resolved checkboxes still do not replace verification. specification updates, pause and resume preserve progress; replacing or clearing a goal resets it. saved sessions and forks carry the goal and its plan together.

the live panel follows the current work and retains finished goal progress. `/plan` shows every step without a model call. the model's plan context is limited to a 12-step window for large plans; `update_plan` without edits reads other steps using zero-based `offset` and `limit` (default 20, maximum 50). standalone analysis lists do not start a goal or modify a finished goal's progress.

`/goal <objective>` sets an objective explicitly and `/goal pause|resume|clear` controls it. goals continue until verified complete or genuinely blocked. interrupts, failed turns and three autonomous turns without work tool calls pause the goal rather than restarting indefinitely. user pauses and interrupts require `/goal resume`.

## openai and ollama support

inference runs on the codex cli's chatgpt-subscription credentials (`~/.codex/auth.json`), so log in with codex first, then run `bhai` in the directory you want to work in. or point it at a model on your own machine with `bhai --model ollama:<name>`.
