# PowerPoint compatibility is separate from rendering

A valid ZIP, successful `python-pptx` reopen, or a LibreOffice PDF does not prove that Microsoft PowerPoint accepts the PPTX. On a PowerPoint repair/open failure, preserve the failing bytes and SHA-256; compare the transferred hash before diagnosing download damage. Track this as a package compatibility defect independently of visual findings.

Run the read-only preflight on the actual delivered editable and blank files:

```sh
python skills/pptx/scripts/check_pptx_compat.py exports/deck.pptx --out verification/compatibility.json
```

Its scope is deliberately limited: ZIP CRC, duplicate parts, XML parsing, and DrawingML autofit exclusivity. A passing report is not full OOXML schema validation, relationship validation, a visual verdict, or a PowerPoint-open result. Use a full Open XML validator where available and actual target-application opening when available. Record the application/version, candidate hash, slide count and whether any repair prompt appeared. If target-app testing is unavailable, say so; do not substitute a PDF result for that evidence. A user-reported open failure remains unresolved until a new candidate is tested in that application.

## Autofit serialization failure

`a:bodyPr` can contain zero or one choice from `a:noAutofit`, `a:normAutofit`, and `a:spAutoFit`. Two identical `a:noAutofit` children are invalid too. In python-pptx, assigning `text_frame.auto_size = MSO_AUTO_SIZE.NONE` already writes the appropriate element and removes the other modes. Manually appending another one afterwards corrupts the choice. Prefer the public setter and inspect serialized output; if manual OOXML is necessary, preserve exclusivity and schema child order. Repeated application of a helper must not accumulate XML elements.

Correct the producer rather than merely hand-patching one exported ZIP. Rebuild under a distinct candidate name and keep the failing artifact. Re-render to check that content, typography, media and geometry remain intact; scope checks to the change. Do not regenerate images for a package-only defect. When the target app opens successfully, record that bounded result; it does not close unrelated visual findings.

## Inspect native-app appearance separately

Successful opening is not a visual-layout pass. When PowerPoint is available, inspect slides exported or captured from the exact candidate in that application, retaining the PPTX hash and application/version. A LibreOffice export may differ in fonts, wrapping or clipping. If only that fallback exists, state it explicitly and leave native PowerPoint visual status unverified. Follow [PPTX-first audit](pptx-first-audit.md) for package, target-app pixels and optional PDF geometry as three separate evidence sources.
