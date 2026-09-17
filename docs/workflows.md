# Workflows

A workflow is a handful of child agent steps run in dependency order under one token
budget. Each step is an ordinary subagent: same approval pipeline, same sidechain
transcript, same per-child cache key and usage attribution.

Only you start a workflow. It is not a model tool, so the agent cannot start one itself.

- `/workflows` in the TUI lists the definitions
- `/workflow <name> [input]` runs one
- `bhai --workflow <name> [input]` runs one without the TUI; add `--workflow-yes` to
  answer the confirmation, or the plan is only printed. Any other approval is rejected,
  since nothing is there to answer it

Before anything runs, the confirmation prompt shows the step count, the identity of each
step and the budget. While it runs, child usage is summed: once the budget is spent no
more steps are launched, and the report says which ones did not run.

## Files

Markdown with frontmatter. Searched lowest precedence first, later ones replace earlier
ones by `name`:

- `~/.config/bhai/workflows/*.md`
- `<project>/.bhai/workflows/*.md`

Keys:

- `name`, `description`
- `budget_tokens`: tokens the whole run may spend, 200000 by default
- `max_parallel`: steps launched at once, 1 by default, never more than the child agent
  fan-out cap of 3
- `steps`: a list of `id`, `identity` (`general` by default), `prompt`, `needs` (step
  ids, optional) and `on_fail` (`stop` by default, or `continue`)

The body is prose shown by `/workflows`.

A prompt may use `{{input}}` and `{{steps.<id>}}`, which is that step's final message.
A step may only read a step it `needs`. A cycle, a missing dependency or a placeholder
with no value is a load error, so a broken definition never starts.

## Example

[`workflows/review.md`](workflows/review.md), copied to `.bhai/workflows/review.md`:

```markdown
---
name: review
description: Summarise the working tree diff and the test run, then review the changes.
budget_tokens: 120000
max_parallel: 2
steps:
  - id: diff
    prompt: |
      Run `git diff` and summarise what changed, file by file. Scope: {{input}}
  - id: tests
    on_fail: continue
    prompt: |
      Run the project's test command once and report the failures, with no fixes.
  - id: review
    identity: rust-engineer
    needs: [diff]
    prompt: |
      Review these changes for bugs, missed edge cases and anything the tests do not
      cover. Do not change any files.
      {{steps.diff}}
---
`diff` and `tests` run together; `review` waits for the diff. A failing test run does
not stop the review.
```

Run it with `/workflow review only the parser files` or
`bhai --workflow review "only the parser files" --workflow-yes`.
