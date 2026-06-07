---
id: ADR-001
title: Use event sourcing for the audit trail
status: accepted
depends_on:
  - PRD-001
---

# ADR-001: Use event sourcing for the audit trail

## Status

Accepted

## Context

PRD-001 requires a tamper-evident audit trail retained for seven years and
queryable by tenant, actor, and time window. The current implementation
writes audit rows to a mutable SQL table, which allows administrative
updates and is hard to prove intact after the fact.

## Decision

We store all domain state changes as an append-only event stream backed by
a write-only log. Current-state projections are derived from the stream and
rebuilt as needed. The log's hash chain is signed at hourly boundaries.

Rejected alternatives:

- Mutable audit table with trigger-enforced write-only constraints. Rejected
  because superuser access can still bypass triggers.
- External SaaS audit log. Rejected on data-residency grounds.

## Consequences

- Events are immutable by construction; no administrator can edit history
  without invalidating the next signature.
- Query paths need projections built from the stream, adding operational
  complexity and a new class of bugs (projection drift).
- Rebuilding projections after a schema change takes hours at current
  volume — see PMR-001 for a real example of rebuild-related impact.
