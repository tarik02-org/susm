---
status: superseded by ADR-0013
date: 2026-08-27
---

# Store raw logs by execution and attempt

Store unmodified stdout and stderr separately under `%LOCALAPPDATA%\susm\logs\<workload-id>\<execution-id>\attempt-000\stdout-000.log` and `stderr-000.log`. Supervisors own rotation and retention so logging continues while the controller is unavailable. Segment numbering makes rotation explicit and keeps attempts from different restarts distinguishable.

## Consequences

Log write or rotation failures do not terminate the workload. The supervisor records dropped byte counts and exposes the degraded logging state so operators can distinguish incomplete output from a quiet process.
