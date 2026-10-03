# ADR 002: Opaque session ids, and retiring `state.json`

- Status: Implemented (2026-09-15)
- Scope: session identity, session storage, and the filesystem paths derived from them

> Shipped as "canonical ids": a session id is the path minus its filename
> (`<ws>-<key>` for a default session, `<ws>-<key>/conversations/<uuid>` for a
> saved one, `inbox-<agent>` for an inbox). `sessions.legacy_id` holds the
> pre-canonical path-shaped id and is dual-read, so an in-flight reference still
> resolves. Phase 5 deleted the five `state.json` files; the `.meta.json` and
> `.nonces.json` sidecars are still READ by live code and were left in place.

## Problem

A session's id is currently a **filesystem path**, e.g.:

```
snippet-service-61c2d836aee8dc5b/state.json
snippet-service-61c2d836aee8dc5b/conversations/a902d958-....json
```

The file that path names is not read any more — `read_session_state` is
store-only. So the id is a path-shaped string kept alive long after the reason
for its shape disappeared. That has three costs:

1. **The suffix leaks into prompts and gets mangled.** A model handed
   `session:snippet-service-61c2d836aee8dc5b/state.json` drops `/state.json`,
   because the suffix is a filesystem detail with no business in an identifier.
   A bare-id reply was then rejected, and the reply never sent. This is the
   concrete bug that motivated this work.
2. **Ids containing `/` cannot traverse URL path params.** Verified live:
   `/coordination/sessions/<id>/agents` returns **404** for a real id, because
   axum's `Path<String>` does not capture `/`. The mobile client already calls
   these routes with `Uri.encodeComponent`, so the encoding and the route
   disagree today. Four routes are affected.
3. **Identity is not self-describing.** `sessions` already carries
   `workspace_key` and `workspace` — the id duplicates information that is
   already stored, in a form that invites misuse.

## What the file actually is

Measured on the real database:

- `HarnessState` has **no `id` field**. The file does not contain the id.
- The file is gzipped msgpack: 915 KB on disk, 4.5 MB decompressed.
- It is **stale** — newest is 06:54; nothing has written one since the store
  took over.
- Live state lives in `sessions.state_json` (read by `load_session_scalar`),
  with messages and events in their own tables.

The only code that reads a file's *contents* is `read_session_file`, whose sole
callers are the CLI migration commands in `main.rs` — not the daemon. All 32
sessions already have store rows, so there is nothing left to migrate.

**The file is dead. The path string is load-bearing.**

## Id shapes in the database today

The id is already heterogeneous, which tells us nothing depends on a single
uniform shape:

| Count | Shape |
| --- | --- |
| 25 | `<ws>/conversations/<uuid>.json` |
| 5 | `<ws>/state.json` |
| 1 | `inbox-<agent>/state.json` (synthetic, for agent inboxes) |
| 1 | `mission-control` (literal) |

## Decision

**Give every session an opaque id, and derive paths from stored columns.**

The id becomes a UUID. The workspace it belongs to is already in
`sessions.workspace`, and the *kind* of session (default conversation, extra
conversation, agent inbox, mission control) becomes an explicit column rather
than something parsed out of a suffix.

```
sessions
  id            TEXT PRIMARY KEY   -- opaque uuid
  legacy_id     TEXT UNIQUE        -- previous path-shaped id, NULL for new rows
  workspace_key TEXT NOT NULL      -- unchanged
  workspace     TEXT NOT NULL      -- unchanged
  kind          TEXT NOT NULL      -- 'default' | 'conversation' | 'inbox' | 'mission_control'
```

Path derivation moves behind one function, so the 51 existing call sites keep
working while the representation changes underneath them:

```rust
pub fn state_path_for_id(&self, id: &str) -> Option<PathBuf>
```

It resolves the row, then builds `<workspace>/state.json` or
`<workspace>/conversations/<uuid>.json` from `workspace` + `kind`. It also
**accepts a `legacy_id`** so an in-flight reference during cutover still
resolves.

## Migration

### Phase 0 — preflight (no writes)

- Back up the database.
- Assert every session row has non-empty `workspace` and `workspace_key`.
  Verified true today: 0 empty in either column.
- Record the 1 pre-existing FK violation (`session_messages` row 55024). It is
  present in a pristine copy too, so it is not ours to fix here — but it must be
  excluded from "did the migration introduce damage" checks.

### Phase 1 — additive schema

