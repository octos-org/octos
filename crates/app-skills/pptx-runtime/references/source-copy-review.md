# Independent source-copy review

Use this action when a separate audit of source wording was requested before production. The assigned reviewer can reconcile draft `editable_copy` with the original before the coordinator freezes the plan. Standard reconstruction starts with the maker and triggers independent review when the rendered candidate is available.

1. In the assigned review session call `{"action":"source_copy_review","page":"overview"}`. Inspect the returned original image, using native crops when details are small. Walk the page spatially before comparing with the prepared copy. Inspect repeated regions individually, including table row and column headings, diagram labels, units, legends and embedded lettering where present.
2. Compare that reading against the draft row. Write a report file in this page’s review directory. For each visual region give its location and observation. Record short or repeated standalone labels in `labels`, one full occurrence per item, with duplicates. Each occurrence consumes one editable-copy entry across the page. The runtime adds any remaining confirmed standalone occurrences to the copy with distinct IDs. Place each occurrence in just one region. List unchanged draft IDs actually verified in `confirmed_ids`; the runtime accounts separately for entries in `changes`. Use a full entry when a unit or letter belongs inside a longer label. Use an empty list for a region with no independently counted labels. Record questions affecting essential wording or occurrence counts in `unresolved`. Typography, spacing and stylistic notes with agreed wording belong in optional `notes`.

```json
{
  "regions": [
    {"location":"Legend under the graph","observation":"Three individually labelled swatches in reading order","labels":["Control","Low","High"]}
  ],
  "confirmed_ids":["control","low-dose"],
  "changes": [
    {"action":"replace","id":"axis-unit","text":"µm","reason":"Vertical axis uses a micro sign"},
    {"action":"exclude","id":"mark","reason":"At native resolution this is the border joining two cells"}
  ],
  "unresolved":[],
  "notes":[]
}
```

3. Call `{"action":"source_copy_review","page":"overview","expected_sha256":"<from opening the page>","report":"review/overview/source.json"}`. The runtime binds the original, the assigned reviewer, the report and the draft. On a complete review it applies the named changes and any additional standalone labels to this page in the draft plan. The coordinator can then freeze the resulting plan directly. Preserve the roster as updated by the reviewers. A pending review leaves the draft copy intact and returns the proposed additions, all unconfirmed draft IDs, and open questions. Inspect those source regions, reopen the page to get the current revision, then submit the resolved report including its changes.
4. `evidence_init` validates any source reviews already recorded on the plan. It reports which recorded reviews are stale or incomplete. The independent reviewer checks the original against the published render and editable objects at delivery.
