---
status: accepted
date: 2026-08-27
---

# Identify manager sessions with host-issued UUIDs

The LocalSystem host issues a random manager-session UUID for each active `SID + token AuthenticationId` pair and stores the mapping in a host-owned volatile `HKEY_LOCAL_MACHINE` registry key until logoff. The Windows authentication LUID correlates duplicated tokens within one logon but is not itself persisted as the manager-session identity because Windows guarantees LUID uniqueness only until restart.

## Consequences

Host and controller process restarts recover the same manager session, while logoff or a full shutdown prevents stale desired state from becoming current later. The UUID is not an authorization token: every host action still authenticates the Windows token and SID. This volatile mapping is the host's only manager-session state and does not grant it authority over workload definitions or lifecycle policy.

The UUID also names a host-created manual-reset ending event. Signaling that event marks the session as ending for every controller and supervisor that owns the UUID; it does not authorize any other operation.
