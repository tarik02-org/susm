# susm

Sucking User Service Manager is a per-user manager for long-running services and one-shot jobs on Windows.

The v1 implementation manages per-user services and one-shot jobs through authenticated Windows named pipes. Definitions are strict TOML, controller intent and history are durable in SQLite, and each active execution runs in an independently recoverable supervisor with Job Object isolation and rotating binary journals.

## Binaries

| Binary | Role |
| --- | --- |
| `susm` | Command-line client |
| `susm-host` | Stable Windows service that anchors the per-user controller |
| `susmd` | Per-user controller |
| `susm-supervisor` | Supervisor for one active managed workload |

## Development

Install the pinned toolchain and check the workspace:

```powershell
mise install
cargo check --workspace
```

Build a local release bundle:

```powershell
cargo xtask package --version 0.1.0
```

Install `susm-host` once from an elevated PowerShell, then register a user bundle without elevation:

```powershell
.\dist\susm-0.1.0-x86_64-pc-windows-msvc\host\susm-host.exe install
.\dist\susm-0.1.0-x86_64-pc-windows-msvc\user\susm.exe install --user
```

Workload definitions live in `%USERPROFILE%\.config\susm\workloads.d`. A minimal service is:

```toml
kind = "service"
executable = "my-server.exe"
arguments = ["--listen", "127.0.0.1:9000"]
```

Use `susm reload`, then `susm start <name>`. `susm --help` lists jobs, history, logs, enablement, upgrade, rollback, JSON output, and completions.

Architectural decisions live in [`docs/adr`](docs/adr). Project terminology lives in [`CONTEXT.md`](CONTEXT.md).

## Design contracts

- [`configuration`](docs/configuration.md)
- [`state machines`](docs/state-machines.md)
- [`protocol`](docs/protocol.md)
- [`storage`](docs/storage.md) and [`supervisor recovery`](docs/supervisor-runtime.md)
- [`workload logging`](docs/logging.md) and [`SUSM diagnostics`](docs/diagnostics.md)
- [`filesystem layout`](docs/filesystem-layout.md)
- [`host bootstrap`](docs/host-bootstrap.md)
- [`installation and upgrade`](docs/installation-and-upgrade.md)
- [`CLI`](docs/cli.md)
- [`Windows runtime`](docs/windows-runtime.md)
- [`dependency choices`](docs/dependencies.md)

## License

Licensed under either the MIT License or Apache License 2.0, at your option.
