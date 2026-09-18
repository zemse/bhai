# Permissions

Every tool call that changes something is decided before it runs: allowed, rejected, or
put to you as an approval prompt. Nothing skips that path, children and workflow steps
included, since they all share the session's policy.

## Modes

- `ask`: every call that changes something prompts. The only mode an untrusted project
  has
- `auto`: allow rules and read-only commands (`ls`, `cat`, `grep`, `sed -n`, `git status`,
  and friends) run, plus the relaxations below; what is left goes to the judge. The
  default, once the project is trusted
- `bypass`: everything runs, except ask rules and protected paths, which still prompt

Start in one with `bhai --mode ask`, or set `permission_mode` in the global config.
`shift+tab` cycles the mode in the tui, `POST /mode` sets it over the debug server.
`/permissions` prints the current mode, every rule and the file it came from.

## Trusting a project

Opening bhai in a project the trust store does not know asks, before anything else runs:

```
┌ do you trust this folder? ───────────────────────────────────┐
│/Users/z/code/ripgrep                                         │
│auto mode writes inside this project and runs its build and   │
│test commands without asking, and the judge decides the rest. │
│That runs code this project supplies.                         │
│                                                              │
│[y] trust it, use auto mode   [n]o, stay in ask mode          │
└──────────────────────────────────────────────────────────────┘
```

`y` records the project in `trust.json` and puts the session in the mode the config asked
for. `n` leaves it in `ask`, where every change prompts; `/trust` can still change that
once you have looked at the code. Until the answer is yes, `auto` and `bypass` are not on
offer at all: `shift+tab` stays on `ask`, `POST /mode` reports back the mode actually in
force, and `--mode auto` without `--trust` starts in `ask` with a notice. The answer is
remembered per project and asked again whenever its settings files change.

## What `auto` runs by itself

`auto` mode in a trusted project also runs, with no rule of its own:

- writes and edits whose target is inside the project root, once `..` and symlinks are
  resolved. A protected path, a path outside the root and anything a `deny` or `ask` rule
  matches still prompt
- the project's own build and test commands: `cargo build|check|test|fmt|clippy|run`,
  `npm|pnpm|yarn run|test|install`, `pytest`, `python3 <file in the project>`,
  `go build|test`, `make`, `just`. An argument naming a path outside the project, a
  `--config` flag, `sudo` or anything the tokenizer refuses drops back to a prompt

These run the project's own code, which is what trusting a project means, so an untrusted
project gets none of them and cannot be in `auto` in the first place. `ask` mode gets none
of them either. Turn each off with `auto_project_writes = false` and
`auto_project_commands = false`; a project config file may turn them off but never on.
`/permissions` prints which apply.

## The judge

What is left in `auto` in a trusted project is every call no rule names: an unknown
binary, a write the relaxations do not cover, an MCP tool. Rather than prompt for each,
a small model call decides whether the call is a reasonable step toward the task you
actually asked for and whether its blast radius stays inside the project. It answers
`approve` or `deny`, nothing else.

The judge only ever sees what would otherwise have prompted, so allow and deny rules are
never overridden. These never reach it and prompt as before: a protected path, a write or
edit outside the project root, `sudo`, and any command the tokenizer refuses. A deny is
final for that call and goes back to the model as `denied by auto policy: <reason>`, so
it can adapt; you are not asked afterwards. Anything else the judge cannot settle, an
error, a timeout, a malformed reply or a spent budget, falls back to the prompt.

The request is a compact summary, a thousand tokens or so at its fullest, laid out so
what the session has done comes first and the call to decide comes last: the project root
and cwd, your last few messages, the calls made so far one line each, the verdicts given
so far, your current task, then the tool and the exact command or path (with a diff
summary for an edit). Everything before the call only ever grows, so each request extends
the one before it and the shared part is served from the cache rather than re-read. It runs on its own
`prompt_cache_key`, is never appended to the conversation, and its tokens are counted
apart in `/context` and `GET /state`. A verdict is reused for an identical call for the
rest of the session, and at most `judge_max_per_turn` calls are judged per turn.

It runs on the session's model at `low` effort unless `judge_model` names another, so it
costs a small fraction of a turn. While it decides, the row above the prompt says `auto
mode is checking <call>`; every decision is appended to `.bhai/debug/judge.jsonl`, and
the transcript shows `auto-approved: <reason>` or `auto-denied: <reason>`. `/permissions` prints whether the
judge is on and what is left of this turn's budget.

```toml
judge = false            # off; it is on by default and only ever runs in `auto`
judge_model = "gpt-5.5"  # the session's model unless set
judge_effort = "low"
judge_timeout_ms = 15000
judge_max_per_turn = 20
```

## Rules

Claude Code syntax: `Bash(git log:*)`, `Bash(cargo *test)`, `Read(~/notes/**)`,
`Edit(src/**)`, `mcp__tavily__tavily_search`, or a bare tool name for all of it. A
trailing `:*` makes a command rule a prefix; a `*` anywhere in a bash rule matches within
a word. An `Edit` rule covers `write` as well, and `mcp__server` covers every tool of
that server. `deny` wins over `ask`, which wins over `allow`.

Commands are tokenized before matching, so `git log && rm -rf /` is two commands and both
have to pass. Wrappers (`sudo`, `env`, `xargs`) and commands run as arguments
(`find . -exec git push \;`) are matched on the real program too. Anything the tokenizer
does not understand gets no rule match and prompts.

Rules come from, lowest precedence first:

- `[permissions]` in `~/.config/bhai/config.toml`
- Claude Code's `~/.claude/settings.json`, `<project>/.claude/settings.json` (checked
  into the repo, so its `allow` rules are ignored) and
  `<project>/.claude/settings.local.json`, unless `import_claude_permissions = false`
- `<project>/.bhai/settings.local.json`, where approvals bhai remembers are written
- `[permissions]` in `<project>/.bhai/config.toml`, whose `allow` list and
  `permission_mode` are ignored, so a cloned repo cannot approve its own commands

```toml
permission_mode = "auto"

[permissions]
allow = ["Bash(cargo *)", "Read"]
ask = ["Bash(git push:*)"]
deny = ["Bash(curl:*)"]
```

## Protected paths

`.git/`, `.env*`, `.bhai/config.toml`, `.bhai/settings.local.json` and
`.claude/settings*.json` are never changed without asking, whatever the mode and rules
say. A command that names one, or globs into one, prompts too.

## Remembering an approval

The prompt takes `y` for once, `a` to remember that exact call and `p` to remember its
prefix (`Bash(cargo test:*)`, `Edit(src/**)`); `n`, `r` or escape rejects. What you
remember is written to `.bhai/settings.local.json`.

## Trust

Allow rules that come from the repo (`.bhai/settings.local.json`,
`.claude/settings.local.json`) are ignored until you trust that repo, so cloning a
project does not hand it your shell. `deny` and `ask` rules from those files always
count.

`/trust` honours them and records a hash of the files in `trust.json` under bhai's config
directory; editing either file drops the repo back to untrusted until you run it again.
`/untrust` reverses it, `bhai --trust` does it at startup.
