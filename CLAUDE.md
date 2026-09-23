# bhai

An agent harness in Rust: a terminal UI over a loop that calls a model, runs the tools it
asks for, feeds the results back, and repeats. Inference runs on the Codex backend (the
ChatGPT subscription's credentials) or on Ollama, picked by the model id. Responses API
items are the internal format everywhere, so both backends carry the same history.

## Structure

`src/` is flat, one module per subsystem.

- **loop**: `agent.rs` (the turn, child agents), `client.rs` and `ollama.rs` (backends),
  `compact.rs`, `limits.rs`, `workflow.rs`
- **tui**: `app.rs` (state and keys), `ui.rs` (rendering), `input.rs`, `entries.rs`,
  `markdown.rs`, `wrap.rs`, `diff.rs`, `commands.rs`, `models.rs` (the `/model` picker),
  `branch.rs` (the branch on the status bar)
- **session**: `session.rs` (the hub every consumer reads), `sessions.rs` (on disk),
  `server.rs` (the debug server)
- **context**: `prompt.rs`, `instructions.rs`, `identity.rs`, `skills.rs`, `config.rs`,
  `profile.rs`, `tokens.rs`, `cache.rs`
- **tools**: `tools/` one file per tool, `mcp/`, `permissions/` (rules, settings, trust),
  `judge.rs` (auto-approval)

`AgentEvent` (agent.rs) is what the loop emits; `session::Event` is what consumers see.
Both are matched exhaustively in several places on purpose, so a new variant forces a
decision everywhere it matters.

## Conventions

- Record what lands in `CHANGELOG.md`, newest first. The commit message carries the
  reasoning; the changelog carries the shape.
- `TASKS.local.md` is the backlog, gitignored. Not `TASKS.md`.
- Tests are offline. bhai runs on the Codex subscription, so a live model call costs real
  quota: use `--model ollama:<name>` to try something end to end, and `--serve --headless`
  to drive a session over HTTP without a terminal.
- Match the surrounding style: comments state the non-obvious fact and stop.
