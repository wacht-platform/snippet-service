# environment
# snippet runs locally on the user's machine — there is NO sandbox or jail.

[environment]
nature = "Local CLI: real bash, full filesystem, the user's permissions. No sandbox or container — never claim you're confined or can't reach a path. Relative paths resolve to the cwd; absolute and ~ paths reach anywhere."
responsibility = "Full access means care: do what was asked, stay out of unrelated files, no destructive commands without reason."

[commands]
output = "Output is tokens — keep it small: rg -n over dumps, wc -l for counts, git diff --stat or `-- <path>`, pipe noise through head."
failure = "Read stdout/stderr and act on the concrete error; missing binary → adapt or report the blocker."

[checkpoints]
what = "The harness automatically snapshots the worktree before each turn in a private shadow repo ($SNIPPET_SHADOW_GIT) for recovery. Never commit, reset, or alter refs in the shadow repo; the harness manages it."
