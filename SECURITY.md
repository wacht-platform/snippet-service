# Security policy

## Supported versions

Snippet is in alpha. Security fixes are made on `main` and shipped in the next release, so only the latest release is supported.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Report it privately through [GitHub security advisories](https://github.com/intellinesia/snippet/security/advisories/new). If the problem turns out to sit in the companion app, report it [there](https://github.com/intellinesia/snippet-app/security/advisories/new) or say so in your report and it will be moved.

A useful report includes:

- the version or commit affected, and the operating system;
- the steps to reproduce the problem;
- what an attacker gains, and what access they need first;
- a suggested fix, if you have one.

Remove any real API keys, tokens or personal data from logs before sharing them.

## What happens next

We aim to acknowledge a report within seven days, and will keep you informed while a fix is prepared. Once a fix has shipped we will publish an advisory and credit you, unless you prefer to remain anonymous.

Please give us reasonable time to release a fix before disclosing the problem publicly. This is a volunteer-run project and no bounty is offered.

## Scope

In scope:

- bypassing the `serve` token, or reaching the daemon's API without it;
- reading stored secrets, or finding them in API responses, logs or transcripts;
- one session or client reaching data it should not, through the daemon;
- injection through content the daemon handles, where it leads to action the user did not allow;
- ambient recordings or transcripts being stored or exposed against the recording policy.

Out of scope:

- actions an agent takes with permissions the user gave it. Snippet runs with the user's own access and has no sandbox by design;
- attacks that already need the user's account, machine or `serve` token;
- problems in a model provider or tunnel service, which belong with that provider.
