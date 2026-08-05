---
title: Serverless vs Always-On Cost Crossover for Bursty APIs
research:
  type: analysis
---

# Serverless vs Always-On Cost Crossover for Bursty APIs

## Summary

For a bursty HTTP API, AWS Lambda is cheaper than an always-on container
until sustained utilisation crosses a break-even band. This report locates
that band for a representative workload.

## Findings

At a 512 MB function and 120 ms median duration, Lambda undercuts a single
always-on Fargate task below roughly 3.2 million invocations per month; above
that the always-on task wins on unit cost.

## Evidence and sources

- AWS Lambda pricing, us-east-1, January 2026
- AWS Fargate pricing, us-east-1, January 2026
- Internal load profile, 30-day production traffic sample

## Limitations

Cold-start latency cost is excluded from the dollar model. The crossover
assumes steady traffic; spiky diurnal patterns shift the band.
