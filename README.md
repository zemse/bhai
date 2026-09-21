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


## openai and ollama support

inference runs on the codex cli's chatgpt-subscription credentials (`~/.codex/auth.json`), so log in with codex first, then run `bhai` in the directory you want to work in. or point it at a model on your own machine with `bhai --model ollama:<name>`.
