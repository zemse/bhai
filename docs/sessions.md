# Sessions and compaction

Every session appends to `.bhai/sessions/<id>.jsonl` as it runs: a header line with the
id, identity, model, effort, working directory and a fingerprint of the prompt prefix,
then each history item as the agent stores it. Nothing is rewritten, so a crash loses at
most the turn in flight.

`bhai sessions` lists them, newest first, each line starting with the `--resume <id>` that
continues it, then when it started, the identity, how many items and the opening prompt.

## Resuming

`bhai --resume` continues the newest session of this directory, `bhai --resume <id>`
one by its id or a unique prefix. The identity comes from the header, so `--resume` takes
no `--as`; if that identity no longer exists the session resumes as `general` with a
warning.

Leaving an interactive session that saved anything prints its id and the command to pick
it up, on a clean exit and after a panic. The bare `bhai --resume` is offered while it is
still the newest session of this directory, otherwise the id comes with it.

Resuming keeps the session's cache key, so the cached prefix is still there if it has not
expired. When the model, the instruction files or the tool list have changed since, the
fingerprint no longer matches and a warning says the prefix will differ.

## Compaction

A call that would read more than 80% of the context window compacts the history first.
Old tool outputs go first: they are replaced in place, newest six kept, which is cheap and
loses nothing the model has already acted on. Only when that is not enough are the earlier
turns replaced by a summary the model writes itself, keeping the first user message and
the last turn, aiming for 60% of the window.

`/compact` does it now, as a turn of its own. Compaction is deliberately not append-only,
so it resets the cache guard rather than tripping it. An interrupted turn is never
followed by automatic compaction, since the history at that point is the one the
interrupt left.

Both numbers are configurable:

```toml
context_window = 272000
compact_at = 0.8
```

Unset, the window is the model's own where bhai knows it (272k for the gpt-5 family).
