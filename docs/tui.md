# The terminal UI

Assistant text is rendered as markdown: headings, lists, emphasis, code blocks with their
language tag wrapped to the width. Rendering is display only; what is stored in history
and written to the session file is the model's text unchanged.

A running command streams its output into the transcript as it arrives, so a long build
or test run can be watched rather than waited on. Tool output collapses to a few lines;
click it to expand.

A prompt typed while the agent works joins a queue rather than being refused. It shows
dim in the transcript, the status bar counts what is waiting, and each one runs as a turn
of its own in the order it was typed. `/queue` lists them and `/queue clear` drops them;
an interrupt drops them too, because stop means stop.

## The command menu

Typing `/` as the first character opens a menu above the prompt. It filters as the name
is typed, `up`/`down` pick a row, `tab` completes the name and `enter` runs it, or leaves
a trailing space when the command takes more input. `esc` shuts the menu without clearing
what was typed, and the next keystroke opens it again.

The highlighted row is also offered in the prompt itself: the rest of its name is drawn
grey just past the cursor, so `/co` reads as `/compact` with `mpact` dim, and `tab` fills
it in. Picking another row with the arrows changes what is offered. It shows only with
the cursor at the end of what is typed, so grey text never sits mid-prompt.

The session's skills are listed in the same menu after the commands, so `/<skill>
[input]` is a prompt: it asks the agent to use that skill, which it then loads through
the `skill` tool. The transcript shows the line as it was typed, not the sentence it is
sent as. A `/word` that is neither a command nor a skill is refused rather than sent; a
first word that is not name-shaped, such as `/usr/bin/env is missing`, is a prompt like
any other.

The top bar carries only session facts (model, token totals, cache and rate-limit
headroom) plus the answer keys while an approval waits. The spinner, the queue count and
the interrupt key sit on their own row just above the prompt, where the eye already is,
and that row is there only while a turn is actually running. The rest of the keys are in
`/help`.

## Keys

| key | what it does |
| --- | --- |
| `enter` | send |
| `shift+enter`, `alt+enter`, `ctrl+j` | newline (most terminals cannot report shift+enter) |
| `up`, `down` | pick a row in the `/` menu, move between a multi-line prompt's rows, else walk the prompt history |
| `ctrl+p`, `ctrl+n` | the same walk through the history, from anywhere in the prompt |
| `ctrl+up`, `ctrl+down` | scroll the transcript a line |
| `pgup`, `pgdn` | scroll a page |
| `shift+tab` | cycle the permission mode |
| `ctrl+t` | show every token badge |
| `ctrl+y` | copy the selection, or the whole input when nothing is selected |
| `ctrl+v` | insert what the clipboard reads back |
| `esc` | interrupt the turn |
| `ctrl+c` | interrupt the turn, or quit when idle |
| `ctrl+d` | quit on an empty input |
| `tab` | fill in the grey completion, while the `/` menu is open |

At an approval prompt: `y` runs it once, `a` remembers that exact call, `p` remembers its
prefix, and `n`, `r` or `esc` rejects.

A history walk starts from what is typed: `up` keeps it as the draft, and walking back
down past the newest prompt restores it. In a prompt of several rows the arrows move
between the rows first, and walk the history only off the top or the bottom of it.

Emacs and macOS motions work in the input (`alt+b`/`alt+f` and `alt+←`/`alt+→` by word,
`cmd+←`/`cmd+→` to the ends, `ctrl+w`, `cmd+backspace`). Prompts are kept in
`~/.config/bhai/history.jsonl`, the last thousand of them, shared across projects.

## Mouse

The wheel scrolls, the scrollbar drags, clicking tool output expands it, clicking an
entry pins its token badge, and clicking a choice answers an approval prompt. Hovering an
entry shows its badge, hovering the rate-limit segment of the status bar shows when each
window resets.

A left drag over the transcript selects the text it covers, a double click takes a word
and a triple the line. Releasing the drag copies it to the clipboard on its own, with a
`copied N chars` note on the input's border for a few seconds rather than a line in the
transcript; a click that selects nothing copies nothing. `ctrl+y` still copies on demand
(the selection, or the whole input when there is none), `esc` clears the selection, and a
new prompt clears it.
Capture takes click-drag away from the terminal, so the terminal's own selection needs
shift (or option) held; `/mouse` turns capture off and on for the times that is what you
want.

## The diff pane

`/diff` opens the working tree against HEAD: the changed files on the left, the selected
file's hunks on the right, tabs expanded so indentation survives. `tab` switches the
focus, `j`/`k` or the arrows move, `pgup`/`pgdn` page, `r` refreshes, `q` or `esc`
closes. It is a read-only view; git runs directly without an approval prompt.

The terminal is restored on exit and on panic, keyboard, paste and mouse modes included.
