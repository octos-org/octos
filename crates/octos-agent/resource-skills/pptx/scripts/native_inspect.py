#!/usr/bin/env python3
"""Measure a bound native page cycle once; findings are review candidates, not visual approval.

No source-pixel similarity target, inferred owner regions or automatic layout repair.
The generated overview and evidence panels support batched agent inspection.
"""
import argparse
from collections import Counter
import hashlib
import json
import math
from pathlib import Path
import re

import fitz
from PIL import Image, ImageChops, ImageDraw, ImageFont
from pptx import Presentation

from audit_text_regions import inside, locate, norm, overlaps, pdf_characters, rendered_lines, union
from native_deck import PREFIX, atomic_json, effective_style, read_json, require, sha
from label_geometry import region as label_region, frame_quad, bounds, angle_delta, outside_ink

VERSION = '1.5'


def bind(path, digest, bindings, name):
    path = Path(path).resolve(strict=True)
    require(path.is_file() and sha(path) == digest, f'{name} changed or has a stale hash')
    bindings[str(path)] = digest
    return path


def bound_cycle(cycle_path):
    """Verify files actually inspected, not only declarations in a cycle receipt."""
    cycle_path = Path(cycle_path).resolve(strict=True)
    cycle = read_json(cycle_path)
    require(cycle.get('state') == 'ready_for_visual_review', 'Cycle is not ready for inspection')
    bindings = {str(cycle_path): sha(cycle_path)}
    root = Path(cycle['workspace']).resolve(strict=True)
    record_path = bind(cycle['build_record'], cycle['build_record_sha256'], bindings, 'Build record')
    record = read_json(record_path)
    for path, digest in record['dependencies'].items():
        bind(path, digest, bindings, 'Build dependency')
    artifacts = {}
    for name, item in record['artifacts'].items():
        artifacts[name] = bind(record_path.parent/item['path'], item['sha256'], bindings, name)
    doc = read_json(artifacts['resolved_spec'])
    require([p['id'] for p in doc['pages']] == record['page_order'], 'Resolved page order differs')
    for page in doc['pages']:
        for role in ['background', 'design']:
            if role in page:
                bind(page[role]['path'], page[role]['sha256'], bindings, role)
    result = {}
    for role in ['editable', 'blank']:
        render = cycle['renders'][role]
        receipt = render['receipt']
        require(render['state'] == 'verified' and receipt['status'] == 'completed', 'Render is not complete')
        payload = receipt['result']; report = payload['report']
        source = artifacts[role+'_pptx']; digest = sha(source)
        require(receipt.get('verified_input_sha256') == digest and
                report.get('input_sha256') == digest and payload.get('snapshot_sha256') == digest,
                f'{role}: render does not bind to actual PPTX')
        out = (root/payload['out']).resolve(strict=True)
        require(out.is_relative_to(root), 'Render output escaped workspace')
        # Locate the actual PDF by its recorded bytes; a filename is not evidence.
        pdfs = [p for p in out.glob('*.pdf') if sha(p) == report.get('pdf_sha256')]
        require(len(pdfs) == 1, f'{role}: expected one PDF matching the render hash')
        pdf = bind(pdfs[0], report['pdf_sha256'], bindings, role+' PDF')
        expected_pages = list(range(1, len(doc['pages'])+1))
        require(report.get('pages') == len(expected_pages) and
                report.get('rendered_pages') == expected_pages, 'Incomplete render coverage')
        items = report.get('page_images', [])
        require([i.get('page') for i in items] == expected_pages, 'Page image coverage/order differs')
        images = []
        for item in items:
            image_path = (out/item['path']).resolve(strict=True)
            require(image_path.is_relative_to(out), 'Page image escaped render output')
            image_path = bind(image_path, item['sha256'], bindings, 'Page image')
            with Image.open(image_path) as image:
                require(list(image.size) == report.get('export_pixels'), 'Image dimensions differ from report')
            images.append(image_path)
        result[role] = {'pptx': source, 'pdf': pdf, 'images': images, 'report': report}
    require(result['editable']['report']['export_pixels'] == result['blank']['report']['export_pixels'],
            'Full and blank render dimensions differ')
    return doc, result, bindings


def xyxy(box, canvas):
    require(isinstance(box, list) and len(box) == 4 and
            all(type(v) in (int, float) and math.isfinite(v) for v in box), 'Use finite pixel [x,y,w,h]')
    x, y, w, h = box; width, height = canvas['width_px'], canvas['height_px']
    require(x >= 0 and y >= 0 and w > 0 and h > 0 and x+w <= width and y+h <= height,
            'Declared region lies outside canvas')
    return [x/width, y/height, (x+w)/width, (y+h)/height]


def normalized_quad(quad, canvas):
    return [[p[0]/canvas['width_px'], p[1]/canvas['height_px']] for p in quad]


def pxbox(box, width, height):
    return [round(box[0]*width, 2), round(box[1]*height, 2),
            round((box[2]-box[0])*width, 2), round((box[3]-box[1])*height, 2)]


def clearances(inner, outer, width, height):
    return {'left': round((inner[0]-outer[0])*width, 2),
            'top': round((inner[1]-outer[1])*height, 2),
            'right': round((outer[2]-inner[2])*width, 2),
            'bottom': round((outer[3]-inner[3])*height, 2)}


def font_runs(shape):
    return [{'text': run.text, 'font': run.font.name,
             'size_pt': round(run.font.size.pt, 4) if run.font.size is not None else None,
             'bold': run.font.bold, 'italic': run.font.italic}
            for paragraph in shape.text_frame.paragraphs for run in paragraph.runs if run.text]


