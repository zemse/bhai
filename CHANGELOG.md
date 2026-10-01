# Changelog

What has landed, newest first. Add an entry when you land something: one line per change,
grouped under the day. The commit message carries the reasoning and the verification; this
carries the shape of what is there.

## 2026-10-01

- A failed model call is classified by its error `code`/`type` before its HTTP status: `context_length_exceeded`, `insufficient_quota`, `usage_not_included`, `usage_limit_reached` (even as a 429) and the policy codes fail at once instead of being sent twice more. Known overload codes still retry. A retry now waits the server's `Retry-After` (or `retry-after-ms`) when it is 30s or less, and otherwise fails naming the wait. Its own backoff is jittered to 80-120%, and every retry shows an info line `retrying (2/3) in 0.5s: <reason>`.
- Bash, read and MCP output has the values bhai knows are secret (withheld environment values, the Codex tokens, MCP header values) replaced with `[REDACTED]` before it enters history, before any truncation, so a value straddling the cut leaves no piece behind. Base64 and other encodings still get past it.
- A reply with no tool call no longer ends the turn when the backend says `end_turn: false`: the model is sampled again, up to 8 times in a row. Commentary-phase text gets its own entry, drawn dimmer than the answer and kept on resume.
- A stdio MCP server runs as the leader of its own process group, and the whole group is killed on shutdown or when its start fails. The real server that `npx`, `uvx` or `docker run` starts underneath is no longer left running.
- `/goal <objective>` (or `POST /goal`) gives the session a goal to work on by itself. Whenever it is idle and the goal is active, it opens a turn on it, until the model marks the goal complete or blocked with its `goal` tool, the user runs `/goal pause|clear`, or the token budget is used up (default 200k, set with `/goal budget <n>`, accepts 50k or 1m). The budget counts uncached input plus output, children included, and is hard: a turn the harness opened stops before its next call once the budget is spent. The model cannot resume a goal or change its budget. An interrupt, a failed turn or a goal turn that cost nothing pauses the goal. It is saved in the session file and comes back paused on `--resume`. Spent/budget is shown on the status bar (`goal 10.5k/50.0k`) and in `/state`.
- The `agent` tool takes `continue: <id>` to give a finished child more work. The child picks up from its saved history in `child-<id>.jsonl` under the same id, identity and model, and keeps its pane and profiler row. A child that is still running, an unknown id, or a `continue` that also gives `identity`/`model`/`effort` is refused.
- A workflow step's `submit_result` tool is declared strict to the Codex backend when its `output_contract` is closed (`additionalProperties: false`), lists every property as required and has no `minItems`/`maxItems`, so the model is held to the schema while it samples. Any other contract is sent non-strict and is checked only by `submit_result`, which is also the check on Ollama.
- A workflow step can declare `output_contract` (a JSON Schema with an object at the top; `output: json` is implied). Its child is the only one given a `submit_result` tool. Each call is checked against the schema, and a refused call gets back up to 10 short paths without the values, so the child can fix it in the same turn. Plain JSON in the answer is never taken: with no accepted call the step fails. A message typed into the child after an accepted result cancels that result, and every string in the result is neutralised. A contract keyword the checker does not enforce is a load error.
- A message to a running subagent is limited to 2048 bytes: typing a longer one into its pane shows an error instead of sending it, and `POST /steer` answers 413 with the limit. Nothing over it reaches the child's mailbox.
- Bash takes an optional `workdir`, so the model passes the directory instead of writing `cd dir && ...`: the approval line reads `command  (in dir)`, the judge is told "runs in dir", and the rules see the bare command, so `Bash(cargo test:*)` matches. A workdir inside the project is followed the way a leading `cd` is; one outside it asks, and one that is not a directory is refused before approval.
- A bash result ends with a trailer line `[output: N lines, X.Ys]` giving the lines the command printed in all (trimmed ones included) and its wall time, after the output so the first-line exit status that the transcript and `outcome()` read is unchanged; a timed-out command gets it too.
- Bash commands run with a fixed non-interactive environment (`TERM=dumb`, `NO_COLOR=1`, `PAGER=cat`, `GIT_PAGER=cat`, `GIT_TERMINAL_PROMPT=0` and a UTF-8 `LANG`/`LC_ALL`), set after the credential scrub, so a pager or a git credential prompt can no longer hang a call and output carries no colour codes. Stdio MCP servers keep the inherited environment.
- One running subagent can be stopped without stopping the turn or its siblings: ctrl+x inside its pane (the pane hint now says so), `POST /children/<id>/interrupt` (404 when no such child is running), or the model's `close_agent` tool, which comes with `agent`. A stopped child still reports, and its pane notes "stopped by you". Workflow steps share their run's flag, so they cannot be stopped one at a time.
- The session file is fsynced when a turn ends (and after an explicit compaction), not only flushed to the OS, so a power loss no longer drops the tail of a turn that finished on screen. A failed sync is reported as a transcript error, and resume still drops an unanswered call.
- A `--resume` of a session that ended while a child agent was running appends a report for that child saying it did not finish and was not restarted, in the history and in the session file, so the model no longer waits on a report that cannot come. A child that did report is left alone, and a second resume adds nothing.
- A prompt starting with `!` runs that command in the shell yourself and shows the result as a note: it goes through the bash permission check (only a deny rule or a refusing mode stops it, since typing it is the approval), is written to `permissions.jsonl` with `by` set to `you`, and never reaches the model's history.
- An edit or write approval shows the diff it would make, folded under the call in the transcript. The preview reads only a regular file of 1 MiB or less, so a path like /dev/zero or a fifo cannot hang the prompt.
- `tests/fake_backend.rs` runs the real binary with `--serve --headless` against a local fake Responses backend and drives it over HTTP: a text answer, an approved bash call and a rejected one, checking the requests bhai sends, what `/events` and `/state` report, and the files on disk, with no quota spent. `BHAI_TEST_BASE_URL` points the ChatGPT backend root (responses, models, usage) at the fake, and only in a debug build.
- Bash and stdio MCP children no longer inherit variables whose name has a KEY, TOKEN, SECRET, PASS, PASSWORD, AUTH, COOKIE or CREDENTIAL part (split on `_`), so `env` no longer prints bhai's tokens into the transcript. `[bash] pass_env = ["NAME", ...]` in the global config lets chosen names through; a login profile can still export a secret again, and a server's own `env` in the config is still set.
- `/events` frames carry the session's event `seq` as their SSE id (1 for the first event of the run). The newest 4096 events are kept, and a `Last-Event-ID` header replays the ones after it before going live: `0` replays all that are kept, a reader further back gets `{"type":"lagged","data":n}` first, a non-number is a 400 and an id past the newest (from another run) is a 409. `/state` has a `seq` field, so a snapshot followed by `/events` from that id misses nothing.
- A test runs /export-debug in a child process with a fake auth.json (via CODEX_HOME) and a secret env var, and checks that neither credential reaches the written file.
- `POST /approve` and `POST /reject` take an optional `id` (the approval's id from `/state` or `/events`); an approval other than that one is left unanswered and the request is a 409. Every `/events` frame carries an SSE `id:` counting the events since the connection opened, those a lagging consumer missed included, so a jump in it is a gap.
- `BHAI_MODE=ask|auto` sets the permission mode for a run where `--mode` is not given, over the config file's `permission_mode`; `--mode` still wins. `BHAI_MODE=bypass` is refused with an error, so the environment cannot turn the safeties off. `BHAI_MODEL` and `BHAI_EFFORT` already worked the same way.
- Esc while busy asks "esc again to interrupt" and only interrupts on a second press within a second; ctrl+c still interrupts at once.
- `bhai exec <prompt>` runs one prompt through the same session as the TUI with no terminal and no port, on an unattended policy so a call the rules leave at Ask is refused (pass `--mode auto` to let tools run). It prints the model's text on stdout, or with `--json` every `session::Event` as one JSON line (the shape of `/events`), and exits 1 when the turn fails or is interrupted. `-` as the prompt reads it from stdin.
- ctrl+c with text in the prompt clears it instead of quitting, and ctrl+z puts it back; on an empty prompt the first ctrl+c says "press again to quit" and a second within a second quits. ctrl+d on an empty prompt asks the same way while a turn or a child agent is running.
- `permissions.jsonl`, `judge.jsonl` and the `BHAI_DEBUG_SSE` file are created 0600, and the first two's `debug` directory 0700, through the same `private_*` helpers as the other logs; they used to land 0644 under the default umask.
- `POST /prompt` takes an optional `id`. A retry with an id the server has already accepted, and the same text, gets the first answer back (`{"ok": true}` or the queue position) and submits nothing; the same id with different text is a 409. Failed submits are not remembered, and the ids live in memory for the run of the server.
- A long approval no longer cuts its command off silently: the box shows `[+N lines hidden]` and scrolls with up/down, j/k, PageUp/PageDown and Home/End. `y`, `a` and `p` (and clicks on them) do nothing until the last line of the command has been on screen; `n`, `r` and esc always answer.
- `Auth` no longer derives `Debug`: its impl prints the access token as `<redacted>` and keeps the account id, so a `{:?}` of an `Auth` or a struct holding one cannot leak the bearer token.
- `.config/nextest.toml` defines a `ci` profile for `cargo nextest run --profile ci` that does not stop at the first failure and kills a test still running after five minutes (60s slow-timeout, terminate after 5 periods); CI itself still runs `cargo test`.
- A 401 from the Codex backend re-reads `auth.json` and takes the token there if another process rotated it, otherwise forces one refresh, then retries the call once. A token revoked before its `exp` no longer kills the turn, and a second 401 still fails with "Run `codex login`".
- `.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` on macOS and Linux for every push to main and every pull request, and `cargo deny check advisories` alongside. Actions are pinned by commit SHA and checkout does not persist credentials.
- `deny.toml` sets the dependency policy for `cargo deny check`: yanked crates and wildcard versions are denied, only listed licences and the crates.io registry are allowed, and RUSTSEC-2025-0141 (bincode via syntect) is ignored with its reason.
- A token refresh that the server refuses, because the `codex` CLI or another bhai spent the same refresh token first, adopts the tokens that process wrote to `~/.codex/auth.json` instead of failing the turn. A `refresh_token_reused` refusal with nothing newer on disk says the token was already spent.
- The release binary is stripped and built with thin LTO, so it is about 12% smaller
  (28.2 MB to 24.7 MB). The crate now denies unsafe code except for the one `killpg`
  call, and clippy warns on redundant clones.
- A token refresh writes `~/.codex/auth.json` readable only by the user: the temp file is
  created 0600 and synced before it is renamed over the old one, where it used to land
  0644.
- `/compact` sent while a turn runs queues instead of failing with `a turn is already
  running`, and runs once that turn ends. Prompts queued ahead of it still join the
  running turn; those queued after it wait for the compaction. `/events` carries its
  start as `compacting`.
- `bypass` no longer prints an `auto-allowed: <call> (bypass mode)` line under every
  call it lets through. `ask` and `auto` still say which rule allowed one.
- A selection over an assistant message copies the markdown that drew it: a heading
  with its `#`s, a list item with its bullet, part of a bold run or a link as the whole
  of it. A table cut across rows, or a code block a selection runs out of, comes whole,
  fences and pipes included; a selection inside one code block is just the code. Every
  code block has a `[copy]` label above it, beside the language, that a click copies the
  code by. Table columns are measured in screen columns, so a CJK or emoji cell keeps
  them lined up.
- A transcript copy leaves out what the renderer drew in front of the text: the `⏺ `
  and `› ` marks, the indent under them, a code block's indent and a wrapped list
  item's. A word the wrap split mid-word, a key say, comes back whole.
- Every turn ends with a dim `✻ 9m 54s chabāyā · 12:58 PM khatam` line in the
  transcript: how long it ran from start to its end, and the local time it ended. Each
  turn picks one of 52 Hinglish pairs, Delhi and Mumbai slang among them: the working
  row says `chabārau...` while it runs and the done line `chabāyā` once it ends. A queued prompt that
  follows is timed as its own turn. `/events` carries it as `done`.
- An idle conversation whose cache is about to lapse gets a compacted copy, summarised
  three minutes before the cache's expected 30 while the history still reads from it.
  A normal message carries on with the full history; `/compact-then <prompt>` runs the
  prompt on the copy instead. While the prompt starts with `/compact-then`, the status
  bar's context fill is the copy's, with `fork uncached: ~Nk tokens` in place of the
  cache timer; the `fork` statusline variable says the same. Only on Codex, and only
  past 16k tokens.
- A message typed while a turn runs joins that turn before its next model call, after
  the tool results or the answer it was typed during, instead of waiting for the turn
  to end. Everything waiting goes in together, each as its own message, and shows in
  the transcript where the model reads it. An interrupt still keeps the queue.
- Up on an empty prompt takes the queued messages back into the prompt box to edit,
  one per line.
- `/model-default` and `/effort-default` save the model and effort new sessions start
  on to `~/.config/bhai/config.toml`, leaving the running session where it is.
  `/model-default` opens the `/model` picker on the current default; with a name it
  saves without asking.

## 2026-09-30

- The subagent panel stays up while any child is still running, not only while the
  turn that started it is. Children run detached, so the panel used to go at the end
  of the turn and hide the ones still working.
- The rate-limit windows and the credits refresh every minute, idle or not, instead of
  only alongside a model call at most every five minutes.
- The status bar's context fill drops as soon as history is compacted, by an estimate
  of what the compaction took out, rather than holding the old number until the next
  call. `/clear` blanks it. The `compacted` event on `/events` carries the estimate as
  `freed`.

## 2026-09-29

- The running token totals and the cache hit rate are off the status bar. They were
  the two segments a narrow terminal dropped first, and `$tokens_in`, `$tokens_out`
  and `$cache` still carry them for a status line template.
- The prompt cache is assumed to last 30 minutes from when a call was sent, not 5 from
  when it finished, and the status bar counts down its last five: `cache expires in
  4:07`, then the `cache expired` hint as before. `$cache_timer` shows the same for a
  status line template. An effort change that would re-read the conversation uncached
  now waits out the longer lifetime too.
- `bhai --cache-check [minutes]` makes one more call that long after the others and
  judges it even past the cache's assumed lifetime, so how long the Codex backend really
  keeps a prefix can be measured instead of assumed.
- Codex credits sit next to the rate-limit windows: `credits 8.3k/10.0k` on the status
  bar, coloured by how much of the allowance is spent, and `$credits`, `$credits_left`,
  `$credits_used`, `$credits_limit` and `$credits_reset` for a status line template. The
  balance is read from `/wham/usage`, a Team seat's under `spend_control` and other
  plans' from `credits.balance`, once at startup and then alongside a model call at most
  every five minutes. A plan without credits shows nothing. `/usage` and `bhai usage`
  print the plan, each window and the credits in full.
- A workflow step can name its own `model` and `effort`, so an identity's model is a
  default there too rather than a ceiling. The plan the confirmation shows and the final
  report both name the model a step runs on when it is not the session's. A bad `effort`
  is a load error; a model no backend serves is named before the run starts, so a typo
  costs nothing instead of failing at the first step.
- A workflow step can fan out: `for_each: <step id>` runs it once per item of that
  step's answer, with `{{item}}` in the prompt, which is the one shape the markdown
  could not express before. The items are the lines of that answer, or its JSON array
  when it is one, so a step with nothing to list says `[]` and the fan-out runs nothing
  rather than blocking what needs it. A bullet or a number in front of an item is
  dropped. The step that reduces them reads every output under the item it came from,
  one failed item does not take the others down under `on_fail: continue`, and the
  report says how many of the items ran.

- A workflow step can say what it answers with and branch on it. `output: json` asks
  the child for one JSON object and holds it to that, so an answer of another shape is
  a failed step rather than a value the steps after it have to guess at; nothing of it
  is cached either. `{{steps.<id>.<field>}}` reads a top-level field of that object, and
  `when: {{steps.<id>.<field>}} == value` (or `!=`) runs a step only when it holds. A
  gate that does not hold skips its own step and nothing else; a field the object does
  not have skips it too, and says which field was missing rather than reading as false.

- What a workflow step answered is neutralised on the way into the next step's prompt,
  the way a child's report already was on the way into its parent's conversation. It is
  the same untrusted text: a step that read a file wrote it. A `when` still compares
  what the step answered rather than the neutralised copy.

- A `--workflow` run tells a child that a refused call is refused for good. Nothing is
  attached to such a run that can answer an approval, so the usual "ask what they want
  instead" is advice it cannot take, and a model spends its steps rephrasing the same
  call into the same no.

- `budget_tokens` stops a chunk it would not cover, rather than only a chunk that has
  already passed it. A launch costs nothing measurable until it has run, so the old
  check could only ever notice the budget after it was spent: a 20k budget spent 25.7k
  in a live run, and three children of a fan-out can cost more than the whole budget
  between one check and the next. A chunk is now weighed against the most expensive
  launch so far, which keeps a run under the number the file asked for at the price of
  stopping with some of it unspent.

- A fan-out instance is named by what tells it apart. A long item is cut in the middle
  rather than at the end, so four paths under one directory no longer read as four of
  the same label, and each instance's answer carries its item at the head: instances
  finish in whatever order they finish, and the tool event pair carries no id, so the
  answers were arriving under one another's headers.

- `examples/agents/worker.md`: a lean identity for a workflow step that only runs a
  command. A step on `general` carries the instruction files and the skill list, which
  measured 8.6k input tokens a call against 750 for the same step as `worker`, and a
  fan-out pays that once per item.

- `max_fanout` sets how many instances one `for_each` step may run, default 20 and
  clamped to 100. `/workflows` says the cap for a definition that fans out.

- A `/model` switch no longer doubles how many child agents can run at once. The cap
  was rebuilt with the rest of the tool registry, so children still running from before
  the switch held slots nobody was counting and a fan-out could reach six.

- The `agent` tool refuses an unknown `model` or `effort` before it spawns the child. The
  catalogue behind that check is loaded at most once a minute and shared with `/model`
  and the `models` tool, so a fan-out pays for one list rather than one each. A backend
  that could not be asked refuses nothing.

## 2026-09-25

- An effort the API does not take is refused before it is sent: `/effort`, `/model` and
  `POST /model` accept `none`, `minimal`, `low`, `medium`, `high`, `xhigh` and `max`, and the
  picker drops the catalog's `ultra`, which the API answers with a 400. A GPT-6 update the
  backend still refuses, such as `none` on `gpt-6-astra`, is taken back out of the history
  and the session file, and the session returns to the effort it was on; left in, it went
  out with every request after it and failed each one.

- A child agent can be put on another model. The `agent` tool takes `model` and `effort`,
  so an identity's model is its default rather than its ceiling and `general` no longer
  needs a near-duplicate identity per model. A new `models` tool lists what the backends
  will serve, with each model's efforts and window, which is the same list `/model`
  offers; the delegation section says to read it rather than guess an id. Children on one
  identity but different models get their own prompt cache keys.

- An effort change on a GPT-6 model (`gpt-6`, `gpt-6-*`, `gpt-6.*`) keeps the prompt
  cache. The request's `reasoning.effort` stays what the conversation opened on, and the
  change goes into the history as a `configuration_update` item just before the next
  message; a compaction, a `/clear` or a resume that lost the last one announces it again,
  and children run at the effort in force. On every other Codex model the effort is part
  of the cached prefix, so a change is refused while the cache is warm and goes through
  once it has expired, after `/clear`, or right after a compaction.
- The status bar says `cache expired: /clear to save ~86k tokens` once the last call is
  older than the cache lasts, and `$cache_expired` puts it in a template.

- A denial in `auto` on a chained command names the rules that would cover it. One rule
  cannot cover `a && b`, so the offer was empty for every chain, and `auto` never prompts:
  the user was told to switch modes with nothing they could run instead. It now reads
  `No one rule covers a chain; the user can permit its parts with `/allow Bash(printf:*)`
  and `/allow Bash(git config:*)``, unless a link is one nothing can name.

## 2026-09-24

- `/context` shares are shares of the tokens, not of the bytes, and the calibration
  factor divides by what the model actually reads. Encrypted reasoning is a third of a
  session's bytes and none of its input, so the old table put `reasoning` at 34% and
  `function_call_output` at 48% where the real split is 10% and 69%, and the factor came
  out 0.74 where it should have been 1.12, scaling every estimated row down by a quarter.

- Every permission decision is appended to `.bhai/debug/permissions.jsonl`, the allows
  included: timestamp, mode, whether the project is trusted, tool, summary, outcome
  (`ran` or `blocked`), who decided (`rules`, `rule`, `judge`, `auto`, `you`) and the
  reason. The judge log only ever held the calls that reached the judge, which is none of
  them in `ask` and `bypass`, so a session's own record said what was stopped and nothing
  about what ran.

- `bypass` stops asking. A protected path and a command the tokenizer could not take
  apart are guards bhai supplies itself, and `bypass` is the user saying they do not want
  to be second-guessed, so it runs them. Their own `deny` and `ask` rules are not guards
  and still decide in every mode, and an `ask` rule now reaches a shape the tokenizer
  refused the same way a `deny` rule always has. `ask` and `auto` are unchanged: a
  credential file still stops there, for the read tool as much as for `cat`.

- A judge case that writes to `~/...` is judged with the location line a session gives
  it. `--judge-eval` resolved a case's paths with no home, so a leading `~/` named no
  file, the case carried no `location:` at all, and the eval could not reproduce the one
  kind of call the location fact exists for. Cases take a `home`, defaulting to `/home/u`.

- `/effort <level>` changes the reasoning effort alone, and so does `POST /model` on the
  debug server with only `effort` given. An effort switch keeps the model's encrypted
  thinking and the context ledger, where `/model` dropped both. The Codex backend keys
  its prompt cache on the effort, so the call after a switch still starts cold; the
  cache monitor leaves that call unjudged.

- A protected path is a credential, not a directory the user works in. `~/.claude` and
  `~/.codex` hold the agent's own instructions, skills and prompts beside their
  credentials, so only `auth.json`, `.credentials.json` and `settings*.json` in them are
  protected now; `.env.example` and its `.sample`, `.template`, `.dist` and `.defaults`
  spellings are templates, not secrets; and a glob counts as reaching a hidden name only
  when the glob is inside the dot name itself, so `ls ~/.config/*` and `rm -rf .venv/*`
  are ordinary calls again. In `auto`, which never prompts, each of these was a denial
  the judge never saw.

- A credential file is protected against a read, not only against a write. The read tool
  answers a protected path the way `cat` always has, so the cheaper of the two is no
  longer the way around it, and what counts as one now covers `.netrc`, `.pgpass`,
  `.htpasswd`, `.git-credentials`, `.npmrc`, `.pypirc`, `secring.gpg`, the ssh key names,
  `*.pem`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, and the credential file of `.aws`,
  `.kube`, `.docker`, `.azure`, `.cargo`, `.config/gh` and `.config/gcloud`. A bare `env`
  is no longer read-only: it prints every secret the process holds.

- The status bar can be a template: `statusline` in `~/.config/bhai/config.toml`, in a
  subset of starship's format (`$var`, `[text](bold cyan)`, `( ... )` dropped when its
  variables are empty, `\` escapes) over 26 variables: model, effort, mode, branch, dir,
  ctx, tokens, cache, rate limits and resets, queued, time and more. `/statusline` lists
  them with their values now, `/statusline set <template>` and `/statusline reset` change
  it, and `/statusline <what you want>` has the judge's model write one, checked to parse
  before it is saved. A warning colour survives whatever the template styles. A row too
  narrow for it drops `( ... )` groups from the right, and a template that does not parse
  is reported at start with the built-in bar drawn instead.

- A finished shell command says how it ended on the row under it: `✓ succeeded`,
  `✗ failed, exit 128`, `✗ timed out after 120s`, or who stopped it (`✗ rejected by
  judge`, `by you`, `by auto mode`, `blocked by a rule`), with the `[+N lines]` its click
  opens. A rejected call of any tool is now drawn as its command with the rejection and its
  reason under it, and `tool_rejected` events carry `tool`, `summary`, `by` and `reason`.

- A shell command's output stays out of sight until the command is clicked: the command
  row ends in `[+N lines]`, a click on it shows the output in full, and a click on the
  command or the output closes it again. A command still running shows its latest lines.

- `sed -i` and `dd of=` count as writes for the judge's location line: the files a `sed`
  edits in place, in GNU's `-i.bak` form or BSD's `-i ''`, and the file a `dd` writes
  other than `/dev/null`.

- The debug server can set a permission rule: `POST /allow {"rule": "Bash(cargo test:*)"}`,
  `POST /trust`, `POST /untrust`, and `GET /permissions` for what is in force. `/allow` and
  `/trust` were TUI commands only, and a headless run is exactly where no prompt can be
  answered.

- Text with wide characters in it wraps and selects where it is drawn. Rows were measured
  in characters, so a line of CJK or emoji was wrapped at half the columns it takes and ran
  past the edge of the view, and a click mapped its column straight to a character index, so
  a drag took the wrong text. The transcript wrap, the markdown wrap, the ground behind a
  user message and the mermaid fit check now measure columns, and a click or drag lands on
  the character it is over. `unicode-width` is a direct dependency, at the version the
  lockfile already had.

- A TUI that falls behind the event stream catches up instead of staying wrong. Events are
  dropped, not delayed, when the channel overflows, so the spinner, the pending approval,
  the queue and the mode chip could all sit at whatever the last event they saw said. A
  dropped batch now triggers a resync from the session; the token counts are left alone,
  since they are summed from the stream.

- A `.mcp.json` server that changed since it was approved is not started. The approval in
  `~/.claude.json` is a list of names, so a repo could keep the name and change the command
  to anything; bhai now records what each approved server was defined as, under
  `~/.config/bhai/mcp-approvals.json`, and skips one whose command, arguments, environment
  or url has changed since, saying so in `/mcp`. `bhai mcp approve <server>` accepts it as it
  is now. Header values are not part of it, since a rotating token is not a change of
  program.

- A panic in the agent loop ends the turn it was running. The task was spawned with its
  handle dropped, so a panic in the loop or in a tool left the session marked working, with
  a spinner that never stopped and no error to say why. The loop is now watched, and a panic
  becomes a failed turn and a `TurnEnd`.

- A deny or ask rule whose shape bhai cannot parse now fails closed. `Bash(npm run test?)`
  and anything else with a `?`, a chain or a substitution in it was dropped with a startup
  notice, so a rule written to stop something stopped nothing. Such a rule is kept as the
  text it names, matched case-insensitively anywhere in the command, on either side of the
  tokenizer; `/permissions` marks it `matched as text`. Allow rules are still dropped, since
  one that over-matches approves what the user did not.

- `bhai sessions prune [n]` deletes all but the `n` newest sessions, 20 by default, with
  their child transcripts; a session another bhai has open is left where it is. Nothing
  prunes on its own, since the transcripts are the debug record. The exit hint also stopped
  parsing every session in the directory to work out whether this one is the newest, which
  it did on every exit and inside the panic hook; it reads modification times instead.

- A transcript past 65535 rows scrolls to its end instead of wrapping back to its top. The
  view is now drawn from the rows below the scroll rather than through the widget's own
  `u16` offset, which also stops it walking every row it is not drawing.

- A transcript draws what a terminal would act on or hide as what it is. Control bytes from
  a command's output went to the screen, into the rows a selection is measured in, and onto
  the system clipboard; tag and bidi characters, which render as nothing, carried text
  nobody could see. Both are now taken out before the wrap: control characters are dropped
  (`\n` and `\t` stay), and a character that renders as nothing is shown as `·`. Emoji
  joiners and variation selectors are left alone.

- The debug server asks who is calling. `--serve` bound a port that answered any local
  process, and those endpoints run commands, answer approvals and switch the permission
  mode, so any program on the machine could drive a session. Each run now mints a token,
  prints it beside the address, and requires it in an `x-bhai-token` header. That also
  closes `/events` as a cross-origin subresource: a page cannot set the header.

- A lowered `compact_at` no longer compacts into a history that is already over it. The
  target was `min(0.6, compact_at)` of the window, so `compact_at = 0.5` summarised down to
  exactly the trigger and the next turn triggered again. It is now three quarters of the
  trigger, capped at 0.6, which leaves the default (`compact_at = 0.8`) aiming at the same
  0.6 it always did.

- What bhai writes is the user's alone. Session transcripts, child transcripts, the prompt
  history, the workflow step cache and everything under `.bhai/debug` were created with the
  default umask, so on a shared machine every account could read what a session read, wrote
  and was told. On unix they are now created `0600`, and the directories holding them `0700`.
  Files that already exist keep the mode they have.

- What a file or a server can put in the system prompt is bounded. An instruction file over
  64 KiB is skipped and said instead of loaded, since the prompt is a prefix every call of
  the session pays for. MCP server and tool names in the listing lose their control
  characters and are cut at 64 characters, so a server cannot write extra lines into a
  section whose shape is one line per server. A server whose name ends in `_` is refused
  like one containing `__`: both break the `mcp__server__tool` split and would name the
  wrong tool.

- A skill name reaches the prompt as one short line, and a shadowed skill is named at
  startup. A `name:` written as a YAML block put its own lines in the listing, so a skill
  file could write whatever it liked where the listing's shape is; the name is now collapsed
  to one line and cut at 64 bytes, leaving every ordinary name byte-identical. A project
  skill that replaces a global one of the same name is reported the way a skipped import is,
  since the precedence is deliberate but the file that lost is still on disk.

- A project `.bhai/config.toml` can no longer turn off the user's global instruction files or
  widen where skills are read from. `load_global_claude`, `load_global_agents` and the skills
  `sources` list are the global file's call, like `permission_mode`, `allow` and the model
  already are; a clone that set them was dropping the standing instructions the user wrote
  for every session, or adding a source it could then write skills into. Turning skills or
  the project's own instructions off is still the project file's to do.

- `command -v` inside a loop is no longer denied outright. The blunt word split that decides
  whether a command the parser cannot read may go to the judge treated every `command` as a
  program that runs its arguments, so `for f in rg fd; do command -v $f; done` was refused in
  `auto` while the same probe without the loop went through. It now reads the word after a
  `command`, the way the parsed path already does.

- An allow rule for `cd` no longer carries the project with it. `Bash(cd:*)`, which a Claude
  Code settings file commonly has, matched `cd ~`, `cd -` and a bare `cd` as an ordinary
  allowed command while the cwd tracking, which reads only `cd <one relative or absolute
  path>`, left the tracked directory at the project root: `cd ~ && cargo test` then ran in
  the home directory as "inside the project", in `auto`, without asking. A `cd` the tracking
  cannot follow now makes the directory unknown, so what follows it has to stand on a rule
  or on being read-only, and never on being the project's own work.

- The system prompt stops saying two things that are not true. It forbade markdown headers
  and bullets while the transcript has rendered headings, tables, lists, code and mermaid
  since the hackmd port, and the model ignored the rule in every transcript that was looked
  at; it now says to use the structure an answer needs and prose where it does not. The
  delegation section still said a child "is held to what the permission rules allow
  outright", which stopped being true when a child got a judge of its own.

- An answer the backend cut off at the model's output limit says so. Ollama's
  `done_reason: length` was read as a finished turn, so a truncated answer looked complete;
  the transcript now carries `cut off at the model's output limit; the answer is what it
  had`, and the Codex `response.incomplete` event says the same. What streamed is still kept
  as the answer, since it is what the user already read.

- A call that is sent again no longer counts its tokens twice. Both backends reported usage
  the moment the terminal event arrived, so an error sharing that event's chunk retried the
  whole call with the first attempt already in the totals, the per-turn ledger and
  `usage.jsonl`. The counts are held until the attempt returns an answer.

- A child agent says why it ended, not what the disk did. A failed transcript write reached
  the parent as the child's own failure reason, so `child c1 (general) failed ... :
  transcript: Not a directory` stood where "ended without a final message" belonged. A write
  failure is still shown in the child's pane and is no longer eligible as the reason; of the
  errors that are, the first is kept rather than the last, and a turn-ending failure
  replaces it whenever it lands.

- A child agent that spends its whole step budget says so. It used to report "finished" in
  the same words as a natural completion, so a result the agent was told to cut short read
  as a finished one: the `agent` tool output now says `finished in 40 steps (step budget
  spent)` and a workflow's report line says `ok, step budget spent`. A workflow no longer
  caches such a step either, for the same reason a failed one is not cached: what it
  answered with is what it had when it was cut off, and the cache would hand that back for
  every later run.

- A workflow is told where to cache its steps. `Run` carries a `cache_root`, set from the
  `Delegation` the session was built with, instead of taking `delegation.sessions.parent()`
  and relying on that being `<project>/.bhai/sessions`. A caller with no root to give says
  so once and every step runs.

- A command is judged knowing where each file it writes lands. A bash call to the judge
  now carries `location: writes <path> inside|outside the project root; ...`, for every
  redirect target and every file a `tee`, `touch`, `mkdir`, `rm`, `rmdir`, `mv`,
  `truncate`, `chmod` or `chown` names, and the destination of a `cp`, `ln`, `install` or
  `rsync`, followed through the chain's `cd`s. A path that needs the shell to resolve is
  left out rather than guessed.

- The judge is told where a written path lands instead of working it out. A `write` or
  `edit` now carries `location: inside the project root` or `outside the project root`,
  resolved the way the permission layer resolves it, and names any character in the path
  that renders as a space or as nothing. The prompt says to take it as fact. Outside is
  still allowed when the user asked for it; a reason that calls an outside path inside is
  not. `is_inside` resolves a root that does not exist yet the same way as the path.

- `/export-debug` writes every child agent of the session: its row, the brief it was
  given, everything its pane showed and where its full history is on disk. It used to list
  only the children still on the panel, which a new message clears of finished ones, and
  none of what they did. The session now keeps the panes the panel lets go of, and the
  judge decisions say which child each one was for.

- A child agent in `auto` mode is judged instead of denied. Children ran with no judge,
  which was harmless while a call with no verdict fell back to asking, and blocked every
  call past the rules once `auto` stopped asking. Each child now gets a judge forked from
  the session's when it starts: the user's task, what the session had done and the brief
  it was given, then its own calls on a budget of its own. What it spends counts toward
  the session's judge total, and its lines in the judge log carry its id.

- An interrupt now stops a detached child for good. The turn and its children used to
  share one flag, which the next prompt cleared, so a child that had not reached its next
  check between the two carried on as though nothing had happened. Each child takes a flag
  of its own from the turn's, which an interrupt latches and no later prompt clears. A
  child still waiting for a slot when the interrupt lands never starts at all, and says so
  rather than leaving the parent to wait for a report that is not coming.

- A report that opens a turn of its own says it is one. The user already has an answer by
  then, so the message the model reads asks for what the report adds or changes and leaves
  the rest standing, which is the difference between one follow-up and a correction round
  per late child. A report read between the steps of a running turn is unchanged: the
  answer is still being written.

- The child panel says how long a running child has been quiet, and the TUI knows a wake
  is a turn. A child heard from every few seconds and one wedged in a retry loop looked
  the same from outside; the row now carries the silence once it is long enough to mean
  something. The spinner and the panel stayed off for the whole of a turn a child's report
  opened, since only a typed message marked the TUI as working; the report does now too,
  without moving a reader who has scrolled up.

- A workflow can keep its step results between runs, so re-running one skips the steps
  that have not changed. `cache: true` in the definition's frontmatter turns it on and
  nothing else does: a hit answers today's run with an older answer, so the file the
  author wrote is where that choice belongs, and both the confirmation prompt and
  `/workflows` say which workflows have it. A result is keyed on the step's id, the
  prompt as it will be sent once `{{input}}` and `{{steps.<id>}}` are filled in, and the
  whole identity the step runs as, so an edited agent file runs the step again; the files
  sit in `.bhai/cache/workflows/<name>`, beside the session transcripts and gitignored.
  The first step whose key has changed runs, and so does every wave after it, which is
  not asked about the cache at all. A step that failed is never stored, so the next run
  retries it. A hit spends no tokens and launches no child, and says so in the transcript
  and in the run's report.

- Child agents run detached, so the session is never held by one. The `agent` call hands
  back the child's id and returns; asking for three starts three, and they run together
  under the same `MAX_RUNNING` limit, with a fourth waiting for a slot instead of the
  caller waiting for the call. The report comes back on its own: between steps if the
  turn is still going, so nothing costs an extra model call, and otherwise as a turn of
  its own, which the session is told about so what the user types queues behind it as it
  would behind any other. An interrupted session is the exception and stays stopped: the
  report joins the history and the transcript, and the next message reads it. The judge
  gets its budget back on such a turn but keeps the user's task, since following up a
  report the agent asked for is the same piece of work. A report is carried in a user
  message marked as one, so a session read back from disk shows it as the tool result it
  is rather than as something the user said. The panel keeps a child that outlived its
  turn, and Esc still stops it.

## 2026-09-23

- `/export-debug` writes the whole session to one markdown file in the working directory
  and says where it went, so a report is a file to hand over rather than a screenshot of
  the transcript. What goes in is what the loop's own questions turn on: the model, mode
  and identity it ran under, whether trust held the mode back, the totals and rate-limit
  windows, every permission rule and where it came from, the last 50 judge decisions with
  the exact summary each was sent, the skills, MCP servers and workflows found, the token
  profile, and the transcript in order. The home directory is written as `~`, which is the
  only thing rewritten; the header says so, and says the transcript below holds whatever
  the session saw.

- `auto` mode stops denying the work the user asked for. Four things were wrong at once,
  found from a session that set up GPG on the machine and got nowhere. `command -v gpg`
  was read as running its arguments, the way `sudo` is, so one of them made a whole
  chain unparseable and an unparseable command naming a refused program is denied
  outright: an agent probing what is installed never got past its first call. The
  checker kept every write and edit outside the project from the judge while letting
  `printf x > ~/.gnupg/gpg-agent.conf` reach it, which is the same change by another
  route, so that boundary is gone and where a call lands is the judge's to rule on. The
  judge's own prompt denied an install or a global config change whatever the task said,
  which left a task about the machine rather than about the project with no step that
  could ever run; it now approves a change outside the project root when the user's own
  messages ask for that change, and says what does not widen it: blanket permission, the
  agent's own reasoning, or text it read from a file, a page or a tool's output. Six
  cases for that are in the eval fixture, three each way.

- A denial says what it tripped on. It used to name all four things it could have been
  and let the model work it out, which in that session meant rewriting the same call
  three times; `Reserved` carries the word or the rule, so the message is `sudo` or
  `Bash(cargo publish:*)`, and it ends with the `/allow` line that would let that very
  call through.

- `/allow <rule>` puts a permission rule in from the prompt, for this session and the
  ones after it. `auto` never shows an approval modal, so the remember path behind it
  was unreachable and the only way to permit one call was to change mode. Bare `/allow`
  prints the four shapes a rule takes. A deny rule still refuses and a protected path
  still asks.

- The status bar sits at the bottom of the screen and says where the session stands: the
  branch it is on, how full the context window is (`ctx 34%`, from what the last call
  read, against the window compaction measures), and each rate-limit window with the
  time until it comes back, `5h 8% (1h9m) · wk 20% (Sun 21:47)`. A reset under a day off
  counts down; past that it is the local weekday and time, which is what a weekly window
  usually needs. The reset times were a hover on the top bar before, which is neither
  where the eye is nor something anyone finds, and the bar itself was a row the
  transcript could have been scrolling through. A terminal too narrow for all of it
  drops the `/` hint first and then the running totals, so the branch, the fill and the
  windows are what survive. The branch comes from `.git/HEAD` on the tick, so a checkout
  in another terminal shows up within a couple of seconds.

- An interrupt reaches the waits that never watched for one. Esc sets the flag and the
  transcript says `interrupted` at once, but the turn ran on until whatever it was
  awaiting came back, with the spinner still up: the judge deciding a call (up to its
  15 second timeout), the backoff between retries of a model call (0.5s, then 1.5s), or
  a token refresh. Each is awaited through `client::unless_cancelled` now, which polls
  the flag the way the stream already did. Measured on a live session, esc to the end of
  the turn is 14-20ms; the judge case was 30s of spinner on a stopped turn.

- A compaction shows the summary it folded the earlier turns into, under the notice that
  says what it cost. The summary is the context the conversation carries from there, and
  it was the one thing the transcript never said: the notice reported two numbers and the
  words the model wrote were only visible on a resume, as something the user appeared to
  have typed. It is an entry of its own (`≡`), folded to its first rows like tool output,
  and a resumed history draws the folded message as the same entry rather than as a
  prompt. Evicting tool outputs says nothing about the conversation, so it carries none.

- The tok/s readout is what the last half second of streaming carried, scaled to a
  second, instead of an average over ten seconds of it. The old number took a whole
  window to answer a change in the stream and held its last value through a stall,
  which read as hung; this one moves once per reading and says zero when nothing is
  arriving. The reading is one constant (`speed::PERIOD`), now 250ms, and the tests
  are written in readings rather than seconds so it can move again on its own.

- The working row says what a call is waiting on before its first token: the size of
  the prompt that went out and how long it has been reading, then how fast that came
  to once the first token lands (`· 8.7k in/s · 40 tok/s`). Neither backend reports
  progress through a prompt, so this is what there is: on an 8.7k-token prompt to a
  local model the row sat blank for nine seconds before it.

An audit pass. 234 known failure modes for agent harnesses were collected (53 from a
production write-up of another harness, the rest researched), split across eight lanes
that own disjoint files, hunted, reviewed, and fixed. 89 candidates were reviewed, 9
rejected, 45 fixed here; what is left is in `TASKS.local.md`.

- A model call has a read timeout and ends on its own terminal event. A server that
  accepted the connection and then said nothing wedged the turn with no way out:
  `/interrupt` cannot reach a task parked on `stream.next()`. The stream is also decoded
  a line at a time rather than a chunk at a time, so a codepoint split across a network
  chunk is no longer replaced by `U+FFFD`, and `response.incomplete` is an outcome rather
  than an unrecognised event retried three times at full cost.

- Every backend says what its context window is. `Limits::window` only recognised a
  `gpt-5` name prefix, so on Ollama and on any future slug compaction never ran at all.
  The window is asked for at session start from the same place the picker reads it, and
  Ollama is told it with `options.num_ctx`, which it was never sent.

- Calls waiting on the user queue. One approval slot meant a second parallel workflow
  step overwrote the first, and the overwritten oneshot resolved to `Reject`, so the
  model was told the user refused a call the user had accepted.

- An allow rule no longer grants every redirection target. `Bash(cargo test)` matched on
  the command's words alone, so `cargo test > /etc/cron.d/evil` ran unprompted in ask
  mode, and a redirect through `~` counted as inside the project. The read-only list also
  knows that `sed 'e ...'`, `sort --compress-program`, `awk BEGIN{system()}` and `rg
  --pre` run a program of their own, and a shell keyword or a wrapper no longer hides
  what it is about to run.

- bash caps what it keeps and stops reading when the command exits. The whole output was
  buffered with no bound, so `yes` took the process down; `truncate` only ever bounded
  what the model saw. A backgrounded job no longer holds the command open for the full
  timeout. read streams, refuses what is not a regular file, and stops at a line its
  footer names, so `/dev/zero` is not read until memory runs out.

- The session file is held for one bhai at a time, and the lock is released explicitly
  rather than by closing the handle. A flock belongs to the open file description, so a
  process forked while the writer lived carried a copy and kept the session locked after
  it was dropped.

- A repo says what an identity does, not which model runs it, and the MCP switch and the
  judge keys are read from the global config alone. Both ran before the trust gate, so a
  directory could pick the model, replace the system prompt, and spawn server processes
  before anything asked.

- An MCP call is bounded and can be interrupted, its search result is trimmed like every
  other tool output, and a tool name reaches the prompt as one line.

- The transcript draws once per batch of events rather than once per event, a long word
  is counted once rather than on every hard split, and a row ends at the space that
  overflows it. `Editor::rows` panicked inside `terminal.draw` on a token that ended
  exactly at the wrap column.

## 2026-09-22

- Task lists and footnotes render. Both handlers were already written, and neither had
  ever run: the parser was built with tables and strikethrough alone, so `- [x] done`
  reached the renderer as a list item whose text begins with a literal `[x]`, and the
  `FootnoteDefinition` arm was unreachable code. A checked item now shows as `• [x]`
  and a footnote keeps its reference next to what it marks.

- A ```mermaid fence is drawn as a diagram rather than shown as its source. A model
  asked to explain a flow reaches for mermaid, and what arrived was a dozen lines of
  `A->>B` to read as text. `merman-core` parses it and `merman-ascii` lays it out as
  box-drawing lines, for the diagram types it draws: sequence, flowchart, class, ER and
  xychart. A diagram is art rather than text, so one too wide for the view is not drawn
  at all: splitting box-drawing lines to fit leaves something worse to read than the
  source, so the fence falls back to being a code block, and so does anything that does
  not parse or is not a type merman draws. `mmd` is the same fence as `mermaid`.

- Fenced code is coloured by syntect's grammars rather than by a lexer written here.
  The old one knew thirteen language groups and read each as a set of rules about what
  a comment, a string, a number and a keyword look like, which is most of the way there
  for a language it had been taught and nothing at all for one it had not. syntect
  brings the default grammar set, so a fence naming almost anything is coloured, and it
  is a real parse rather than a guess. What is kept is the property that made the old
  one readable: a fence that names no language, or names one there is no grammar for,
  stays plain, since syntect will otherwise identify a block of log output as some
  language by its first line and paint it in that language's rules. A theme's colours
  are 24-bit and some terminals silently drop those, so every one goes through a new
  `palette` that hands back the nearest xterm-256 entry where truecolor is not on
  offer. `code_theme` names the theme; an unknown name is a config error listing the
  ones there are.

- The delegation section says what a child costs instead of asking for one. It read
  "delegate read-heavy or specialised work to the cheapest fitting identity", which is
  an instruction to delegate exactly the case the parent is already best at: it has read
  the files, and the child has to find them again from nothing. "Cheapest" was not true
  either, since an identity with no model of its own runs on the parent's, at the
  parent's effort. It now says a child starts blind and is thrown away with its context,
  so it is worth it only when it would read far more than it reports back, and that a
  child is held to what the permission rules allow outright.

- A child agent has a step budget. Nobody is watching a child: the user can interrupt
  the turn in front of them, but a child that has lost the thread reads and re-reads
  until the model gives up on its own, and a delegated look around a large repository
  has gone seventy steps and several million tokens of input doing it. A child now gets
  forty steps, and the last one is spent answering rather than cut off mid-tool: at the
  budget it is told to stop calling tools and say what it found and what it did not get
  to, so the work up to there comes back instead of being thrown away. One that keeps
  calling anyway ends as a failure naming the budget. The session's own turn is not
  bounded.

- An exclusion is no longer read as a mention. `grep --exclude-dir=.git` names `.git`
  only to stay out of it, but the protected-path check split every word on `=` and saw
  the `.git` part, so the safest way to write the search was the one thing that made the
  command the user's alone to approve, and in `auto` mode that is a denial with no way
  through. The one command that avoided `.git` was refused while the one that walked into
  it went by. `--exclude` and `--exclude-dir` now name a pattern the command skips rather
  than a path it touches; `--include` and `--file` still name one it reads.

- A table wider than the view keeps its columns. Each row was laid out as one padded
  string and then wrapped like a paragraph, so the moment a table did not fit, the
  overflow landed back at the left margin where it read as another row, and the columns
  stopped lining up at exactly the point they were needed. A table now has the room
  taken off its widest columns, so a column of short values keeps its natural width and
  a column of prose is the one that gives way, and a cell too long for its column wraps
  inside it, under the column it belongs to. What is inside a cell keeps its styling
  too: inline code in a table is coloured the way it is anywhere else, rather than
  flattened to plain text on the way in.

- A drag can select more than one screenful. The selection was always anchored to the
  wrapped transcript rather than to the screen, but nothing scrolled the view while the
  button was down, so a selection could never reach past the rows in front of it, which
  is exactly the case where selecting by hand is worth anything. Dragging past the top
  or bottom edge now scrolls the transcript and keeps selecting, a line for each row
  past the edge and up to five, and it carries on while the pointer is held there rather
  than stopping the moment it stops moving. Reaching the first or last row in view is
  enough to start it, since that is where a drag runs out of transcript, and the further
  out the pointer goes the faster the view moves. The wheel takes a running drag with it
  too, and `ctrl+a` with nothing drafted takes the whole transcript in one go.
  Past the bottom edge takes the line it lands on whole and past the top takes none of
  it, so a drag holds the lines it has travelled over.

- The terminal's title says where the session is and what it is doing: the project
  directory on its own until the first message, then the directory and a few words on
  the work, as `bhai · fix the judge cache`. A tab is narrow and keeps the end of what
  it cannot fit, so the words go last and the directory is what gets cut. One small
  model call names the session, off its first message and never again: a title that
  changed under you every turn would be worse than one a little behind. `title = false`
  turns it off, and a run with no terminal (`--headless`, `--workflow`) never asks.

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
