# ADR 001: SQLite agent coordination control plane

- Status: Accepted for implementation
- Scope: specialized agents, coordination board, assignments, handoffs, and session turn leases

## Decision

Use one SQLite database, accessed through `rusqlite` with the `bundled` feature:

`~/.snippet/mission-control/coordination.sqlite3`

The database is authoritative from first initialization. This is a clean-slate feature: do not import, mirror, dual-write, or preserve the existing JSON Mission Control task/session contract.

## Operating profile

- WAL journal mode.
- Foreign keys enabled.
- Busy timeout configured on every connection.
- Synchronous durability configured explicitly; production defaults to `NORMAL` unless a stricter setting is selected.
- Short `BEGIN IMMEDIATE` transactions for state transitions.
- No model, filesystem, network, or websocket operation inside a transaction.
- One daemon owns the database file. Multiple independent daemons sharing the file are unsupported.

SQLite supports concurrent readers and one serialized writer. This is sufficient for thousands of logical agents in one daemon when writes are short, partitioned logically, and bounded by quotas.

## Event and ownership rules

Every state-changing transaction writes its state mutation and transactional outbox record together. Board events are ordered per partition, not globally. Clients replay by partition cursor and deduplicate by event ID.

A shared session has one active turn lease. Lease transitions increment a fencing token. Protected worker mutations must present the current lease ID and fencing token; stale workers are rejected.

Ownership transfer requires an immutable handoff. The successor acknowledges the handoff content hash before acquiring the next lease.

## Rejected alternatives

- DuckDB: analytical workload and single-process write model are not appropriate for coordination mutations.
- redb: key-value storage would require rebuilding relational constraints, indexes, migrations, and query behavior.
- libSQL/Turso: replication and remote-primary features are outside the local-only requirement.
- PostgreSQL or another server database: explicitly out of scope for this implementation.
