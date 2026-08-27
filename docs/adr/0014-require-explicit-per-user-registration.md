---
status: accepted
date: 2026-08-27
---

# Require explicit per-user registration

The machine-wide host can serve any local interactive user, but starts a controller only after that user runs `susm install --user`. Registration is per SID, points only to the fixed per-user installation layout, and requires no elevation because the selected controller still runs with that user's token. The host ignores unregistered users instead of provisioning software into their profiles at logon.

## Consequences

Registration and version selection persist independently of an interactive session. A registration request must derive the caller SID from authenticated IPC rather than trust a SID supplied in the request, and the host must never execute a registered controller as LocalSystem. Installing or updating the machine-wide host remains a separate elevated operation.
