#!/usr/bin/env python3
"""Read-only text/region checks for a PPTX and its hash-bound PDF render.

Regions must be read independently from artwork, never copied from the candidate's
text frames. This is a scoped detector, not a visual-quality certificate. PDF
character bounds are font bounds, not exact raster ink; inspect flagged crops.
Requires PyMuPDF, python-pptx and Pillow. See references/text-region-audit.md.
"""
import argparse
import hashlib
import json
import math
import unicodedata
from pathlib import Path

import fitz
from PIL import Image
from pptx import Presentation


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def norm(text):
    # Preserve case, punctuation, operators and subscripts. Whitespace alone may
    # change during rendering. NFKC would hide meaningful glyph differences.
    return ''.join(c for c in unicodedata.normalize('NFC', text) if not c.isspace())


def rectangle(value):
    if (len(value) != 4 or not all(isinstance(v, (int, float)) and math.isfinite(v) for v in value)
            or not (0 <= value[0] < value[2] <= 1 and 0 <= value[1] < value[3] <= 1)):
        raise ValueError('Region must be normalized [x0,y0,x1,y1] within the page')
    return tuple(value)


def center(rect):
    return ((rect[0] + rect[2]) / 2, (rect[1] + rect[3]) / 2)


def union(rects):
    return (min(r[0] for r in rects), min(r[1] for r in rects),
            max(r[2] for r in rects), max(r[3] for r in rects))


def inside(inner, outer, tolerance=(0, 0)):
    tx, ty = tolerance
    return (inner[0] >= outer[0] - tx and inner[1] >= outer[1] - ty
            and inner[2] <= outer[2] + tx and inner[3] <= outer[3] + ty)


def overlaps(a, b):
    return min(a[2], b[2]) > max(a[0], b[0]) and min(a[3], b[3]) > max(a[1], b[1])


def pdf_characters(page):
    stream, records = [], []
    for bi, block in enumerate(page.get_text('rawdict', sort=False)['blocks']):
        for li, line in enumerate(block.get('lines', [])):
            for span in line['spans']:
                for char in span['chars']:
                    box = char['bbox']
                    rec = {'rect': (box[0] / page.rect.width, box[1] / page.rect.height,
                                    box[2] / page.rect.width, box[3] / page.rect.height),
                           'line': (bi, li), 'origin': char['origin'], 'size': span['size'],
                           'direction': line.get('dir')}
                    for c in norm(char['c']):
                        stream.append(c)
                        records.append(rec)
    return ''.join(stream), records


def locate(text, stream, records, native_rect):
    """Match actual native copy to PDF glyphs; ambiguity is unresolved, not pass."""
    needle = norm(text)
    if not needle:
        return None, 'empty text'
    start, candidates = 0, []
    while True:
        pos = stream.find(needle, start)
        if pos < 0:
            break
        chars = records[pos:pos + len(needle)]
        box = union([c['rect'] for c in chars])
        score = math.dist(center(box), center(native_rect))
        candidates.append((score, box, chars))
        start = pos + 1
    if not candidates:
        return None, 'PDF text mapping absent; inspect extraction order, glyphs and pixels'
    candidates.sort(key=lambda x: x[0])
    diagonal = math.dist(native_rect[:2], native_rect[2:])
    if candidates[0][0] > max(.035, diagonal * 1.5):
        return None, 'Only distant PDF matches; do not substitute another occurrence'
    if len(candidates) > 1 and candidates[1][0] - candidates[0][0] < .002:
        return None, 'Ambiguous PDF occurrence; needs local pixel review'
    return {'rect': candidates[0][1], 'characters': candidates[0][2]}, None


def rendered_lines(chars):
    # Ignore tiny sub/superscript baseline shifts: cluster by vertical origin.
    groups = []
    for char in chars:
        baseline = char['origin'][1]
        target = next((g for g in groups if abs(g['baseline'] - baseline) <= .45 * char['size']), None)
        if target is None:
            target = {'baseline': baseline, 'characters': []}
            groups.append(target)
        target['characters'].append(char)
    return sorted(groups, key=lambda g: g['baseline'])


