# Integration deletion audit — T-0505 follow-up

Scope: the 17 deleted integration targets not restored by the five representative tests in this follow-up. This is a disposition audit, not a claim that every deletion has equivalent coverage and not a request to restore whole files.

| Deleted target | Equivalent retained coverage or deletion reason |
|---|---|
| `acp_embedding` | **Partial; acceptable deletion.** `acp_integration` externally exercises ACP transport/factory traits and initialize/new/prompt/load. `AcpCommand::factory`, its no-provider error, and default iteration budget are not covered. |
| `auth_work_secret_cli` | **Targeted security gap.** `session_ingress_work_secret` covers valid WS access, revocation, raw-RPC denial, and capability filtering—not CLI stdout secrecy, non-disclosing list output, malformed secrets, wrong-session, or expiry. Use focused CLI/store tests, not the old broad suite. |
| `build_git_metadata` | **Justified cost deletion; no equivalence claim.** Fifteen nested offline Cargo-build tests covered linked worktrees, packed refs, and hash freshness. Valuable build hygiene, but expensive and not runtime/security coverage; `build.rs` remains untested. |
| `build_output_dir_validation` | **High-value security gap.** Workflow tests retain slug/output mappings only. Malicious metadata, traversal, template mismatch, symlink/TOCTOU confinement, and HTTP 400 mapping need compact validator/no-follow tests. |
| `cli_tests` | **Broad-suite deletion reasonable.** `pre-release.sh` retains basic status, skills, completion, clean, and init smoke checks. Dynamic completion SSOT, secret-store reporting, overwrite, and cleanup edges are narrow follow-ups, not grounds to restore 27 process tests. |
| `coding_multi_session` | **Strong secondary integration gap.** Generic path confinement does not prove two same-profile sessions with different workspace hints cannot read each other's files or histories. A compact two-session isolation regression is justified if this area is revisited. |
| `content_classifier_wiring` | **Deletion justified; tautological test.** It assigned `AppState.content_classifier` and recreated the enabled branch, never startup or routing. See the independent dormant-wiring finding below. |
| `credential_pool_wiring` | **Deletion justified; tautological test.** `octos-llm::credential_pool` retains concurrency, corruption quarantine, redaction, schema, and repair-failure coverage. Startup fallback and actual router rotation remain uncovered; see below. |
| `preview_auth` | **High-value security gap.** No retained equivalent for unauthenticated rejection, cross-tenant preview/session denial, forged `X-Profile-Id`, or admin Host/header routing. Session-mutation ownership tests do not exercise preview routes. |
| `preview_signed` | **High-value security/failure gap.** No retained coverage for sign authorization, unknown/expired token 404, revoked issuer 403, traversal, no-referrer, token caps/sweeper, or 429 `Retry-After`. Add focused current-router/token-cache tests, not the old 946-line file. |
| `rfc3_lane_routing_wiring` | **Partial.** `lane.rs` covers default candidates/order and adaptive tests cover generic breaker failover. Lane-specific filtering/fallthrough, topic stamping, and profile serde remain uncovered. |
| `skill_compat_gate` | **Reasonable deletion.** Playwright covers install/reload/remove lifecycle and bundled-manifest tests cover schemas. Deleted run/secret assertions invoked the fixture directly, not Octos's dispatcher. |
| `speculative_overflow_delivery` | **Covered in two halves.** `session_actor_tests::test_speculative_overflow_concurrent` covers overflow production; bus API-channel tests cover committed watcher fanout/correlation. |
| `swarm_api` | **Core covered; HTTP not directly covered.** `octos-swarm` integration tests cover orchestration, policy, validators, and costs. Thin-route/review coverage was removed; reviewer identity and sink failures are separate follow-ups. |
| `ws_slash_commands` | **Helper-level, not WS E2E.** Slash interception, `/new sites`, and workspace-root override lack direct tests, but the old target never exercised the actual WS call site either. Small inline helper tests suffice. |
| `ws_token_url_decode` | **Small auth gap.** Ordinary query-token authentication is retained elsewhere; `%21`, literal `+` versus `%2B`, and wrong-decoded-token rejection are not. Add a table-driven middleware regression rather than the old harness. |
| `x_profile_id_strip` | **High-value security gap.** Session-mutation ownership and cross-profile frame filtering do not exercise trusted/untrusted header stripping across REST/WS, admin/non-admin hops, or cross-tenant messages/files/workspace handlers. Group with `preview_auth` in a compact auth-boundary suite. |

## Independent lead-facing finding

At both `0f903d237` and `b823eb0ae`, `serve.rs` constructs and stores credential-pool and content-classifier fields, but `AdaptiveRouter::attach_credential_pool` and `set_content_classifier` have no call sites outside their definitions. The deleted wiring tests merely assigned `AppState` fields and could not detect this. This predates the deletion commit and was reported to lead separately: confirm whether an external host intentionally consumes the public fields; otherwise file a production bug and test config → builder → router attachment → routing behavior.

No wholesale restoration is recommended. Highest-value follow-ups are compact preview/X-Profile/build-output security tests, work-secret CLI/malformed/mismatch/expiry tests, and one same-profile two-workspace isolation test.
