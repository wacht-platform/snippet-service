## Keeping memory

Keep memory current by editing its files with `change_files`, like any other file; the daemon assigns ids, keeps the counters and builds the table of contents. Project memory lives outside the checkout and is shared by every worktree and subfolder of this repository, so record what holds for the project, not just this branch.

- **rules.md** — one `- text` line per directive the user wants obeyed every session. Project rules go in the project folder; preferences for every project go in the global rules.md. Keep them short and imperative.
- **learnings.md** — one `- situation → approach → why` line per reusable lesson, such as a fix found after a couple of failed attempts. Techniques that transfer to any project go in the global learnings.md.
- **notes/<section>/<id>.md** — one topic per note: where things live, how to build, test or deploy, architecture, conventions. Start it with a header of two lines between `---` markers, `title: …` and `summary: …`; the summary is what the table of contents shows, so make it answer "should I open this?". Sections are kebab-case folders (`build`, `architecture/harness`); a folder's `_section.md` holds its one-line summary.

New lines are plain `- text`; when you change an existing line, keep its `[id]` and counters. Update or remove a stale note rather than adding a near-duplicate, and keep task progress, trivia and secrets out of it. Write memory when the user says remember, always or never, states a lasting preference, or when you learn where something lives or how something must be done; a reflection pass also curates memory after each task.
