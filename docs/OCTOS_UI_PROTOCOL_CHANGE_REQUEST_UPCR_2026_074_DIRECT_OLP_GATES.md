# UPCR-2026-074 — Direct OLP gate execution without protocol contract changes

Status: proposed (implemented for review)
Task: T-0125 / F4
Capability: none (no protocol change)

## Problem

Seven OLP observability gate selectors lived in an integration-test harness whose only behavior was to spawn nested `cargo test` processes for the corresponding api-gated lib scenarios. This repeated Cargo startup and build work in CI without adding protocol coverage.

## Contract

No RPC method, notification, event payload, schema, capability, permission, lifecycle, or client-visible behavior changes. The seven original scenarios and all assertions remain in the api lib harness; the gate selectors call those same scenarios directly. Removing the nested-process adapter does not change the OUP specification or wire behavior.

## Validation

The seven gate names still select exactly seven tests with no ignored scenarios. Independent T-0182 comparison confirmed identical assertions, 7/7 direct execution, and no nested Cargo invocation. The documented nested-harness structural saving is 244.08 seconds; compile/cache differences are not attributed to this record.
