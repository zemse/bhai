# Subagents

The `agent` tool runs a task in a child with a fresh context and returns only its final
message, so a long search or a noisy build does not land in the parent's history. The
call takes an `identity` (`general` by default), a short `description` and the complete
`prompt`.

A child has no `agent` tool of its own, so the tree is one level deep, and at most three
children run at once. Each one writes its own transcript beside the parent's, in
`.bhai/sessions/<session id>/child-<id>.jsonl`.

## Watching one, and talking to it

A running child is a row in the panel above the prompt, and `ctrl+o` or a click goes
inside it. What the child says lands there rather than in the parent's transcript, which
keeps the call and its result and nothing else. Inside the pane the prompt types into
that child: the message joins its history before its next model call and is written to
its transcript like any other, so it is there in the context of every call after it. One
typed while the child is writing its final answer keeps it going rather than being
answered too late. See [the tui](tui.md) for the keys.

`GET /children` on the debug server lists the same rows with each child's transcript, and
`POST /steer` posts a message to one, which is how the flow is tested without a terminal.

## Permissions and tokens

The call itself needs no approval; every tool call the child makes goes through the
session's policy, so an approval prompt from a child is answered in the same place as any
other, tagged with the child it belongs to rather than hidden inside its pane.

Children of one identity share a cache key of their own (`<session>-<identity>`), so a
second child under the same identity reuses the cached prefix of the first. Their usage
is summed into the session totals and shown separately in the `child:` part of the
parent entry's token badge.

## What comes back

On success the parent gets the child's final message, its step count and its usage. On
failure it gets the reason and how far the child got, so the model can decide whether to
retry or do the work itself.

For several children in dependency order under one budget, see
[workflows](workflows.md).
