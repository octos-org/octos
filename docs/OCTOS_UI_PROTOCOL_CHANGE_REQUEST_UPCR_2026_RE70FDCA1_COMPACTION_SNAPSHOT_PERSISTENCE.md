# UPCR-2026-RE70FDCA1 — Compaction snapshot persistence failure reporting

Status: proposed (implemented for review)
Task: T-0205 / D-06
Capability: none (existing context lifecycle/status surfaces only)

## Problem

A manual `session/compact` request could report `compacted=true` and
`status=installed` after its context snapshot write failed. Automatic
compaction likewise published an installed completion while retaining only an
in-memory generation. A later reopen therefore could not recover the
reportedly installed snapshot.

## Contract

No RPC method, notification name, event schema, capability, permission, or
compaction algorithm changes. Manual compaction now returns an internal RPC
error with `kind=compaction_persist_failed` and operation/session/path context
when snapshot persistence fails; it does not return an installed result or
emit the successful completion notifications. Automatic compaction (AppUI and SessionActor) may retain
its in-memory generation for the current turn, but its existing completion
record is marked `failed` and its context status includes `degraded=true` and
`persistence_failed` details instead of presenting a durable installation.
Later ordinary status publication retains that marker for the same failed
compaction; a subsequent successful generation clears it. Successful
compaction and recovery behavior are unchanged.

## Validation

Production snapshot writes are fault-injected by replacing the
`context_ledgers` directory with a file. Manual, pre-turn automatic, and
in-loop automatic cases fail closed or publish explicit degradation; normal
manual compaction persists and reopens an installed snapshot. The focused
in-loop and compact suites, formatting, registry lint/self-test, and quality
checks are run with the change.
