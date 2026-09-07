# Workload configuration

This document defines the accepted workload configuration and how the controller turns it into an execution snapshot. It is authoritative for TOML parsing and validation.

## Files and workload IDs

The controller reads one direct `*.toml` child of `%USERPROFILE%\.config\susm\workloads.d` per managed workload. It does not recurse or follow directory junctions. The filename stem is its workload ID; the TOML document cannot override it. IDs contain at most 128 ASCII bytes, use lowercase letters, digits, `.`, `_`, and `-`, and must start and end with a letter or digit. The complete path contract is in `docs/filesystem-layout.md`.

One file may contain at most 1 MiB, one reload at most 4,096 workload files and 16 MiB of TOML bytes. A missing `workloads.d` is an error; `susm install --user` creates it. An existing empty directory is a valid generation that removes every current definition while preserving enablement, active state, and history.

Reload opens only regular non-reparse files, reads them through bounded buffers, and verifies the directory entry set and each file's identity, size, and last-write time again before commit. A concurrent edit, replacement, creation, or deletion aborts the candidate as `configuration_changed`; the operator retries reload. The controller never commits a generation assembled across two visible directory states.

Reload parses all files with unknown-field rejection and either installs one complete configuration generation or keeps the previous generation unchanged. File enumeration order has no semantic effect. Two filenames that compare equal under Windows case-insensitive comparison are an error.

## Canonical hashes

`ConfigGeneration` is the SHA-256 digest of a versioned canonical encoding of all validated workload definitions, sorted by workload ID. `ProcessDefinitionHash` uses the same encoding rules but includes only one workload's process-creation fields. TOML comments, whitespace, key order, and spelling of inserted defaults do not affect either digest.

The canonical encoder writes explicit variant tags, length-prefixed UTF-8 strings and bytes, integer nanoseconds for durations, numeric limits, and environment names normalized for Windows case-insensitive comparison. Its leading format tag is versioned independently from the TOML schema so an encoding change cannot silently reuse old hashes.

When it creates an execution, the controller separately computes `ExecutionConfigHash` from a versioned canonical encoding of the immutable execution input snapshot: the source `ProcessDefinitionHash`, bare or absolute executable specification, working-directory specification, literal arguments, fresh base user environment, and configured environment set/unset operations. Controller-to-supervisor adoption authenticates this execution hash even before the first preflight succeeds. `restart_required` compares the active execution's source `ProcessDefinitionHash` with the latest definition hash.

The resolved absolute executable path and final expanded environment are results of successful preflight, not inputs to `ExecutionConfigHash`. The supervisor adds them to its runtime checkpoint on first success. A hash therefore remains stable across failed lookups and automatic attempts while the resolved path, once known, remains pinned for the rest of the execution.

Changes to the user or machine environment do not create a configuration generation and do not set `restart_required`. A new execution builds and hashes a new resolved environment snapshot.

## Complete shape

The main process fields stay at the document root because every workload definition owns exactly one workload process:

```toml
kind = "service" # or "job"
executable = "my-server.exe"
arguments = ["--listen", "127.0.0.1:9000"]
working_directory = "${USERPROFILE}\\my-server"
success_exit_codes = [0]

[environment]
unset = ["UNWANTED_VARIABLE"]

[environment.set]
RUST_LOG = "info"
PATH = "${PATH};${LOCALAPPDATA}\\Programs\\my-server"

[restart]
policy = "on-failure"
max_restarts = 10
reset_after = "5m"

[restart.backoff]
initial = "250ms"
multiplier = 2
maximum = "30s"

[stop]
method = "ctrl-break"
timeout = "10s"

[logging]
capture = true
segment_size = "16MiB"
segment_age = "1h"
retention_size = "1GiB"
retention_age = "30d"
```

Only `kind` and `executable` are required for a service using defaults. A job also uses defaults except that its restart policy is `never`. Omitted `arguments`, environment `set`, and environment `unset` are empty. Unknown fields and tables are errors at every nesting level.

Logging limits use the defaults shown above. `retention_size` and `retention_age` also accept `"unlimited"`; rotation limits do not. `capture = false` rejects other logging keys because they would have no effect.

## Process executable

`executable` is a program, never a shell command line. `arguments` is an array of literal argument strings.

An executable value may be an absolute path or a bare filename. SUSM expands `${NAME}` references in path-valued fields from the execution's user environment. `$$` encodes a literal `$`; SUSM does not apply cmd or PowerShell expansion rules. An expanded path with directory components must be absolute.

For a bare filename, SUSM checks each `PATH` directory in order for the exact name and, when the name has no extension, for the name plus `.exe`. It skips empty `PATH` entries and resolves relative entries against the execution working directory. It does not use `PATHEXT`, search the working directory implicitly, or invoke file associations. `.cmd`, `.bat`, and `.ps1` are rejected as direct executables; a definition must name `cmd.exe` or `pwsh.exe` and pass the script through literal arguments.

The execution snapshots the user environment before its first attempt. Failed variable expansion or executable lookup ends that attempt as `launch_failed` and follows the workload restart policy. A later attempt repeats lookup against the same snapshotted environment. Once resolution succeeds, the supervisor persists the resolved absolute path and every later attempt in that execution reuses it even if `PATH`, the definition, or another matching installation changes. A new execution starts from the latest accepted definition and current user environment again.

SUSM performs no shell parsing or expansion in arguments. It encodes the argument array into a Windows command line at the Win32 process adapter and guarantees that the child receives the original argument strings under the standard Microsoft C runtime parsing rules. Programs with nonstandard command-line parsing receive the same encoded command line but may interpret it differently.

