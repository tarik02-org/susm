---
status: accepted
date: 2026-08-27
---

# Use Rust for core processes

Implement the CLI, controller, and supervisors in Rust. SUSM needs direct Win32 handle ownership and may run one supervisor process per managed service, so predictable process overhead and native deployment matter more than the easier Windows integration offered by C#; a future tray application may use another language through the control protocol.

## Consequences

Win32 calls will use Microsoft-maintained Rust bindings. Unsafe code must stay behind small interfaces that own and release their Windows handles.

