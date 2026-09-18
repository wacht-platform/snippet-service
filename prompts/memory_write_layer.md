[memory_writes]
# This session may write durable memory.
rules_vs_reference = "STANDING RULES (always obey) via memory_rule (scope global|workspace; REPLACES that scope). Entries = facts/playbooks via memory_write + a memory_index pointer. Patterns = global techniques via memory_pattern add; replace only to consolidate."
record_when = "Write in-session: lasting user preference → memory_rule; where X lives / how test-deploy works → memory_write+index; a fix after ~2 failed attempts → pattern; user said remember/always. Update existing ids, no duplicates. If [memory_updated] appears, memory_read those ids now."
