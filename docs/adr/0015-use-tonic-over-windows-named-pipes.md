---
status: accepted
date: 2026-08-27
---

# Use Tonic over Windows named pipes

Use Tonic gRPC over byte-mode Windows named pipes for host, controller, supervisor, and CLI RPC. Tonic owns HTTP/2 framing, multiplexing, flow control, streaming, deadlines, cancellation, and status codes; SUSM exposes no TCP listener or public HTTP endpoint. Buf emits a `FileDescriptorSet`, and `tonic-prost-build` generates Prost messages and Tonic client and server code without `protoc`.

## Consequences

Keep public CLI-to-controller, privileged host, and internal controller-to-supervisor Protobuf packages separate and versioned. The named-pipe adapter still owns pipe creation, explicit ACLs, remote-client rejection, reconnection, and authentication. It copies the authenticated caller SID into Tonic request extensions through `Connected`; handlers never trust identity from Protobuf fields or gRPC metadata. Generated Protobuf and Tonic types stop at the protocol adapter and are converted into domain commands.

Tonic does not define SUSM operation idempotency, protocol capability negotiation, event replay cursors, supervisor adoption, or whether an accepted workload mutation continues after its initiating RPC is cancelled. SUSM specifies those semantics explicitly. The executable spike under `spikes/tonic-named-pipe` proves unary RPC, server streaming, transport cancellation, a protected System-and-user DACL, remote-client rejection configuration, caller SID and client-process-ID propagation, server-process-ID inspection, and Buf descriptor-set code generation on Windows.

Supervisors connect to one per-user internal controller pipe and keep one bidirectional attachment stream. The controller commits each observation before acknowledging its replay sequence. Control clients do not receive a generic mutation-idempotency ledger.
