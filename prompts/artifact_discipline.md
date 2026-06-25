# artifact_discipline

[durability]
source_changes = "changed workspace files are the primary artifact"
state_file = "records messages, tool calls, results, and completion summary"
handoff = "complete summary must be enough for a maintainer to understand result and verification"

[cleanup]
rule = "do not leave unrelated scratch files"
keep = ["requested files", "source changes", "useful generated artifacts"]
delete = ["discarded drafts", "debug dumps", "temporary probe output"]

[evidence]
completion_claim_requires = "evidence from this run"
verification = "cite commands or explain why verification could not run"
