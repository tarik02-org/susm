# Workload journal

This document defines workload output capture, segment naming, rotation, retention, and read behavior.

## Layout

Each attempt writes below:

```text
%LOCALAPPDATA%\susm\logs\<workload-id>\<execution-id>\attempt-000001\
```

The active segment name is `<UTC-open-time>-<segment>.susm-journal.open`. UTC time uses `yyyyMMddTHHmmss.nnnnnnnnnZ`, and the zero-based segment number is padded to six digits. Closing a segment first renames it to `.susm-journal`, then compression atomically replaces it with `.susm-journal.zst`.

An attempt directory contains both stdout and stderr. Each Journal Export entry identifies its stream and capture sequence, so readers can merge streams in observed order or reproduce either stream's captured bytes.

## Entry encoding

Segments use the Journal Export binary-field framing, not line-oriented text. Scalar fields are encoded as `NAME=value` followed by LF. Raw payload uses `MESSAGE`, LF, an unsigned little-endian 64-bit byte length, the exact bytes, and LF. One empty LF terminates each record. Field names and scalar values are ASCII where their domain permits it and UTF-8 otherwise.

Every output record contains:

```text
SUSM_FORMAT=1
SUSM_RECORD=output
SUSM_TIMESTAMP=<UTC RFC 3339 timestamp>
SUSM_WORKLOAD=<workload-id>
SUSM_EXECUTION=<execution UUID>
SUSM_ATTEMPT=<decimal attempt number>
SUSM_SEQUENCE=<decimal capture sequence>
SUSM_STREAM=stdout|stderr
MESSAGE=<binary field containing the captured chunk>
```

One capture sequence belongs to one pipe-read chunk; SUSM does not infer lines or character encoding. A gap record has no `MESSAGE` or independent capture sequence. It identifies the inclusive dropped sequence range, total chunks and bytes, and stdout/stderr byte subtotals through `SUSM_FIRST_SEQUENCE`, `SUSM_LAST_SEQUENCE`, `SUSM_DROPPED_CHUNKS`, `SUSM_DROPPED_BYTES`, `SUSM_STDOUT_BYTES`, and `SUSM_STDERR_BYTES`.

The reader rejects a malformed complete record but accepts complete earlier records in a segment with an incomplete final field or record. A `.zst` file is one standard Zstandard frame containing the exact finalized segment bytes.

## Rotation

Defaults are:

```toml
[logging]
capture = true
segment_size = "16MiB"
segment_age = "1h"
retention_size = "1GiB"
retention_age = "30d"
```

The supervisor rotates before writing a pipe-read chunk that would take a non-empty segment beyond `segment_size`, and rotates an open segment when `segment_age` elapses even if no new output arrives. A single entry may exceed the nominal size limit. Changing rotation settings applies to the active execution; lowering a limit can close the current segment immediately.

`capture = false` drains and discards both output streams without creating journal files. SUSM still counts discarded bytes but does not report this explicit choice as logging degradation.

`segment_size` and `segment_age` must be positive. Each retention limit accepts a positive value or the string `"unlimited"`; limits act independently. Setting both retention values to `"unlimited"` disables automatic deletion. With `capture = false`, the other logging keys are invalid rather than silently ignored.

Pipe readers assign one execution-wide capture sequence before enqueueing each observed chunk. The journal writer queue is bounded to 8 MiB per execution. If it is full, readers continue draining the workload pipes and drop new chunks rather than applying process-visible backpressure.

The supervisor aggregates dropped bytes, chunks, streams, and capture-sequence ranges. Once the writer has capacity, it appends a synthetic gap entry before later captured output. Status exposes cumulative dropped byte counts. Raw and follow readers report encountered gaps separately from reproduced stream bytes.

## Retention

Retention limits apply to one workload across all executions and attempts. The supervisor scans the workload's journal tree after segment finalization, after a retention-setting reload, and periodically while it owns an active execution.

It first deletes finalized segments older than `retention_age`, then deletes the oldest remaining finalized segments until actual stored bytes are at or below `retention_size`. Open segments are included when measuring stored bytes but are never deletion candidates. Empty attempt and execution directories may be removed after retention deletes their last segment.

A retention, rotation, compression, or directory-cleanup failure does not stop the workload. The supervisor reports the failure and retries later.

## Crash and read behavior

Journal output is operational history, not a transactional record. The writer flushes its userspace buffer at least once per second and whenever it closes a segment. Before renaming an open segment to its finalized name, it calls `FlushFileBuffers`. It does not sync each entry. A supervisor or machine crash may therefore lose roughly the most recent second of output from the active segment.

A reader consumes all complete entries in an open or truncated segment and ignores an incomplete tail. Readers open files with delete sharing, snapshot the matching filenames at query start, and tolerate a retention pass removing a file before it is opened. A follow stream subscribes to the supervisor for new entries after reading existing files and uses capture sequence to remove overlap.
