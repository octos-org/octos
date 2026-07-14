#!/usr/bin/env python3
"""Derive `dashboard/src/providers.json` from the canonical `model_catalog.json`.

`model_catalog.json` is the single source of truth for provisionable models. The
web admin dashboard (`dashboard/src/components/tabs/LlmProviderTab.tsx`) imports
`providers.json` directly for its onboarding + LLM-selector, so this script keeps
that file a *derived* artifact instead of a hand-maintained one that silently
drifts from the backend catalog.

Behavior (idempotent, append-only):
  - The model LINEUP is derived from the catalog: any catalog model missing from
    its family is appended (grouped by the family = first path segment; the model
    `id` is the remainder, e.g. `nvidia/deepseek-ai/deepseek-v3.2` -> family
    `nvidia`, id `deepseek-ai/deepseek-v3.2`). This is what keeps the dashboard's
    provisionable set from silently drifting behind the backend catalog.
  - Existing model entries are kept VERBATIM (order + `input`/`output`/
    `max_output`/`endpoints`). Per-model cost/output values are NOT overwritten:
    the catalog and this file can legitimately differ on those and reconciling
    them is a data-accuracy question that needs a human, not a blind sync.
  - Family `env` (the API-key env var) is preserved — it is not part of the
    catalog — and must mirror `octos_llm::registry`.

Run from the repo root:  python3 scripts/sync-dashboard-providers.py
Fails (non-zero) if it had to CREATE a family or drop a web-only model, so those
cases get a human's attention rather than silently changing the picker.
"""
import json
import sys
from collections import OrderedDict, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CATALOG = ROOT / "model_catalog.json"
PROVIDERS = ROOT / "dashboard" / "src" / "providers.json"


def _render_endpoint(ep: dict) -> str:
    keys = [k for k in ("id", "label", "base_url", "api_key_env") if k in ep]
    return "{ " + ", ".join(f"{json.dumps(k)}: {json.dumps(ep[k])}" for k in keys) + " }"


def _render_model(m: dict) -> str:
    """Match the committed one-line-per-model layout (models with `endpoints`
    expand across lines, like the hand-authored file)."""
    fields = [
        f'"id": {json.dumps(m["id"])}',
        f'"input": {json.dumps(m["input"])}',
        f'"output": {json.dumps(m["output"])}',
        f'"max_output": {json.dumps(m["max_output"])}',
    ]
    if m.get("endpoints"):
        head = "      { " + ", ".join(fields) + ', "endpoints": ['
        eps = ",\n".join("        " + _render_endpoint(ep) for ep in m["endpoints"])
        return f"{head}\n{eps}\n      ] }}"
    return "      { " + ", ".join(fields) + " }"


def render(providers: "OrderedDict") -> str:
    blocks = []
    for family, fam in providers.items():
        models = ",\n".join(_render_model(m) for m in fam["models"])
        blocks.append(
            f"  {json.dumps(family)}: {{\n"
            f'    "env": {json.dumps(fam["env"])},\n'
            f'    "models": [\n{models}\n    ]\n'
            f"  }}"
        )
    return "{\n" + ",\n".join(blocks) + "\n}\n"


def catalog_entry(model: dict) -> dict:
    """Project a catalog model onto the providers.json model schema."""
    _, model_id = model["provider"].split("/", 1)
    entry = {
        "id": model_id,
        "input": model.get("cost_in", 0.0),
        "output": model.get("cost_out", 0.0),
        "max_output": model.get("max_output", 0),
    }
    if model.get("endpoints"):
        entry["endpoints"] = model["endpoints"]
    return entry


def main() -> int:
    catalog = json.loads(CATALOG.read_text())
    providers = json.loads(PROVIDERS.read_text(), object_pairs_hook=OrderedDict)

    # family -> { model_id -> projected entry }, preserving catalog order.
    by_family: dict[str, "OrderedDict[str, dict]"] = defaultdict(OrderedDict)
    for model in catalog["models"]:
        family = model["provider"].split("/", 1)[0]
        entry = catalog_entry(model)
        by_family[family][entry["id"]] = entry

    warnings: list[str] = []
    added = 0

    for family, fam in providers.items():
        catalog_models = by_family.get(family, OrderedDict())
        existing_ids = [m["id"] for m in fam.get("models", [])]
        # Keep existing entries verbatim (curated order + values preserved).
        merged: list[dict] = list(fam.get("models", []))
        for model_id in existing_ids:
            if model_id not in catalog_models:
                warnings.append(f"{family}/{model_id} is in providers.json but NOT in the catalog")
        # Append catalog models not already present, in catalog order.
        for model_id, entry in catalog_models.items():
            if model_id not in existing_ids:
                merged.append(entry)
                added += 1
        fam["models"] = merged

    for family in by_family:
        if family not in providers:
            warnings.append(f"family '{family}' is in the catalog but NOT in providers.json (needs an env var)")

    PROVIDERS.write_text(render(providers))
    print(f"synced providers.json from catalog: appended {added} model(s)")
    for w in warnings:
        print("  WARNING:", w, file=sys.stderr)
    return 1 if warnings else 0


if __name__ == "__main__":
    sys.exit(main())
