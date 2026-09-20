# Changelog

What has landed, newest first. Add an entry when you land something: one line per change,
grouped under the day. The commit message carries the reasoning and the verification; this
carries the shape of what is there.

## 2026-09-20

- Documentation moved here. `docs/` is gone and `CLAUDE.md` holds the orientation; the
  example workflow it shipped now lives in `examples/workflows/`.
- Subagent panes: every child of the running turn gets a row above the prompt, `ctrl+o` or
  a click goes inside one and `esc` leaves. A child's work lands in its own transcript
  rather than the parent's, which keeps the `agent` call and its result. Typing inside a
  pane posts to that child: the message joins its history before the next model call, and
  one typed while it is writing keeps it going rather than landing too late.
  `GET /children` and `POST /steer` do the same over the debug server.
- `/model` picks the model mid-session and the reasoning effort it takes, from the list
  each backend answers with. The switch lands between turns, resets the prompt cache and
  drops the previous model's encrypted reasoning.
- The working row says how fast the model is answering, over the last ten seconds it was
  actually streaming, so tool runs and approval waits are left out.
- The `/` menu's highlighted name completes grey in the prompt itself; tab fills it in.
- A session runs on any model: `--model ollama:<name>` routes to Ollama on this machine,
  anything else to the Codex backend. Responses items stay the internal format, so tools,
  subagents, workflows and sessions work either way.

## 2026-09-18

- An auto-approval judge decides the calls `auto` mode would have prompted for, with the
  turn's ledger as context, and `--judge-eval` scores it against a file of cases.
- The trust question on opening a project, which `auto` and `bypass` wait on. `auto` is
  the default mode.
- A `/` command menu in the prompt box, filtering as you type, skills listed with the
  commands. The working spinner moved from the top bar to just above the prompt.
- Prompts typed during a turn queue behind it; the arrows walk the prompt history.
- Copy and select: a drag over the transcript or the prompt marks a span and copies as it
  ends, `ctrl+y` copies, `ctrl+v` pastes.
- Permission hardening: a program hidden behind `find -exec` or a wrapper flag is refused,
  a `cd` into the project is followed when deciding a chain but not inside a pipeline.

## 2026-09-17

Most of the harness, ported from the prototype and built out over one day.

- **Agent loop and backends**: the turn loop, the Codex client, history compaction when
  the window fills, `--resume` from `.bhai/sessions/<id>.jsonl`.
- **Tools**: a registry with bash, read, write and edit; running commands stream their
  output into the transcript.
- **Permissions**: three modes, Claude Code rule syntax with `*` wildcards, a bash
  tokenizer, protected paths, approvals remembered in `.bhai/settings.local.json`, and
  repo-supplied allow rules honoured only once the project is trusted.
- **Identities**: `--as <name>` narrows the skills, tools, instructions and model a
  session carries, fixed for the session so the prompt cache holds.
- **Subagents and workflows**: the `agent` tool runs a child with a fresh context under
  any identity, with its own transcript and usage; workflows run several in dependency
  order under one token budget.
- **Skills**: `SKILL.md` discovery, the `skill` tool, `/skills`.
- **MCP**: stdio and streamable http servers, read from Claude Code's config too, with
  schemas kept out of the tool list behind `mcp_search` and `mcp_call`.
- **Token accounting**: exact per-call deltas and o200k_base counting, per-entry badges,
  the `/context` report and the `--profile` usage log.
- **Prompt cache guard**: every request checked for being an append-only extension of the
  one before it, with a runtime miss monitor, `--cache-check` and `--strict-cache`.
- **TUI**: markdown rendering, a `/diff` pane, mouse and a draggable scrollbar,
  collapsible tool output, multi-line input, rate-limit headroom in the status bar.
- **Debug server**: `--serve [port]` over localhost, `--headless` without the tui,
  refusing browser and foreign-host requests.

## 2026-08-26

- The working agent ported from the bhai-simple prototype.

## 2026-07-28

- Init: a minimal cargo binary for the agent harness.
