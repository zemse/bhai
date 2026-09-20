# Models and backends

Inference runs on one of two backends, and the model id picks which:

- anything else is the **Codex** backend, the Responses API behind the ChatGPT
  subscription, using the Codex CLI's credentials in `~/.codex/auth.json`.
- `ollama:<name>` is **Ollama** on this machine, e.g. `ollama:gemma4:e2b`. Name the model
  exactly as `ollama list` shows it.

`/model` says which one a session is on. The model is fixed for the session, so the
cached prefix holds for the whole conversation.

## Choosing one

Highest precedence first:

1. `bhai --model <name>` and `bhai --effort <level>`
2. the identity's `model` and `effort` ([identities](identities.md))
3. `BHAI_MODEL` and `BHAI_EFFORT` in the environment
4. `model` and `effort` in `~/.config/bhai/config.toml`
5. `model` and `model_reasoning_effort` in `~/.codex/config.toml`
6. `gpt-5.5` at `medium`

```toml
model = "ollama:gemma4:e2b"
ollama_url = "http://localhost:11434"   # or BHAI_OLLAMA_URL
```

Those three keys are read from the global file only, never from a project's
`.bhai/config.toml`: where inference runs, and what it is sent to, is not a cloned repo's
decision.

Before the TUI opens, the backend is checked: Codex for credentials, Ollama for a server
that is up and a model that is pulled, so a typo in the name fails with the list of what
is installed instead of on the first call.

## What Ollama does not have

bhai speaks the Responses API's items everywhere, and the Ollama backend translates them
on the way out and back, so tools, subagents, workflows and sessions all work unchanged.
Three things do not survive the translation:

- **reasoning replay**. A thinking model's output is streamed into the transcript, but
  nothing is carried into the next request: Ollama has no encrypted reasoning to replay.
- **the prompt cache**. Ollama reuses a prefix without reporting how much it reused, so
  the cache guard, the hit monitor and `--cache-check` sit out and the status bar shows
  no hit rate. `--strict-cache` has nothing to refuse.
- **the context window**. It is not read off the model, so compaction only runs if
  `context_window` is set in `config.toml`.

Rate-limit headroom is a subscription's concern and is likewise absent.

## Mixing them

The judge runs wherever `judge_model` says, whichever backend the session itself is on,
so a local model can approve commands for a Codex session:

```toml
judge_model = "ollama:gemma4:e2b"
```

An identity's `model` works the same way, so a subagent can run locally while its parent
does not ([subagents](subagents.md)).
