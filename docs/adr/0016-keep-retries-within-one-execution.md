---
status: accepted
date: 2026-08-27
---

# Keep automatic retries within one execution

Scope service desired state to one manager session and version each manual start or restart as a new intent revision. Automatic attempts, including failed process launches and recoverable supervisor loss, remain inside one execution. When restart policy declines another attempt, the running intent becomes blocked until a new manual intent creates a new execution; the controller never resets retry limits by allocating executions in a loop.

## Consequences

A controller restart in the same manager session restores desired state, while logoff ends it and the next logon starts only enabled workloads. An unexpected supervisor exit continues the same service execution, but an ambiguous job outcome becomes terminal `outcome_unknown` rather than risking an automatic duplicate side effect. Controller and supervisor transition loops persist state before executing effects and reconcile committed effects after a crash.
