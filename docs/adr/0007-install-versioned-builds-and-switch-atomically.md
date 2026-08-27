---
status: accepted
date: 2026-08-27
---

# Install versioned builds and switch atomically

`susm upgrade <directory|zip>` accepts a local user bundle, validates its TOML manifest, file hashes, Windows architecture, schema range, and protocol compatibility, installs the build under the invoking user's `%LOCALAPPDATA%\Programs\susm\versions\<version>-<manifest-hash>`, and atomically switches that user's `current.susm-install`. The host then restarts the user's controller, which adopts compatible supervisors from older versions without restarting their workloads. V1 does not download URLs.

## Consequences

Keep old versions while a live supervisor uses them. Provide `susm versions`, `susm rollback`, and `susm upgrade gc`; garbage collection must never remove an in-use version. The release bundle groups the user bundle and host image for distribution, but updating the stable host remains a separate elevated operation.

After selection, the host retries the chosen controller indefinitely with capped backoff even if it has not yet become ready. Automatic database restoration and build rollback are deliberately omitted; recovery requires an explicit `susm rollback` or a fixed build.

A non-terminal execution pins the supervisor build that owns its runtime-journal format, not merely the currently live process. If its supervisor dies after a controller upgrade, the replacement launches from the pinned version. Controller-to-supervisor adoption depends on protocol capabilities rather than equal build versions.

Official builds within protocol major v1 must preserve baseline adoption compatibility with every earlier official v1 supervisor. Upgrade validation treats that as a release invariant and refuses an invalid manifest before switching versions; it never repairs incompatibility by silently restarting workloads.
