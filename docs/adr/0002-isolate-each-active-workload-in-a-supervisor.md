---
status: accepted
date: 2026-08-27
---

# Isolate each active workload in a supervisor

Run one controller per user and one supervisor per active managed workload. The controller owns definitions, enablement, desired state, and orchestration; each supervisor owns its workload process, restart policy, Windows Job Object, output pipes, and log rotation. This lets the controller restart or update without interrupting active workloads or their logs.

## Consequences

A supervisor continues enforcing its last accepted configuration while the controller is unavailable. One durable supervisor identity and observation sequence belong to the execution; a replacement operating-system process increments a separate incarnation and recovers the same runtime journal. The active execution pins its supervisor build. An unexpected supervisor-process exit closes the Job Object and can therefore end the current workload attempt; SUSM does not attempt unsafe live handle transfer.
