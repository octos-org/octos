spec: task
name: "Reload startup profile models for the next turn"
tags: [oup, model, runtime, profiles]
---

## Contract

A successful profile LLM select, upsert, or delete invalidates cached sessions
and replaces the profile runtime, including runtimes loaded at startup. The
choice is shared by sessions using that profile. Existing turns retain their
original runtime; replacement reuses open stores and preserves the startup
signing policy and effective profile defaults.

The immutable startup map stops being eligible after the first configuration
commit. Removing the final model returns `deferred`; failed bootstrap returns
`persisted_but_not_live` and can be retried by a later turn. Neither condition
may silently restore the startup provider or advertise a saved model as live.
Store-backed switches do not require a server restart. Reselecting the current
model does not invent a restart requirement.

## Verification

- `should_reload_startup_profile_llm_mutations_without_restart`
- `should_switch_startup_model_for_next_turn_and_not_restore_deleted_boot_model`
  covers cached session replacement, old runtime retention, signing policy,
  shared stores, selection stamps, idempotent selection, and final deletion.
- `should_not_fall_back_to_startup_runtime_when_replacement_fails`
- `should_reload_runtime_while_in_flight_turn_holds_episode_store`
- `should_refuse_stale_profile_runtime_insert_after_generation_bump`
