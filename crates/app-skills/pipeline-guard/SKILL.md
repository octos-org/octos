---
name: pipeline-guard
description: Validates and optimizes run_pipeline DOT graphs with model selection from QoS catalog
version: 0.1.0
author: octos
always: true
---

# Pipeline Guard

Automatically validates DOT graphs and assigns optimal models before `run_pipeline` executes.

This is a lifecycle hook — it runs transparently before every pipeline execution.

## What it does

1. **Validates DOT structure** — checks for cycles, disconnected nodes, malformed syntax
2. **Assigns models from QoS catalog** — reads `model_catalog.json` scores and assigns:
   - Search workers (dynamic_parallel) → FAST models (round-robin across pool)
   - Analyze/synthesize nodes → STRONG models (best QoS score)
   - Planner model → STRONG
3. **Concurrent distribution** — PID-based random start index so parallel pipelines use different models

## No user action needed

The LLM writes DOT graphs without `model=` attributes. This hook injects them automatically based on live QoS scores.
