# Identities

An identity narrows what a session carries: skills, tools, instruction files and extra
prompt. Pick one with `bhai --as <name>`; it is fixed for the session so the prompt cache
holds. `bhai identities` lists them with their baseline context cost.

Built-ins: `general` (everything, the default) and `router` (no skills, `read` only,
lists the other identities).

## Files

Markdown with frontmatter, the same shape as Claude Code agent files. Searched lowest
precedence first, later ones replace earlier ones by `name`:

- `~/.claude/agents/*.md`
- `~/.config/bhai/agents/*.md`
- `<project>/.claude/agents/*.md`
- `<project>/.bhai/agents/*.md`

Keys:

- `name`, `description`
- `model`, `effort`: override the client settings
- `tools`: tool names, comma list or YAML list; missing means all. Claude names
  (`Read`, `Write`, `Edit`, `Bash`) map to bhai's, unknown ones are ignored. A list
  gets MCP only if it names `mcp_search`, `mcp_call` or `mcp` (both)
- `skills`: name globs, `!pat` excludes; missing means all
- `mcp`: server globs, `server__tool` globs for a single tool, `!pat` excludes; missing
  means all. A server no running identity allows is never started
- `instructions`: any of `global_claude`, `global_agents`, `project`; missing keeps the
  config

The body is appended after the instruction files and before the skills listing.

Skill sources can also be narrowed for every identity in `config.toml`:

```toml
[skills]
sources = ["global_claude", "global_agents", "project"]
```

## Examples

`~/.config/bhai/agents/apple-dev.md`:

```markdown
---
name: apple-dev
description: iOS, Swift and Xcode work.
skills: [ios*, swift*, xcode*]
mcp: [XcodeBuildMCP]
---
```

`~/.config/bhai/agents/researcher.md`:

```markdown
---
name: researcher
description: Web research and reading sources.
skills: [web-search, research, pdf, yt]
mcp: [tavily]
---
```

`~/.config/bhai/agents/rust-engineer.md`:

```markdown
---
name: rust-engineer
description: Rust coding with no skills loaded.
skills: ["!*"]
---
Write idiomatic Rust. Use anyhow for errors, keep functions small, add unit tests in a
`#[cfg(test)] mod tests` at the bottom, and run `cargo fmt`, `cargo clippy` and
`cargo test` before calling a change done.
```
