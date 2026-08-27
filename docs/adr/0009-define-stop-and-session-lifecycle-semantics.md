---
status: accepted
date: 2026-08-27
---

# Define stop and session lifecycle semantics

Support three stop modes. `ctrl-break` sends `CTRL_BREAK` to the workload's private console process group, waits for the configured timeout, then terminates its Windows Job Object. `command` starts a direct, shell-free stop command as the same user but outside the workload Job Object, waits for the workload to exit, and still terminates it at the deadline regardless of the command's exit code. `kill` terminates the Job Object immediately.

## Consequences

Stop is idempotent, and stopping during restart backoff cancels the pending retry. Restarting a service closes its current execution and creates another. Workstation lock and RDP disconnect have no effect; logout gracefully stops services and cancels jobs, while persistent enablement causes the appropriate service starts and at most one enabled-job execution at the next logon. Lingering after logout is deferred.

The LocalSystem host signals a manager-session ending event observed directly by the controller and every supervisor. Session shutdown therefore does not depend on a live controller RPC path.

Logoff allows 30 seconds total. Workload graceful phases use at most the first 25 seconds; supervisors reserve the remaining time for forced Job Object termination and durable terminal state. A graceful machine shutdown signals every manager session through the same path; the host service requests a 45-second SCM preshutdown window.
