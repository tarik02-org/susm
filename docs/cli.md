# Command-line interface

This document fixes the v1 `susm` command surface and its automation rules. Each mutating workload command addresses exactly one workload; v1 has no implicit glob or bulk partial-success semantics.

## Commands

```text
susm config path
susm reload

susm list
susm status <workload>
susm start <service>
susm stop <service>
susm restart <service>
susm run <job>
susm cancel <job>
susm rerun <job>
susm enable <workload>
susm disable <workload>

susm executions <workload>
susm execution <execution-id>
susm logs <workload> [--execution <id>] [--attempt <number>]
          [--stream stdout|stderr|all] [--follow] [--raw]

susm controller status
susm controller restart

susm install --user [directory-or-zip]
susm uninstall --user
susm upgrade <directory-or-zip>
susm upgrade gc
susm versions
susm rollback <version-or-manifest-prefix>

susm completions powershell|bash|zsh|fish
```

Service-only commands reject jobs and job-only commands reject services with `FAILED_PRECONDITION`. `enable` and `disable` affect the next manager-session trigger only, as defined by the state machines. Lifecycle mutations return after the controller transaction commits; they do not wait for a process to reach running or terminal state.

`logs` defaults to the active execution, or otherwise the newest execution, and all attempts and streams selected within it. `--raw` requires exactly one stream and writes only captured message bytes to stdout; gaps and diagnostics go to stderr. Non-raw output includes UTC timestamp, attempt, stream, and gap records. `--follow` reconnects from a new snapshot when a status-only subscription is lost, but it never fabricates bytes across a reported journal gap.

## Output and exit behavior

Human-readable output is the default. Global `--json` emits one stable JSON document for unary commands and newline-delimited JSON records for streams; it never serializes generated Protobuf objects directly. Diagnostics go to stderr.

Workload and execution status distinguish `supervisor_process_id` from `workload_process_id` and include the attempt number. Workload status also includes the latest active error. Zero means that no process or attempt currently occupies that field; the CLI does not present a live supervisor PID as the workload PID or call an execution `running` before `attempt-started` is committed.

Exit code `0` means the requested command was accepted or was an idempotent no-op. Exit code `2` is CLI syntax or local boundary validation. All host, transport, configuration, compatibility, and lifecycle failures return `1` with their typed diagnostic. Scripts that need the exact failure category use `--json` rather than branching on a large exit-code enum.

The CLI never automatically retries a mutation after an ambiguous connection failure. It tells the operator that acceptance is unknown and names the status command that can disambiguate current state. Read-only list, status, history, and log snapshot calls may retry once after reconnecting.

## Stable executable and completion

Per-user installation puts the command at `%LOCALAPPDATA%\Programs\susm\bin\susm.exe` and adds only that stable `bin` directory to the user PATH. Upgrade atomically replaces this CLI after installing and selecting the immutable version. A crash between pointer and CLI replacement can leave an older v1 CLI temporarily selected; v1 protocol compatibility keeps its existing methods valid, and the next successful install or rollback reconciles the copy.

`susm completions` prints static Clap-generated shell completion. PowerShell workload-name completion may issue read-only `susm list --json`; completion never starts a controller, reloads configuration, or performs a mutation.

A PowerShell module with `Start-SusmService`-style wrappers is a later interface over the same CLI/RPC contract, not a second lifecycle implementation.

When `susm install --user` omits its source, it installs the user bundle beside the running `susm.exe`. An explicit directory or ZIP remains available for other installation sources. `susm upgrade` always requires an explicit source.
