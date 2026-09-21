# Changelog

What has landed, newest first. Add an entry when you land something: one line per change,
grouped under the day. The commit message carries the reasoning and the verification; this
carries the shape of what is there.

## 2026-09-21

- A missing Ollama reads as one line in the `/model` picker: "no server at <url>. Start
  one with `ollama serve`.", rather than that sentence with reqwest's whole connect
  chain unrolled after it. A timeout says the same; any other failure still carries
  its own reason.
- The `/` menu completes a name wherever it is typed, not only at the start of the
  prompt: it opens on the `/word` at the cursor, tab writes the name in place and leaves
  the rest of the prompt alone. A `/` that starts no word, in a path, a url or a date,
  still keeps it shut, and mid-sentence enter sends the prompt rather than taking the
  highlighted row.
- The permission tokenizer reads a quoted heredoc as the data it is, so `python3 - <<'PY'`
  is the command `python3 -` rather than something unreadable, and reads `< file` too. An
  unquoted delimiter, a here string and `<>` still refuse the command. A denial in `auto`
  now says what kind of thing the checker would not pass, so the agent can write it
  plainer.
- The judge rules on a scratch file and on reading outside the project, which used to be
  held for the user: `/tmp` and the system temp directory are scratch, not the machine.
  A protected path, and a write anywhere else outside the project, still are the user's
  alone.
- The permission tokenizer reads a redirection instead of refusing the command it is on:
  `> f`, `>> f` and `2> f` are writes, checked where a write is checked. A redirect into
  the project is relaxed in `auto` like any write there; one into a protected path asks;
  one this parser cannot read as a plain filename still refuses the command.
- `--resume` comes back on the model the session was last on, since that is what its
  cached prefix and its encrypted reasoning belong to. A `/model` switch is recorded in
  the session file, so a session that changed model mid-way resumes where it ended;
  `--model` still wins over both.
- The judge asks again when its answer is not the agreed object, up to ten times and
  never past its timeout, since a model is sampled and the next answer may well parse. An
  error or a timeout is not asked again, and a model that never takes shape is put once
  for the rest of the session.
- `auto` mode never prompts: a call the judge cannot decide, or one it never sees, is
  denied with the reason rather than put to the user. The agent is told to ask in what it
  writes, or the user can switch to `ask` mode.
- `/clear` drops the conversation: the agent's history goes, the transcript goes with it
  and the session file records it, so a resume starts from nothing too.
- `/compact <prompt>` steers the summary: what the user asks for rides along with the
  request, so the summary keeps what the next turns need.
- The user's own words sit on a ground of their own, the width of the transcript, rather
  than in a colour of their own.
- Copying a selection undoes the wrapping: a line the terminal broke comes back on one
  line and only the newlines the text really has survive. The note a drag leaves lands
  beside where the drag ended, on a background of its own, rather than on the prompt's
  border.

## 2026-09-20

- The transcript draws a tool call as the tool it is. A shell command keeps the `$`, and
  a skill, read, write, edit or subagent call gets its own mark instead of looking like
  one. A long shell command folds to its first rows the way output does.
- Thinking comes down to the one line that says what it is thinking about, with the rows
  it holds back named on the end of it. A click opens it and `[collapse]` closes it, as
  it now does for tool output too.
- The tokens a second readout stops decaying while the model is quiet: the window ends at
  the last token rather than at now, so thinking time and retries no longer drag it to
  zero.
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
