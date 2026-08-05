---
id: TASK-001
title: Normalize source units to per-million
status: done
agents: [developer]
---

# TASK-001: Normalize source units to per-million

## Goal

Convert every adapter's declared rate unit to the canonical per-million form.

## Files allowed

- `src/normalize.rs`

## Requirements

- A `per_token` source is scaled; a `per_million` source is not.
- An unrecognised unit is an error, never a guess.

## Acceptance

- [x] A `per_token` source's rates appear x1,000,000 in the canonical record.
- [ ] An unrecognised declared unit yields `SourceUnitUnknown`.

## Out of scope

Deliberately deferred, and never a finding — the rule scans the acceptance
heading only, so an open box here must stay silent.

- [ ] Per-request pricing, which no adapter reports yet.
