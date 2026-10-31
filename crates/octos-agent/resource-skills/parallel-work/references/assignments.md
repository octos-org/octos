# Assignments and returns

These are examples, not a mandatory new protocol. Prefer an existing project or
domain contract. A short sourced question needs a short brief, not a job framework.

## Compact worker brief

```text
Goal: <the user outcome this unit contributes to>
Assignment: <one answerable question or owned deliverable>
Inputs: <authoritative paths/URLs/versions; mark unreviewed drafts>
Method: <applicable skill and shared tool paths; relevant setup already verified>
Context: <interfaces, terminology, dependencies, relevant user constraints>
Access: <actual capabilities; configuration paths rather than copied credentials>
Ownership: <read-only scope, or exact writable outputs; shared inputs stay stable>
Return: <answer/artifacts in the existing format; evidence and unresolved issues>
Done when: <observable requirements and appropriate validation>
```

The worker should return the result, what changed, relevant checks actually run,
evidence locations or citations, and remaining limitations. Record an input
revision or artifact hash where changes could otherwise make the evidence stale.
An optional structured return can use `task_id`, `input_version`, `outputs`,
`checks`, and `open_issues`; do not create another manifest when one already exists.

## Choose the partition and the integration check

| Task | Useful independent work | Shared preparation | Parent's final responsibility |
|---|---|---|---|
| Answer a complex question | Distinct claims, sources, or competing explanations | Scope, time period, definitions, source requirements | Reconcile evidence and contradictions; answer the user's question |
| Change software | Modules behind agreed interfaces; read-only diagnostics | Repository instructions, runtime, interface, existing tests | Merge owned changes; test interactions and end-to-end behavior |
| Analyze data | Separable extraction or transformation partitions | Schema, units, missing-value policy, deduplication keys | Global joins/deduplication, correct denominators, aggregation checks |
| Produce a document/site/deck | Independent units using an established design/build method | Content authority, shared style/interfaces, working tools | Order, navigation or cross-references, consistency, final artifact checks |
| Investigate a failure | Independent causal hypotheses or evidence sources | Reproducer, known facts, unchanged baseline | Distinguish hypotheses from causes; implement and verify the selected repair |

For code with coupled edits to one file, use one writer and read-only reviewers,
or sequence the changes. Different functions in the same file are not guaranteed
safe concurrent writes. For data, partial averages are not additive; return sums
and counts or sufficient records, and deduplicate globally when keys cross inputs.
For questions, do not invent parallel work if a direct answer is already supported.

## Useful progress evidence

Record only what helps decide the next step. For repeated repair, this may be:

```text
Input/version: ...
Observed requirement failure: ...
Changed assumption or artifact: ...
Check/outcome: ...
Remaining uncertainty / next justified action: ...
```

A legitimate experiment can keep the same output while disproving a hypothesis.
A regression test may need another run after an environment change. Repetition
alone is not failure; repeated actions without new information require diagnosis.
Use affected-case comparisons where appropriate, not a universal optimization
score. Preserve errors and raw evidence rather than replacing them with summaries.