def ink_components(mask):
    """Connected ink components using horizontal runs, without a new dependency.

    Full components preserve italic overhang and antialias edges which can extend
    past a nominal PDF character box. Eight-connected runs avoid a Python loop
    over every pixel of a 4K page.
    """
    width, height = mask.size
    data = mask.tobytes(); parents, runs = [], []
    previous = []
    def root(index):
        while parents[index] != index:
            parents[index] = parents[parents[index]]
            index = parents[index]
        return index
    for y in range(height):
        current = []
        row = data[y*width:(y+1)*width]
        start = 0
        for match in re.finditer(b'\xff+', row):
            x0, x1 = match.span(); ident = len(parents)
            parents.append(ident); runs.append((y, x0, x1)); current.append((x0, x1, ident))
            while start < len(previous) and previous[start][1] < x0:
                start += 1
            j = start
            while j < len(previous) and previous[j][0] <= x1:
                old = root(previous[j][2]); new = root(ident)
                if old != new:
                    parents[new] = old
                j += 1
        previous = current
    grouped = {}
    for ident, run in enumerate(runs):
        grouped.setdefault(root(ident), []).append(run)
    return [{'runs': parts,
             'box': [min(p[1] for p in parts), min(p[0] for p in parts),
                     max(p[2] for p in parts), max(p[0] for p in parts)+1],
             'pixels': sum(p[2]-p[1] for p in parts)} for parts in grouped.values()]


def pixel_rect(rect, size, padding=1):
    width, height = size
    return [max(0, math.floor(rect[0]*width)-padding), max(0, math.floor(rect[1]*height)-padding),
            min(width, math.ceil(rect[2]*width)+padding), min(height, math.ceil(rect[3]*height)+padding)]


def component_touches(component, box):
    if not overlaps(component['box'], box):
        return False
    x0, y0, x1, y1 = box
    return any(y0 <= y < y1 and x0 < end and x1 > start for y, start, end in component['runs'])


def attribute_ink(mask, mappings):
    """Assign actual changed pixels only where mapped characters identify one owner.

    A component crossing several objects' nominal masks is ambiguous. Components
    outside all masks remain unassigned. Neither case becomes a spatial pass.
    """
    parts = ink_components(mask)
    candidates = {}
    for key, info in mappings.items():
        mapped = info['mapped']
        # PDF extraction can split one ligature into an ordinary glyph plus a
        # zero-advance Unicode cell (for example "fi"). Such a cell has no
        # independently measurable ink; requiring it would invent missing text.
        cells = [c for c in mapped['characters'] if c['rect'][2] > c['rect'][0]+1e-9 and
                 c['rect'][3] > c['rect'][1]+1e-9] if mapped else []
        boxes = [pixel_rect(c['rect'], mask.size) for c in cells]
        candidates[key] = {'boxes': boxes, 'union': union(boxes) if boxes else pixel_rect(info['frame'], mask.size),
                           'shared_or_zero_advance_cells': len(mapped['characters'])-len(cells) if mapped else 0,
                           'assigned': [], 'ambiguous': 0}
    unassigned, ambiguous = [], []
    for part in parts:
        owners = []
        for key, candidate in candidates.items():
            if not overlaps(part['box'], candidate['union']):
                continue
            if candidate['boxes']:
                touches = any(component_touches(part, box) for box in candidate['boxes'])
            else:
                touches = component_touches(part, candidate['union'])
            if touches:
                owners.append(key)
        if len(owners) == 1 and candidates[owners[0]]['boxes']:
            candidates[owners[0]]['assigned'].append(part)
        elif owners:
            ambiguous.append(part)
            for key in owners:
                candidates[key]['ambiguous'] += 1
        else:
            unassigned.append(part)
    measured = {}
    width, height = mask.size
    for key, candidate in candidates.items():
        assigned = candidate['assigned']; reasons = []
        if not candidate['boxes']:
            reasons.append('PDF occurrence is not mapped')
        if candidate['ambiguous']:
            reasons.append('Ink also intersects another native object or an unmapped object frame')
        if not assigned:
            reasons.append('No uniquely attributable ink above the difference threshold')
        missing_cells = sum(not any(component_touches(part, box) for part in assigned)
                            for box in candidate['boxes'])
        if missing_cells:
            reasons.append('Mapped character cells have missing or low-contrast ink')
        # Do not claim complete attribution if nearby disconnected overhangs were
        # left outside every nominal character box.
        nearby = [candidate['union'][0]-2, candidate['union'][1]-2,
                  candidate['union'][2]+2, candidate['union'][3]+2]
        if any(component_touches(part, nearby) for part in unassigned):
            reasons.append('Unassigned ink touches the attribution boundary')
        box = union([p['box'] for p in assigned]) if assigned else None
        measured[key] = {'state': 'attributed' if not reasons else 'unknown', 'reasons': reasons,
                         'missing_character_cells': missing_cells, 'ambiguous_components': candidate['ambiguous'],
                         'shared_or_zero_advance_cells': candidate['shared_or_zero_advance_cells'],
                         'components': len(assigned), 'pixels': sum(p['pixels'] for p in assigned),
                         'rect': [box[0]/width, box[1]/height, box[2]/width, box[3]/height] if box else None,
                         '_components': assigned}
    return measured, {'components': len(parts), 'unassigned_components': len(unassigned),
                      'unassigned_pixels': sum(p['pixels'] for p in unassigned),
                      'ambiguous_components': len(ambiguous)}


