# Controller storage and recovery

This document defines controller persistence, transaction boundaries, and reconciliation. SQLite is the controller's durable source of truth for user intent and its latest accepted view of execution state.

## Ownership

The controller owns one SQLite connection behind a mutex. Callers use the synchronous `Storage` interface and never receive a connection, transaction, row, SQL string, or generated protocol type.

The controller runs migrations before opening its RPC pipes or starting lifecycle reconciliation. A migration or integrity failure leaves the controller unavailable and does not recreate or discard the database automatically. Existing supervisors continue enforcing their persisted execution state.

## Durable state, not an outbox

SUSM does not keep a generic side-effect outbox. Durable lifecycle state contains every identity and deadline needed to derive unfinished work:

- `NeedsExecution` requires one execution allocation for its intent revision;
- `LaunchPending` requires launch or discovery of its recorded supervisor identity;
- a persisted stop phase requires delivery or resumption of its stop plan;
- a persisted retry deadline requires timer re-arming or immediate transition when already due.

The controller commits a lifecycle transition before running its returned effects. After a crash it loads all non-terminal state and derives the same effects again. Adapters must either make an effect idempotent by its recorded identity or inspect the external Windows fact before repeating it.

Reconciliation runs at startup and after every committed command or supervisor observation affecting a workload. It is serialized with commands for that workload; it is not a separate writer racing the command loop.

## Transaction rules

One accepted CLI mutation uses one SQLite transaction. That transaction updates lifecycle state and allocates any required execution or intent identity. The controller acknowledges acceptance only after commit.

SUSM keeps no generic client-operation ledger. The CLI never retries a mutating RPC automatically after an ambiguous transport failure. Naturally idempotent commands can be repeated; after an ambiguous `restart`, `rerun`, or `upgrade`, the operator inspects status before deciding whether to issue another command.

Enabled-job startup writes the execution, active job state, and `last_triggered_session` together. Execution terminalization writes the outcome, attempt facts delivered with it, job or service control transition, and supervisor-binding removal together. No caller can observe half of either operation.

Supervisor observations are deduplicated by authenticated durable `SupervisorId` and monotonic `ObservationSequence`; both continue across process incarnations. The transaction that advances the accepted sequence also applies the observation; duplicate delivery is a no-op, and a gap requests replay or resynchronization instead of guessing at missing transitions.

## SQLite operating mode

The database uses WAL mode, foreign keys, and `synchronous = FULL`. Lifecycle command volume is low enough that durability is worth the additional flush. The connection sets a finite busy timeout for interference from maintenance tools, but another SUSM writer is always a defect.

Persisted lifecycle deadlines use 100-nanosecond boot-relative ticks from `QueryInterruptTimePrecise`, not wall-clock UTC. This clock is monotonic across controller and supervisor process replacements and includes time spent in system sleep. A deadline that passes during sleep fires immediately after resume. UTC timestamps are stored separately for history and display. A full reboot ends the volatile manager session, so a new boot never resumes old boot-relative deadlines.

Schema migrations are forward-only and transactional. Upgrade validation must prove that the selected controller can read the existing schema before switching versions. Rollback across an applied incompatible migration is refused unless that release explicitly supplies a downgrade path.

## Relational shape

Mutable domain state uses explicit columns and child tables, not serialized Rust, JSON, or Protobuf blobs. Enum discriminants use stable storage codes with `CHECK` constraints. Variant payload columns and foreign keys make invalid combinations fail at the storage boundary before conversion into domain types.

The v1 logical tables are:

| Table group | Durable facts |
| --- | --- |
| `metadata`, `config_generations` | schema version, canonical-encoding version, accepted generation hash and acceptance time |
| `managed_workloads` | workload ID, last accepted kind, persistent enablement, and enabled-job last-triggered manager session |
| `definitions` and definition child tables | the complete latest accepted validated definition, ordered arguments, environment set/unset, success codes, restart, stop, and logging values |
| `manager_sessions` | host-issued identity, start/end state, and which session-scoped control rows belong to it |
| `service_controls`, `job_activities` | the controller state-machine discriminant, intent revision, active or draining execution identity, and pending rerun |
| `executions` and execution child tables | origin, source definition hash, execution-config hash, executable and working-directory specifications, ordered arguments, snapshotted base environment and overlay, resolved executable and final environment when available, phase, supervisor PID, live workload PID, current or next attempt, latest error, deadlines, stop intent, and terminal outcome |
| `attempts` | sequential attempt number, start/end timestamps, raw launch error or exit code, runtime duration, and factual end kind |
| `supervisor_bindings` | durable supervisor identity, expected process incarnation and Win32 process identity, binding phase, failure count, deadlines, and last accepted observation sequence |
| `installed_versions` | installed manifest identity, architecture, path, selection state, and live-use references |

`managed_workloads` does not require a current `definitions` row. This preserves enablement, last known kind, active state, and history while a definition is missing. Definition child rows use `ON DELETE CASCADE`; execution and history rows do not.

## Accepted configuration

The database stores the complete accepted normalized definition values. Controller startup loads them without reading workload TOML as a new generation. Editing files while the controller is down therefore cannot bypass explicit reload or destroy the last valid policy.

Environment values, including possible credentials, are stored as ordinary SQLite text and repeated in the supervisor runtime checkpoint because the user-authored TOML already contains them in plaintext. The directory DACL is the at-rest boundary; v1 adds no secret provider or misleading secondary encryption. Status, diagnostics, and ordinary definition RPCs never return or log environment values.

`susm reload` parses the whole candidate directory, computes its semantic hashes, enforces kind-change quiescence, and replaces all accepted-definition rows in one transaction. If the candidate generation equals the accepted generation, reload returns `unchanged` and performs no writes. Invalid candidates leave every accepted-definition row untouched.

## History retention

The controller automatically retains terminal execution and attempt metadata for 365 days and at most 10,000 terminal executions per workload, applying whichever limit removes more old rows. It always preserves the newest terminal execution for a workload so status can show the last outcome. Active executions, current manager-session control state, enablement, and accepted definitions are never history-retention candidates.

Retention runs after terminalization and as bounded maintenance during controller startup. It deletes small batches in separate transactions so pruning a noisy job cannot stall lifecycle commands for a long transaction. Workload journal retention is independent and follows `docs/logging.md`.
