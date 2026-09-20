# Models and backends

Inference runs on one of two backends, and the model id picks which:

- anything else is the **Codex** backend, the Responses API behind the ChatGPT
  subscription, using the Codex CLI's credentials in `~/.codex/auth.json`.
- `ollama:<name>` is **Ollama** on this machine, e.g. `ollama:gemma4:e2b`. Name the model
  exactly as `ollama list` shows it.

## Picking one while it runs

`/model` opens a picker. It asks both backends what they will serve (the Codex backend its
own list, falling back to the one the Codex CLI cached in `~/.codex/models_cache.json`;
Ollama whatever `/api/tags` says is pulled) and shows them in one list, the running model
marked. A backend that cannot be reached is a note under the list rather than an empty
picker.

Picking a model that takes a reasoning effort asks for one next, listing only the efforts
**that model** supports, with its own default marked; `esc` there goes back to the models.
An Ollama model is switched to in one step: nothing in its request body carries an effort,
so there is no second question to ask.

`/model <name> [effort]` skips both lists, for a model already known by name. Either way
the switch is refused while a turn is running: it takes effect between turns, never
mid-call.

A switch costs the prompt cache, since a different model reads a different cached prefix,
and drops the thinking of the model before it, since encrypted reasoning cannot be replayed
to another model. Everything else, the conversation included, carries over. The context
window comes with the new model where its backend reports one, so compaction still knows
when to run; a `context_window` in `config.toml` still wins.

## Choosing one to start on

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
