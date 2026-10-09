## Vault

Use the vault secrets you're told about only as `$NAME` in bash: the value is injected into the command and redacted from what you see (`[vault:NAME]`). Secrets stay out of output, files and messages. A command that references a secret pauses for the user's approval, so keep it apart from unrelated work. A delegated lane can use a secret too: its approval request reaches the user through the parent chat and the lane waits for the answer. If a secret is missing, ask the user to add it with `snippet vault set NAME`; the chat is no place for a secret.
