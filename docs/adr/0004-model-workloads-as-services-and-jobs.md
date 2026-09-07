---
status: accepted
date: 2026-08-27
---

# Model workloads as services and jobs

Every managed workload declares `kind = "service"` or `kind = "job"`. A service has a desired state and uses `start`, `stop`, and `restart`; a job creates a finite execution and uses `run`, `cancel`, and `rerun`. An execution may contain multiple launch attempts created by its restart policy, including attempts where process creation fails, but v1 permits only one active execution for a workload.

## Consequences

Services default to restarting on failure and jobs default to never restarting. `restart.policy = "always"` is invalid for jobs, while `"on-failure"` jobs require a finite `restart.max_restarts`. Enabling a service starts it at each logon; enabling a job creates at most one execution per logon, even if the controller restarts.
