<img src="https://raw.githubusercontent.com/zemse/bhai/main/assets/banner.png" alt="bhai running a task in the terminal" width="100%">

**bhai** is an agent harness for running advanced long running workflows on systems.

```sh
cargo install bhai
```

`bhai --serve [port]` also starts a localhost debug server (default 7878) with `/state`, `/events` (SSE), `/prompt`, `/approve` (optional body `{"remember": "exact"|"prefix"}`), `/reject` and `/interrupt`. add `--headless` to run it without the tui.

> wip - most of the features mentioned are not built yet. at this point this is just a hobby project.

## feature set

- minimal tool set: bash, read, write, edit, web search.
- programable context compaction so very long sessions can sustain.
- customisable web search: multiple free and paid options pluggable.
- permission modes including explicit approval, bypass and auto mode.
- fuzzy multipass edits + openai's custom patch format.
- subagents with agent to agent support (only between parent child).
- prompt caching and optional compaction just before cache expiry.
- hooks for tool calls and skill invocations.
- daemon for centralised session management.
- graph based memory.
- p2p remote over static IP or iroh relay.
- voice interface for hands free session management.
- x402 and crypto wallet, explorer integrations.
- secret hiding to prevent occurences in transcripts.
