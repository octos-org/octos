#!/usr/bin/env python3
"""Lint source references in architecture docs.

Doc citations rot silently as code moves (e.g. a transcription section that
kept describing a deleted GroqTranscriber, a QueueMode cite pointing two
crates-worth of lines away from the enum). This lint keeps the flagship
architecture doc honest by verifying that its references still resolve:

1. Path citations — ``path.rs:12`` / ``path.rs:12-34`` and backticked paths —
   must resolve to an existing file (repo-root-relative, ``crates/``-prefixed,
   or a unique suffix under ``crates/``), with cited lines inside the file.
2. Type-name citations — PascalCase tokens in prose, inline code, or bold
   spans — must resolve to a workspace symbol definition (struct/enum/
   trait/fn/... including enum variants) or appear in the documented
   external-names allowlist.
3. Range citations sharing a line with a type name must still contain that
   name's definition in the cited file (catches ``Foo`` in ``path.rs:A-B``
   after the definition moves).

Known blind spots (deliberate, to keep the checker cheap and silent on
prose): bare SCREAMING_SNAKE env-var/const names, single-line (non-range)
citations are not anchored to definitions, and bare filenames without a
directory or line number are not path-checked.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path


DEFAULT_DOCS = ("docs/ARCHITECTURE.md",)

SOURCE_EXTENSIONS = ("rs", "ts", "tsx", "toml", "yml", "yaml", "py", "sh")

# Tokens that look like types but are std/dependency types or external
# product/platform names. Each entry needs a reason — a bare name list rots
# the same way the citations did.
EXTERNAL_NAMES: dict[str, str] = {
    "AtomicBool": "std sync primitive",
    "AtomicU32": "std atomic",
    "AtomicU64": "std atomic",
    "BufWriter": "std io type",
    "DashScope": "external LLM provider name",
    "DateTime": "chrono dependency type",
    "DeepSeek": "external LLM provider name",
    "DistCosine": "instant-distance dependency type",
    "DoS": "general security abbreviation",
    "FromStr": "std trait",
    "GitHub": "external platform name",
    "HashMap": "std collection",
    "HashSet": "std collection",
    "MiniMax": "external LLM provider name",
    "NanoCloud": "external Node.js agent framework name",
    "NaN": "float value name",
    "OminiX": "external voice/ASR platform name",
    "OpenAI": "external LLM provider name",
    "OpenClaw": "external product name",
    "OpenRouter": "external LLM provider name",
    "OpenSSL": "external library name",
    "PathBuf": "std path type",
    "R9S": "external platform name",
    "RwLock": "std sync primitive",
    "SecretString": "secrecy dependency type",
    "WalkBuilder": "ignore dependency type",
    "WebP": "image format name",
    "WeChat": "external product name",
    "WeCom": "external product name",
    "WebSocket": "protocol name",
}

# Common Rust prelude names that would otherwise trip the PascalCase filter.
PRELUDE_NAMES = frozenset(
    {
        "Ok",
        "Err",
        "Some",
        "None",
        "Vec",
        "String",
        "Option",
        "Result",
        "Self",
    }
)

SYMBOL_DEF_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"(?:struct|enum|trait|union|type|fn|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)",
    re.M,
)

# Enum variants: indented PascalCase opening a variant body (`Name {`,
# `Name(...)`) or a unit variant (`Name,` / bare `Name`).
VARIANT_DEF_RE = re.compile(r"^\s{2,}([A-Z][A-Za-z0-9_]*)\s*(?:[\({,]|$)", re.M)

# Two-hump PascalCase (`EpisodeStore`, `QueueMode`); single-hump names are too
# ambiguous against prose.
TYPE_TOKEN_RE = re.compile(r"^[A-Z][a-z0-9]+(?:[A-Z][a-zA-Z0-9]*)+$")

TOKEN_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

INLINE_CODE_RE = re.compile(r"`([^`\n]+)`")

BOLD_RE = re.compile(r"\*\*([A-Za-z_][A-Za-z0-9_]*)\*\*")

# `path.ext` optionally followed by `:LINE` or `:LINE-LINE`. Requires either a
# line number or a directory component so bare filenames stay prose.
PATH_CITE_RE = re.compile(
    r"(?P<path>[A-Za-z0-9_][A-Za-z0-9_/.-]*\.(?P<ext>"
    + "|".join(SOURCE_EXTENSIONS)
    + r"))(?::(?P<start>\d+)(?:-(?P<end>\d+))?)?"
)


class Violation:
    def __init__(self, doc: Path, line_no: int, message: str):
        self.doc = doc
        self.line_no = line_no
        self.message = message

    def __str__(self) -> str:
        return f"{self.doc}:{self.line_no}: {self.message}"


@dataclass
class Workspace:
    """Workspace symbol index: name -> definition (file, line) list."""

    root: Path
    defs: dict[str, list[tuple[Path, int]]] = field(default_factory=dict)
    crate_files: list[Path] = field(default_factory=list)

    @classmethod
    def scan(cls, root: Path) -> "Workspace":
        ws = cls(root=root)
        crate_root = root / "crates"
        for path in sorted(crate_root.rglob("*.rs")) if crate_root.is_dir() else []:
            ws.crate_files.append(path)
            try:
                text = path.read_text()
            except OSError:
                continue
            for regex in (SYMBOL_DEF_RE, VARIANT_DEF_RE):
                for line_no, line in enumerate(text.splitlines(), 1):
                    match = regex.match(line)
                    if match:
                        ws.defs.setdefault(match.group(1), []).append((path, line_no))
        return ws

    def resolve(self, cite: str) -> Path:
        """Resolve a citation path to exactly one file, or raise LookupError.

        Resolution order: as-written from the repo root, `crates/`-prefixed,
        then a unique path-suffix match under `crates/` (docs cite
        crate-relative paths like `tools/policy.rs`).
        """
        direct = self.root / cite
        if direct.is_file():
            return direct
        prefixed = self.root / "crates" / cite
        if prefixed.is_file():
            return prefixed
        suffix = "/" + cite
        matches = [p for p in self.crate_files if str(p).endswith(suffix)]
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            raise LookupError(f"ambiguous path (matches {len(matches)} files): {cite}")
        raise LookupError(f"path not found: {cite}")


def line_tokens(line: str) -> list[str]:
    """PascalCase tokens on a markdown line: prose words, inline code, bold."""
    tokens: list[str] = []
    seen: set[str] = set()
    for span in [line] + INLINE_CODE_RE.findall(line) + BOLD_RE.findall(line):
        for token in TOKEN_RE.findall(span):
            if token in seen:
                continue
            seen.add(token)
            if TYPE_TOKEN_RE.match(token):
                tokens.append(token)
    return tokens


def unknown_type_names(doc: Path, ws: Workspace) -> list[Violation]:
    violations = []
    for line_no, line in enumerate(doc.read_text().splitlines(), 1):
        for token in line_tokens(line):
            if token in PRELUDE_NAMES or token in EXTERNAL_NAMES:
                continue
            if token not in ws.defs:
                violations.append(
                    Violation(
                        doc,
                        line_no,
                        f"type name `{token}` does not resolve to any "
                        "workspace symbol (add it to EXTERNAL_NAMES only "
                        "if it is a std/dependency type or external "
                        "product name)",
                    )
                )
    return violations


def check_path_citations(doc: Path, ws: Workspace) -> list[Violation]:
    violations = []
    for line_no, line in enumerate(doc.read_text().splitlines(), 1):
        for match in PATH_CITE_RE.finditer(line):
            if match.start() > 0 and line[match.start() - 1] == "/":
                continue  # inside a URL (`https://host/...`), not a citation
            path = match.group("path")
            if "/" not in path and not match.group("start"):
                continue  # bare filename in prose, no line anchor — skip
            try:
                resolved = ws.resolve(path)
            except LookupError as exc:
                violations.append(Violation(doc, line_no, str(exc)))
                continue
            start = match.group("start")
            if not start:
                continue
            source_lines = resolved.read_text().splitlines()
            total = len(source_lines)
            start_line = int(start)
            end_line = int(match.group("end") or start)
            if start_line < 1 or start_line > end_line or end_line > total:
                violations.append(
                    Violation(
                        doc,
                        line_no,
                        f"{path}:{start}"
                        + (f"-{end_line}" if match.group("end") else "")
                        + (f" out of bounds ({path} has {total} lines)"
                           if start_line <= end_line else " is an invalid range"),
                    )
                )
                continue
            if match.group("end"):
                violations.extend(
                    _anchor_violations(doc, line_no, line, resolved, path,
                                       start_line, end_line, ws)
                )
    return violations


def _anchor_violations(
    doc: Path,
    line_no: int,
    md_line: str,
    cited_file: Path,
    cite_path: str,
    start_line: int,
    end_line: int,
    ws: Workspace,
) -> list[Violation]:
    """A range cite naming a type on the same line must contain its def."""
    in_file: list[tuple[str, int]] = []
    for token in line_tokens(md_line):
        for def_path, def_line in ws.defs.get(token, []):
            if def_path == cited_file:
                in_file.append((token, def_line))
    if not in_file:
        return []  # no anchor on this line — range is not type-anchored
    anchored = [entry for entry in in_file if start_line <= entry[1] <= end_line]
    if anchored:
        return []
    token, def_line = in_file[0]
    return [
        Violation(
            doc,
            line_no,
            f"range {cite_path}:{start_line}-{end_line} no longer contains "
            f"the definition of `{token}` (now at {cite_path}:{def_line})",
        )
    ]


def lint_doc(doc: Path, ws: Workspace) -> list[Violation]:
    return check_path_citations(doc, ws) + unknown_type_names(doc, ws)


def run(docs: list[Path], root: Path) -> int:
    ws = Workspace.scan(root)
    violations: list[Violation] = []
    for doc in docs:
        if not doc.is_file():
            print(f"error: doc not found: {doc}", file=sys.stderr)
            return 2
        violations.extend(lint_doc(doc, ws))
    if violations:
        for violation in violations:
            print(str(violation))
        print(f"\n{len(violations)} unresolved reference(s)")
        return 1
    print(f"ok: {len(docs)} doc(s), all references resolve")
    return 0


def self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        lib = root / "crates/demo/src/lib.rs"
        lib.parent.mkdir(parents=True)
        lib.write_text(
            "pub struct Alpha {}\n"  # line 1
            "\n"
            "pub enum Kind {\n"      # line 3
            "    BetaValue(String),\n"    # line 4 (tuple variant)
            "}\n"
        )
        # Second crate whose `tools/helper.rs` collides with demo's, to pin
        # the ambiguity refusal.
        demo_helper = root / "crates/demo/src/tools/helper.rs"
        demo_helper.parent.mkdir(parents=True)
        demo_helper.write_text("pub struct Delta {}\n\n")
        other_helper = root / "crates/other/src/tools/helper.rs"
        other_helper.parent.mkdir(parents=True)
        other_helper.write_text("pub struct Gamma {}\n")

        ws = Workspace.scan(root)

        doc = root / "docs/ARCHITECTURE.md"
        doc.parent.mkdir(parents=True)

        def violations_for(content: str) -> list[str]:
            doc.write_text(content)
            return [v.message for v in lint_doc(doc, ws)]

        doc.write_text(
            "# Doc\n"
            "`Alpha`, bare prose BetaValue and `crates/demo/src/lib.rs:1` are fine.\n"
            "Shorthand `demo/src/tools/helper.rs:1` resolves.\n"
            "`HashMap` is allowlisted.\n"
            "Tuple variant `BetaValue` resolves.\n"
            "`Delta` named in `demo/src/tools/helper.rs:1-2` anchors.\n"
            "A `https://host/x/blob/main/crates/demo/src/lib.rs` URL is ignored.\n"
        )
        clean = lint_doc(doc, ws)
        assert clean == [], f"clean doc must pass, got {[str(v) for v in clean]}"

        cases = [
            ("bare prose GhostType here\n", "GhostType"),
            ("dead line `crates/demo/src/lib.rs:99`\n", "out of bounds"),
            ("dead file `crates/demo/src/nope.rs:1`\n", "path not found"),
            ("dead shorthand `tools/helper.rs:1`\n", "ambiguous path"),
            ("range `crates/demo/src/lib.rs:2-1`\n", "invalid range"),
            (
                "`BetaValue` in `crates/demo/src/lib.rs:1-2` drifted\n",
                "no longer contains the definition of `BetaValue`",
            ),
        ]
        for content, fragment in cases:
            messages = violations_for(content)
            assert any(fragment in m for m in messages), (
                f"expected {fragment!r} in {messages}"
            )

        # Unanchored range (no type named on the line) must pass.
        assert violations_for("`crates/demo/src/lib.rs:1-2` stands alone.\n") == []
        # Bare filename with no directory and no line stays prose.
        assert violations_for("mentioned lib.rs in passing\n") == []
    print("ok: self-test")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--doc",
        action="append",
        default=None,
        help="doc file to lint (relative to repo root; repeatable; "
        f"default: {', '.join(DEFAULT_DOCS)})",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the built-in fixtures and exit",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    root = Path(__file__).resolve().parent.parent
    docs = [root / d for d in (args.doc or DEFAULT_DOCS)]
    return run(docs, root)


if __name__ == "__main__":
    sys.exit(main())
