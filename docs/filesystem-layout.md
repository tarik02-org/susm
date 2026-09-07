# Filesystem and object layout

This document fixes the v1 locations of user-authored configuration, durable manager state, runtime recovery data, installed builds, named pipes, and the manager-session ending event.

## User-authored configuration

The default configuration root is:

```text
%USERPROFILE%\.config\susm\
  workloads.d\
    <workload-id>.toml
```

`susm reload` reads exactly the direct `*.toml` children of `workloads.d`; it does not recurse, follow directory junctions, or treat other files as workloads. `susm config path` prints this directory. V1 has no global manager TOML and no implicit secondary configuration roots.

Per-user installation creates this directory if absent. Reload treats a missing directory as an error and an existing empty directory as an intentional empty configuration generation.

The CLI and controller resolve `%USERPROFILE%` from the authenticated manager-session user token. They do not trust a client-supplied profile path or inherit the host service's environment.

## Manager data

All manager-owned per-user data lives below `%LOCALAPPDATA%\susm`:

```text
%LOCALAPPDATA%\susm\
  data\
    state.db
    state.db-wal
    state.db-shm
  logs\
    <workload-id>\
      <execution-id>\
        attempt-000001\
  diagnostics\
    controller\
    supervisors\
  runtime\
    sessions\
      <manager-session-id>\
        executions\
          <execution-id>\
            execution.pb
            runtime.susm-runtime.open
            result.pb
```

SQLite owns the files below `data`. Supervisors own their runtime journals and workload log trees. A controller scans the current and older session directories at startup: it may import terminal observations from an ended session, but it never resumes an old session's execution.

The manager creates directories with an explicit DACL granting full access only to LocalSystem and the owning user SID. It rejects reparse points while walking manager-owned data. Temporary sibling files used for atomic replacement carry a random suffix and are never authoritative until synced and renamed.

Uninstalling a per-user installation does not silently remove configuration, logs, history, or runtime recovery data. A separate explicit purge operation will own that destructive behavior after v1.

## Per-user program installation

Per-user builds live below:

```text
%LOCALAPPDATA%\Programs\susm\
  bin\
    susm.exe
  current.susm-install
  versions\
    <version>-<manifest-hash>\
      manifest.toml
      susm.exe
      susmd.exe
      susm-supervisor.exe
```

Version directories are immutable after validation. `current.susm-install` is a small manager-owned selection record that names one installed manifest identity; upgrade writes and syncs a sibling file, then replaces the selection atomically. The host accepts only a validated version-directory name below this fixed root and launches `susmd.exe` with the registered user's token.

The stable `bin\susm.exe` is an atomically replaced copy of the selected CLI, not a launcher or symlink. Only `bin` is added to the user's PATH. Every immutable version also retains its own CLI so an operator can invoke it by absolute path for manual recovery.

An active execution pins its version directory until its runtime journal is terminal and committed. Garbage collection also preserves the current selection and every version referenced by controller recovery data.

## Named kernel objects

V1 uses these logical names:

```text
\\.\pipe\susm\host\v1
\\.\pipe\susm\users\<sid>\sessions\<manager-session-id>\control\v1
\\.\pipe\susm\users\<sid>\sessions\<manager-session-id>\supervisors\v1
Global\SUSM-manager-session-<manager-session-id>-ending
```

The host pipe and ending event are created by the LocalSystem host. The controller creates both per-user pipes. Object names correlate an expected manager session but never authorize a caller; the explicit DACL and authenticated connection token remain authoritative.

The event is manual-reset and exists for the manager session's lifetime. Signaling it is irreversible and means only that this manager session is ending.
