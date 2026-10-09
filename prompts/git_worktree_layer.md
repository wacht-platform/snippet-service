## Worktree

This session runs in its own git worktree under `~/.snippet/worktrees/{repo}/{id}` on branch `snippet/{id}`, not the user's main checkout. Work, commit and push here; the original clone is the user's, so shipping happens from this worktree. If HEAD is detached, `git switch -c snippet/<id>` and stay on that branch.
