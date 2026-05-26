#!/usr/bin/env python3
"""Lint tool manifest descriptions for ambiguous content-type ownership.

The LLM-visible tool description is part of the API. When sibling tools all
mention the same generic content noun, weaker models can route by the wrong
description unless non-owner tools explicitly disclaim that noun.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


DEFAULT_ROOTS = (
    "crates/app-skills",
    "crates/platform-skills",
    ".crew/bundled-app-skills",
    ".crew/platform-skills",
    "dashboard/.crew/bundled-app-skills",
    "dashboard/.crew/platform-skills",
)

CONTENT_NOUNS: dict[str, tuple[str, ...]] = {
    "slides": ("slide", "slides", "deck", "decks", "presentation", "presentations", "ppt", "pptx"),
    "site": (
        "site",
        "sites",
        "website",
        "websites",
        "webpage",
        "webpages",
        "landing page",
        "landing pages",
    ),
    # `short`/`shorts` intentionally excluded — too commonly used as the
    # adjective "a short clip" / "the short summary" to safely flag without
    # qualifying context. mofa-youtube currently does not advertise "Shorts"
    # support, so dropping these aliases eliminates a false-positive class
    # (see harness-starter-audio "a short WAV clip").
    "video": ("video", "videos", "youtube"),
    "podcast": ("podcast", "podcasts"),
    "cards": ("card", "cards"),
    "comic": ("comic", "comics"),
    "frame": ("frame", "frames"),
    "infographic": ("infographic", "infographics"),
}


@dataclass(frozen=True)
class ToolDescription:
    manifest: Path
    manifest_name: str
    tool_name: str
    description: str

    @property
    def display_name(self) -> str:
        return f"{self.manifest_name}:{self.tool_name}"

    @property
    def identity(self) -> tuple[str, str]:
        return (self.manifest_name, self.tool_name)


def normalize(text: str) -> str:
    return re.sub(r"\s+", " ", text.lower()).strip()


def alias_pattern(alias: str) -> re.Pattern[str]:
    escaped = re.escape(alias.lower()).replace(r"\ ", r"\s+")
    if " " in alias:
        return re.compile(rf"(?<![a-z0-9_]){escaped}(?![a-z0-9_])")
    return re.compile(rf"\b{escaped}\b")


def text_mentions(text: str, aliases: Iterable[str]) -> bool:
    normalized = normalize(text)
    return any(alias_pattern(alias).search(normalized) for alias in aliases)


def tool_owns_noun(tool: ToolDescription, aliases: Iterable[str]) -> bool:
    haystack = normalize(f"{tool.manifest_name} {tool.tool_name}")
    return any(alias_pattern(alias).search(haystack) for alias in aliases)


def tool_disclaims_noun(
    tool: ToolDescription,
    aliases: Iterable[str],
    owner_tool_name: str,
) -> bool:
    text = normalize(tool.description)
    owner_parts = [
        re.escape(part)
        for part in re.split(r"[-_\s]+", normalize(owner_tool_name))
        if part
    ]
    owner = r"[-_ ]+".join(owner_parts)
    mentions_owner = re.search(rf"(?<![a-z0-9_]){owner}(?![a-z0-9_])", text) is not None
    if not mentions_owner:
        return False

    for alias in aliases:
        escaped = re.escape(alias.lower()).replace(r"\ ", r"\s+")
        disclaimer = re.compile(
            rf"\bnot\s+(?:for|to|intended\s+for|use(?:d)?\s+for)\s+"
            rf"(?:[a-z0-9_/-]+\s+){{0,4}}{escaped}(?![a-z0-9_])"
        )
        if disclaimer.search(text):
            return True
    return False


def manifest_paths(roots: Iterable[Path]) -> list[Path]:
    paths: list[Path] = []
    for root in roots:
        if root.is_file() and root.name == "manifest.json":
            paths.append(root)
        elif root.is_dir():
            paths.extend(root.rglob("manifest.json"))
    return sorted(set(paths))


def load_tools(paths: Iterable[Path]) -> tuple[list[ToolDescription], list[str]]:
    tools: list[ToolDescription] = []
    errors: list[str] = []

    for path in paths:
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except Exception as exc:  # noqa: BLE001 - report path-specific JSON/read errors.
            errors.append(f"{path}: failed to read manifest: {exc}")
            continue

        manifest_name = str(raw.get("name") or path.parent.name)
        manifest_description = str(raw.get("description") or "")
        for entry in raw.get("tools") or []:
            if not isinstance(entry, dict):
                continue
            tool_name = str(entry.get("name") or "")
            if not tool_name:
                errors.append(f"{path}: tool entry is missing name")
                continue
            tool_description = str(entry.get("description") or "")
            combined = f"{manifest_description}\n{tool_description}".strip()
            tools.append(
                ToolDescription(
                    manifest=path,
                    manifest_name=manifest_name,
                    tool_name=tool_name,
                    description=combined,
                )
            )

    return tools, errors


SOURCE_PATH_HINTS: tuple[str, ...] = (
    # Workspace source-of-truth roots whose copies must be preferred when the
    # same `(manifest_name, tool_name)` appears in both the source dir and a
    # snapshot dir (e.g. `.crew/bundled-app-skills/...`). Without this hint
    # `sorted()` makes the snapshot win because dot-prefixed paths sort first,
    # which leaves stale descriptions shadowing freshly-fixed source manifests.
    "crates/app-skills/",
    "crates/platform-skills/",
)


def _is_source_path(path: Path) -> bool:
    posix = path.as_posix()
    return any(hint in posix for hint in SOURCE_PATH_HINTS)


def lint_tools(tools: Iterable[ToolDescription]) -> list[str]:
    failures: list[str] = []
    unique_tools: dict[tuple[str, str], ToolDescription] = {}
    for tool in tools:
        existing = unique_tools.get(tool.identity)
        if existing is None:
            unique_tools[tool.identity] = tool
            continue
        # Prefer the source-of-truth manifest over snapshot copies so a fix
        # to `crates/app-skills/<x>/manifest.json` is the one we lint, not
        # the stale `.crew/bundled-app-skills/<x>/manifest.json` snapshot.
        if _is_source_path(tool.manifest) and not _is_source_path(existing.manifest):
            unique_tools[tool.identity] = tool

    for noun, aliases in CONTENT_NOUNS.items():
        mentioned = group_mentions(unique_tools.values(), aliases)
        if len(mentioned) < 2:
            continue

        owners = {
            manifest_name: group
            for manifest_name, group in mentioned.items()
            if any(tool_owns_noun(tool, aliases) for tool in group)
        }
        if len(owners) != 1:
            failures.append(
                format_failure(
                    noun,
                    flatten_groups(mentioned.values()),
                    None,
                    "choose one owning tool or rename descriptions so only one tool "
                    "claims this noun",
                )
            )
            continue

        owner_manifest, owner_group = next(iter(owners.items()))
        owner = first_owner(owner_group, aliases)
        missing = [
            group
            for manifest_name, group in mentioned.items()
            if manifest_name != owner_manifest
            and not any(tool_disclaims_noun(tool, aliases, owner.tool_name) for tool in group)
        ]
        if missing:
            failures.append(
                format_failure(
                    noun,
                    flatten_groups(missing),
                    owner,
                    f'add "NOT for {noun} - use `{owner.tool_name}` for {noun}"',
                )
            )

    return failures


def group_mentions(
    tools: Iterable[ToolDescription],
    aliases: Iterable[str],
) -> dict[str, list[ToolDescription]]:
    mentioned: dict[str, list[ToolDescription]] = {}
    for tool in tools:
        if text_mentions(tool.description, aliases):
            mentioned.setdefault(tool.manifest_name, []).append(tool)
    return mentioned


def flatten_groups(groups: Iterable[list[ToolDescription]]) -> list[ToolDescription]:
    return [tool for group in groups for tool in group]


def first_owner(
    tools: list[ToolDescription],
    aliases: Iterable[str],
) -> ToolDescription:
    owners = [tool for tool in tools if tool_owns_noun(tool, aliases)]
    if not owners:
        return tools[0]
    return max(owners, key=owner_score)


def owner_score(tool: ToolDescription) -> tuple[int, str]:
    name = tool.tool_name.lower()
    score = 0
    if any(word in name for word in ("generate", "make", "create")):
        score += 3
    if any(word in name for word in ("list", "voices", "styles", "save", "delete")):
        score -= 2
    return (score, name)


def format_failure(
    noun: str,
    tools: list[ToolDescription],
    owner: ToolDescription | None,
    suggestion: str,
) -> str:
    lines = [f"ambiguous content noun `{noun}` in tool descriptions"]
    if owner is not None:
        lines.append(f"  owner: {owner.display_name} ({owner.manifest})")
    for tool in tools:
        lines.append(f"  - {tool.display_name} ({tool.manifest})")
    lines.append(f"  suggestion: {suggestion}")
    return "\n".join(lines)


def run_self_test() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        slides = root / "mofa-slides" / "manifest.json"
        site = root / "mofa-site" / "manifest.json"
        slides.parent.mkdir()
        site.parent.mkdir()
        slides.write_text(
            json.dumps(
                {
                    "name": "mofa-slides",
                    "description": "Presentation generation.",
                    "tools": [
                        {
                            "name": "mofa_slides",
                            "description": "Generate presentation slides and decks.",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        site.write_text(
            json.dumps(
                {
                    "name": "mofa-site",
                    "description": "Website generation.",
                    "tools": [
                        {
                            "name": "mofa_site",
                            "description": "Generate websites, decks, and slides.",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        tools, errors = load_tools(manifest_paths([root]))
        if errors:
            raise AssertionError(errors)
        failures = lint_tools(tools)
        if not failures:
            raise AssertionError("ambiguous fixture unexpectedly passed")

        site.write_text(
            json.dumps(
                {
                    "name": "mofa-site",
                    "description": "Website generation.",
                    "tools": [
                        {
                            "name": "mofa_site",
                            "description": (
                                "Generate websites. NOT for slides - use `mofa_slides` "
                                "for slides."
                            ),
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        tools, errors = load_tools(manifest_paths([root]))
        if errors:
            raise AssertionError(errors)
        failures = lint_tools(tools)
        if failures:
            raise AssertionError("\n".join(failures))

        # Fixture 3 — when the same `(manifest_name, tool_name)` appears in a
        # snapshot and the source-of-truth path, dedup must keep the source
        # entry so a fix in `crates/app-skills/...` actually gets checked.
        # Snapshot lives under `.crew/bundled-app-skills/` so it sorts *before*
        # `crates/app-skills/` via `sorted()`. Without source-preference dedup
        # the stale snapshot would shadow the fixed source and this fixture
        # would fail.
        snapshot = root / ".crew" / "bundled-app-skills" / "deep-crawl" / "manifest.json"
        source = root / "crates" / "app-skills" / "deep-crawl" / "manifest.json"
        site2 = root / "mofa-site-2" / "manifest.json"
        snapshot.parent.mkdir(parents=True)
        source.parent.mkdir(parents=True)
        site2.parent.mkdir()
        snapshot.write_text(
            json.dumps(
                {
                    "name": "deep-crawl",
                    "tools": [
                        {
                            "name": "deep_crawl",
                            # Stale: missing disclaimer.
                            "description": "Crawl a website.",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        source.write_text(
            json.dumps(
                {
                    "name": "deep-crawl",
                    "tools": [
                        {
                            "name": "deep_crawl",
                            # Fresh: has disclaimer for mofa_site_2.
                            "description": (
                                "Crawl a website. NOT for site - use "
                                "`mofa_site_2` for site."
                            ),
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        site2.write_text(
            json.dumps(
                {
                    "name": "mofa-site-2",
                    "tools": [
                        {
                            "name": "mofa_site_2",
                            "description": "Generate static websites and sites.",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        # `snapshot.parent.parent.parent` == `<root>/.crew`,
        # `source.parent.parent.parent` == `<root>/crates`. Sanity check the
        # path-sort assumption that motivates the source-preference dedup.
        roots = [snapshot.parent.parent.parent, source.parent.parent.parent, site2.parent]
        scanned = manifest_paths(roots)
        if scanned.index(snapshot) >= scanned.index(source):
            raise AssertionError(
                "fixture invariant: `.crew/...` snapshot must sort before "
                "`crates/...` source so dedup actually exercises the "
                "source-preference branch"
            )
        tools, errors = load_tools(scanned)
        if errors:
            raise AssertionError(errors)
        failures = lint_tools(tools)
        if failures:
            raise AssertionError(
                "snapshot-shadow fixture should pass because source manifest "
                "carries the disclaimer:\n" + "\n".join(failures)
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Scan Octos tool manifests for ambiguous content noun ownership."
    )
    parser.add_argument(
        "--root",
        action="append",
        type=Path,
        help="Manifest root or manifest.json to scan. Repeatable. Defaults to bundled Octos roots.",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in positive/negative fixtures before scanning.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        run_self_test()

    roots = args.root or [Path(root) for root in DEFAULT_ROOTS]
    paths = manifest_paths(roots)
    if not paths:
        print("tool-description lint: no manifest.json files found", file=sys.stderr)
        return 2

    tools, errors = load_tools(paths)
    failures = lint_tools(tools)
    if errors or failures:
        print("tool-description lint failed:", file=sys.stderr)
        for error in errors:
            print(error, file=sys.stderr)
        for failure in failures:
            print(failure, file=sys.stderr)
        return 1

    print(f"tool-description lint passed: {len(tools)} tools across {len(paths)} manifests")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