`working_directory` uses the same `${NAME}` interpolation and must become an absolute existing directory during preflight. If omitted, it defaults to `${USERPROFILE}` from the snapshotted base user environment. A missing or non-directory path is a `launch_failed` preflight result and follows restart policy.

## Environment

At execution creation, SUSM builds a fresh user environment from the manager-session user token rather than copying the controller process environment. It applies the definition's explicit `set` and `unset` operations, normalizes variable names with Windows case-insensitive comparison, and rejects a definition that names the same variable more than once under different casing.

SUSM interpolation is deliberately smaller than shell expansion. It recognizes `${NAME}` and `$$` only in documented path and environment fields. Every reference reads the fresh base user environment, never another configured `set` entry, so TOML key order has no effect and cycles cannot exist. An unknown variable is a preflight launch failure. Arguments and other strings remain literal.

The `set` and `unset` name sets must be disjoint under Windows case-insensitive comparison. SUSM expands all `set` values against the base environment, applies those values simultaneously, then removes `unset` names.

The base environment and overlay specification belong to the immutable execution input snapshot. The first successful preflight produces the final environment, including `PATH`, and every process attempt receives those same values. A later execution snapshots a fresh base environment again and can therefore observe user or machine environment changes without restarting the controller.

## Exit classification

`success_exit_codes` is a deduplicated array of Windows `u32` process exit codes and defaults to `[0]`. An empty array is valid and classifies every normal process exit as failure. Launch failures and supervisor loss are never success codes.

Success-code classification is hot-reloadable restart policy, not process-creation state. A policy reload while an execution waits in restart backoff reclassifies the recorded raw exit code. If the pending retry is no longer permitted, the execution becomes terminal immediately; otherwise its existing deadline remains unchanged.

## Restart policy

The optional `[restart]` table has this shape:

```toml
[restart]
policy = "on-failure"          # "never", "on-failure", or service-only "always"
max_restarts = 10              # non-negative integer or service-only "unlimited"
reset_after = "5m"             # services only

[restart.backoff]
initial = "250ms"
multiplier = 2
maximum = "30s"
```

Services default to the values above. Jobs default to `policy = "never"`; a job using `on-failure` must supply a finite integer `max_restarts`, and `reset_after` is invalid because the job budget covers the whole execution. `always` and `"unlimited"` are invalid for jobs.

`max_restarts` counts retries after the initial attempt. Integer `0` is accepted and canonicalized to `never`; domain state never contains a retrying policy with no retries. With `policy = "never"`, retry-only keys and `[restart.backoff]` are errors rather than ignored settings.

Backoff durations must be positive, `maximum` must not be less than `initial`, and `multiplier` is a positive integer. Omitted backoff fields use the values shown above. V1 adds no jitter.

## Stop plan

Without a `[stop]` table, SUSM uses `method = "ctrl-break"` and `timeout = "10s"`. It sends one `CTRL_BREAK` to the workload's private console process group, waits for the whole Windows Job Object to become empty, then terminates the Job Object at the deadline.

`method = "kill"` has no timeout and terminates the Job Object immediately. `method = "command"` requires a direct shell-free stop executable and uses the configured timeout. The stop executable and arguments are separate fields; it reuses the active execution's snapshotted environment and working directory and resolves through that environment if given as a bare filename.

`ctrl-break` requires `timeout` and rejects a `command` table. `command` requires `timeout`, `command.executable`, and accepts an optional `command.arguments` array. `kill` rejects both `timeout` and a `command` table. The default stop plan is inserted before conversion to domain types.

Command and immediate-kill examples are:

```toml
[stop]
method = "command"
timeout = "15s"

[stop.command]
executable = "my-server-ctl.exe"
arguments = ["shutdown"]
```

```toml
[stop]
method = "kill"
```

The supervisor starts a stop command outside the workload Job Object but inside its own ephemeral Job Object. Stop-command resolution, launch, or exit failure never extends the deadline or changes the workload's terminal outcome. When the workload exits or the deadline arrives, the supervisor terminates any remaining stop-command tree.

The supervisor snapshots the current stop plan when a stop, cancel, restart, or manager-session shutdown begins. A later reload does not alter an in-progress stop. If the definition has since disappeared, the supervisor uses the last stop plan it accepted for that execution.

## Reload effects

One successful reload commits the generation before sending any supervisor policy update. Delivery is reconciled and idempotent by generation: a supervisor persists the newer policy in its runtime journal before acknowledging it. Until acknowledgement, status includes `policy_sync_pending`; controller loss or pipe loss retries delivery after attachment. A missing definition keeps the last accepted execution policy rather than sending an empty policy.

| Field change | Active execution behavior |
| --- | --- |
| `executable`, `arguments`, `working_directory`, `[environment]` | Keep the immutable execution snapshot and set `restart_required`. |
| `kind` | Accept only while quiescent; otherwise reject the entire reload. |
| `success_exit_codes` | Apply to the next classification and immediately re-evaluate a recorded exit in restart backoff. |
| `[restart]` and `[restart.backoff]` | Apply to future attempt-end decisions; an existing retry keeps its deadline but is cancelled if the new policy denies it. |
| `[stop]` | Apply to the next stop/cancel/restart; never mutate a stop already in progress. |
| `[logging]` rotation and retention | Apply immediately; lowering limits may rotate or prune finalized segments. |
| `[logging].capture` | `true` starts journaling future drained chunks; `false` finalizes the active segment and discards future chunks. |

Reload itself never starts, stops, restarts, runs, cancels, enables, or disables a workload. Reconciliation may start an already-enabled workload whose previously missing definition becomes available, because that manager-session trigger was already durable before reload.
