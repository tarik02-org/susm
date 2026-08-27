---
status: accepted
date: 2026-08-27
---

# Anchor the controller with a Windows service host

Install a small machine-wide `susm-host` Windows service under the Service Control Manager as LocalSystem. It observes logon and logout, starts or restarts a version-selected `susmd` with each registered local interactive user's token, and coordinates session shutdown. The host does not parse workload TOML, manage workloads, or execute arbitrary user-provided commands as LocalSystem.

## Consequences

Installing or replacing the host is an infrequent elevated operation. Controllers and supervisors stay unprivileged and versioned under their owning user's local application data. The host owns only registration and volatile manager-session mappings; workload state remains per-user. A Task Scheduler bootstrap may be added later as a no-elevation fallback, but is not the v1 lifecycle authority.

Each volatile manager session also owns a manual-reset Windows ending event readable by that user. The host signals it at logoff so supervisors can apply their own persisted stop policy even if the controller is unavailable; this event carries no workload-specific authority.

The host retries an unexpectedly exited controller indefinitely with deterministic 250-millisecond to 30-second backoff, reset after 5 minutes alive. Active supervisors continue independently while the controller is unavailable.

The installer also configures SCM to restart the host itself after 1 second, 5 seconds, and then every 30 seconds, with its failure count reset after 5 minutes stable.

The host obtains the interactive user's primary token through `WTSQueryUserToken`, builds a non-inherited Unicode environment, and calls `CreateProcessAsUserW` with an absolute validated controller image. Required token privileges stay confined to this small host; it never passes token handles through IPC or launches a caller-supplied path.
