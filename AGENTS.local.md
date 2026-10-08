# Local agent instructions

## Required landing workflow

- Every valid update must be committed after formatting and relevant tests/builds pass. Do not leave completed work uncommitted.
- Stage only the changes belonging to the task. Never commit, discard, or overwrite unrelated work.
- When working on `main`, commit directly on `main`. Do not create a branch unless asked.
- When working in a worktree, bring the branch up to date with `main`, resolve conflicts, verify the result, and merge the completed commits into `main`. A commit left only in a worktree is not completed work.
- Once a worktree's work is merged into `main`, remove the redundant worktree and its temporary branch. First confirm all its commits are on `main`, it has no uncommitted or untracked work, and no session is still using it. Never force removal or delete unmerged work.
- Push the completed `main` commits to GitHub with a normal push. Never force push unless explicitly asked. If the push is rejected, resolve safely without rewriting published history.
- Do not report a task as complete until verification, commit, merge (if applicable), safe worktree cleanup (if applicable), and the GitHub push have succeeded. If any step is blocked, state the blocker and what remains.
