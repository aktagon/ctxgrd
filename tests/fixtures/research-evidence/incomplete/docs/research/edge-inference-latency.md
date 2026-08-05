# On-Device LLM Inference Latency on Consumer Hardware

## Background

Small language models (1-3B parameters) are increasingly run locally on
laptops and phones to avoid network round-trip latency and per-token API
cost.

## Findings

A 4-bit-quantised 3B model reaches 30-45 tokens per second on an Apple M3 and
8-12 tokens per second on a mid-range Android SoC, measured at a 512-token
context window.

## Final conclusion

On-device inference is viable for latency-sensitive, short-context tasks on
current flagship hardware, but batch and long-context workloads still favour
a hosted endpoint.
