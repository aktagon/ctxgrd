---
title: Approximate Nearest-Neighbour Recall Under Memory Pressure
research:
  type: academic
---

# Approximate Nearest-Neighbour Recall Under Memory Pressure

## Introduction

Vector databases trade recall for memory footprint when the index no longer
fits in RAM. This study measures how two index families degrade once the
working set spills past available memory.

## Results

HNSW retained 0.98 recall@10 until the index exceeded 1.4x physical memory,
then fell sharply. IVF-PQ held a flatter 0.88 recall@10 across the same
pressure range at one-third the resident footprint.

## Discussion

For memory-constrained deployments the product-quantised index is the safer
default; HNSW is preferable only when the graph is guaranteed to stay
resident.

## References

- Malkov and Yashunin, Hierarchical Navigable Small World graphs, 2018
- ann-benchmarks.com results, sift-128-euclidean, 2025 snapshot

## Limitations

Only two index families were measured; scalar-quantised Flat was excluded.
Recall was evaluated on a single dataset, so cross-domain generalisation is
unverified.