def crop_pair(page, design_path, region, out_dir, stem):
    region = (max(0, region[0] - .015), max(0, region[1] - .015),
              min(1, region[2] + .015), min(1, region[3] + .015))
    if region[0] >= region[2] or region[1] >= region[3]:
        return None
    clip = fitz.Rect(region[0] * page.rect.width, region[1] * page.rect.height,
                     region[2] * page.rect.width, region[3] * page.rect.height)
    render_path = out_dir / (stem + '-render.png')
    page.get_pixmap(matrix=fitz.Matrix(2.5, 2.5), clip=clip, alpha=False).save(render_path)
    with Image.open(design_path) as im:
        box = (int(region[0] * im.width), int(region[1] * im.height),
               math.ceil(region[2] * im.width), math.ceil(region[3] * im.height))
        source_path = out_dir / (stem + '-source.png')
        im.crop(box).save(source_path)
    return {'normalized_crop': region, 'source': str(source_path.resolve()),
            'render': str(render_path.resolve()), 'source_sha256': digest(source_path),
            'render_sha256': digest(render_path)}


def run(pptx_path, pdf_path, spec_path, binding_path, out_dir):
    spec = json.loads(spec_path.read_text())
    binding = json.loads(binding_path.read_text())
    if binding.get('input_sha256') != digest(pptx_path) or binding.get('pdf_sha256') != digest(pdf_path):
        raise ValueError('Render binding does not match actual PPTX and PDF hashes')
    if spec.get('version') != 1 or not spec.get('pages'):
        raise ValueError('Expected version 1 and nonempty pages')
    prs = Presentation(pptx_path)
    doc = fitz.open(pdf_path)
    if len(doc) != len(prs.slides):
        raise ValueError('PDF/PPTX page-count mismatch')
    out_dir.mkdir(parents=True, exist_ok=False)
    evidence, findings, scope, images = [], [], [], []
    for pg in spec['pages']:
        num = pg['page']
        if not 1 <= num <= len(doc):
            raise ValueError('Page outside deck')
        page, slide = doc[num - 1], prs.slides[num - 1]
        design_path = (spec_path.parent / pg['design_image']).resolve()
        artwork_path = (spec_path.parent / pg['artwork_image']).resolve()
        images.append({'page': num, 'design': str(design_path), 'design_sha256': digest(design_path),
                       'artwork': str(artwork_path), 'artwork_sha256': digest(artwork_path)})
        if not pg.get('region_basis'):
            raise ValueError('Explain independent region basis for every page')
        shapes = [s for s in slide.shapes if s.has_text_frame and s.text.strip()]
        native = {s.name: [t for t in shapes if t.name == s.name] for s in shapes}
        stream, chars = pdf_characters(page)
        tolerance = (pg.get('tolerance_pt', 1) / page.rect.width,
                     pg.get('tolerance_pt', 1) / page.rect.height)
        checked_names, ids = set(), set()
        for idx, obj in enumerate(pg.get('objects', [])):
            oid, name = obj['id'], obj['native_name']
            if oid in ids or name in checked_names:
                raise ValueError('Object ids and native names must be unique per page')
            ids.add(oid); checked_names.add(name)
            owner = rectangle(obj['owner_rect'])
            if not obj.get('owner_id') or not obj.get('source_evidence'):
                raise ValueError('Each object needs owner_id and independent source_evidence')
            row = {'page': num, 'object': oid, 'native_name': name, 'owner_id': obj['owner_id'],
                   'owner_rect': owner, 'findings': []}

            def issue(code, details):
                item = {'page': num, 'object': oid, 'code': code, 'details': details}
                findings.append(item); row['findings'].append(code)

            matches = native.get(name, [])
            rects = [owner]
            if len(matches) != 1:
                issue('NATIVE_OBJECT_UNRESOLVED', {'matches': len(matches)})
            else:
                sh = matches[0]
                actual = (sh.left / prs.slide_width, sh.top / prs.slide_height,
                          (sh.left + sh.width) / prs.slide_width, (sh.top + sh.height) / prs.slide_height)
                row['native_rect'] = actual
                rects.append(actual)
                if norm(sh.text) != norm(obj['text']):
                    issue('COPY_MISMATCH', {'expected': obj['text'], 'actual': sh.text})
                if sh.rotation:
                    issue('ROTATED_TEXT_UNRESOLVED', {'rotation': sh.rotation})
                    mapped, error = None, None
                else:
                    mapped, error = locate(sh.text, stream, chars, actual)
                if error:
                    issue('PDF_MAPPING_UNRESOLVED', error)
                if mapped:
                    rects.append(mapped['rect']); row['rendered_rect'] = mapped['rect']
                    if not inside(mapped['rect'], owner, tolerance):
                        issue('TEXT_OUTSIDE_OWNER', {'rendered_rect': mapped['rect'], 'tolerance_pt': pg.get('tolerance_pt', 1)})
                    for forbidden in pg.get('forbidden', []):
                        if oid in forbidden.get('allow_objects', []):
                            continue
                        obstacle = rectangle(forbidden['rect'])
                        if any(overlaps(c['rect'], obstacle) for c in mapped['characters']):
                            issue('TEXT_OVER_ARTWORK_REGION', {'region_id': forbidden['id'], 'rect': obstacle})
                    lines = rendered_lines(mapped['characters'])
                    row['rendered_lines'] = len(lines)
                    if obj.get('max_lines') is not None and len(lines) > obj['max_lines']:
                        issue('UNEXPECTED_WRAP', {'actual_lines': len(lines), 'max_lines': obj['max_lines']})
                    if obj.get('check_orphan') and len(lines) > 1 and len(lines[-1]['characters']) <= 2:
                        issue('SHORT_FINAL_LINE_REVIEW', {'last_line_characters': len(lines[-1]['characters'])})
            # Include context even for a wrongly mapped object far from its owner.
            if obj.get('review_rect'):
                rects.append(rectangle(obj['review_rect']))
            row['evidence'] = crop_pair(page, design_path, union(rects), out_dir, f'p{num:02d}-{idx:03d}')
            evidence.append(row)
        for check in pg.get('counts', []):
            matches = [s.name for s in shapes if norm(s.text) == norm(check['text'])]
            if len(matches) != check['expected']:
                findings.append({'page': num, 'object': check['id'], 'code': 'EXACT_NATIVE_COUNT_MISMATCH',
                                 'details': {'text': check['text'], 'expected': check['expected'],
                                             'actual': len(matches), 'native_names': matches}})
        unsupported = [s.name for s in slide.shapes if s.shape_type == 6 or s.has_table]
        scope.append({'page': num, 'checked_objects': len(ids), 'total_top_level_text_objects': len(shapes),
                      'uncovered_native_names': [s.name for s in shapes if s.name not in checked_names],
                      'unsupported_group_or_table_shapes': unsupported,
                      'region_basis': pg['region_basis']})
    report = {'schema_version': 1, 'pptx_sha256': digest(pptx_path), 'pdf_sha256': digest(pdf_path),
              'spec_sha256': digest(spec_path), 'binding_sha256': digest(binding_path), 'images': images,
              'scope': scope, 'findings': findings, 'objects': evidence,
              'visual_review_required': True, 'whole_deck_pass': None,
              'limitations': ['Caller supplies independent semantic ownership; this script cannot validate it.',
                             'PDF glyph bounds are approximate, not raster-ink measurements.',
                             'Unmapped/reordered text, groups, tables and raster-only labels need separate review.',
                             'Exact native counts exclude occurrences inside longer paragraphs and baked-in raster text.',
                             'Crops use common normalized source/render coordinates; inspect registration if artwork moved.',
                             'Zero findings means only these declared checks found nothing.']}
    (out_dir / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    return report


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('pptx', type=Path); ap.add_argument('pdf', type=Path)
    ap.add_argument('--regions', required=True, type=Path)
    ap.add_argument('--render-report', required=True, type=Path)
    ap.add_argument('--out', required=True, type=Path, help='Fresh directory; existing evidence is never overwritten')
    args = ap.parse_args()
    report = run(args.pptx, args.pdf, args.regions, args.render_report, args.out)
    print(json.dumps({'findings': len(report['findings']), 'objects': len(report['objects']),
                      'report': str(args.out / 'report.json'), 'visual_review_required': True}))
    raise SystemExit(2 if report['findings'] else 0)


if __name__ == '__main__':
    main()
