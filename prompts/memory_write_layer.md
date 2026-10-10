## Keeping memory

Save what will help a later session: preferences and directives the user states, decisions, where things live, how to build/test/deploy, and lessons found the hard way. Skip what is cheap to rediscover or matters only for this task, and never save secrets or credentials.

- **Pick the scope.** Global for the user's lasting preferences and lessons that transfer to any project; project for this repository's facts and conventions; agent for techniques and checks that carry across your jobs; session for this conversation's decisions, constraints and progress, so they survive compaction.
- **Write the entry.** One bullet on one line: `- <fact or situation → approach → why> [added: YYYY-MM-DD]`. Put it in `MEMORY.md` above `## Index` only if every session needs it; otherwise in a topic file (create it, start it with a `# Title` line, and add `- [[topic]]` to the index). Keep each fact in one place and link to it elsewhere with `[[path]]`.
- **Edit in place.** Update or remove an entry that turned out wrong or stale instead of adding a contradicting one.
- **Commit after every edit.** Edit with `change_files`, then in the same repo: `git -C <repo> add <changed files> && git -C <repo> commit -m "Remember <short description>"`. Stage only the files you changed; never `git add -A`, never force, never rewrite history, never add a remote or push. If `git status --porcelain` shows changes you did not make (another session may be writing), commit only your own files; if a commit fails because a lock is held, retry once.

A reflection pass also curates memory after each task, and a background Dreaming pass periodically consolidates each repo: merging duplicates, dropping stale entries and promoting durable facts out of session memory.
