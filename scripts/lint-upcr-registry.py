#!/usr/bin/env python3
"""Keep the UPCR file namespace unambiguous.

`docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN[_SLUG].md` filenames are
the protocol's decision-record foreign keys: the spec, the wire inventory, and
Rust doc comments cite records as `UPCR-2026-NNN`. Nothing checked that the
NNN was unique or that filenames followed one shape, so three numbers (022,
026, 027) ended up claimed by two unrelated files each, and the registry
introduced alongside this lint (#2423) had no mechanical backstop. A duplicate
silently reopens that ambiguity — the exact failure that forced #2730 to
renumber a freshly-landed record.

This lint fails the `check` job when:

1. Two files claim the same `UPCR-2026-NNN` number.
2. A filename deviates from
   `OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN_SHORT_SLUG.md` (uppercase
   underscore slug; the bare `..._2026_022.md` form was the collision's
   other symptom).
3. The registry table in `docs/upcr/README.md` and the UPCR files disagree:
   a file with no row, a row naming no file or a different file, duplicate
   rows, or a File cell whose digits disagree with the row's number. #2423
   left the rows as a review convention ("no second source of truth"), and
   #2776 caught the cost: both open claimants of 042 predated the registry,
   neither carried its row, and the #2769 invariant ("whichever lands first
   keeps the number and adds its row") would have broken silently on the
   first merge while the lint stayed green. The table is that second source
   of truth; this check keeps the two in lockstep.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

UPCR_DIR = Path("docs")
REGISTRY = Path("docs/upcr/README.md")
UPCR_PATTERN = re.compile(
    r"^OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_(\d{3})(?:_([A-Z0-9_]+))?\.md$"
)
REGISTRY_ROW_PATTERN = re.compile(
    r"^(?:\.\.\.|OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026)_(\d{3})(?:_([A-Z0-9_]+))?\.md$"
)


def check_upcr_filenames(names: list[str]) -> list[str]:
    """Check a list of UPCR *basenames* for number collisions and shape."""
    problems: list[str] = []
    seen: dict[str, str] = {}
    for name in sorted(names):
        match = UPCR_PATTERN.match(name)
        if match is None:
            problems.append(f"non-conforming UPCR filename (want ..._2026_NNN_SHORT_SLUG.md): {name}")
            continue
        number = f"UPCR-2026-{match.group(1)}"
        if match.group(2) is None:
            problems.append(f"{name}: missing SHORT_SLUG suffix (bare-number filename)")
        previous = seen.get(number)
        if previous is not None:
            problems.append(f"duplicate {number}: {previous} and {name}")
        else:
            seen[number] = name
    return problems


def registry_rows(lines: list[str]) -> tuple[dict[str, str | None], list[str]]:
    """Parse the registry table into {UPCR-2026-NNN: canonical row filename}.

    None means the File cell is `—` (a legitimately file-less row such as
    never-assigned 013); any other non-matching cell is a broken row.
    """
    rows: dict[str, str | None] = {}
    problems: list[str] = []
    for line in lines:
        if not line.lstrip().startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) < 5 or not re.fullmatch(r"\d{3}", cells[0]):
            continue
        number = f"UPCR-2026-{cells[0]}"
        if number in rows:
            problems.append(f"docs/upcr/README.md: duplicate row for {number}")
            continue
        file_cell = cells[3].strip("`")
        if file_cell == "—":
            rows[number] = None
            continue
        match = REGISTRY_ROW_PATTERN.match(file_cell)
        if match is None:
            problems.append(f"docs/upcr/README.md row {number}: File cell {cells[3]!r} is neither a UPCR filename nor —")
            continue
        if match.group(1) != cells[0]:
            problems.append(f"docs/upcr/README.md row {number}: File cell {cells[3]!r} names a different number")
            continue
        canonical = f"OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_{cells[0]}"
        if match.group(2) is not None:
            canonical += f"_{match.group(2)}"
        rows[number] = canonical + ".md"
    return rows, problems


def check_upcr_registry(files: dict[str, str], rows: dict[str, str | None]) -> list[str]:
    """Check the registry table and the UPCR files agree in both directions."""
    problems: list[str] = []
    for number, name in sorted(files.items()):
        if number not in rows:
            problems.append(f"{name}: no docs/upcr/README.md row for {number} — add it in the same PR")
        elif rows[number] is None:
            problems.append(f"{name}: docs/upcr/README.md row for {number} says no file exists")
        elif rows[number] != name:
            problems.append(f"{name}: docs/upcr/README.md row for {number} names {rows[number]!r}")
    for number in sorted(set(rows) - set(files)):
        if rows[number] is not None:
            problems.append(f"docs/upcr/README.md row for {number} names {rows[number]!r} but no such file exists")
    return problems


def run_checks(repo: Path) -> list[str]:
    paths = [
        path
        for path in sorted((repo / UPCR_DIR).rglob("OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_*"))
        if path.is_file()
    ]
    problems = check_upcr_filenames([path.name for path in paths])
    readme = repo / REGISTRY
    if not readme.is_file():
        problems.append("docs/upcr/README.md not found — the registry is part of the UPCR namespace (#2769)")
        return problems
    rows, row_problems = registry_rows(readme.read_text(encoding="utf-8").splitlines())
    problems.extend(row_problems)
    files = {}
    for path in paths:
        match = UPCR_PATTERN.match(path.name)
        if match is not None:
            files[f"UPCR-2026-{match.group(1)}"] = path.name
    problems.extend(check_upcr_registry(files, rows))
    return problems


def self_test() -> int:
    failures: list[str] = []

    def expect_problems(label: str, problems: list[str], needle: str) -> None:
        if not any(needle in p for p in problems):
            failures.append(f"{label}: expected a problem containing {needle!r}, got {problems}")

    good = [
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_043_SESSION_SANDBOX_NARROWING.md",
    ]
    if check_upcr_filenames(good):
        failures.append(f"clean set flagged: {check_upcr_filenames(good)}")

    duplicate = good + ["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_SESSION_POLICY.md"]
    expect_problems("duplicate number", check_upcr_filenames(duplicate), "duplicate UPCR-2026-022")

    expect_problems(
        "bare filename",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md"]),
        "missing SHORT_SLUG",
    )
    # A bare-number filename must still register its number for the duplicate
    # check — a `continue` after the slug warning would silently let a bare
    # file and a slugged file share a number again.
    bare_collision = check_upcr_filenames(
        [
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md",
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md",
        ]
    )
    expect_problems("bare/slug collision", bare_collision, "duplicate UPCR-2026-022")
    expect_problems(
        "bad slug",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_wire-contracts.md"]),
        "non-conforming UPCR filename",
    )
    expect_problems(
        "missing number",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_NO_NUMBER.md"]),
        "non-conforming UPCR filename",
    )
    # Exactly three digits: a four-digit number must not parse as NNN + junk.
    expect_problems(
        "four-digit number",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_0123_RECORD.md"]),
        "non-conforming UPCR filename",
    )

    # Registry rows: header and separator lines are skipped, backticks and
    # whitespace are cosmetic, and a `—` File cell means "no file".
    table = [
        "# UPCR Registry",
        "",
        "| # | Title | Status | File | Landed |",
        "|---|-------|--------|------|--------|",
        "| 001 | Add typed approval fields | accepted | `..._001_TYPED_APPROVAL.md` | #1228 |",
        "| 013 | — | never assigned | — | — |",
    ]
    rows, row_problems = registry_rows(table)
    if row_problems:
        failures.append(f"clean table flagged: {row_problems}")
    if rows != {
        "UPCR-2026-001": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md",
        "UPCR-2026-013": None,
    }:
        failures.append(f"clean table parsed wrong: {rows}")

    # A file with no row at all — the #2776 instance.
    expect_problems(
        "missing row",
        check_upcr_registry(
            {"UPCR-2026-042": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_042_SESSION_APPEND_MESSAGE.md"},
            {"UPCR-2026-001": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md"},
        ),
        "no docs/upcr/README.md row for UPCR-2026-042",
    )
    # A row that disagrees with the filename (stale slug after a renumber).
    expect_problems(
        "row slug mismatch",
        check_upcr_registry(
            {"UPCR-2026-022": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md"},
            {"UPCR-2026-022": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_SESSION_POLICY.md"},
        ),
        "names 'OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_SESSION_POLICY.md'",
    )
    # A file-less row plus an existing file — the inverse of 013.
    expect_problems(
        "fileless row with file",
        check_upcr_registry(
            {"UPCR-2026-013": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_013_SOMETHING.md"},
            {"UPCR-2026-013": None},
        ),
        "says no file exists",
    )
    # A bare file agrees with a bare File cell (shape lint still flags both).
    if check_upcr_registry(
        {"UPCR-2026-022": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md"},
        {"UPCR-2026-022": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md"},
    ):
        failures.append("matching bare row flagged")
    expect_problems(
        "broken File cell",
        registry_rows(["| 001 | t | s | `..._001-typed.md` | #1 |"])[1],
        "neither a UPCR filename",
    )
    # Two rows for one number: the first row must win loudly, not silently.
    expect_problems(
        "duplicate row",
        registry_rows(
            [
                "| 042 | a | s | `..._042_A.md` | #1 |",
                "| 042 | b | s | `..._042_B.md` | #2 |",
            ]
        )[1],
        "duplicate row for UPCR-2026-042",
    )
    # A File cell whose digits disagree with the row number — the silent
    # residue a renumber can leave behind (#043/044/045 were all renumbered).
    expect_problems(
        "stale File-cell digits",
        registry_rows(["| 042 | t | s | `..._041_PEER_ROUTE_OWNERSHIP.md` | #1 |"])[1],
        "names a different number",
    )
    # The full filename is accepted next to the `...` shorthand.
    if check_upcr_registry(
        {"UPCR-2026-046": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_046_X.md"},
        {
            "UPCR-2026-046": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_046_X.md",
            "UPCR-2026-099": "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_GHOST.md",
        },
    ) != ["docs/upcr/README.md row for UPCR-2026-099 names "
          "'OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_GHOST.md' but no such file exists"]:
        failures.append(f"ghost row not the only problem: {check_upcr_registry(
            {'UPCR-2026-046': 'OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_046_X.md'},
            {
                'UPCR-2026-046': 'OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_046_X.md',
                'UPCR-2026-099': 'OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_GHOST.md',
            },
        )}")

    # run_checks wiring: a scratch repo covers the README-missing branch,
    # the recursive glob (docs/upcr/ lives inside docs/), and the clean path.
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp)
        (repo / "docs" / "upcr").mkdir(parents=True)
        (repo / "docs" / "upcr" / "README.md").write_text(
            "| # | Title | Status | File | Landed |\n"
            "|---|-------|--------|------|--------|\n"
            "| 001 | t | s | `..._001_TYPED_APPROVAL.md` | #1228 |\n"
            "| 002 | t | s | `..._002_PANE_SNAPSHOTS.md` | #1228 |\n",
            encoding="utf-8",
        )
        (repo / "docs" / "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md").write_text("", encoding="utf-8")
        (repo / "docs" / "upcr" / "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_002_PANE_SNAPSHOTS.md").write_text("", encoding="utf-8")
        if run_checks(repo):
            failures.append(f"clean scratch repo flagged: {run_checks(repo)}")
        (repo / "docs" / "upcr" / "README.md").unlink()
        expect_problems("missing README", run_checks(repo), "docs/upcr/README.md not found")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("ok: self-test")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the checker against synthetic fixtures instead of the repo",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()

    repo = Path(__file__).resolve().parent.parent
    problems = run_checks(repo)
    if problems:
        for p in problems:
            print(f"FAIL: {p}")
        if any("duplicate" in p for p in problems):
            print(
                "Claim the next free number (check docs/upcr/README.md and the "
                "in-flight note) and add your row in the same PR."
            )
        elif any("docs/upcr/README.md" in p for p in problems):
            print("Fix docs/upcr/README.md to match the UPCR files (see the README Rules).")
        else:
            print("Rename to OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN_SHORT_SLUG.md.")
        return 1
    print("ok: UPCR filename namespace and registry rows (#2423, #2776)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
