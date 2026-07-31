#!/usr/bin/env python3
"""Hermetic smoke test for the native `octos` pyo3 extension.

No network: it only imports the module, builds a Config/Brief, and asserts that
constructing a Runtime with a bogus provider raises OctosError. Provider
construction fails offline, so nothing here touches an API.

This is intentionally NOT part of `cargo test` (that runs the Rust suite). Run
it after building the extension into the active interpreter:

    pip install maturin
    cd crates/octos-pyo3
    maturin develop            # builds + installs `octos` into the venv
    python tests/smoke.py

Exits non-zero on any failure.
"""

import sys


def main() -> int:
    import octos

    # Exception type is exported.
    assert hasattr(octos, "OctosError"), "octos.OctosError missing"

    # Config/Brief take kwargs, including api_type.
    cfg = octos.Config(
        provider="openai",
        model="gpt-4o-mini",
        api_key="sk-smoke-dummy",
        api_type="responses",
        max_iterations=3,
    )
    assert cfg.provider == "openai"
    assert cfg.api_type == "responses"
    assert cfg.max_iterations == 3
    print("Config OK:", repr(cfg))

    # The raw api_key must NOT be readable back from Python (P1 secret-leak
    # guard): there is no `api_key` getter, only a boolean presence accessor.
    assert not hasattr(cfg, "api_key"), "api_key must not be attribute-accessible"
    assert cfg.api_key_is_set is True, "api_key_is_set should be True"
    assert "sk-smoke-dummy" not in repr(cfg), "repr must not leak the raw key"
    print("api_key is write-only (not exposed); api_key_is_set =", cfg.api_key_is_set)

    brief = octos.Brief(prompt="Reply OK")
    assert brief.prompt == "Reply OK"
    assert brief.max_iterations is None
    print("Brief OK:", repr(brief))

    # Bogus provider must raise OctosError (offline, before any network).
    try:
        octos.Runtime(octos.Config(provider="totally-not-a-real-provider", model="x"))
    except octos.OctosError as e:
        print("Runtime(bad provider) correctly raised OctosError:", e)
    else:
        print("FAIL: expected OctosError for a bogus provider", file=sys.stderr)
        return 1

    print("SMOKE OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
