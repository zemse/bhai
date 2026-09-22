# Changelog

What has landed, newest first. Add an entry when you land something: one line per change,
grouped under the day. The commit message carries the reasoning and the verification; this
carries the shape of what is there.

## 2026-09-22

- `--judge-eval` scores the shapes the judge was getting wrong. Every case it shipped
  with was a single message stating the whole task, so it read 29 of 29 while real
  sessions were denying work the user had asked for one message earlier. Ten cases now
  cover a follow-up that states no goal on its own, and a command the tokenizer cannot
  take apart, in both directions. A bash case with no detail of its own is marked
  unreadable exactly as a session would mark it, so the eval and the approval path
  cannot drift.

- A shell command the permission tokenizer cannot take apart now goes to the judge
  instead of being denied outright. The tokenizer refuses anything holding a variable,
  a command substitution or a loop, and in `auto`, which never prompts, that refusal was
  the end of it: `cat $HOME/.cargo/config.toml`, `cd $(git rev-parse --show-toplevel) &&
  cargo build` and a `for` loop were all dead ends, while auto mode is the mode that
  does its work through the shell. The judge is told the command is one it has to read
  as written. What still never reaches it is what can be read off the text whatever
  shape it is in: `sudo`, `eval`, a shell running its argument, a protected path, or a
  glob that may reach one.

- A call `auto` mode denies now says which of the five things happened, and a turn's
  budget of judged calls went from 20 to 200. Since a turn runs as long as the task
  takes, a long one used up the 20 and then had every remaining call denied, each with
  "the judge could not decide it", which is what an undecided call says too. The budget
  is a bound on what a confused loop can cost, so it now sits past any real turn, and
  when it does run out the agent is told that is what happened and that the user's next
  message refills it.

- The judge is told what the user asked before this message, not only the message it is
  judging against. A goal is stated once and then referred to: "now do the same for the
  other file", or a question about which tool to use, says nothing on its own, and a
  judge reading the latest sentence as the whole task denied the step the earlier
  message had asked for. The last three messages now sit beside the task, where a fold
  of the ledger cannot take them away.

- A denied call is no longer denied for the rest of the session. The judge remembered
  one verdict per call, so once something was refused, saying "yes, run that" got the
  same refusal back with the old reason and the model was never asked again. A verdict
  is now remembered against the task it was given under, so the next message judges the
  call afresh. The detail is part of that too: two different edits to one file used to
  share one verdict, and the first one decided both.

## 2026-09-21

- A message from the model now carries a `⏺` in the left margin, and the whole of it
  sits in from that mark. Every other entry already had a sigil of its own, so a message
  was the one thing in the transcript with nothing to say where it started: after a wall
  of tool output it read as more output. Thinking keeps its `✱`.

- Interrupting a turn no longer throws away the prompts queued behind it. Typing a
  correction while a turn goes wrong and then stopping that turn used to drop the
  correction with it, which is the one keypress guaranteed to be followed by wanting
  it. The interrupt cancels the call in flight; the front of the queue starts as soon
  as that turn ends, so one interrupt stops one turn. `/queue clear` still drops what
  is waiting.

- A turn no longer stops after 40 model calls. The cap was there so a confused loop
  could not run forever, but a long task hits it while it is still working and gets
  `stopped after 40 steps without finishing` instead of an answer. The turn now runs
  until the model stops calling tools. What still ends a runaway: three consecutive
  rounds where every tool call failed, an interrupt, and the backend's rate limits.

- The `tok/s` readout says what the text on screen is doing. It is measured from one
  token to another rather than from the start of the model call, so the seconds spent
  thinking before the first token are no longer in the denominator with nothing
  against them: a stream at a flat 28 tok/s used to read 1, climb for ten seconds and
  only then say 28. It now says 28 from its first reading, about half a second after
  the model starts writing, and says nothing before that rather than a number it
  would have to take back.
  Reasoning a call reports but never streamed is no longer added in: it was never on
  screen, and it only went in to offset the same dead time.
- A prompt typed while a turn runs no longer appears in the transcript at the moment
  it is typed, which put it before the rest of that turn's answer while the model saw
  it after. It waits in a `queued` panel above the prompt box and joins the transcript
  when its own turn starts, in the place the model's history puts it. `/queue` and
  `/queue clear` are unchanged.
- A fenced code block is coloured by the language its fence names: comments grey,
  strings green, numbers yellow, keywords magenta, upper case names cyan, the rest a
  plain grey. A fence that names no language, or one this does not know, stays plain
  rather than being guessed at and painted in another language's rules. Rust, Python,
  JavaScript and TypeScript, Go, the C family, Ruby, shell, JSON, TOML, YAML, SQL and
  CSS are known.
- The transcript scrollbar is drawn in greys rather than the terminal's brightest
  white, with a thin thumb against the screen edge in place of the solid block, on a
  single faint rule in place of the double one.
- The subagent panel goes once the turn is over, rather than sitting between the
  transcript and the prompt until the next message. Opening a pane brings it back,
  since the panel is what that pane is titled by and read from.
- The open pane's row says how to leave it: a `✕ close` at its right end, and a
  hint that reads `ctrl+o next · esc close`. Clicking the row already closed the
  pane, but nothing on screen said so.
- A turn that ends on a failure says so as a failure rather than an error, and the
  transcript offers to run it again: clicking it re-sends the call on the history
  the agent still holds, so a request that broke on the wire no longer has to be
  restarted by typing something. The offer stands only while the failure is the
  last thing said and nothing is running. `POST /retry` does the same over the
  debug server.
- A token badge is drawn on the blank row under its entry rather than over the last
  row of the text, so hovering a message no longer covers the words being read.
  Nothing reflows: the row was already there, between the entry and the next one.
  An entry the bottom edge cuts off has no such row in view, and keeps the badge
  on its last row.
- Startup says the same when a session is put on an `ollama:` model with nothing
  listening: the preflight sentence alone, not that sentence with reqwest's own
  wording for a refused connection parenthesised inside it.
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
