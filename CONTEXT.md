# User workload management

SUSM manages long-running services and one-shot jobs owned by a signed-in Windows user. This glossary distinguishes definitions, requested lifecycle, logical executions, and operating-system processes.

## Language

**Managed workload**:
A named process definition managed by SUSM. It is the umbrella term for a service or job.
_Avoid_: unit, managed service when the kind is unknown

**Service**:
A managed workload that SUSM keeps in a requested lifecycle state.
_Avoid_: Windows service

**Job**:
A managed workload whose execution is expected to terminate.
_Avoid_: Windows Job Object, one-shot service

**Execution**:
One logical run requested for a workload. A service restart closes one execution and creates another.
_Avoid_: process, attempt

**Attempt**:
One launch try within an execution. It ends in a launch failure or owns one workload process until that process exits; a restart policy may create multiple attempts.
_Avoid_: execution, retry

**Restart budget**:
The number of automatic retries still permitted inside one execution. A service budget resets after a stable run; a job budget is finite for the whole execution.
_Avoid_: execution count, supervisor failure count

**Workload process**:
The operating-system process for one attempt.
_Avoid_: child, target, application, service process

**Host**:
The small machine-wide LocalSystem Windows service that starts or restarts a controller for each eligible local interactive user.
_Avoid_: controller, supervisor, service manager

**Controller**:
The per-user authority for workload definitions, enablement, desired state, and orchestration.
_Avoid_: central daemon, manager

**Supervisor**:
The durable owner of the actual state and lifetime of one active managed workload execution, implemented by one supervisor process incarnation at a time.
_Avoid_: runner, per-service daemon

**Supervisor incarnation**:
One operating-system process generation implementing a durable supervisor. Replacement increments the incarnation while preserving supervisor identity, runtime journal, and observation sequence.
_Avoid_: supervisor ID, workload attempt

**Supervisor-launch budget**:
The controller-owned limit for failures while discovering, launching, or replacing a supervisor for one execution. It is independent of the workload restart budget.
_Avoid_: restart budget, process retry count

**Desired state**:
The requested lifecycle condition of a service within one manager session, independent of its current execution and attempt.
_Avoid_: target state

**Actual state**:
The observed lifecycle condition of a workload, execution, and attempt.
_Avoid_: current status

**Observation**:
A sequenced fact published by a supervisor after it durably records an execution transition. The controller commits observations into its own state and history.
_Avoid_: desired state, command, log entry

**Enablement**:
A persistent request to start a service or create at most one job execution in each manager session.
_Avoid_: desired state, autostart flag

**Registration**:
A persistent per-user opt-in that makes a SID eligible for its version-selected controller to be started by the host.
_Avoid_: enrollment, automatic provisioning

**Release bundle**:
The native-architecture directory produced for one SUSM release. It contains one user bundle and one host image.
_Avoid_: user bundle, installation

**User bundle**:
The manifest, CLI, controller, and supervisor installed and selected for one user.
_Avoid_: release bundle, host image

**Host image**:
The `susm-host.exe` binary installed machine-wide through an explicit elevated operation.
_Avoid_: user bundle, controller

**Manager session**:
One SUSM management lifetime for a user, beginning at logon and ending at the matching logoff. A controller may restart several times within it.
_Avoid_: controller process lifetime, Windows session

**Windows Job Object**:
The Windows kernel object that contains a workload process tree and enables hard termination.
_Avoid_: job
