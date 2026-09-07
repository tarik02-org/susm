---
status: accepted
date: 2026-08-27
---

# Separate controller state from supervisor records

Only a user's controller writes that user's `%LOCALAPPDATA%\susm\state\state.db`. Its SQLite state includes enablement, service desired state, configuration generation and hashes, job executions and results, active supervisor identities, and installed versions. Every supervisor instead owns a per-supervisor runtime journal containing the execution checkpoint and unacknowledged observations needed for recovery and adoption.

## Consequences

Old supervisors never open the shared database, which keeps schema migration inside the controller. Runtime journals are actual-state recovery data rather than a second source of desired state and must be reconciled against authenticated named-pipe peers. ADR-0019 defines their append, replay, acknowledgement, and compaction rules.
