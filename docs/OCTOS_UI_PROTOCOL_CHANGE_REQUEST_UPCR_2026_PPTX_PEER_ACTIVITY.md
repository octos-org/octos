# UPCR 2026 PPTX peer activity dispatch

The Octos host binds a page plan to its coordinator session, workspace, digest and assigned peer stage IDs. A batch peer send uses that authorization to dispatch independent page work. The coordinator can reconcile a backlog of checkpoint lifetimes in its active session. A peer completing an admitted activity can wake the assigned reviewer or worker on the bound occurrence. The cover workflow uses OctoScript to select the next page operation from durable events.

The existing peer tool success and failure envelope stays compatible. The host additionally accepts a bound page dispatch, checkpoint handoff and activity result through those tools. Workspace, originator, peer identity and occurrence bindings are checked on receipt. Stale or duplicate events cannot authorize new work. Host-managed app peers retain their existing authorization boundaries. Clients without the corresponding tool capability use the existing peer send and gather paths.

The cover activity limits replay to 512 calls per capability and a bounded event file. Exhausting those limits ends that path with an error. A failure after publishing a peer intent but before persisting its queue entry can leave the intent pending; recovery of that interval needs reconciliation.

Retained regressions cover dispatch to the staged identities, restart replay, worker exit before producing a candidate, signed repair routing, and refusal of stale or foreign checkpoint handoffs. Verify the host with and without its API feature.
