# Installation and upgrade

This document defines the v1 user bundle, validation, installation, selection, rollback, and garbage-collection contracts. Updating the machine-wide host is a separate elevated installer operation.

`cargo xtask package --version <version>` produces this release bundle:

```text
dist\susm-<version>-<target>\
  user\
    manifest.toml
    susm.exe
    susmd.exe
    susm-supervisor.exe
  host\
    susm-host.exe
```

Run `user\susm.exe install --user` to install that adjacent user bundle. An already installed CLI may receive the `user` directory or ZIP explicitly. Run `host\susm-host.exe install` from an elevated shell when installing or replacing the machine-wide host.

## Accepted input

`susm upgrade <path>` accepts either a local user-bundle directory or a local user-bundle `.zip` file. V1 does not download URLs. A caller that received an artifact remotely downloads and authenticates it before invoking SUSM.

The input root contains `manifest.toml` and every file named by that manifest. ZIP extraction rejects absolute paths, `..`, alternate data streams, duplicate paths under Windows case-insensitive comparison, reparse points, and entries not declared by the manifest. Directory input applies the same path and reparse-point checks.

V1 accepts exactly the manifest plus its three declared executables. `manifest.toml` is limited to 64 KiB, each executable to 256 MiB, total expanded payload to 768 MiB, and the ZIP itself to 1 GiB. ZIP entries may use Store or Deflate only and may not be encrypted. A limit or codec failure is reported before selection changes.

## Bundle manifest

The manifest has this v1 shape:

```toml
bundle_format = 1
version = "0.1.0"
target = "x86_64-pc-windows-msvc" # or "aarch64-pc-windows-msvc"
protocol_major = 1
controller_schema_read_min = 1
controller_schema_read_max = 1
controller_schema_write = 1
supervisor_runtime_formats = [1]

[[files]]
path = "susm.exe"
size = 123456
sha256 = "<64 lowercase hex characters>"

[[files]]
path = "susmd.exe"
size = 123456
sha256 = "<64 lowercase hex characters>"

[[files]]
path = "susm-supervisor.exe"
size = 123456
sha256 = "<64 lowercase hex characters>"
```

Unknown manifest keys are errors. `version` is a SemVer release identifier without build metadata; content identity comes from the manifest digest. `target` must match the native Windows architecture selected for this user. The three v1 executables are required exactly once; v1 permits no additional executable payload.

`controller_schema_read_min..=controller_schema_read_max` is the inclusive range the controller can open before migration. `controller_schema_write` is the schema it reaches after its transactional migrations. The upgrade command refuses a build that cannot open the current database schema. Rollback refuses a build whose read range excludes the current schema.

`supervisor_runtime_formats` lists the formats this build's supervisor can create and recover. An active or recoverable execution remains pinned to the exact older build that created its journal, so selection of a new build does not reinterpret an old runtime journal.

The manifest identity is SHA-256 over the exact `manifest.toml` bytes. File identity is checked independently against every declared size and digest. Embedded hashes provide deterministic integrity checking, not publisher authenticity; v1 trusts the local artifact selected by the user.

## Install and select

Upgrade validates the complete source before changing durable state. It then copies or extracts into a randomly named sibling staging directory, verifies the installed bytes again, flushes its files and directory metadata as available on Windows, and renames it to:

```text
%LOCALAPPDATA%\Programs\susm\versions\<version>-<manifest-sha256>
```

An existing directory with the same identity is reused only after all files revalidate. A mismatch is corruption and aborts the operation.

Selection writes a new `current.susm-install` containing `bundle_format`, `version`, and `manifest_sha256`, flushes it, and atomically replaces the prior selection record. The host resolves that identity only within the fixed versions directory and revalidates the selected manifest before launching `susmd.exe` as the registered user.

Upgrade then atomically replaces `%LOCALAPPDATA%\Programs\susm\bin\susm.exe` with the selected version's validated CLI copy. Selection is authoritative if a crash occurs between those two replacements; an older v1 CLI remains protocol-compatible, and the next successful install, upgrade, or rollback reconciles the stable copy.

The controller records installed versions and live execution pins in SQLite for status and garbage collection, but the selection record remains independently readable by the host while no controller is running.

After the atomic selection switch, the host intentionally stops the old controller and starts the selected build. If the new controller cannot become ready, the normal controller recovery policy retries that selected build indefinitely with capped backoff. SUSM does not automatically restore a database snapshot or switch builds again.

Running supervisors continue and buffer observations while the controller is unavailable. Recovery is explicit: the operator uses `susm rollback` to select a compatible older installation or installs a fixed build. Rollback never rewrites the database to fit an older schema.

## Rollback and garbage collection

`susm versions` lists validated installations, current selection, schema compatibility, and live pins. `susm rollback <version-or-manifest-prefix>` selects an unambiguous compatible installed build through the same atomic switch path. It never mutates the database to make an old build compatible.

`susm upgrade gc` removes only non-current versions that have no active or recoverable execution pin. It rechecks the fixed-root path and manifest identity before deleting a version directory. Failed or abandoned staging directories may be removed only when no live upgrade owns them.
