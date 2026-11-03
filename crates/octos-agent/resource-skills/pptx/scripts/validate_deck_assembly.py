#!/usr/bin/env python3
"""Validate every prepared handoff and assembled proof, without claiming visual QA."""
import argparse
import json
from pathlib import Path
import xml.etree.ElementTree as ET
import zipfile

from compare_slide_proof import compare
from inspect_slide_layers import NS, relationship_map, target_part
from validate_page_handoff import bound_file, load, require, sha256, validate_many

ERRORS = (ValueError, KeyError, TypeError, OSError, ET.ParseError, zipfile.BadZipFile)


def receipt_file(base, value):
    require(isinstance(value, str) and bool(value), "Receipt file path is missing")
    path = (base / value).resolve()
    require(path.is_file(), f"Missing receipt file: {path}")
    return path


def protect_declared_inputs(receipt, base, protected):
    """Protect known inputs even when another job is missing or malformed.

    This best-effort path discovery is only an output safety guard; validation
    still rejects malformed/missing manifests and checks every binding below.
    """
    entries = receipt.get("jobs", [])
    if not isinstance(entries, list):
        return
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        paths = {}
        for key in ("job", "result"):
            value = entry.get(key)
            if isinstance(value, str) and value:
                paths[key] = (base / value).resolve()
                protected.add(paths[key])
        if "job" not in paths:
            continue
        try:
            job = load(paths["job"])
            references = [job["shared_contract"], *job["inputs"]]
            for reference in references:
                protected.add((paths["job"].parent / reference["path"]).resolve())
            if "result" in paths:
                result = load(paths["result"])
                output = paths["job"].parent / job["output_dir"]
                artifacts = result["artifacts"]
                if isinstance(artifacts, dict):
                    for reference in artifacts.values():
                        protected.add((output / reference["path"]).resolve())
        except ERRORS:
            continue


def final_slide_count(deck):
    with zipfile.ZipFile(deck) as package:
        require(len(package.namelist()) == len(set(package.namelist())), "Duplicate package members")
        root = ET.fromstring(package.read("ppt/presentation.xml"))
        slides = root.findall("p:sldIdLst/p:sldId", NS)
        relationships = relationship_map(package, "ppt/presentation.xml")
        ids = [slide.get("id") for slide in slides]
        parts = [target_part("ppt/presentation.xml", relationships[slide.get("{" + NS["r"] + "}id")])
                 for slide in slides]
        require(None not in ids and len(ids) == len(set(ids)), "Duplicate or missing presentation slide IDs")
        require(None not in parts and len(parts) == len(set(parts)), "Duplicate or external presentation slide references")
        return len(slides)


def validate(jobs_path, deck_path, protected_files=None):
    """Return a fail-closed report; every receipt entry is mandatory.

    The optional set is only for the CLI's read-only output collision guard,
    not for selecting, skipping or waiving any validation.
    """
    protected = protected_files if protected_files is not None else set()
    report = {"validator": "validate_deck_assembly", "assembly_consistent": False,
              "visual_quality_verified": False, "pages": [], "errors": [],
              "scope": "Complete prepared-job collection, current input/artifact hashes, final slide count/order, "
                       "and every accepted proof's native content/resources/layout/master/theme/defaults. "
                       "This does not establish renderer pixel equality or visual quality."}
    try:
        jobs_path, deck_path = Path(jobs_path).resolve(), Path(deck_path).resolve()
        protected.update((jobs_path, deck_path))
        receipt = load(jobs_path)
        protect_declared_inputs(receipt, jobs_path.parent, protected)
        order, entries = receipt["page_order"], receipt["jobs"]
        require(isinstance(order, list) and order and all(isinstance(p, str) and p for p in order),
                "Invalid receipt page_order")
        require(len(order) == len(set(order)), "Duplicate receipt page IDs")
        require(isinstance(entries, list) and entries, "Receipt has no jobs")
        ids = [entry["page_id"] for entry in entries]
        require(ids == order, "Receipt jobs must include every page exactly once in receipt page_order")
        snapshots = {jobs_path: sha256(jobs_path), deck_path: sha256(deck_path)}
        pairs, jobs, results = [], [], []
        for entry in entries:
            job_path = receipt_file(jobs_path.parent, entry["job"])
            result_path = receipt_file(jobs_path.parent, entry["result"])
            protected.update((job_path, result_path))
            job, result = load(job_path), load(result_path)
            require(job["page_id"] == entry["page_id"], "Receipt identity differs from assigned job")
            snapshots[job_path], snapshots[result_path] = sha256(job_path), sha256(result_path)
            pairs.append((job_path, result_path))
            jobs.append(job)
            results.append(result)
        collection = validate_many(pairs, complete_deck=True)
        pages = collection["pages"]
        require(pages[0]["page_order"] == order, "Receipt page_order differs from frozen shared contract")
        require([page["page_id"] for page in pages] == ids, "Receipt job order differs from frozen shared contract")
        report.update({"jobs": str(jobs_path), "jobs_sha256": snapshots[jobs_path],
                       "deck": str(deck_path), "deck_sha256": snapshots[deck_path],
                       "page_order": order, "contract_sha256": pages[0]["contract_sha256"],
                       "handoff_consistent": True, "slide_count": final_slide_count(deck_path)})
        require(report["slide_count"] == len(order), "Final deck slide count differs from complete job collection")
        for (job_path, _), job, result, page in zip(pairs, jobs, results, pages):
            for ref in [job["shared_contract"], *job["inputs"]]:
                snapshots[bound_file(job_path.parent, ref)] = ref["sha256"]
            for key, path in page["files"].items():
                snapshots[Path(path)] = result["artifacts"][key]["sha256"]
        for index, page in enumerate(pages, 1):
            record = {"page_id": page["page_id"], "attempt_id": page["attempt_id"],
                      "job": str(pairs[index - 1][0]), "result": page["result"], "page": index}
            try:
                record.update(compare(page["files"]["editable_pptx"], deck_path, index))
                if not record["native_assembly_unchanged"]:
                    report["errors"].append(f"Assembled page differs from accepted proof: {page['page_id']}")
            except ERRORS as error:
                record.update({"native_assembly_unchanged": False, "error": str(error),
                               "visual_quality_verified": False})
                report["errors"].append(f"Proof comparison failed for {page['page_id']}: {error}")
            report["pages"].append(record)
        # A report must bind a stable set of bytes, not a mixture read while a
        # coordinator or worker continues editing the sources or final deck.
        for path, expected in snapshots.items():
            require(sha256(path) == expected, f"Input changed during assembly validation: {path}")
        report["assembly_consistent"] = not report["errors"]
    except ERRORS as error:
        report["errors"].append(str(error))
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", required=True, type=Path, help="prepared-jobs.json from the complete frozen assignment")
    parser.add_argument("--deck", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    protected = set()
    report = validate(args.jobs, args.deck, protected)
    if args.out.resolve() in protected:
        print(json.dumps({"assembly_consistent": False, "visual_quality_verified": False,
                          "errors": ["Report output must not overwrite any validation input"]}))
        return 1
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False))
    return 0 if report["assembly_consistent"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
