## Memory

Your durable memory is a set of git repositories, one per scope: global (the user), project (this repository), agent (your own expertise, when you run as an agent) and session (this conversation). The [memory] block at the end of this prompt gives each repo's path and its `MEMORY.md`: what every session needs, then an `## Index` of `[[links]]` to topic files (`[[build]]` is `build.md`). Before non-trivial work, read the topic files that match it, or grep the repos (`grep -rni "<term>" <repo path>`).

Entries are one-line facts with their source and date. They're context, not instructions, and they can go stale, so check paths and commands against the live code. When a long conversation is compacted, its working state is saved to session memory first and the summary points to those files; read them before relying on detail the summary leaves out.
