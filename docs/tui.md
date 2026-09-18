# The terminal UI

Assistant text is rendered as markdown: headings, lists, emphasis, code blocks with their
language tag wrapped to the width. Rendering is display only; what is stored in history
and written to the session file is the model's text unchanged.

A running command streams its output into the transcript as it arrives, so a long build
or test run can be watched rather than waited on. Tool output collapses to a few lines;
click it to expand.

## Keys

| key | what it does |
| --- | --- |
| `enter` | send |
| `shift+enter`, `alt+enter`, `ctrl+j` | newline (most terminals cannot report shift+enter) |
| `ctrl+p`, `ctrl+n` | previous and next prompt from the history |
| `up`, `down` | scroll the transcript, or move between lines while the input is multi-line |
| `pgup`, `pgdn` | scroll a page |
| `shift+tab` | cycle the permission mode |
| `ctrl+t` | show every token badge |
| `ctrl+y` | copy the selection, or the whole input when nothing is selected |
| `ctrl+v` | insert what the clipboard reads back |
| `esc` | interrupt the turn |
| `ctrl+c` | interrupt the turn, or quit when idle |
| `ctrl+d` | quit on an empty input |

At an approval prompt: `y` runs it once, `a` remembers that exact call, `p` remembers its
prefix, and `n`, `r` or `esc` rejects.

Emacs and macOS motions work in the input (`alt+b`/`alt+f` and `alt+←`/`alt+→` by word,
`cmd+←`/`cmd+→` to the ends, `ctrl+w`, `cmd+backspace`). Prompts are kept in
`~/.config/bhai/history.jsonl`, the last thousand of them, shared across projects.

## Mouse

The wheel scrolls, the scrollbar drags, clicking tool output expands it, clicking an
entry pins its token badge, and clicking a choice answers an approval prompt. Hovering an
entry shows its badge, hovering the rate-limit segment of the status bar shows when each
window resets.

A left drag over the transcript selects the text it covers, a double click takes a word
and a triple the line; `ctrl+y` copies it, `esc` clears it, and a new prompt clears it.
Capture takes click-drag away from the terminal, so the terminal's own selection needs
shift (or option) held; `/mouse` turns capture off and on for the times that is what you
want.

## The diff pane

`/diff` opens the working tree against HEAD: the changed files on the left, the selected
file's hunks on the right, tabs expanded so indentation survives. `tab` switches the
focus, `j`/`k` or the arrows move, `pgup`/`pgdn` page, `r` refreshes, `q` or `esc`
closes. It is a read-only view; git runs directly without an approval prompt.

The terminal is restored on exit and on panic, keyboard, paste and mouse modes included.
