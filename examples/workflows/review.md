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
