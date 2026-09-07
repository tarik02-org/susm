# Windows process lifecycle

This executable checks the Win32 assumptions behind workload launch and stop:

- `CreateEnvironmentBlock` produces a fresh non-inherited user environment containing the loaded interactive profile;
- a hidden `CREATE_NEW_CONSOLE` supervisor can create a workload as a suspended process group;
- the supervisor can assign that process to a kill-on-close Job Object before resuming it;
- `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group_id)` reaches the workload without reaching the supervisor;
- terminating the Job Object ends a workload-created descendant as well as its root process.
- the Win32 command-line encoder round-trips empty, spaced, quoted, and trailing-backslash arguments through the standard parser.

Run from the repository root:

```powershell
mise exec -c "cargo run -p windows-process-lifecycle-spike"
```
