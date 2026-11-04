#!/usr/bin/env python3
"""Mint a decentralized UPCR record id and scaffold the document.

Usage: scripts/new-upcr.py <short-slug> [<short-slug> ...]

Prints the created `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_YYYY_RXXXXXXXX_<SLUG>.md`
path. The id is `R` + 8 random uppercase hex digits, generated BEFORE the PR
exists (T-0328): uniqueness never depends on claiming a slot from a shared
counter, so two in-flight PRs cannot race for the same number and neither has
to renumber — or touch the shared registry table — when the other lands first.
Legacy `UPCR-YYYY-NNN` records keep their merge-order numbers; only new
records use this scheme.
"""

from __future__ import annotations

import re
import secrets
import sys
from datetime import UTC, datetime
from pathlib import Path

TEMPLATE = Path("docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_TEMPLATE.md")
BODY = """\
# {record_id} — <Title>

Status: proposed
PR: TBD

## Decision record

<What changes, why, and the durability/compat contract. Cite this record
elsewhere as `{record_id}`.>
"""


def main() -> int:
    slugs = sys.argv[1:]
    if not slugs:
        print(__doc__)
        return 2
    repo = Path(__file__).resolve().parent.parent
    year = datetime.now(UTC).year
    for slug in slugs:
        if slug in ("--help", "-h"):
            print(__doc__)
            return 0
        if slug.startswith("-"):
            # `--help` and other flags used to be normalized into "__HELP" and
            # mint a stray record file (two of those landed on main).
            print(f"bad slug {slug!r}: flags are not slugs; see usage above", file=sys.stderr)
            return 2
        slug = slug.strip().upper().replace("-", "_")
        if not re.fullmatch(r"[A-Z0-9_]+", slug):
            print(f"bad slug {slug!r}: use UPPERCASE_UNDERSCORE words", file=sys.stderr)
            return 2
        while True:
            token = f"R{secrets.token_hex(4).upper()}"
            record_id = f"UPCR-{year}-{token}"
            name = f"OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_{year}_{token}_{slug}.md"
            target = repo / "docs" / name
            if not target.exists():
                break  # retry only on the (astronomically unlikely) collision
        target.write_text(BODY.format(record_id=record_id), encoding="utf-8")
        print(target.relative_to(repo))
    return 0


if __name__ == "__main__":
    sys.exit(main())
