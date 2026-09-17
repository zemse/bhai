# Permissions

Every tool call that changes something is decided before it runs: allowed, rejected, or
put to you as an approval prompt. Nothing skips that path, children and workflow steps
included, since they all share the session's policy.

## Modes

- `ask`: every call that changes something prompts. The default
- `auto`: allow rules and read-only commands (`ls`, `cat`, `grep`, `git status`, and
  friends) run; everything else prompts
- `bypass`: everything runs, except ask rules and protected paths, which still prompt

Start in one with `bhai --mode auto`, or set `permission_mode` in the global config.
`shift+tab` cycles the mode in the tui, `POST /mode` sets it over the debug server.
`/permissions` prints the current mode, every rule and the file it came from.

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
