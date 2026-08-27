# Host bootstrap and user registration

This document defines installation and startup of the machine host, durable user registration, manager-session discovery, and controller launch.

## Machine host installation

The release bundle contains the native-architecture host image at `host\susm-host.exe`. From an elevated PowerShell, `susm-host.exe install` copies itself into an immutable version directory below `%ProgramFiles%\SUSM\host\versions`, creates or updates the `SUSMHost` service to point at that absolute image, applies the required DACL and SCM recovery settings, and starts it. `susm-host.exe uninstall` intentionally stops and deletes only the service and host program files; it never removes per-user data.

The service starts automatically as LocalSystem and accepts session-change and preshutdown notifications. Its required-privilege list includes `SeTcbPrivilege`, `SeAssignPrimaryTokenPrivilege`, and `SeIncreaseQuotaPrivilege`; it does not enable a privilege until the operation that needs it. The control handler only validates and enqueues SCM notifications so it never blocks the service dispatcher on I/O or process shutdown.

The installer configures a 45-second SCM preshutdown timeout. On `SERVICE_CONTROL_PRESHUTDOWN`, the host signals every live manager-session ending event in parallel, reports `SERVICE_STOP_PENDING`, allows the existing 30-second workload shutdown contract, finalizes host diagnostics, and reports stopped. Ordinary host upgrade restarts do not end manager sessions; existing controllers and supervisors survive the short host absence.

Host upgrades remain explicit elevated operations. They install a new immutable directory, update the service image path, and restart the service. They are independent of `susm upgrade`, which changes only one user's CLI, controller, and supervisor build.

## Registration

`susm install --user [directory-or-zip]` performs the per-user install from `docs/installation-and-upgrade.md`, then calls the authenticated host pipe. When source is omitted, the CLI uses the bundle beside its own executable. The host derives the SID and Windows session from the caller token and creates:

```text
HKEY_LOCAL_MACHINE\SOFTWARE\SUSM\Registrations\<sid>
```

The key records only registration state and format version. It contains no caller-provided executable or profile path. The selected build always comes from `%LOCALAPPDATA%\Programs\susm\current.susm-install` for the authenticated user's profile.

Registration requires no elevation because the LocalSystem host performs the fixed-shape registry write after authenticating the local caller. It immediately starts a manager session when the caller belongs to the one eligible interactive session for that SID. If no eligible session exists, registration remains dormant until a later logon.

`susm uninstall --user` first asks the host to end the current manager session, waits through the normal 30-second shutdown contract, and removes the registration. It leaves user configuration, state, logs, and installed versions intact. V1 has no recursive purge command.

## Session discovery

On service startup and each logon notification, the host enumerates interactive Windows sessions and obtains the logged-on user's primary token with `WTSQueryUserToken`. It verifies the token SID, token type, authentication LUID, elevation restrictions, and Windows session ID before using it. Every token and process handle has one RAII owner and is never exposed through RPC.

The host supports one interactive Windows session per SID in v1. If another eligible session for the same SID appears concurrently, it reports the conflict and starts no second controller.

The manager-session mapping lives below a volatile host-owned registry key:

```text
HKEY_LOCAL_MACHINE\SOFTWARE\SUSM\Runtime\ManagerSessions\<sid>
```

The volatile value set contains the authentication LUID, manager-session UUID, Windows session ID, controller PID, and controller creation time. The host accepts the mapping only when both the SID and authentication LUID still match the enumerated logon.

It stores the host-issued manager-session UUID, Windows session ID, ending-event name, and the last controller PID plus creation time. Volatile keys survive a host process crash but not reboot. On recovery, the host enumerates Windows sessions and validates every recorded process identity before adopting it or launching a replacement.

## Controller launch

The host resolves the selected `susmd.exe` below the fixed per-user installation root, validates its manifest and architecture, and calls `CreateProcessAsUserW` with the user's primary token. It supplies an explicit absolute application path, an explicit Unicode environment from `CreateEnvironmentBlock` with inheritance disabled, an explicit version-directory working directory, no inherited handles, and no visible console or interactive desktop.

The host never loads workload configuration, opens the controller database, or constructs a workload command. The launched controller receives only fixed bootstrap identities and object names. It connects back to the host, and the host matches its pipe PID, token SID, creation time, image path, manager session, and selected build before accepting readiness.

Interactive logon normally has already loaded the user profile. A missing profile or incomplete user environment is a transient controller-launch failure and follows the controller's indefinite capped backoff; the host does not manufacture `%USERPROFILE%` from its own environment.
