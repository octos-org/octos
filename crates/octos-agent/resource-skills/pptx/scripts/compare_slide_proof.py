#!/usr/bin/env python3
"""Detect assembly changes against a one-page proof. Read-only; not visual acceptance."""
import argparse
import hashlib
import json
from pathlib import Path
import xml.etree.ElementTree as ET
import zipfile

from inspect_slide_layers import NS, relationship_map, target_part


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True).encode()).hexdigest()


def signature(path, page, require_single=False):
    with zipfile.ZipFile(path) as z:
        presentation = ET.fromstring(z.read("ppt/presentation.xml"))
        slides = presentation.findall("p:sldIdLst/p:sldId", NS)
        if require_single and len(slides) != 1:
            raise ValueError("A worker proof must contain exactly one page")
        if not 1 <= page <= len(slides):
            raise ValueError("Page outside presentation order")
        rels = relationship_map(z, "ppt/presentation.xml")
        part = target_part("ppt/presentation.xml", rels[slides[page-1].get("{" + NS["r"] + "}id")])

        def content(part, stack=()):
            if part in stack or len(stack) > 12:
                raise ValueError("Cyclic or unsupported resource dependency")
            raw = z.read(part)
            if not part.endswith(".xml"):
                return {"binary_sha256": hashlib.sha256(raw).hexdigest()}
            rel = relationship_map(z, part)

            def dependency(rid):
                relation = rel[rid]
                if relation.get("TargetMode") == "External":
                    return {"external": relation["Target"], "type": relation["Type"]}
                return {"type": relation["Type"],
                        "content": content(target_part(part, relation), (*stack, part))}

            def node(e):
                attributes = dict(e.attrib)
                for key, value in list(attributes.items()):
                    if key.startswith("{" + NS["r"] + "}"):
                        attributes[key] = dependency(value)
                # Master layout registration IDs do not affect the selected
                # slide's appearance and would introduce layout/master cycles.
                children = [node(c) for c in e if c.tag != "{" + NS["p"] + "}sldLayoutIdLst"]
                return [e.tag, attributes, e.text or "", children]

            root = ET.fromstring(raw)
            inherited = []
            inherited_type = {"sld": "slideLayout", "sldLayout": "slideMaster", "sldMaster": "theme"}.get(root.tag.rsplit("}", 1)[-1])
            for rid, relation in rel.items():
                if relation["Type"].rsplit("/", 1)[-1] == inherited_type:
                    inherited.append(dependency(rid))
            return {"xml": node(root), "inherited": inherited}

        defaults = [ET.tostring(e, encoding="unicode") for e in presentation
                    if e.tag in ("{" + NS["p"] + "}sldSz", "{" + NS["p"] + "}defaultTextStyle")]
        return {"slide_content_sha256": digest(content(part)),
                "presentation_defaults_sha256": digest(defaults)}


def compare(proof, deck, page):
    proof, deck = Path(proof), Path(deck)
    a, b = signature(proof, 1, True), signature(deck, page)
    differences = [key for key in a if a[key] != b[key]]
    return {"proof_sha256": hashlib.sha256(proof.read_bytes()).hexdigest(),
            "deck_sha256": hashlib.sha256(deck.read_bytes()).hexdigest(), "page": page,
            "native_assembly_unchanged": not differences, "changed_components": differences,
            "proof_signature": a, "assembled_signature": b,
            "visual_quality_verified": False,
            "scope": "Slide XML, embedded resources and layout/master/theme/defaults. Notes excluded. "
                     "Any change invalidates reuse of prior QA until checked. Native equality alone "
                     "does not establish renderer pixel equality or that the proof was good."}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--proof", required=True)
    p.add_argument("--deck", required=True)
    p.add_argument("--page", required=True, type=int)
    p.add_argument("--out", required=True, type=Path)
    a = p.parse_args()
    try:
        result = compare(a.proof, a.deck, a.page)
    except (ValueError, KeyError, OSError, ET.ParseError, zipfile.BadZipFile) as e:
        result = {"native_assembly_unchanged": False, "error": str(e), "visual_quality_verified": False}
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(result, ensure_ascii=False))
    return 0 if result["native_assembly_unchanged"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
