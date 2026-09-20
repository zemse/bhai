<img src="https://raw.githubusercontent.com/zemse/bhai/main/assets/banner.png" alt="bhai running a task in the terminal" width="100%">

**bhai** is an agent harness for running advanced long running workflows on systems.

```sh
cargo install bhai
```

inference runs on the codex cli's chatgpt-subscription credentials (`~/.codex/auth.json`), so log in with codex first, then run `bhai` in the directory you want to work in. or point it at a model on your own machine with `bhai --model ollama:<name>`.

> wip, a hobby project. everything under "what it does" is built; the wishlist at the bottom is not.

## what it does

- **tools**: bash, read, write, edit, skill and agent, plus any mcp tool. running commands stream their output into the transcript while they run.
- **permissions**: three modes (auto by default, plus ask and bypass), claude code rule syntax with `*` wildcards, approvals bhai remembers, and a trust question on opening a project, which is what auto and bypass wait on. [docs](docs/permissions.md)
- **models**: `--model` picks what the session talks to, the chatgpt subscription or a local model through ollama; the same items go to both, so tools, subagents and sessions work either way. [docs](docs/models.md)
- **identities**: `bhai --as <name>` narrows the skills, tools, instructions and model a session carries. fixed for the session, so the prompt cache holds. [docs](docs/identities.md)
- **subagents**: the agent delegates a task to a child with a fresh context, under any identity, up to three at a time; each one gets a row above the prompt you can step inside and talk to. [docs](docs/subagents.md)
- **workflows**: a handful of child steps in dependency order under one token budget, started only by you. [docs](docs/workflows.md)
- **skills**: `SKILL.md` directories listed in the prompt, bodies loaded on demand. [docs](docs/skills.md)
- **mcp**: stdio and streamable http servers, read from claude code's config as well as bhai's. schemas stay out of the tool list; the model finds tools with `mcp_search` and runs them with `mcp_call`. [docs](docs/mcp.md)
- **sessions**: every turn appended to `.bhai/sessions/<id>.jsonl`, `--resume` to continue one, automatic compaction when the window fills. [docs](docs/sessions.md)
- **token accounting**: where the context actually goes, per entry, with hover badges in the tui and a full report from `/context`. [docs](docs/context.md)
- **prompt cache guard**: every request is checked for being an append-only extension of the one before it; `--cache-check` proves the backend serves it, `--strict-cache` refuses to send a request that would break it. [docs](docs/context.md#the-cache-guard)
- **tui**: markdown rendering, a `/diff` pane, a `/` command menu, mouse (a drag selects and copies as it ends), multi-line input, prompt history, collapsible tool output and rate-limit headroom in the status bar. the spinner, the tokens a second the model is answering at and the interrupt hint sit above the prompt, not at the top. [docs](docs/tui.md)
- **debug server**: `--serve` exposes the running session over localhost http, `--headless` runs it without the tui. [docs](docs/server.md)

## slash commands

type `/` in the prompt box for the menu: it filters as you type, arrows pick a row, the rest of the highlighted name shows grey in the prompt, tab fills it in and enter runs it. skills appear in the same menu, so `/<skill> [input]` hands the skill to the agent.

| command | what it does |
| --- | --- |
| `/help` | the commands and the keys |
| `/context` | write the token profile and transcript to `.bhai/debug/` |
| `/compact` | summarise the history now |
| `/diff` | open the working tree diff pane |
| `/permissions` | print the modes, rules and where they came from |
| `/trust`, `/untrust` | honour, or stop honouring, the repo's own allow rules |
| `/as [name]` | show the session's identity and how to switch |
| `/skills` | list the loaded skills and what each costs |
| `/model` | the model this session talks to, and how to change it |
| `/mcp` | list the mcp servers, their tools and any that failed |
| `/workflows` | list the workflow definitions |
| `/workflow <name> [input]` | run one |
| `/queue` | list the prompts waiting behind the running turn; `/queue clear` drops them |
| `/mouse` | turn mouse capture off and on, for the terminal's own selection |
| `/tokens` | show every entry's token badge, not just the hovered one |
| `/copy` | copy the selection to the clipboard |
| `/quit` | leave bhai |

## flags

```
bhai [identities] [sessions] [--probe [prompt]] [--cache-check]
     [--judge-eval [file]] [--as <identity>] [--resume [id]]
     [--model <name>] [--effort <level>]
     [--workflow <name> [input] [--workflow-yes]]
     [--serve [port] [--headless]] [--profile] [--strict-cache]
     [--mode ask|auto|bypass] [--trust] [--no-global] [--no-project] [--bare]
```

`identities` and `sessions` print what is available and exit. `--probe` does one non-interactive model call to check auth and the wire format. `--judge-eval` scores the auto-approval judge against a file of cases, `tests/fixtures/judge-cases.jsonl` by default, and exits 1 when any verdict is not the one the case expected. `--model` and `--effort` override the configured model for one run; an `ollama:` prefix names a model served locally. `--profile` logs every call's usage and response headers under `.bhai/debug/`. `--trust` trusts the repo's allow rules at startup. `--no-global`, `--no-project` and `--bare` drop instruction files and skills, `--bare` all of them.

## not built yet

- customisable web search: multiple free and paid options pluggable.
- fuzzy multipass edits + openai's custom patch format.
- hooks for tool calls and skill invocations.
- daemon for centralised session management.
- graph based memory.
- p2p remote over static IP or iroh relay.
- voice interface for hands free session management.
- x402 and crypto wallet, explorer integrations.
- secret hiding to prevent occurences in transcripts.
