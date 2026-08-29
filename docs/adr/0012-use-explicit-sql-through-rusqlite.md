---
status: accepted
date: 2026-08-27
---

# Use explicit SQL through rusqlite

Use `rusqlite` with bundled SQLite and `rusqlite_migration`, not an ORM or async database framework. One synchronous connection behind a mutex matches the single-writer state model and keeps transactions visible instead of expressing lifecycle transitions through a CRUD abstraction.

## Consequences

Storage code maps rows into its own records and then into domain types. Callers use the synchronous `Storage` interface and never receive a connection, transaction, row, or SQL string. SQLite operations may briefly block a Tokio worker; lifecycle command volume is low, and avoiding a worker thread, command enum, and reply channels keeps the implementation smaller.

Use WAL mode with `synchronous = FULL` and foreign keys enabled. Persist lifecycle state that can derive unfinished effects during reconciliation rather than maintaining a generic side-effect outbox beside it.
