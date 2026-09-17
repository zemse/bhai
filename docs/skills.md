# Skills

A skill is a `<name>/SKILL.md` directory: frontmatter with `name` and `description`, then
a body of instructions and whatever files the body refers to. Only the name and the
description go in the system prompt; the body is loaded on demand when the model calls
the `skill` tool, so a long skill costs nothing until it is used.

`/skills` lists what is loaded, where each came from and what its listing line costs.

## Where they come from

- `~/.claude/skills/*`
- `~/.agents/skills/*`
- `<project>/.claude/skills/*`
- `<project>/.agents/skills/*`

Lowest precedence first, so a project skill replaces a global one of the same name.
Symlinked skill directories are followed. Descriptions longer than 250 characters are cut
in the listing.

## Narrowing them

Turn them off in `config.toml` with `skills = false`, or pick the sources:

```toml
[skills]
sources = ["global_claude", "global_agents", "project"]
```

An identity narrows further with name globs, `!pat` to exclude. See
[identities](identities.md).
