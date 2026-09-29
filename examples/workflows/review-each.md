---
name: review-each
description: Review every file the working tree changed, one child per file, then sum up.
budget_tokens: 150000
max_parallel: 3
steps:
  - id: files
    effort: low
    prompt: |
      Run `git diff --name-only` and answer with a JSON array of the paths and nothing
      else. Answer `[]` when nothing changed.
  - id: review
    for_each: files
    identity: rust-engineer
    on_fail: continue
    prompt: |
      Review {{item}} for bugs and missed edge cases, in the light of {{input}}. Read
      the file and its diff. Change nothing.
  - id: verdict
    needs: [review]
    prompt: |
      Split these reviews into what has to be fixed and what can wait.
      {{steps.review}}
---
`review` runs one child per path `files` listed, three at a time, up to the fan-out cap
of 20. A file whose review fails does not stop the others and is simply missing from
what `verdict` reads. A step can also name a `model` of its own, not just an `effort`.
