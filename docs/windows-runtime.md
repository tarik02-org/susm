# Windows runtime behavior

This document defines the Win32 launch, process-tree, console-control, clock, and atomic-file operations used by host, controller, and supervisors.

## Workload launch

The host, controller, and supervisors call `CreateEnvironmentBlock` with inheritance disabled against the manager-session user's primary token. They require the interactive profile to be loaded, copy the Unicode block into validated environment entries, and destroy the Win32 block after snapshotting it. They never inherit LocalSystem host variables or a stale controller-start environment. The process-lifecycle spike verifies that the fresh block contains the loaded user's `USERPROFILE`.

The supervisor owns one hidden private console and one kill-on-close Windows Job Object for its active execution. For every attempt it:

1. persists `Launching` in the runtime journal;
2. resolves preflight paths and builds the final environment, or reuses the resolved values already persisted for this execution;
3. opens `NUL` for stdin and creates stdout/stderr pipes even when capture is disabled;
4. calls `CreateProcessW` with `CREATE_SUSPENDED`, `CREATE_NEW_PROCESS_GROUP`, `CREATE_UNICODE_ENVIRONMENT`, `EXTENDED_STARTUPINFO_PRESENT`, explicit standard handles, and the snapshotted working directory and environment;
5. restricts inherited handles with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` to stdin and the two child pipe ends;
6. assigns the suspended process to the execution Job Object;
7. starts asynchronous pipe reads;
8. resumes the primary thread.

No workload instruction runs outside the Job Object. Descendants remain in the job unless Windows nested-job rules place them in a stricter nested job; SUSM never enables breakaway. Closing or terminating the execution Job Object ends the contained process tree.

V1 workload stdin is always the null device; SUSM has no interactive input RPC. The adapter rejects an encoded Windows command line whose terminating NUL would exceed the Win32 32,767 UTF-16-code-unit limit as a typed preflight launch failure. It passes the absolute executable separately as `lpApplicationName`, never asks Windows to infer it from the command line, and embeds a long-path-aware application manifest in every SUSM executable.

The controller starts the supervisor with `CREATE_NO_WINDOW`. At startup the supervisor uses `AllocConsoleWithOptions(ALLOC_CONSOLE_MODE_NO_WINDOW)` to create a private console session that has no window. The workload inherits that console but starts a new process group. `ctrl-break` targets the workload root process-group ID, so the supervisor does not receive its own stop signal.

The process adapter converts the literal argument vector with the standard Microsoft C runtime quoting algorithm. SUSM never concatenates config values into a shell command. Scripts require an explicitly configured shell executable.

The executable spike in `spikes/windows-process-lifecycle` verifies hidden-console creation, suspended Job Object assignment, targeted `CTRL_BREAK`, descendant termination, and argument round-trip on Windows.

## Controller restart

The host monitors the per-user controller process handle throughout the manager session. It retries an unexpected exit indefinitely with deterministic backoff from 250 milliseconds, doubled to a 30-second cap. Keeping the controller alive for 5 minutes resets the backoff exponent.

Active supervisors continue while the controller retries. `susm controller restart` authenticates directly to the host, cancels any pending delay, resets the exponent, and launches the user's selected controller build immediately. Manager-session ending and an intentional upgrade handoff are expected exits and do not enter restart backoff.

An upgrade handoff resets the retry exponent and starts the newly selected controller immediately. If that build cannot become ready, the same indefinite capped-backoff policy continues until the operator explicitly selects another build.

## Host service recovery

The elevated installer registers `susm-host` as an automatic LocalSystem Windows service and configures SCM failure actions to restart after 1 second, 5 seconds, and 30 seconds. SCM repeats the last action for later failures, so host recovery also continues indefinitely with a 30-second cap. Five minutes without failure resets the SCM failure count. Failure actions apply to crashes and nonzero service exits, not a reported successful stop.

Upgrade or uninstall disables the service before an intentional stop when it must prevent an already queued SCM restart. This follows the documented `ChangeServiceConfig2W` queued-restart behavior.

## Deadlines

The clock adapter reads `QueryInterruptTimePrecise`. Domain state stores 100-nanosecond boot-relative instants. These instants remain comparable across process replacement, include system sleep, and never cross a manager-session reboot boundary.

Manager-session ending creates one hard deadline 30 seconds after the host signals the ending event. All active workloads begin stop or cancel in parallel. The supervisor clamps a configured graceful timeout to the first 25 seconds, reserving the final 5 seconds for Job Object termination, a terminal runtime frame, and journal-segment finalization. At the hard deadline it prioritizes killing the process tree and syncing terminal runtime state over optional log compression or retention.

A normal machine shutdown enters the same path for every live manager session through the host service's preshutdown notification. SCM grants the host 45 seconds, so signaling and the complete 30-second workload deadline finish before the service reports stopped. Sudden power loss remains crash recovery, not graceful shutdown.

## Atomic file replacement

Runtime-journal compaction, journal-segment finalization, compressed-segment installation, and small pointer-file updates use a same-directory staging file. The writer flushes the staging file, closes any writable destination handle, then calls `ReplaceFileW` with write-through semantics. Initial installation uses `MoveFileExW` with replace-existing and write-through flags when no destination exists.

Readers open files with read, write, and delete sharing. A reader that already holds the old file continues reading those bytes after replacement; a new open sees the replacement. The executable spike in `spikes/windows-atomic-replace` verifies both Rust's default reader sharing and the adapter's explicit sharing on Windows.
