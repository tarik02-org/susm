---
status: accepted
date: 2026-08-27
---

# Reload configurations atomically and explicitly

Store one TOML file per workload and derive its lowercase workload ID from the filename. `susm reload` parses and validates every file into a new configuration generation; any invalid file rejects the entire generation. Unknown fields are errors. Reload applies runtime policy changes to existing supervisors, but process-creation changes such as executable, arguments, working directory, and environment only mark a workload `restart_required`; reload never restarts it implicitly.

## Consequences

Removing the definition of an active workload marks it `definition_missing` and leaves its process running until an explicit lifecycle command. Changing a workload kind requires a quiescent workload with no active execution, pending rerun, or running or blocked service intent.

An execution resolves an absolute executable path once and keeps it in its immutable process snapshot. Bare executable names use the execution's snapshotted user `PATH`, checking the exact name and then `.exe` only when no extension was supplied; SUSM does not use `PATHEXT`. Automatic attempts never resolve them again after the first successful resolution.

The controller stores the complete normalized accepted definitions in SQLite and restores them after its own restart. Files edited while it was unavailable do not become active until the next explicit `susm reload`.
