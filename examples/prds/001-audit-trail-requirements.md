---
id: PRD-001
title: Audit trail requirements
status: accepted
---

# PRD-001: Audit trail requirements

## Overview

The product needs a tamper-evident audit trail covering every domain state
change. Regulators, customer auditors, and the internal security team are
the primary consumers.

## Goals

- Every domain state change (create, update, delete, permission change,
  access) produces one audit record.
- Records are immutable after write and retained for seven years.
- A customer auditor can prove the log has not been edited.

## Requirements

### FR-001 — Coverage

Every write path in the system produces one audit record. The write and
the record commit in the same transaction, or the write fails.

### FR-002 — Tamper evidence

The audit store publishes hourly hash roots signed by an HSM-held key.
Any third party can verify that a claimed record existed at a claimed
time by checking the signed root.

### NFR-001 — Retention

Records are retained for seven years and produceable in under 60 seconds
via the query API.

### NFR-002 — Volume

Target throughput: 10k records/second sustained, 50k peak.

## Success metrics

- Coverage: 100 % of write endpoints emit an audit record, measured by the
  continuous-coverage job.
- Verifiability: a random sample of 100 records per day is verified against
  the signed root; failure count is zero.
- Query latency: p99 of "last 24 h for tenant T" queries under 2 s.
