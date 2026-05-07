---
id: PMR-001
title: Dropped audit events on 2026-03-15
status: closed
incident_date: 2026-03-15
depends_on:
  - ADR-001
---

# PMR-001: Dropped audit events on 2026-03-15

## Summary

Between 09:12 UTC and 09:47 UTC on 2026-03-15, the audit write path
silently dropped approximately 14 000 events after a deploy of the
projection service. No customer-visible data loss occurred — the events
were re-emitted from upstream replay — but the hourly signed root for
the 09:00–10:00 window had to be recomputed and re-signed, which invalidated
the pre-incident root published to customers.

## Impact

- 14 031 events missing from the primary stream during the incident
  window, all recovered.
- Signed root for 09:00–10:00 UTC re-issued, superseding the earlier
  publication.
- Two customer audit exports scheduled between 09:12 and 10:15 were
  re-run at our initiative.

## Timeline

| Time (UTC) | Event                                                              |
| ---------- | ------------------------------------------------------------------ |
| 09:00      | Deploy of projection service v4.7.2 begins.                        |
| 09:12      | Write-path worker exits on startup due to config schema change; no |
|            | events persisted after this moment.                                |
| 09:31      | Alert fires on "audit write lag > 10 minutes".                     |
| 09:39      | On-call rolls back to v4.7.1.                                      |
| 09:47      | Write path catches up; backfill begins from upstream replay.       |
| 11:04      | Backfill complete, signed root recomputed and re-published.        |

## Root Cause

Config schema change in v4.7.2 renamed `stream.endpoint` to
`stream.primary_endpoint` but the write-path worker's loader still
required the old key. The worker crashed on startup rather than degrading,
and the orchestrator retried the failed container rather than alerting on
zero-healthy-workers. See ADR-001 for the invariant the write path is
expected to maintain.

## Action Items

- Add a CI check that every renamed config key has a compatibility shim
  for one release.
- Add an alert on "zero healthy write-path workers for > 2 minutes" that
  pages on-call immediately, not at the 10-minute lag threshold.
- Add a deploy-gate that blocks rollout if the write path's startup
  probes fail on any replica.
