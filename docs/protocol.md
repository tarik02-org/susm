# Local RPC contracts

This document defines the v1 RPC packages, connection ownership, authentication context, mutation acceptance, streaming, and compatibility rules. Protobuf messages are boundary types; handlers parse them into domain commands before calling lifecycle modules.

## Packages and connections

SUSM uses three independent versioned packages:

| Package | Pipe server | Clients | Purpose |
| --- | --- | --- | --- |
| `susm.control.v1` | per-user controller | `susm` CLI and a future tray | workload configuration, lifecycle, status, history, and logs |
| `susm.supervisor.v1` | per-user controller | supervisors owned by that user | bootstrap, adoption, commands, observations, and acknowledgement |
| `susm.host.v1` | machine-wide LocalSystem host | CLI and controllers | registration, manager-session bootstrap, version selection, and shutdown coordination |

The controller exposes separate control and supervisor named pipes. A supervisor is always the client and reconnects to the SID-scoped supervisor pipe after controller loss. It opens one long-lived bidirectional `Attach` RPC; SUSM does not create one pipe server per workload.

## V1 RPC methods

`susm.control.v1.ControlService` exposes:

- unary `Reload`, `ListWorkloads`, `GetWorkload`, `Start`, `Stop`, `Restart`, `Run`, `Cancel`, `Rerun`, `Enable`, and `Disable`;
- unary `ListExecutions` and `GetExecution` for retained history;
- server-streaming `WatchWorkloads` and `ReadLogs`.

`Reload` returns accepted, unchanged, or a structured list of source-spanned configuration diagnostics. It does not force generic command errors into that diagnostic schema. List methods use an opaque cursor, default to 100 rows, and cap a page at 1,000 rows.

`susm.supervisor.v1.SupervisorControlService` exposes only the bidirectional `Attach` stream. `susm.host.v1.HostControlService` exposes unary registration, version-selection, controller-status, and `RestartController` methods plus a controller attachment used for bootstrap readiness and diagnostics. Manager-session ending does not depend on that RPC connection; the host signals the session's Windows event directly.

All unary and streaming Protobuf messages are capped at 4 MiB decoded size. Log stream payload chunks are capped at 64 KiB. The runtime-journal frame maximum is also 4 MiB so any stored observation can cross the matching protocol major without fragmentation.

## Named-pipe authentication

Every server creates a byte-mode overlapped pipe with `PIPE_REJECT_REMOTE_CLIENTS`, a protected explicit DACL, and no inherited access. A per-user controller pipe grants full access only to LocalSystem and its owning user SID. The machine host pipe grants local authenticated users enough access to connect and grants full access to LocalSystem; handlers still reject ineligible token types and identities.

On connection, the adapter impersonates the named-pipe client at identification level, reads `TokenUser` and `TokenStatistics`, copies the SID and authentication LUID into immutable connection context, then reverts impersonation before handing the stream to Tonic. Tonic request extensions receive that context through `Connected`. Handlers never read an authoritative SID from Protobuf fields or gRPC metadata.

For an expected controller or supervisor, the server also reads the named-pipe client process ID, opens that process for limited query access, and matches its creation time and image path against the process identity recorded at launch. Creation time prevents PID reuse from adopting a stale binding.

The host creates its well-known pipe as the first pipe instance. Host clients verify that the server process token is LocalSystem before sending a registration or controller-session message. Per-user clients treat processes running as the same SID as the same security principal; protocol identities still prevent accidental attachment to stale executions.

## Manager-session ending event

For every manager session, the host creates one manual-reset named Windows event keyed by the host-issued `ManagerSessionId`. Its DACL grants LocalSystem and that session's user SID. The event starts nonsignaled and remains signaled once logoff begins.

The host signals this event before waiting for controller shutdown. Controllers and supervisors wait on it independently. A supervisor that observes it persists `ManagerSessionEnded` and executes its snapshotted service stop or job cancel plan even when no controller is alive. The event communicates only the session lifecycle fact; the host never chooses a workload command or stop method.

## Supervisor attachment

The first supervisor message is exactly one `Hello`. It identifies manager session, workload, execution, durable supervisor, process incarnation, execution-config hash, runtime-journal format, executable build, and whether the process needs bootstrap configuration or resumes a journal. Identity fields correlate records; the named-pipe adapter supplies the authenticated SID and client process identity separately.

