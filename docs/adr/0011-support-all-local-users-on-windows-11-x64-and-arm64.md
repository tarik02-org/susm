---
status: accepted
date: 2026-08-27
---

# Support all local users on Windows 11 24H2+ x64 and ARM64

Target native x64 and ARM64 builds of Windows 11 24H2 or newer. The machine-wide host can serve every registered local interactive user while excluding built-in and service identities. It keeps at most one controller per user SID. Workstation lock and session disconnect do not affect the controller; user logoff triggers graceful workload shutdown and controller exit.

## Consequences

IPC names, runtime discovery, and host bookkeeping are keyed by SID rather than session ID, although the host still tracks the Windows session used to obtain the user's token and lifecycle notifications. Concurrent interactive sessions for the same SID are outside v1: the host does not start a duplicate controller and reports the condition explicitly. User configuration, state, logs, and version selection remain in that user's profile. Release manifests and upgrade validation identify the architecture. The 24H2 baseline lets supervisors create private console sessions without a visible console window through `AllocConsoleWithOptions`. V1 does not promise earlier Windows 11 releases, Windows 10, Windows Server, Windows multi-session, x86, or emulated cross-architecture payloads.