- Add `legacy_id TEXT` and `kind TEXT` to `sessions` (`ensure_column`, the
  existing additive-migration helper).
- Backfill: `legacy_id = id`, and `kind` derived from the id shape.
- No behaviour change. Ship and soak.

### Phase 2 — dual-read resolver

- `state_path_for_id` / `session_id_for_state_path` accept both an opaque id and
  a legacy id. Lookup by `id` first, then by `legacy_id`.
- Fixes the `session:` reply bug on its own, without any id rewrite.

### Phase 3 — id rewrite (the cutover)

Rewrite `sessions.id` to a uuid for every row whose `id` is not already opaque,
keeping `legacy_id` for rollback.

**Verified safe.** On a copy of the real database, in one transaction:

```
PRAGMA defer_foreign_keys = ON
UPDATE session_messages SET session_id = <new> WHERE session_id = <old>
UPDATE session_events   SET session_id = <new> WHERE session_id = <old>
UPDATE request_nonces   SET session_id = <new> WHERE session_id = <old>
UPDATE sessions         SET id = <new>         WHERE id = <old>
COMMIT
```

Result: 77 child rows moved, 0 orphans at the old id, and
`PRAGMA foreign_key_check` reported the **same single pre-existing violation**
as the pristine copy — the rewrite introduced none.

Only two tables have a real FK (`session_messages`, `session_events`, both
`ON DELETE CASCADE`). These nine hold session ids as plain strings and must be
updated in the same transaction, but cannot fail on a constraint:

`assignments.session_id`, `session_leases.session_id`, `handoffs.session_id`,
`tasks.session_id`, `tasks.reporting_session`, `request_nonces.session_id`,
`agent_board.session_id`, `control_settings.mission_control_session_id`,
`recurring_jobs.session_id`.

Also scan the JSON payloads that embed a session id — `notification_journal.payload_json`
and `board_events.payload_json` (`origin_session`, `recipient`) — and rewrite
the fields that name a session.

### Phase 4 — stop deriving identity from paths

- `workspace_state_paths` (the filesystem walk in `conversations.rs`) becomes a
  one-off import tool, not a discovery mechanism.
- `read_session_file` moves behind the CLI migration command only.
- Session creation no longer computes an id from a path.
- The `is_inbox_session_id` / `is_session_id` string parsers are replaced by the
  `kind` column, and the four id shapes collapse to one.
- Fix the four `{session_id}` routes: either accept the encoded id, or move to
  a body/query parameter. Ids no longer contain `/`, which fixes this for new
  ids, but the routes should not depend on that.

### Phase 5 — delete the files

Only after Phase 4 has soaked:

- Delete the 5 `state.json` files. Verified: every one has a store row.
- Delete the orphaned sidecars. Their write paths differ, so they need
  different treatment:
  - `.profile` and `.role` — `write_session_profile` and `write_session_sidecar`
    write to the **store only**. No file is created. Nothing to remove.
  - `.nonces.json` — **read-only** fallback (`serve/mod.rs` reads it, nothing
    writes it). Goes once the shim is removed.
  - `.meta.json` — the remaining gap. `write_session_meta` is store-backed, but
    `freeze_session_activity` still writes the file, and it is called from
    `persist_state_to_file`. That function is only reachable when a session is
    **file-backed** (`backing != BACKING_DB`), which needs a state file with no
    store row. None exist, so nothing has been written since the store took over
    — but the code path is still live. Phase 5 must **remove the writer**, not
    only the files, or a future file-backed session would silently recreate
    them.

## Rollback

`legacy_id` is the whole rollback story. Before Phase 4, reverting the binary is
enough: the old code reads `id`. After Phase 5 the files are gone, so a rollback
would restore from the Phase 0 backup.

## Risks

| Risk | Mitigation |
| --- | --- |
| An id rewrite that half-applies | One transaction, `defer_foreign_keys`, `foreign_key_check` gate before commit |
| A reference we did not find | `legacy_id` dual-read through Phase 3; grep the payload JSON columns |
| Clients caching a path-shaped id | Return both `id` and (temporarily) `legacy_id` in session API responses |
| Confusing this with `state_json` | It is not: `state_json` is the live scalar state and stays. Only the *file* is retired. |

## What this does not change

- `sessions.state_json` — live session state, unrelated to the file.
- `workspace` / `workspace_key` — still the workspace's own identity.
- The workspace *directory* layout. Paths are still derived, just no longer
  used *as* ids.
