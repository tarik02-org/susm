---
status: accepted
date: 2026-08-27
---

# Use an append-only supervisor runtime journal

Each supervisor durably records execution checkpoints and sequenced observations in one append-only, checksummed, framed Protobuf runtime journal. It flushes a transition frame before executing external effects. A controller acknowledges an observation only after applying it and advancing its cursor in one SQLite transaction. The supervisor can then atomically compact acknowledged frames into a new checkpoint.

## Consequences

A supervisor continues attempts while the controller is unavailable without losing intermediate attempt history. One file orders runtime state and observations, avoiding an impossible atomic commit between a state snapshot and a separate event spool. Recovery ignores an incomplete tail but rejects corruption before it. Runtime journals remain recovery data owned by supervisors; SQLite remains the controller authority for desired state and accepted history.
