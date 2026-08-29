# Supervisor runtime journal

This document defines how a supervisor survives controller outages and how a replacement controller or supervisor recovers one execution.

## Authority and location

Each active execution owns one append-only runtime journal under `%LOCALAPPDATA%\susm\runtime\sessions\<manager-session-id>\executions\<execution-id>`. It contains the supervisor checkpoint and every observation not yet safe to discard. Replacement processes keep the same `SupervisorId`, increment `SupervisorIncarnation`, and recover this journal. It is not desired state and never overrides a stop, cancel, restart, or manager-session intent already committed by the controller.

The active filename is `<supervisor-id>.susm-runtime.open`. A supervisor finalizes it as `.susm-runtime` after it records a terminal execution state. Runtime journals use length-delimited, checksummed Protobuf frames with a format version independent of the RPC packages.

## File encoding

A runtime file begins with the eight ASCII bytes `SUSMRUN\0`, a little-endian `u32` format version, and a CRC32C of those preceding 12 bytes. V1 format version is `1`.

Each following frame is:

```text
u32 little-endian payload length
u32 little-endian CRC32C of the four length bytes
<deterministically encoded Protobuf payload>
u32 little-endian CRC32C of the payload bytes
```

The payload limit is 4 MiB. Length is validated before allocation. A partial header or a header-valid frame whose declared payload and checksum footer do not fully exist at EOF is an incomplete tail and is ignored. A bad file header, length checksum, payload checksum, oversized length, invalid Protobuf, or invalid domain conversion in any otherwise complete frame is corruption and aborts recovery of that journal.

The outer frame carries no lifecycle fields; its sole job is bounded torn-write detection. The versioned Protobuf payload is a closed `oneof` of checkpoint and transition records, and unknown variants are unsupported runtime format rather than silently skipped.

A replacement process opens the existing journal, verifies the durable supervisor and execution identities, and requires its controller-issued incarnation to be exactly one greater than the last persisted incarnation. Before attaching or repeating any execution effect, it appends and syncs an `IncarnationStarted` transition. An incarnation gap, reuse, or overflow is corruption or controller-state mismatch, never silently normalized.

## Frames

The first frame is a checkpoint containing manager-session, workload, execution, supervisor and current incarnation, source-definition and execution-config identities, the immutable execution input snapshot, current restart and stop policy, current execution phase, restart counters, capture sequence, and next observation sequence. Before successful preflight, resolved process fields are absent; the transition that first resolves them persists the absolute executable and final environment for every later attempt and replacement supervisor process.

Each later transition frame contains the complete lifecycle state after one transition and its sequenced observation. Attempt-end observations contain raw facts, including launch errors, exit codes, run duration, and stop escalation. Replaying complete frames reconstructs the exact supervisor state and the observation stream.

Frame lengths have the fixed 4 MiB maximum above. Recovery accepts complete valid frames and ignores only an incomplete tail; corruption makes the runtime journal unusable rather than guessing at state.

## Persist before effect

For every execution transition, the supervisor:

1. computes the next pure state and declarative effects;
2. appends the transition frame and calls `FlushFileBuffers`;
3. executes WinAPI effects;
4. publishes the frame's observation when a controller is attached.

A crash before step 2 leaves the earlier state authoritative. A crash after step 2 lets a replacement derive and repeat the same idempotent effect. Launch identity, attempt number, stop intent, and deadlines come from the persisted frame.

## Replay and acknowledgement

Observation sequence starts at one for each durable `SupervisorId` and continues across process incarnations. On attach, the controller sends its last committed sequence for that authenticated supervisor. The supervisor streams every later observation in order. The controller accepts the next sequence only, applies it and advances its cursor in one SQLite transaction, then acknowledges that sequence.

Duplicate observations at or below the committed cursor are acknowledged without reapplying them. A sequence gap aborts attachment and requests replay from the committed cursor. The supervisor does not discard an observation merely because it wrote it to the pipe.

## Compaction and cleanup

After acknowledgement, the supervisor may compact by writing a new checkpoint followed by observations newer than the acknowledged cursor, syncing it, and atomically replacing the active journal. A crash before replacement leaves the old journal; a crash after replacement leaves the new checkpoint. Compaction never blocks workload pipe draining.

After a terminal observation is committed, the controller acknowledges it and may delete the finalized runtime journal once no compatible supervisor process is using it. Unacknowledged finalized journals remain importable after controller restart.
