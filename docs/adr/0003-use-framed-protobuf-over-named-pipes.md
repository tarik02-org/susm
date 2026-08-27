---
status: superseded by ADR-0015
date: 2026-08-27
---

# Use framed Protobuf over named pipes

Use full-duplex Windows named pipes for local communication. Each frame contains a little-endian 32-bit payload length followed by a Protobuf message. Buf builds, lints, and checks the schemas; `prost-build` compiles Buf's `FileDescriptorSet` into Rust types without `protoc`. Keep the public CLI-to-controller schema separate from the internal controller-to-supervisor schema, and do not add HTTP or gRPC.

## Consequences

Every receiver must reject oversized or malformed frames before converting wire messages into domain commands. Protocol packages will be versioned independently as `susm.control.v1` and `susm.supervisor.v1`.

Use Tokio named pipes and `tokio-util` length-delimited framing. A small SUSM-owned RPC layer multiplexes request, response, event, and cancellation envelopes; do not add `tonic`, `tarpc`, or another RPC runtime. Generated Protobuf types stop at the protocol boundary and are converted into domain commands before use.

Each user's control pipes allow only that user SID and LocalSystem and reject remote clients. Authorization uses the connected Windows token rather than an identity declared in a Protobuf payload; privileged host operations require successful client impersonation and always revert it. Pipe names include the owning SID so concurrently logged-on users cannot collide.
