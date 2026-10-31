spec: task
name: "Activate MiniMax M Plan Flash Preview without restarting for model changes"
tags: [llm, minimax, model, runtime, profiles]
---

## Contract

The provider registry recognizes `minimax-coding`, `minimax-m-plan`, and
`minimax-token-plan`. Its default is `MiniMax-M3.1-Flash-Preview`, declared in
the canonical model catalog and advertised by `profile/llm/catalog` with the
international OpenAI-compatible endpoint `https://api.minimax.io/v1`.

The family resolves `MINIMAX_CODING_API_KEY`; regular MiniMax and China-region
keys are not implicit aliases. Bare MiniMax model auto-detection remains on the
existing `minimax` family. Account entitlement is checked by the provider.

A configured M Plan model can replace a startup profile's active model for
the next turn. Existing turn handles retain their original runtime. Switching
back preserves both configurations and does not require another server restart.

## Verification

- `minimax_coding_uses_subscription_key_and_latest_plan_model`
- `every_family_with_a_default_resolves_it_from_the_catalog`
- `every_entry_pins_its_user_facing_configuration_surface`
- `minimax_coding_catalog_advertises_subscription_route_and_flash_preview`
- `minimax_coding_select_reloads_startup_session_and_switches_back`
- `llm_select_rejects_keyless_models_before_persisting`
- `llm_select_enforces_scope_and_route_discrimination`

Reference: https://platform.minimax.io/docs/guides/text-generation
