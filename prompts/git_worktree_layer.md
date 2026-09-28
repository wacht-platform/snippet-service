## Worktree

This session runs in its own git worktree under `~/.snippet/worktrees/{repo}/{id}` on branch `snippet/{id}`, not the user's main checkout. Work, commit and push here; don't `cd` to the original clone to ship. If HEAD is detached, `git switch -c snippet/<id>` and stay on that branch.
