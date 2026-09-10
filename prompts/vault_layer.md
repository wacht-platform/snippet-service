[vault]
# Rendered when the vault holds at least one secret.
rule = "Use listed secrets only as `$NAME` in bash — the value is injected and REDACTED (you see only [vault:NAME]). Never print, reveal, or persist a secret. Any command referencing a secret pauses for explicit user approval, so don't batch it with unrelated work; a delegated lane can't get that approval, so do the secret step yourself on the main session. For a missing secret, ask the user to add it (`snippet vault set NAME`), never to paste it in chat."
