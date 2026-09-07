---
status: accepted
date: 2026-08-27
---

# Use finite deterministic recovery defaults

Default services restart on failure up to 10 consecutive times, with deterministic exponential backoff from 250 milliseconds to 30 seconds and no jitter. An attempt that runs for 5 minutes resets both the consecutive retry count and backoff exponent. Jobs still default to no restart; an opted-in job has a finite execution-wide retry limit that never resets.

Supervisor recovery has a separate execution-wide budget of 8 confirmed failures, backoff from 100 milliseconds to 5 seconds, a 5-second handshake timeout, and a 3-second discovery window. A successful supervisor handshake does not reset failures already consumed by that execution. Discovery timeout alone is not a failure until the expected supervisor is confirmed missing or unusable.

## Consequences

Local recovery is reproducible and cannot loop forever without becoming visible as a blocked service or failed job. Workload failures cannot consume infrastructure budget, and supervisor failures cannot consume workload attempts. Persisted retry deadlines survive controller or supervisor replacement unchanged.
