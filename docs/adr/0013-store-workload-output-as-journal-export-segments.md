---
status: accepted
date: 2026-08-27
---

# Store workload output as Journal Export segments

Store each attempt's captured output as an append-only stream using systemd's binary-safe Journal Export Format rather than the systemd journal file format. Each entry contains `SUSM_REALTIME_TIMESTAMP_NS`, `SUSM_MONOTONIC_TIMESTAMP_NS`, `SUSM_SEQUENCE`, `SUSM_STREAM`, and a binary-safe `MESSAGE`; entries represent observed pipe-read chunks, so concatenating one stream's messages reproduces its captured bytes.

## Consequences

Name an active segment `<UTC-open-time>-<segment>.susm-journal.open`, using a colon-free UTC timestamp, then finalize it as `.susm-journal` and compress closed segments to `.susm-journal.zst`. Rotation uses size and age limits. A reader accepts every complete entry in a truncated active segment and ignores an incomplete tail. `susm logs` can merge by capture sequence, filter a stream, emit JSON, or reproduce one stream as raw bytes. Compression, rotation, and retention failures never terminate the workload; the supervisor reports dropped byte counts and keeps an uncompressed segment until compression succeeds.

Default rotation closes a segment at 16 MiB or 1 hour. Retention is per workload across executions and keeps at most 1 GiB and 30 days of finalized history. Active segments are never retention candidates.
