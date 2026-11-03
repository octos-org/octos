#!/usr/bin/env python3
"""Keep the UPCR decision-record namespace unambiguous and self-indexing.

Every record lives at
`docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_YYYY_<ID>_<SLUG>.md` and is cited
as `UPCR-YYYY-<ID>` in the spec, the wire inventory, and Rust doc comments.

Two namespaces coexist:

- **Legacy numbered records**: `<ID>` is a three-digit number (`NNN`), claimed
  from a shared table in merge order. In-flight and landed records keep this
  shape; the lint still rejects duplicate numbers with an explicit diagnostic.
- **New decentralized records** (T-0328): `<ID>` is `R` + 8 random uppercase
  hex digits, generated before the PR exists (see `scripts/new-upcr.py`).
  Uniqueness no longer depends on claiming a slot from a shared counter, so
  two in-flight PRs cannot race for the same id, and no PR has to edit the
  shared `docs/upcr/README.md` table. The README is historical reference only;
  it is NOT a merge gate and receives no new rows.

This lint fails the `check` job when:

1. Two files claim the same full record id, in EITHER namespace
   (`UPCR-2026-042` twice, or `UPCR-2026-R3F9A1C7E` twice).
2. A filename deviates from its namespace's shape:
   `..._UPCR_YYYY_NNN_SHORT_SLUG.md` (legacy; the bare `..._2026_022.md` form
   was the original collision's other symptom) or
   `..._UPCR_YYYY_RXXXXXXXX_SHORT_SLUG.md` (new; slug mandatory).
3. A new-namespace record's body never mentions its exact id — the filename
   is then the only place the id exists and a rename would silently fork the
   record's identity.

History: #2423 introduced the shared table after three numbers (022, 026,
027) were each claimed twice; #2776 turned rows into a lint rule after two
in-flight PRs claimed 042; #2730 renumbered a landed record; and T-0328
(2026-11-03) removed the shared-counter/row requirement after three days of
rebase churn (#2986 conflicted three times, #3007 was ejected from the merge
queue, T-0122 collided on 082).
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

UPCR_DIR = Path("docs")

# Legacy: exactly three digits (a shared merge-order number).
LEGACY_PATTERN = re.compile(
    r"^OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_(\d{4})_(\d{3})(?:_([A-Z0-9_]+))?\.md$"
)
# New decentralized: R + 8 random uppercase hex digits, slug mandatory.
DECENTRALIZED_PATTERN = re.compile(
    r"^OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_(\d{4})_R([0-9A-F]{8})_([A-Z0-9_]+)\.md$"
)


@dataclass
class UpcrFile:
    """One record file: its full citation id and which namespace it is in."""

    name: str
    record_id: str
    namespace: str  # "legacy" | "decentralized"


def classify_filename(name: str) -> tuple[UpcrFile | None, str | None]:
    """Classify one basename; return (file, shape-problem).

    A file with a shape problem still returns its id when one is
    recoverable, so the duplicate check cannot be bypassed by omitting the
    slug (the pre-#2423 bare-number collision).
    """
    new_match = DECENTRALIZED_PATTERN.match(name)
    if new_match is not None:
        return (
            UpcrFile(
                name=name,
                record_id=f"UPCR-{new_match.group(1)}-R{new_match.group(2)}",
                namespace="decentralized",
            ),
            None,
        )
    legacy_match = LEGACY_PATTERN.match(name)
    if legacy_match is not None:
        problem = None
        if legacy_match.group(3) is None:
            problem = f"{name}: missing SHORT_SLUG suffix (bare-number filename)"
        return (
            UpcrFile(
                name=name,
                record_id=f"UPCR-{legacy_match.group(1)}-{legacy_match.group(2)}",
                namespace="legacy",
            ),
            problem,
        )
    return None, (
        f"non-conforming UPCR filename (want ..._UPCR_YYYY_NNN_SHORT_SLUG.md for a "
        f"legacy record or ..._UPCR_YYYY_RXXXXXXXX_SHORT_SLUG.md for a new one): {name}"
    )


def check_files(records: list[UpcrFile]) -> list[str]:
    """Duplicate full-id detection across both namespaces."""
    problems: list[str] = []
    seen: dict[str, str] = {}
    for record in sorted(records, key=lambda r: r.name):
        previous = seen.get(record.record_id)
        if previous is not None:
            problems.append(
                f"duplicate {record.record_id}: {previous} and {record.name}"
            )
        else:
            seen[record.record_id] = record.name
    return problems


def check_decentralized_body(path: Path, record: UpcrFile) -> list[str]:
    """A new-namespace record must cite its exact id in the body.

    The filename is the only other place the id exists; without an in-body
    citation a rename would silently fork the record's identity and every
    `UPCR-YYYY-RXXXXXXXX` reference would dangle. Legacy records predate the
    convention and are exempt.
    """
    try:
        body = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        return [f"{record.name}: unreadable UPCR document: {error}"]
    if record.record_id not in body:
        return [
            f"{record.name}: body never cites its id {record.record_id} "
            f"(name the record `{record.record_id} — <title>` on the first line)"
        ]
    return []


def collect_records(repo: Path) -> tuple[list[tuple[Path, UpcrFile]], list[str]]:
    """Scan docs/ for record files, returning (classified, shape problems)."""
    records: list[tuple[Path, UpcrFile]] = []
    problems: list[str] = []
    paths = [
        path
        for path in sorted((repo / UPCR_DIR).rglob("OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*"))
        if path.is_file()
    ]
    for path in paths:
        record, problem = classify_filename(path.name)
        if problem is not None:
            problems.append(problem)
        if record is not None:
            records.append((path, record))
    return records, problems


def run_checks(repo: Path) -> list[str]:
    records, problems = collect_records(repo)
    problems.extend(check_files([record for _, record in records]))
    for path, record in records:
        if record.namespace == "decentralized":
            problems.extend(check_decentralized_body(path, record))
    return problems


def print_report(repo: Path) -> None:
    """Print a human-readable index generated from the record files.

    Deliberately stdout-only: the shared `docs/upcr/README.md` table is
    frozen history and receives no new rows; anyone wanting the current
    index generates it instead of editing (and conflicting on) a table.
    """
    records, problems = collect_records(repo)
    print("| Record | File |")
    print("|--------|------|")
    for path, record in sorted(records, key=lambda item: item[1].record_id):
        try:
            title = next(
                (
                    line.lstrip("# ").strip()
                    for line in path.read_text(encoding="utf-8").splitlines()
                    if line.startswith("# ")
                ),
                "",
            )
        except (OSError, UnicodeDecodeError):
            title = "(unreadable)"
        print(f"| {record.record_id} | `{path.name}` — {title} |")
    for problem in problems:
        print(f"warning: {problem}", file=sys.stderr)


def self_test() -> int:
    failures: list[str] = []

    def expect_problems(label: str, problems: list[str], needle: str) -> None:
        if not any(needle in p for p in problems):
            failures.append(f"{label}: expected a problem containing {needle!r}, got {problems}")

    # classify_filename shape coverage.
    def classify_ok(name: str) -> UpcrFile:
        record, problem = classify_filename(name)
        if problem is not None:
            failures.append(f"clean filename {name!r} flagged: {problem}")
        if record is None:
            raise AssertionError(f"clean filename {name!r} did not classify")
        return record

    legacy = classify_ok("OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md")
    if legacy.record_id != "UPCR-2026-001" or legacy.namespace != "legacy":
        failures.append(f"legacy id misclassified: {legacy}")
    new_style = classify_ok(
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E_SESSION_POLICY.md"
    )
    if new_style.record_id != "UPCR-2026-R3F9A1C7E" or new_style.namespace != "decentralized":
        failures.append(f"decentralized id misclassified: {new_style}")

    _, problem = classify_filename("OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md")
    if problem is None or "missing SHORT_SLUG" not in problem:
        failures.append(f"bare legacy filename not flagged: {problem}")
    for bad in [
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_wire-contracts.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_NO_NUMBER.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_0123_RECORD.md",
        # Decentralized shape violations: missing slug, lowercase hex, short id.
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_r3f9a1c7e_SESSION_POLICY.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C_SESSION_POLICY.md",
        # Four legacy digits must not parse as NNN + junk.
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_0421_RECORD.md",
    ]:
        record, problem = classify_filename(bad)
        if record is not None or problem is None:
            failures.append(f"{bad!r} unexpectedly classified: {record}")

    # Bare legacy filenames still register their number for the duplicate
    # check (the pre-#2423 collision shape).
    record, _ = classify_filename("OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md")
    if record is None or record.record_id != "UPCR-2026-022":
        failures.append(f"bare legacy id not recovered for dup detection: {record}")

    # Duplicate detection: same full id fails in each namespace; different
    # ids in either namespace pass; a legacy number and an R-id never alias.
    def dup_check(names: list[str]) -> list[str]:
        return check_files([classify_filename(name)[0] for name in names])  # type: ignore[list-item]

    expect_problems(
        "legacy duplicate",
        dup_check(
            [
                "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md",
                "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_SESSION_POLICY.md",
            ]
        ),
        "duplicate UPCR-2026-022",
    )
    expect_problems(
        "decentralized duplicate",
        dup_check(
            [
                "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E_FOO.md",
                "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E_BAR.md",
            ]
        ),
        "duplicate UPCR-2026-R3F9A1C7E",
    )
    if dup_check(
        [
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md",
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R00000022_SESSION_POLICY.md",
        ]
    ):
        failures.append("legacy number aliased with an R-id")
    if dup_check(
        [
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E_FOO.md",
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2027_R3F9A1C7E_BAR.md",
        ]
    ):
        failures.append("same hex token in a different year flagged as duplicate")

    # run_checks over a scratch repo: legacy-only set passes; README absence
    # is fine (historical, not a gate); a decentralized record without an
    # in-body id fails; legacy duplicates fail through the real scan.
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp)
        docs = repo / "docs"
        (docs / "upcr").mkdir(parents=True)
        (docs / "upcr" / "README.md").write_text("# historical table\n", encoding="utf-8")
        (docs / "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md").write_text(
            "# Octos UI Protocol Change Request: Typed Approval Payloads\n", encoding="utf-8"
        )
        (docs / "upcr" / "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E_SESSION_POLICY.md").write_text(
            "# UPCR-2026-R3F9A1C7E — Session Policy\n", encoding="utf-8"
        )
        if run_checks(repo):
            failures.append(f"clean mixed-namespace repo flagged: {run_checks(repo)}")
        # README is historical: deleting it must not fail the lint.
        (docs / "upcr" / "README.md").unlink()
        if run_checks(repo):
            failures.append(f"README-less repo flagged: {run_checks(repo)}")
        # New record without an in-body id citation.
        (docs / "upcr" / "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R3F9A1C7E_SESSION_POLICY.md").write_text(
            "# Session Policy Without Its Id\n", encoding="utf-8"
        )
        expect_problems(
            "missing body id",
            run_checks(repo),
            "body never cites its id UPCR-2026-R3F9A1C7E",
        )
        # Legacy duplicate through the real file scan.
        (docs / "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_OTHER.md").write_text(
            "# Other\n", encoding="utf-8"
        )
        expect_problems(
            "legacy duplicate via scan",
            run_checks(repo),
            "duplicate UPCR-2026-001",
        )

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
    parser.add_argument(
        "--report",
        action="store_true",
        help="print a human-readable index of all records to stdout (not committed)",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()

    repo = Path(__file__).resolve().parent.parent
    if args.report:
        print_report(repo)
        return 0
    problems = run_checks(repo)
    if problems:
        for p in problems:
            print(f"FAIL: {p}")
        if any("duplicate" in p for p in problems):
            print(
                "Two records claim the same id. Legacy numbered records renumber "
                "only when they predate T-0328; new records mint a fresh id with "
                "scripts/new-upcr.py instead of claiming a shared slot."
            )
        else:
            print(
                "Rename to OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_YYYY_NNN_SHORT_SLUG.md "
                "(legacy) or ..._UPCR_YYYY_RXXXXXXXX_SHORT_SLUG.md (new), and cite the "
                "exact id in the document body."
            )
        return 1
    print("ok: UPCR record namespace (legacy numbers + decentralized ids)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
