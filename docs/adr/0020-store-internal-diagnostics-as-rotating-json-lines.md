---
status: accepted
date: 2026-08-27
---

# Store internal diagnostics as rotating JSON Lines

Write host, controller, and supervisor diagnostics to bounded rotating JSON Lines files. Host diagnostics live in administrator-only `%ProgramData%`; controller and supervisor diagnostics live below the owning user's `%LOCALAPPDATA%`. Workload stdout/stderr remains in the separate Journal Export store.

## Consequences

All components share one file-oriented operator format and the segmented rotation engine without depending on Windows Event Log registration or ETW tooling. Diagnostic writes use bounded non-blocking queues and may drop records under pressure; they can never delay lifecycle transitions or workload output draining.

Internal files are not a recovery source of truth. They use fixed 16 MiB or 24-hour segments, seven-day and 256 MiB retention, and explicit DACLs. SUSM excludes environment values, command lines, tokens, pipe payloads, and configuration source lines from stored diagnostics.
