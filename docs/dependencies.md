# Dependency choices

Cargo dependencies use workspace inheritance. `Cargo.lock` pins exact releases; manifests state the compatible release line and enable only required features.

## Runtime and IPC

| Need | Choice | Notes |
| --- | --- | --- |
| Async runtime | `tokio` 1 | Process I/O, timers, channels, and Windows named pipes |
| RPC | `tonic` 0.14 | gRPC over HTTP/2 inside byte-mode Windows named pipes; no TCP listener or public HTTP endpoint |
| Protobuf codec | `tonic-prost` 0.14 | Prost codec used by generated Tonic clients and servers |
| Named-pipe connector adapters | `hyper-util` 0.1, `tower` 0.5 | `TokioIo` adapts client pipes to Hyper I/O; `service_fn` supplies Tonic's custom connector |
| Streams and task cancellation | `tokio-stream` 0.1, `tokio-util` 0.7 | Incoming pipe and RPC stream adapters; `CancellationToken` remains for process-local task lifetimes |

## Interfaces and data

| Need | Choice | Notes |
| --- | --- | --- |
| CLI | `clap` 4.6 | Derive-based command model |
| Shell completions | `clap_complete` 4.6 | Ahead-of-time PowerShell and common shell scripts; live workload-name completion remains SUSM-owned |
| Human CLI tables | `comfy-table` 7.2 | Borderless, width-aware workload, execution, version, and detail views |
| Terminal styling | `anstream` 1, `anstyle` 1 | Automatic Windows terminal color support with `NO_COLOR` and explicit always/never modes |
| Protobuf runtime | `prost` 0.14 | Generated message types at protocol adapters |
| Protobuf generation | Buf 1.72, `tonic-prost-build` 0.14 | Buf emits a `FileDescriptorSet`; `compile_fds` emits Prost messages and Tonic stubs without `protoc` |
| TOML | `serde` 1, `toml` 1.1 | Strict raw structs with unknown fields rejected, followed by validated conversion into domain types |
| Human durations | `humantime-serde` 1 | Values such as `250ms` and `5s` in TOML |
| Human byte sizes | `parse-size` 1.1 | Strict binary-unit parsing for values such as `16MiB`; converted immediately into bounded domain newtypes |
| User diagnostics | `miette` 7 | Source spans and labeled configuration errors |
| JSON CLI output | `serde_json` 1 | Stable machine-readable output, independent of Protobuf wire types |

## Windows and persistence

| Need | Choice | Notes |
| --- | --- | --- |
| Win32 APIs | `windows` 0.62 | Typed Microsoft bindings with narrowly enabled API features; SUSM owns RAII handle wrappers |
| SCM management | `windows-service` 0.8 | Typed installation, configuration, recovery policy, status, start, stop, and deletion; required-privilege configuration uses the service handle with `windows` |
| SCM host loop | `windows-services` 0.26 | Service entry and control handling; token, session, process, Job Object, pipe ACL, and Event Log work uses `windows` |
| SQLite | `rusqlite` 0.40 with `bundled` | One synchronous connection behind a mutex; no pool or database worker thread |
| Schema migrations | `rusqlite_migration` 2.6 | Versioned forward-only SQL migrations run before the controller opens its pipes |
| Workload journal compression | `zstd` 0.13 | Compress only finalized Journal Export segments |
| Runtime-frame checksum | `crc32c` 0.6 | Hardware-accelerated CRC32C with software fallback; corruption detection, not authentication |
| Temporary files | Standard library plus random UUID names | Same-directory staging; atomic replacement uses Win32 write-through operations because `atomic-write-file` 0.3.1 falls back to `std::fs::rename` on Windows |

## Diagnostics and shared types

| Need | Choice | Notes |
| --- | --- | --- |
| Internal diagnostics | `tracing` 0.1, `tracing-subscriber` 0.3 | Structured host, controller, and supervisor diagnostics |
| Off-thread diagnostic writes | `susm-diagnostics` bounded writer | A byte-bounded non-blocking adapter feeds the segmented writer; lifecycle code never waits on diagnostic I/O |
| Typed errors | `thiserror` 2 | Library and boundary errors; no opaque `anyhow` errors in core crates |
| Execution and supervisor IDs | `uuid` 1 with `v7` | Sortable UUIDs wrapped in domain newtypes |
| Time | `jiff` 0.2, `QueryInterruptTimePrecise` | Jiff handles UTC history timestamps and CLI rendering; persisted lifecycle deadlines use boot-relative interrupt-time ticks that include system sleep |
| Hashes | `sha2` 0.11 | Configuration generations and upgrade manifests |
| Versions | `semver` 1 | Upgrade and protocol compatibility checks |

## Upgrade phase

Use `zip` 8 with unnecessary codecs disabled for local archives. V1 has no URL downloader or signature library; it validates the local artifact selected by the user.

## Deliberate omissions

Do not add an ORM, a second RPC framework, a SUSM-owned wire-envelope layer, a general configuration framework, a file-watching library, a log-rotation crate, or a general-purpose error wrapper. Revisit an omission only when a concrete requirement cannot be met by the selected standard library or protocol adapter.