def selections(doc, requested):
    """Bare IDs match each occurrence across pages; PAGE/ID disambiguates."""
    all_keys = {(p['id'], t['id']) for p in doc['pages'] for t in p['texts']}
    if requested is None:
        return all_keys
    selected = set()
    for value in requested:
        matches = {key for key in all_keys if value == key[1] or value == '/'.join(key)}
        require(matches, f'Unknown requested object: {value}')
        selected.update(matches)
    require(selected, 'Object selection cannot be empty')
    return selected


def external_regions(path, doc):
    if path is None:
        return {}
    config = read_json(path)
    require(config.get('schema_version') == 1 and isinstance(config.get('pages'), list),
            'Regions need schema_version 1 and pages')
    ids = {p['id'] for p in doc['pages']}; out = {}
    for page in config['pages']:
        require(set(page) <= {'id', 'basis', 'regions', 'objects', 'forbidden', 'policy', 'tolerance_pt'},
                'Unknown region-page field')
        pid = page['id']
        require(pid in ids and pid not in out, 'Unknown/duplicate region page ID')
        require(isinstance(page.get('basis'), str) and page['basis'].strip(),
                'Explain the independent artwork basis of supplied regions')
        out[pid] = page
    return out


def compare_previous(path, report):
    if not path:
        return None
    old = read_json(path)
    require(old.get('helper') == 'native_inspect' and old.get('schema_version') == 1,
            'Previous report is not a native_inspect report')
    require(old.get('page_order') == report['page_order'], 'Previous report has a different page collection')
    for dependency, digest in old.get('bindings', {}).items():
        require(sha(dependency) == digest, 'Previous inspection evidence changed')
    if old.get('version') != VERSION or old.get('measurement_policy') != report.get('measurement_policy'):
        return {'previous_report': str(Path(path).resolve()), 'previous_sha256': sha(path),
                'measurement_criteria_changed': True, 'no_longer_detected': [],
                'note': 'The helper criteria changed; disappearing findings cannot be counted as repaired.'}
    now_covered = {(r['page'], r['id']) for r in report['objects']}
    before_covered = {(r['page'], r['id']) for r in old['objects']}
    common = (now_covered & before_covered) | {(pid, '__page__') for pid in report['page_order']}
    def keys(items):
        return {(i['page'], i['object'], i['code']) for i in items if (i['page'], i['object']) in common}
    before, after = keys(old['findings']), keys(report['findings'])
    old_rows = {(r['page'], r['id']): r for r in old['objects']}
    def owner_basis(row):
        owner = row.get('owner')
        return {k: owner.get(k) for k in ['id', 'box_px', 'quad_px', 'basis', 'scope', 'baseline_deg']} if owner else None
    changed_owners = {(r['page'], r['id']) for r in report['objects']
                      if (r['page'], r['id']) in old_rows and
                      owner_basis(r) != owner_basis(old_rows[(r['page'], r['id'])])}
    disappeared = before-after
    basis_changed = {k for k in disappeared if k[:2] in changed_owners and k[2] == 'INK_OUTSIDE_OWNER_CANDIDATE'}
    return {'previous_report': str(Path(path).resolve()), 'previous_sha256': sha(path),
            'persisting': [list(k) for k in sorted(before & after)],
            'new': [list(k) for k in sorted(after-before)],
            'no_longer_detected': [list(k) for k in sorted(disappeared-basis_changed)],
            'owner_basis_changed': [list(k) for k in sorted(basis_changed)],
            'not_rechecked': [list(k) for k in sorted(before_covered-now_covered)],
            'note': 'No longer detected means this same scoped check changed; it is not visual approval.'}


def previous_inspection(previous, previous_cycle):
    """Reuse the prior measured report when available; do not launch extra rendering."""
    if previous_cycle and not previous:
        candidate = Path(previous_cycle).resolve().parent/'inspection/report.json'
        if candidate.is_file():
            previous = candidate
    if previous and previous_cycle:
        old = read_json(previous)
        require(Path(old.get('cycle_record', '')).resolve() == Path(previous_cycle).resolve(),
                'Previous inspection does not belong to the previous cycle')
    return previous