The controller rejects an attachment unless the SID, expected supervisor identity and incarnation, execution identity, manager session, execution-config hash, process identity, protocol compatibility, and runtime format all match its durable binding. An unsolicited or stale peer cannot consume the expected supervisor's failure budget.

For a new supervisor, `Welcome` carries the immutable execution configuration and current hot policy. The supervisor writes its initial runtime checkpoint and reports `Ready` before the controller marks it attached. A resumed supervisor receives the controller's last committed observation sequence and replays every later journal observation.

After attachment, supervisor-to-controller messages contain strictly ordered observations. Every observation carries its attempt number and the workload root PID when that process is live. `attempt-started` is published only after process creation, Job Object assignment, resume, and durable runtime-journal persistence. Exit, launch-failure, backoff, and terminal observations clear the workload PID while preserving their attempt and diagnostic detail. Controller-to-supervisor messages contain committed-sequence acknowledgements and reconciled commands such as stop intent or a newer hot-policy generation. Commands carry their domain identity, such as intent revision or configuration generation, rather than a generic RPC operation ID.

Control-plane workload and execution views expose `supervisor_process_id`, `workload_process_id`, and `attempt` as distinct fields. Workload views also expose the latest active error. A supervisor PID proves only that execution infrastructure exists; it does not imply that the workload is running.

## Mutation acceptance

A control mutation is accepted when its controller SQLite transaction commits. The RPC returns only after that point. If the request is cancelled or its deadline expires before the command loop accepts it, the controller may drop it without a state change. Once accepted, client cancellation only drops the response observer; reconciliation continues the mutation.

The CLI does not automatically retry mutating RPCs after an ambiguous transport failure. Read RPCs may retry after reconnect. SUSM keeps no generic client-operation ledger.

Expected command rejection uses ordinary gRPC status codes and a concise human message. V1 does not add a custom error-details envelope. Missing resources use `NOT_FOUND`; duplicate exclusive activity uses `ALREADY_EXISTS`; incompatible state or missing definitions use `FAILED_PRECONDITION`; malformed boundary input uses `INVALID_ARGUMENT`; unavailable host, controller, or supervisor infrastructure uses `UNAVAILABLE`.

An idempotent successful no-op is not an error. Mutation responses contain `changed = false` and the resulting workload status; a committed transition returns `changed = true`.

## Streams

A status watch sends one current snapshot followed by monotonic controller-state revisions. It is not a durable event feed and has no replay cursor. A slow subscriber is disconnected with an explicit resubscribe status; reconnecting starts from a new snapshot.

A log query snapshots matching segment names, reads complete historical entries, then optionally attaches to the active supervisor. Capture sequence removes overlap between the historical tail and live stream. Retention may remove a snapshotted file before it opens; the stream reports that gap and continues.

Dropping a client stream cancels only that subscription. It never cancels a workload mutation, execution, log writer, or supervisor attachment owned by another task.

## Compatibility

The Protobuf package suffix is the protocol major. Peers using different majors do not attach. Inside one major, each handshake sends deduplicated `supported_capabilities` and `required_capabilities`. Attachment succeeds only when each peer's required set is a subset of the other's supported set. Unknown optional capabilities are ignored at the Protobuf boundary; an unknown required capability rejects attachment.

Compatibility is based on capabilities, not executable version or a single protocol minor. Buf breaking checks prevent removal or incompatible reuse inside a published major. New fields remain optional unless a named capability makes their semantics required.

Published control and host v1 RPC methods and fields remain wire- and behavior-compatible for older v1 CLIs. A newer controller may expose additional methods, but replacing or weakening the semantics of an existing mutation requires a new protocol major. This lets the stable CLI copy lag the selected build across an interrupted atomic upgrade without becoming unsafe.

Every official `v1` controller must support baseline attachment, replay, policy update, and stop commands for every official `v1` supervisor. A later v1 release may add optional capabilities but may not add a new required capability to baseline adoption. Release validation rejects a manifest that violates this invariant; runtime does not restart incompatible workloads as an upgrade fallback.

An active execution records the supervisor build that created its runtime journal. If that supervisor must be replaced, the controller launches the same retained build so it can read and compact its own journal format. A newly created execution uses the currently selected build. Version garbage collection treats every non-terminal execution build as in use.
