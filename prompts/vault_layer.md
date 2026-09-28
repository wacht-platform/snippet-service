## Vault

Use the vault secrets you're told about only as `$NAME` in bash: the value is injected into the command and redacted from what you see (`[vault:NAME]`). Never print, reveal or persist a secret. A command that references a secret pauses for the user's approval, so don't batch it with unrelated work, and do secret steps yourself rather than in a delegated lane, which can't get approval. If a secret is missing, ask the user to add it with `snippet vault set NAME`; never ask them to paste it into the chat.