def tile(image_path, box=None, size=(640, 360), marks=None):
    with Image.open(image_path) as im:
        im = im.convert('RGB')
        if marks:
            draw = ImageDraw.Draw(im)
            for rect, color, label in marks:
                if isinstance(rect[0], (list, tuple)):
                    points = [(p[0]*im.width, p[1]*im.height) for p in rect]
                    draw.line(points+[points[0]], fill=color, width=max(2, round(im.width/800)))
                    coords = [points[0][0], points[0][1]]
                else:
                    coords = [rect[0]*im.width, rect[1]*im.height, rect[2]*im.width, rect[3]*im.height]
                    draw.rectangle(coords, outline=color, width=max(2, round(im.width/800)))
                if label:
                    draw.text((coords[0]+3, coords[1]+3), label, fill=color,
                              font=ImageFont.load_default(size=max(14, round(im.width/80))),
                              stroke_width=1, stroke_fill='white')
        if box:
            im = im.crop((math.floor(box[0]*im.width), math.floor(box[1]*im.height),
                          math.ceil(box[2]*im.width), math.ceil(box[3]*im.height)))
        # Inspection derivatives use the same viewport scale for source and render,
        # even when native resolutions differ. Only review derivatives are resized.
        scale = min(size[0]/im.width, size[1]/im.height)
        im = im.resize((max(1, round(im.width*scale)), max(1, round(im.height*scale))), Image.Resampling.LANCZOS)
        out = Image.new('RGB', size, 'white')
        out.paste(im, ((size[0]-im.width)//2, (size[1]-im.height)//2))
        return out


def panels(doc, render, rows, selected, output):
    """Overview for coverage; batch actual flags and explicitly requested objects."""
    outputs = []
    by_page = {p['id']: (i, p) for i, p in enumerate(doc['pages'])}
    for pid, (i, page) in by_page.items():
        paths = [page.get('design', page['background'])['path'],
                 render['editable']['images'][i], render['blank']['images'][i]]
        titles = ['Source region (view only)', 'Actual editable PPTX', 'Actual derived blank']
        image = Image.new('RGB', (1920, 390), '#eeeeee'); draw = ImageDraw.Draw(image)
        for col, (path, title) in enumerate(zip(paths, titles)):
            draw.text((col*640+8, 8), title, fill='black')
            image.paste(tile(path), (col*640, 28))
        target = output/f'page-{i+1:03d}-overview.jpg'; image.save(target, quality=93)
        outputs.append({'kind': 'overview', 'page': pid, 'path': str(target), 'sha256': sha(target)})
        page_rows = [r for r in rows if r['page'] == pid]
        marks, owners = [], set()
        for index, row in enumerate(page_rows, 1):
            row['overlay_index'] = index
            if row['owner']:
                owner = row['owner']
                owner['assigned_objects'] = [r['id'] for r in page_rows if r['owner'] and r['owner']['id'] == owner['id']]
                frame = row.get('frame_px')
                if frame:
                    owner['area_to_frame_ratio'] = round(owner['box_px'][2]*owner['box_px'][3]/(frame[2]*frame[3]), 2)
                if owner['id'] not in owners:
                    marks.append((normalized_quad(owner['quad_px'], doc['canvas']), '#0063c7', ''))
                    owners.add(owner['id'])
            if row.get('frame_px'):
                marks.append((normalized_quad(row['frame_quad_px'], doc['canvas']), '#c04a00', str(index)))
        # Keep the raw render alongside declarations; neither is a semantic verdict.
        ownership = Image.new('RGB', (1920, 620), '#eeeeee'); draw = ImageDraw.Draw(ownership)
        draw.text((8, 8), 'Declared ownership: blue = owner region; orange = text frame; numbers map to overlay_index in report', fill='black')
        ownership.paste(tile(paths[1], size=(960, 570)), (0, 35))
        ownership.paste(tile(paths[1], size=(960, 570), marks=marks), (960, 35))
        target = output/f'page-{i+1:03d}-ownership.jpg'; ownership.save(target, quality=95)
        outputs.append({'kind': 'ownership', 'page': pid, 'objects': [r['id'] for r in page_rows],
                        'path': str(target), 'sha256': sha(target)})
        flagged = [r for r in rows if r['page'] == pid and (r['findings'] or (pid, r['id']) in selected)]
        for start in range(0, len(flagged), 4):
            batch = flagged[start:start+4]
            image = Image.new('RGB', (1920, 286*len(batch)), '#eeeeee'); draw = ImageDraw.Draw(image)
            for j, row in enumerate(batch):
                # The crop is shared normalized context, not a registration claim.
                box = row['_review_rect']
                px, py = max(.012, (box[2]-box[0])*.25), max(.012, (box[3]-box[1])*.35)
                box = [max(0, box[0]-px), max(0, box[1]-py), min(1, box[2]+px), min(1, box[3]+py)]
                for col, path in enumerate(paths):
                    marks = []
                    if row['owner']:
                        q = row['owner'].get('source_quad_px') if col == 0 else row['owner']['quad_px']
                        if q: marks.append((normalized_quad(q, doc['canvas']), '#0063c7', ''))
                    if col == 2 and row.get('frame_px'):
                        marks.append((normalized_quad(row['frame_quad_px'], doc['canvas']), '#c04a00', ''))
                    image.paste(tile(path, box, (640, 250), marks=marks), (col*640, j*286+30))
                label = row['id']+' | '+(','.join(row['findings']) or 'requested measurement')
                # Stable index identifies non-Latin IDs without relying on local UI font coverage.
                safe = label.encode('ascii', 'backslashreplace').decode()[:190]
                draw.text((8, j*286+8), safe, fill='black')
            target = output/f'page-{i+1:03d}-details-{start//4+1:03d}.jpg'; image.save(target, quality=94)
            outputs.append({'kind': 'details', 'page': pid, 'objects': [r['id'] for r in batch],
                            'path': str(target), 'sha256': sha(target)})
    return outputs


def sweep_views(doc, render, page_ids, output):
    """Readable, unannotated views independent of detector findings or text boxes.

    Separate images preserve legibility in viewers that downsize a wide triptych.
    Viewports overlap and cover the entire page; they do not infer visual owners.
    """
    outputs = []
    viewports = [(0., 0., .55, .55), (.45, 0., 1., .55),
                 (0., .45, .55, 1.), (.45, .45, 1., 1.)]
    for i, page in enumerate(doc['pages']):
        if page['id'] not in page_ids:
            continue
        paths = [('editable', render['editable']['images'][i]),
                 ('blank', render['blank']['images'][i])]
        if page.get('design'):
            paths.append(('source', page['design']['path']))
        # Absence of a labeled source stays absent, rather than relabeling blank art.
        for role, path in paths:
            with Image.open(path) as opened:
                source = opened.convert('RGB')
            for index, rect in enumerate(viewports, 1):
                bounds = pixel_rect(rect, source.size, padding=0)
                crop = source.crop(bounds)
                crop_size = crop.size
                crop.thumbnail((1280, 1280), Image.Resampling.LANCZOS)
                target = output/f'page-{i+1:03d}-sweep-{index:02d}-{role}.png'
                crop.save(target)
                outputs.append({'kind': 'sweep', 'page': page['id'], 'role': role,
                                'viewport': index, 'viewport_normalized': list(rect),
                                'input_path': str(path), 'input_sha256': sha(path),
                                'input_size': list(source.size), 'crop_bounds_px': bounds,
                                'crop_size': list(crop_size), 'output_size': list(crop.size),
                                'path': str(target), 'sha256': sha(target),
                                'visual_quality_verified': False})
    return outputs


def revision_evidence(doc, render, old_doc, old_render, output):
    """Show changed objects with their parent and nearby artwork; never claim closure."""
    require(doc['canvas'] == old_doc['canvas'], 'Before/after canvas differs')
    require([p['id'] for p in doc['pages']] == [p['id'] for p in old_doc['pages']],
            'Before/after page order differs')
    changed, outputs = [], []
    c = doc['canvas']
    for i, (page, before) in enumerate(zip(doc['pages'], old_doc['pages'])):
        old_items = {t['id']: t for t in before['texts']}
        new_items = {t['id']: t for t in page['texts']}
        rows = []
        art_changed = page['background']['sha256'] != before['background']['sha256']
        for tid in sorted(old_items.keys() | new_items.keys()):
            a, b = old_items.get(tid), new_items.get(tid)
            if a and b and a == b and effective_style(old_doc, a) == effective_style(doc, b) and not art_changed and before.get('regions') == page.get('regions'):
                continue
            boxes = []
            for item, source in [(a, before), (b, page)]:
                if item:
                    boxes.append(xyxy(item['box'], c))
                    region = source.get('regions', {}).get(item.get('region'))
                    if region: boxes.append(xyxy(region, c))
            context = union(boxes)
            context = [max(0, context[0]-.025), max(0, context[1]-.025),
                       min(1, context[2]+.025), min(1, context[3]+.025)]
            entry = {'page': page['id'], 'id': tid, 'before': a, 'after': b,
                     'review_context_normalized': context, 'artwork_changed': art_changed,
                     'owner_status': 'declared_not_semantically_verified' if b and b.get('region') else 'not_declared',
                     'visual_quality_verified': False}
            changed.append(entry); rows.append(entry)
        for offset in range(0, len(rows), 4):
            batch = rows[offset:offset+4]
            sheet = Image.new('RGB', (1800, len(batch)*340), '#eeeeee'); draw = ImageDraw.Draw(sheet)
            paths = [page.get('design', page['background'])['path'], old_render['editable']['images'][i], render['editable']['images'][i]]
            for j, item in enumerate(batch):
                label = (item['id'] + ' | owner: ' + item['owner_status']).encode('ascii', 'backslashreplace').decode()
                draw.text((8, j*340+5), label, fill='black')
                for col, (path, title) in enumerate(zip(paths, ['Source reference', 'Before: actual PPTX', 'After: actual PPTX'])):
                    draw.text((col*600+8, j*340+22), title, fill='black')
                    sheet.paste(tile(path, item['review_context_normalized'], (600, 290)), (col*600, j*340+45))
            target = output/f'page-{i+1:03d}-revision-{offset//4+1:03d}.jpg'; sheet.save(target, quality=94)
            outputs.append({'kind': 'revision', 'page': page['id'], 'objects': [x['id'] for x in batch],
                            'path': str(target), 'sha256': sha(target)})
    return {'changed_objects': changed, 'changed_count': len(changed),
            'note': 'Source/actual before/actual after share normalized view coordinates; not pixel registration or visual acceptance.'}, outputs


def run(cycle_path, output, regions=None, objects=None, previous=None, previous_cycle=None,
        sweep_pages=None):
    doc, renders, bindings = bound_cycle(cycle_path)
    requested_sweep = set(sweep_pages or [])
    require(requested_sweep <= {p['id'] for p in doc['pages']}, 'Sweep names an unknown page ID')
    config = external_regions(regions, doc)
    selected = selections(doc, objects)
    if regions:
        bindings[str(Path(regions).resolve())] = sha(regions)
    previous = previous_inspection(previous, previous_cycle)
    if previous:
        bindings[str(Path(previous).resolve())] = sha(previous)
    old = None
    if previous_cycle:
        old = bound_cycle(previous_cycle)
        bindings.update(old[2])
    identity = {'version': VERSION, 'engine_sha256': sha(__file__),
                'geometry_engine_sha256': sha(Path(__file__).with_name('label_geometry.py')),
                'mapping_engine_sha256': sha(Path(__file__).with_name('audit_text_regions.py')),
                'bindings': bindings, 'objects': sorted(selected), 'explicit_selection': objects is not None,
                'sweep_pages': sorted(requested_sweep),
                'previous_cycle': str(Path(previous_cycle).resolve()) if previous_cycle else None}
    fingerprint = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    output = Path(output).resolve()
    if output.exists():
        report = read_json(output/'report.json')
        require(report.get('fingerprint') == fingerprint, 'Inputs changed; use a fresh inspection output')
        for artifact in report.get('artifacts', []):
            require(sha(artifact['path']) == artifact['sha256'], 'Inspection evidence changed')
        return report
    native = Presentation(renders['editable']['pptx'])
    blank = Presentation(renders['blank']['pptx'])
    full_pdf = fitz.open(renders['editable']['pdf']); blank_pdf = fitz.open(renders['blank']['pdf'])
    require(len(native.slides) == len(blank.slides) == len(full_pdf) == len(blank_pdf) == len(doc['pages']),
            'PPTX/PDF page count differs')
    rows, findings, coverage, raster = [], [], [], []
    c = doc['canvas']; width, height = c['width_px'], c['height_px']
    for i, page in enumerate(doc['pages']):
        pid = page['id']; rules = config.get(pid, {})
        declared = dict(page.get('regions', {})); declared.update(rules.get('regions', {}))
        owner_geometry = {key: label_region(value, c) for key, value in declared.items()}
        owner_boxes = {key: xyxy(value['box'], c) for key, value in owner_geometry.items()}
        forbidden = rules.get('forbidden', {})
        require(isinstance(forbidden, dict), 'forbidden must map artwork IDs to pixel boxes')
        forbidden_boxes = {key: xyxy(value, c) for key, value in forbidden.items()}
        overrides = rules.get('objects', {})
        require(isinstance(overrides, dict), 'Region objects must map text IDs to checks')
        require(set(overrides) <= {t['id'] for t in page['texts']}, 'Region rule names an unknown object')
        for check in overrides.values():
            require(isinstance(check, dict) and
                    set(check) <= {'region', 'max_lines', 'allow_artwork_regions'}, 'Unknown object check')
            if check.get('max_lines') is not None:
                require(type(check['max_lines']) is int and check['max_lines'] > 0, 'max_lines must be positive integer')
            allowed = check.get('allow_artwork_regions', [])
            require(isinstance(allowed, list) and all(key in forbidden_boxes for key in allowed),
                    'allow_artwork_regions names unknown artwork')
        tolerance = rules.get('tolerance_pt', 1.0)
        require(type(tolerance) in (int, float) and math.isfinite(tolerance) and tolerance >= 0,
                'tolerance_pt must be finite and nonnegative')
        tol = (tolerance/full_pdf[i].rect.width, tolerance/full_pdf[i].rect.height)
        policy = rules.get('policy', {})
        require(isinstance(policy, dict) and set(policy) <= {'min_size_pt'}, 'Unknown explicit policy field')
        min_size = policy.get('min_size_pt')
        if min_size is not None:
            require(type(min_size) in (int, float) and math.isfinite(min_size) and min_size > 0,
                    'min_size_pt must be positive')
        shapes = [s for s in native.slides[i].shapes if s.has_text_frame and s.text.strip()]
        stream, chars = pdf_characters(full_pdf[i])
        native_by_name = {}
        mappings = {}
        for shape in shapes:
            native_by_name.setdefault(shape.name, []).append(shape)
            box = [shape.left/native.slide_width*width, shape.top/native.slide_height*height,
                   shape.width/native.slide_width*width, shape.height/native.slide_height*height]
            quad = frame_quad(box, shape.rotation)
            actual = xyxy(bounds(quad), c)
            mapped, error = locate(shape.text, stream, chars, actual)
            mappings[shape.shape_id] = {'frame': actual, 'box': box, 'quad': quad, 'mapped': mapped, 'error': error}
        with Image.open(renders['editable']['images'][i]) as a, Image.open(renders['blank']['images'][i]) as b:
            diff = ImageChops.difference(a.convert('RGB'), b.convert('RGB'))
            bands = diff.split(); mask = ImageChops.lighter(ImageChops.lighter(bands[0], bands[1]), bands[2])
            mask = mask.point(lambda p: 255 if p > 16 else 0)
        inks, ink_summary = attribute_ink(mask, mappings)
        if ink_summary['unassigned_pixels']:
            findings.append({'page': pid, 'object': '__page__', 'code': 'UNATTRIBUTED_TEXT_INK',
                             'severity': 'unknown', 'details': ink_summary})
        expected_names = {PREFIX+t['id'] for t in page['texts']}
        unexpected = [s.name for s in shapes if s.name not in expected_names]
        blank_text = [s.name for s in blank.slides[i].shapes if s.has_text_frame and s.text.strip()]
        for code, names in [('UNEXPECTED_NATIVE_TEXT', unexpected), ('BLANK_HAS_NATIVE_TEXT', blank_text)]:
            if names:
                findings.append({'page': pid, 'object': '__page__', 'code': code, 'severity': 'error', 'details': names})
        for item in page['texts']:
            tid = item['id']
            if (pid, tid) not in selected:
                continue
            row = {'page': pid, 'id': tid, 'native_name': PREFIX+tid, 'canonical_text': page['copy'][item.get('copy_id', tid)],
                   'findings': [], 'mapping': 'unresolved', 'ink_attribution': {'state': 'unknown'},
                   'owner': None, 'font_runs': []}
            def issue(code, details, severity='review'):
                findings.append({'page': pid, 'object': tid, 'code': code, 'severity': severity, 'details': details})
                row['findings'].append(code)
            matches = native_by_name.get(PREFIX+tid, [])
            base_rect = xyxy(item['box'], c); review_rects = [base_rect]
            check = overrides.get(tid, {})
            owner_name = check.get('region', item.get('region'))
            if owner_name is not None:
                require(owner_name in owner_boxes, 'Unknown declared owner region')
                owner = owner_boxes[owner_name]
                geometry = owner_geometry[owner_name]
                row['owner'] = {'id': owner_name, 'box_px': geometry['box'], 'quad_px': geometry['quad'],
                                'source_quad_px': geometry.get('source_quad'), 'scope': geometry['scope'],
                                'baseline_deg': geometry['baseline_deg'], 'plane': geometry['plane'],
                                'basis': geometry['basis']}
                review_rects.append(owner)
                if geometry.get('source_quad'):
                    review_rects.append(xyxy(bounds(geometry['source_quad']), c))
                if geometry['scope'] != 'label':
                    issue('LABEL_OWNER_UNVERIFIED', 'Owner is a panel or legacy rectangle; local label boundary/angle not measured.', 'unknown')
                if geometry['scope'] == 'label' and geometry['plane']['kind'] == 'perspective_or_shear':
                    issue('LABEL_PLANE_NON_RIGID', geometry['plane'], 'unknown')
            else:
                owner = None
            if len(matches) != 1:
                issue('NATIVE_OBJECT_UNRESOLVED', {'matches': len(matches)}, 'error')
            else:
                shape = matches[0]
                actual = mappings[shape.shape_id]['frame']
                row.update(text=shape.text, frame_px=mappings[shape.shape_id]['box'],
                           frame_quad_px=mappings[shape.shape_id]['quad'], rotation_deg=shape.rotation,
                           font_runs=font_runs(shape))
                if abs(angle_delta(shape.rotation, item.get('rotation', 0))) > .001:
                    issue('DECLARED_ROTATION_MISMATCH', {'declared': item.get('rotation', 0), 'actual': shape.rotation}, 'error')
                if owner is not None and geometry['scope'] == 'label':
                    delta = angle_delta(shape.rotation, geometry['baseline_deg'])
                    row['baseline_error_deg'] = delta
                    if abs(delta) > geometry['angle_tolerance_deg']:
                        issue('LABEL_BASELINE_MISMATCH', {'actual_deg':shape.rotation, 'expected_deg':geometry['baseline_deg'],
                                                       'error_deg':delta,'tolerance_deg':geometry['angle_tolerance_deg']})
                review_rects.append(actual)
                if norm(shape.text) != norm(row['canonical_text']):
                    issue('COPY_MISMATCH', {'expected': row['canonical_text'], 'actual': shape.text}, 'error')
                style = effective_style(doc, item)
                if any(r['font'] != style['font'] or r['size_pt'] is None or
                       abs(r['size_pt']-style['size_pt']) > .011 for r in row['font_runs']):
                    issue('DECLARED_FONT_MISMATCH', {'expected_font': style['font'], 'expected_size_pt': style['size_pt']}, 'error')
                if min_size is not None and any(r['size_pt'] is not None and r['size_pt'] < min_size for r in row['font_runs']):
                    issue('EXPLICIT_MINIMUM_FONT_SIZE', {'minimum_pt': min_size}, 'error')
                mapped, error = mappings[shape.shape_id]['mapped'], mappings[shape.shape_id]['error']
                if error:
                    issue('PDF_MAPPING_UNRESOLVED', error, 'unknown')
                if mapped:
                    row['mapping'] = 'located'; measured = mapped['rect']; review_rects.append(measured)
                    directions = {tuple(ch['direction']) for ch in mapped['characters'] if ch.get('direction')}
                    row['rendered_baselines_deg'] = sorted(math.degrees(math.atan2(d[1],d[0])) for d in directions)
                    if any(abs(angle_delta(a, shape.rotation)) > 1 for a in row['rendered_baselines_deg']):
                        issue('RENDERED_BASELINE_MISMATCH', {'native_rotation':shape.rotation,
                                                          'pdf_baselines':row['rendered_baselines_deg']})
                    row['pdf_font_bounds_px'] = pxbox(measured, width, height)
                    row['font_metric_frame_clearance_px'] = clearances(measured, actual, width, height)
                    row['rendered_lines'] = (len({ch['line'] for ch in mapped['characters']}) if shape.rotation
                                             else len(rendered_lines(mapped['characters'])))
                    if owner is not None:
                        row['font_metric_owner_clearance_px'] = clearances(measured, owner, width, height)
                    if check.get('max_lines') is not None:
                        require(type(check['max_lines']) is int and check['max_lines'] > 0, 'max_lines must be positive integer')
                        if row['rendered_lines'] > check['max_lines']:
                            issue('EXPLICIT_LINE_LIMIT', {'maximum': check['max_lines'], 'actual': row['rendered_lines']})
                ink = inks[shape.shape_id]
                row['ink_attribution'] = {key: value for key, value in ink.items() if not key.startswith('_') and key != 'rect'}
                if ink['state'] != 'attributed':
                    issue('INK_ATTRIBUTION_UNRESOLVED', ink['reasons'], 'unknown')
                else:
                    measured = ink['rect']; review_rects.append(measured)
                    row['ink_bounds_px'] = pxbox(measured, width, height)
                    row['frame_clearance_px'] = clearances(measured, actual, width, height)
                    outside = outside_ink(ink['_components'], mappings[shape.shape_id]['quad'], mask.size, c,
                                          tolerance*width/(c['width_in']*72))
                    if outside:
                        issue('INK_OUTSIDE_FRAME_CANDIDATE', {'tolerance_pt': tolerance, 'outside_pixels': outside})
                    if owner is not None:
                        row['owner_clearance_px'] = clearances(measured, owner, width, height)
                        outside = outside_ink(ink['_components'], geometry['quad'], mask.size, c,
                                              tolerance*width/(c['width_in']*72))
                        if outside:
                            issue('INK_OUTSIDE_OWNER_CANDIDATE', {'owner': owner_name, 'tolerance_pt': tolerance,
                                                                'outside_pixels': outside})
                    for obstacle_id, obstacle in forbidden_boxes.items():
                        if obstacle_id in check.get('allow_artwork_regions', []):
                            continue
                        if any(component_touches(part, pixel_rect(obstacle, mask.size, 0)) for part in ink['_components']):
                            issue('INK_OVER_ARTWORK_CANDIDATE', {'artwork_region': obstacle_id})
            row['_review_rect'] = union(review_rects)
            rows.append(row)
        covered = [t['id'] for t in page['texts'] if (pid, t['id']) in selected]
        coverage.append({'page': pid, 'expected_objects': len(page['texts']), 'measured_objects': len(covered),
                         'unmeasured_objects': [t['id'] for t in page['texts'] if t['id'] not in covered],
                         'without_declared_owner': [r['id'] for r in rows if r['page'] == pid and r['owner'] is None],
                         'unresolved_mapping': [r['id'] for r in rows if r['page'] == pid and r['mapping'] != 'located'],
                         'unresolved_ink_attribution': [r['id'] for r in rows if r['page'] == pid and r['ink_attribution']['state'] != 'attributed'],
                         'unsupported_shapes': [s.name for s in native.slides[i].shapes if s.shape_type == 6 or s.has_table],
                         'unexpected_native_text': unexpected, 'blank_native_text': blank_text})
        delta_box = mask.getbbox()
        raster.append({'page': pid, 'pixel_difference_bounds': list(delta_box) if delta_box else None,
                       'changed_pixels': mask.histogram()[255], 'threshold_255': 16,
                       'attribution': ink_summary,
                       'meaning': 'Actual full-minus-derived-blank ink. Only uniquely attributable components drive spatial candidates.'})
    full_pdf.close(); blank_pdf.close()
    output.mkdir(parents=True, exist_ok=False)
    artifacts = panels(doc, renders, rows, selected if objects is not None else set(), output)
    artifacts.extend(sweep_views(doc, renders, requested_sweep, output))
    revision = None
    if old:
        revision, extra = revision_evidence(doc, renders, old[0], old[1], output)
        artifacts.extend(extra)
    for row in rows:
        row.pop('_review_rect')
    report = {'schema_version': 1, 'helper': 'native_inspect', 'version': VERSION,
              'fingerprint': fingerprint, 'bindings': bindings, 'page_order': [p['id'] for p in doc['pages']],
              'measurement_policy': config, 'canvas': doc['canvas'],
              'sweep': {'requested_pages': sorted(requested_sweep),
                        'meaning': 'Generated comparison views, not inspected-region coverage or visual approval.'},
              'objects': rows, 'findings': findings, 'coverage': coverage, 'raster_difference': raster,
              'cycle_record': str(Path(cycle_path).resolve()), 'revision_comparison': revision,
              'ownership_summary': {
                  'without_declared_owner': [{'page': r['page'], 'id': r['id']} for r in rows if r['owner'] is None],
                  'declared_owner_count': sum(r['owner'] is not None for r in rows),
                  'semantic_ownership_verified': False,
                  'note': 'A declared panel is not proof that a caption belongs there. Inspect the intended card/cell/arrow and its neighbors.'},
              'artifacts': artifacts, 'visual_quality_verified': False, 'visual_review_required': True,
              'limitations': ['PDF font bounds are informational; spatial candidates use actual uniquely attributable full-minus-blank ink.',
                              'Overlapping, missing, low-contrast or unassigned ink stays unresolved and requires visual inspection.',
                              'Ownership is checked only against caller-declared regions; the helper cannot establish their semantic correctness.',
                              'Canonical strings come from the supplied page inventory and still need source reconciliation.',
                              'Source/full/blank crops share normalized coordinates for viewing, not a claim of perfect artwork registration.',
                              'Raster labels, contrast, meaning, artwork occlusion and missing source occurrences need actual pixel inspection.',
                              'No minimum font size or pixel-similarity objective is imposed unless explicitly provided as a policy.']}
    report['comparison'] = compare_previous(previous, report)
    if previous_cycle and not previous:
        report['comparison'] = {'state': 'previous_inspection_unavailable',
                                'previous_cycle': str(Path(previous_cycle).resolve()),
                                'note': 'Before/after pictures exist; no prior measurement report was available to compare findings.'}
    atomic_json(output/'report.json', report)
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cycle', required=True); parser.add_argument('--out', required=True)
    parser.add_argument('--regions'); parser.add_argument('--objects', nargs='+'); parser.add_argument('--previous')
    parser.add_argument('--previous-cycle', help='Bound prior PPTX renders for changed-object before/after panels')
    parser.add_argument('--sweep-pages', nargs='+', metavar='PAGE_ID',
                        help='Separate overlapping source/full/blank views, including unflagged regions; uses existing renders')
    args = parser.parse_args(argv)
    try:
        report = run(args.cycle, args.out, args.regions, args.objects, args.previous,
                     args.previous_cycle, args.sweep_pages)
        from native_review import summarize
        print(json.dumps({'state': 'measured_review_required', 'report': str(Path(args.out).resolve()/'report.json'),
                          'objects': len(report['objects']), 'findings': dict(Counter(f['code'] for f in report['findings'])),
                          'objects_without_declared_owner': len(report['ownership_summary']['without_declared_owner']),
                          'changed_objects': (report.get('revision_comparison') or {}).get('changed_count'),
                          'evidence': [{k:a[k] for k in ['kind', 'page', 'path']} for a in report['artifacts']],
                          'review': summarize(report), 'visual_quality_verified': False}, ensure_ascii=False))
        return 0
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(json.dumps({'state': 'error', 'error': str(error)}, ensure_ascii=False))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
