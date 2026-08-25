# Tasks

- [ ] Compact or trim history when the context window fills up; right now a long session
      just starts failing.
- [ ] Persist sessions to disk and add a `--resume`.
- [ ] Remember approvals: an "always allow this exact command" or per-prefix allowlist.
- [ ] Show rate-limit headroom from the response headers in the status bar.
- [ ] Multi-line input (shift+enter), and an input history on `ctrl+p`/`ctrl+n` (`↑`/`↓`
      now scroll the transcript).
- [ ] Stream the running command's output into the transcript instead of waiting for exit.
