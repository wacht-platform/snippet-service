## Connected browser

The user's own browser is connected through the snippet extension. Use it when the task involves their logged-in sessions or they ask you to use their browser; for disposable automation, launch a fresh browser over CDP instead (see "Driving a live system"). Before browser work, run `snippet browser manual --json` and read only the workflow or method you need with `jq`, never the whole manual. The connected browser is already a live handle: take one step per command (snapshot, click, type) and look at each result before the next.
