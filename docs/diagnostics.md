# SUSM diagnostics

This document defines internal host, controller, and supervisor diagnostics. These are separate from workload stdout/stderr journals and cannot be disabled by workload configuration.

## Locations

```text
%ProgramData%\SUSM\diagnostics\host\
%LOCALAPPDATA%\susm\diagnostics\controller\
%LOCALAPPDATA%\susm\diagnostics\supervisors\<workload-id>\<execution-id>\
```

Host files grant full access only to LocalSystem and administrators. Per-user files grant full access only to LocalSystem and their user SID. Ordinary users do not read another user's or machine-wide host diagnostics; an elevated operator can inspect host files directly.

Each process writes only within its own component tree. Supervisors use execution-scoped directories, so two versions never share a writable file. The diagnostics module owns its JSONL rotation and retention; workload journals keep their separate binary encoder, compression, and policy.

## JSON Lines schema

Every complete UTF-8 line is one JSON object with:

- `timestamp`: UTC RFC 3339 timestamp;
- `level`: `trace`, `debug`, `info`, `warn`, or `error`;
- `component`: `host`, `controller`, or `supervisor`;
- `pid`, `target`, and stable event `name`;
- applicable manager-session, workload, execution, supervisor, and attempt identities;
- `fields`: event-specific scalar values.

An incomplete final line after a crash is ignored. Diagnostics are for operators, not a durable lifecycle source of truth; state recovery never parses them.

SUSM never records complete environment blocks, environment values, command lines, pipe payloads, access tokens, or configuration source lines in diagnostics. Configuration errors may record file, span, field name, and reason; source excerpts are rendered only to the authenticated foreground CLI. OS errors record numeric Win32 or SQLite codes alongside a bounded message.

## Rotation and retention

An active diagnostic segment rotates at 16 MiB or 24 hours, whichever comes first. Finalized segments are retained for 7 days and at most 256 MiB per component tree, applying whichever limit removes more. Open segments are measured but never deleted.

Writers use a bounded 1 MiB non-blocking queue per SUSM process. Diagnostics are dropped rather than blocking lifecycle control or workload pipe draining. When capacity returns, the writer emits one aggregate `diagnostics_dropped` record with the count. It flushes userspace buffers every second and calls `FlushFileBuffers` before finalizing a segment; a crash can lose the newest second.

Segment names use the same colon-free UTC timestamp convention as workload journals and end in `.jsonl.open` while writable, then `.jsonl`. V1 does not compress diagnostics.
