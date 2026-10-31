#!/usr/bin/env python3
"""Reusable data-to-PPTX builder. Never chooses page copy, regions or final layout."""
import argparse
import copy
import hashlib
import importlib.metadata
import importlib.util
import json
import math
import os
import shutil
import sys
from pathlib import Path
import tempfile
import unicodedata

from PIL import Image
from pptx import Presentation
from pptx.dml.color import RGBColor
from pptx.enum.text import MSO_ANCHOR, MSO_AUTO_SIZE, PP_ALIGN
from pptx.oxml.xmlchemy import OxmlElement
from pptx.util import Inches, Pt

from paired_deck import check as check_pair, derive_blank
from render_cache import canonicalize_pptx
from label_geometry import region as label_region, frame_quad, contains, rotation

VERSION = '1.2'
PREFIX = 'TEXT__'
ALIGN = {'left': PP_ALIGN.LEFT, 'center': PP_ALIGN.CENTER, 'right': PP_ALIGN.RIGHT}
ANCHOR = {'top': MSO_ANCHOR.TOP, 'middle': MSO_ANCHOR.MIDDLE, 'bottom': MSO_ANCHOR.BOTTOM}
STYLE_KEYS = {'font', 'size_pt', 'color', 'bold', 'italic', 'align', 'anchor',
              'wrap', 'margins_px', 'line_spacing', 'space_before_pt', 'space_after_pt'}
TEXT_KEYS = {'id', 'copy_id', 'text', 'box', 'style', 'region', 'format', 'rotation'}


def require(ok, message):
    if not ok:
        raise ValueError(message)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def read_json(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.json-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w', encoding='utf-8') as stream:
            json.dump(value, stream, ensure_ascii=False, indent=2, allow_nan=False)
            stream.write('\n')
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def normalized(text):
    return ''.join(c for c in unicodedata.normalize('NFC', text) if not c.isspace())


def number(value, field, positive=False):
    require(type(value) in (int, float) and math.isfinite(value), f'{field}: finite number required')
    require(value > 0 if positive else value >= 0, f'{field}: invalid sign')
    return value


def rect(value, field):
    require(isinstance(value, list) and len(value) == 4, f'{field}: use [x,y,width,height]')
    for i, v in enumerate(value):
        number(v, field, positive=i > 1)
    return value


def within(inner, outer, tolerance=0.01):
    x, y, w, h = inner
    a, b, c, d = outer
    return x >= a-tolerance and y >= b-tolerance and x+w <= a+c+tolerance and y+h <= b+d+tolerance


def asset(value, base):
    value = {'path': value} if isinstance(value, str) else dict(value)
    path = (base / value['path']).resolve(strict=True)
    require(path.is_file(), f'Not a regular asset: {path}')
    digest = sha(path)
    require(not value.get('sha256') or value['sha256'] == digest, f'Asset hash mismatch: {path}')
    return {'path': str(path), 'sha256': digest}


def load_spec(path, validate_input=True, page_id=None):
    """Resolve independent page JSONs and assets; callers never rely on cwd."""
    path = Path(path).resolve(strict=True)
    doc = read_json(path)
    require(isinstance(doc, dict) and doc.get('schema_version') == 1, 'Expected schema_version 1')
    require(isinstance(doc.get('pages'), list) and doc['pages'], 'Nonempty pages required')
    pages = []
    dependencies = {str(path): sha(path)}
    for item in doc.get('pages', []):
        if isinstance(item, dict) and set(item) == {'path'}:
            page_path = (path.parent / item['path']).resolve(strict=True)
            page = read_json(page_path)
            base = page_path.parent
        else:
            page, base = copy.deepcopy(item), path.parent
        require(isinstance(page, dict), 'Page must be an object or a page-file reference')
        if page_id is not None and page.get('id') != page_id:
            continue
        if isinstance(item, dict) and set(item) == {'path'}:
            dependencies[str(page_path)] = sha(page_path)
        for key in ['background', 'design']:
            if key in page:
                page[key] = asset(page[key], base)
                dependencies[page[key]['path']] = page[key]['sha256']
        pages.append(page)
    doc['pages'] = pages
    if page_id is not None:
        require(len(pages) == 1, 'Requested page missing or duplicated')
    if validate_input:
        validate(doc)
    return doc, dependencies


def effective_style(doc, text):
    require(text.get('style') in doc['styles'], f"Unknown style for {text.get('id')}")
    base = doc['styles'][text['style']]
    override = text.get('format', {})
    require(isinstance(override, dict), 'format must be an object')
    require(not base.get('locked') or not override, f"Style {text['style']} is locked")
    style = {k: v for k, v in base.items() if k != 'locked'} | override
    require(set(style) <= STYLE_KEYS, f'Unsupported style fields: {set(style)-STYLE_KEYS}')
    require(isinstance(style.get('font'), str) and bool(style['font']), 'Explicit font required')
    number(style.get('size_pt'), 'size_pt', True)
    color = style.get('color', '000000')
    require(isinstance(color, str), 'Color must be a six-digit RGB string')
    color = color.removeprefix('#')
    require(len(color) == 6 and all(c in '0123456789abcdefABCDEF' for c in color), 'Use six-digit RGB color')
    style['color'] = color
    require(style.get('align', 'left') in ALIGN, 'Invalid horizontal alignment')
    require(style.get('anchor', 'top') in ANCHOR, 'Invalid vertical anchor')
    for key in ['bold', 'italic', 'wrap']:
        require(key not in style or type(style[key]) is bool, f'{key} must be boolean')
    margins = style.get('margins_px', [0, 0, 0, 0])
    require(isinstance(margins, list) and len(margins) == 4, 'margins_px: [left,top,right,bottom]')
    for v in margins:
        number(v, 'margin')
    for key in ['space_before_pt', 'space_after_pt']:
        number(style.get(key, 0), key)
    number(style.get('line_spacing', 1.0), 'line_spacing', True)
    return style


def validate(doc):
    require(doc.get('schema_version') == 1, 'Expected schema_version 1')
    canvas = doc['canvas']
    for key in ['width_px', 'height_px', 'width_in', 'height_in']:
        number(canvas[key], key, True)
    require(abs((canvas['width_px']/canvas['height_px']) /
                (canvas['width_in']/canvas['height_in'])-1) < .001,
            'Canvas pixel and physical aspect ratios differ')
    require(isinstance(doc.get('styles'), dict) and doc['styles'], 'Explicit shared styles required')
    require(isinstance(doc.get('pages'), list) and doc['pages'], 'Nonempty pages required')
    ids = set()
    page_box = [0, 0, canvas['width_px'], canvas['height_px']]
    for page in doc['pages']:
        pid = page.get('id')
        require(isinstance(pid, str) and pid and pid not in ids, 'Page IDs must be nonempty and unique')
        ids.add(pid)
        require('background' in page, f'{pid}: background required')
        for key in ['background', 'design']:
            if key in page:
                with Image.open(page[key]['path']) as im:
                    require(abs((im.width/im.height)/(canvas['width_px']/canvas['height_px'])-1) < .001,
                            f'{pid}: {key} aspect ratio differs; do not stretch artwork')
        copies = page.get('copy')
        require(isinstance(copies, dict) and copies and all(isinstance(k, str) and k and
                isinstance(v, str) and normalized(v) for k, v in copies.items()),
                f'{pid}: copy must map occurrence IDs to nonempty canonical strings')
        regions = page.get('regions', {})
        require(isinstance(regions, dict), f'{pid}: regions must be an object')
        for rid, region in regions.items():
            label_region(region, canvas)
        seen, used = set(), []
        require(isinstance(page.get('texts'), list), f'{pid}: texts must be a list')
        for text in page['texts']:
            require(set(text) <= TEXT_KEYS, f"{pid}: unsupported text fields: {set(text)-TEXT_KEYS}")
            tid = text.get('id')
            require(isinstance(tid, str) and tid and tid not in seen, f'{pid}: duplicate/empty text ID')
            seen.add(tid)
            cid = text.get('copy_id', tid)
            require(cid in copies and cid not in used, f'{pid}/{tid}: missing or reused copy occurrence')
            used.append(cid)
            require(isinstance(text.get('text'), str) and normalized(text['text']) == normalized(copies[cid]),
                    f'{pid}/{tid}: text differs from canonical copy (only whitespace may vary)')
            box = rect(text.get('box'), f'{pid}/{tid}')
            angle = rotation(text.get('rotation', 0))
            corners = frame_quad(box, angle)
            require(contains(frame_quad(page_box), corners), f'{pid}/{tid}: rotated text frame outside canvas')
            if text.get('region') is not None:
                require(text['region'] in regions, f'{pid}/{tid}: unknown owner region')
                owner = label_region(regions[text['region']], canvas)
                require(contains(owner['quad'], corners), f'{pid}/{tid}: frame outside owner region')
            style = effective_style(doc, text)
            left, top, right, bottom = style.get('margins_px', [0, 0, 0, 0])
            require(left+right < box[2] and top+bottom < box[3], f'{pid}/{tid}: margins consume text box')
        require(set(used) == set(copies), f'{pid}: canonical occurrences omitted: {set(copies)-set(used)}')
    return doc


def build_slide(presentation, doc, page):
    """The same function produces page proofs and assembled slides."""
    c = doc['canvas']
    require(abs(presentation.slide_width / Inches(c['width_in'])-1) < 1e-6 and
            abs(presentation.slide_height / Inches(c['height_in'])-1) < 1e-6, 'Assembly canvas differs')
    slide = presentation.slides.add_slide(presentation.slide_layouts[6])
    picture = slide.shapes.add_picture(page['background']['path'], 0, 0,
                                      width=presentation.slide_width, height=presentation.slide_height)
    picture.name = 'ART__background'
    sx, sy = presentation.slide_width/c['width_px'], presentation.slide_height/c['height_px']
    for item in page['texts']:
        x, y, w, h = item['box']
        box = slide.shapes.add_textbox(round(x*sx), round(y*sy), round(w*sx), round(h*sy))
        box.rotation = rotation(item.get('rotation', 0))
        box.name = PREFIX + item['id']
        style = effective_style(doc, item)
        tf = box.text_frame
        tf.auto_size = MSO_AUTO_SIZE.NONE
        tf.word_wrap = style.get('wrap', True)
        tf.vertical_anchor = ANCHOR[style.get('anchor', 'top')]
        left, top, right, bottom = style.get('margins_px', [0, 0, 0, 0])
        tf.margin_left, tf.margin_right = round(left*sx), round(right*sx)
        tf.margin_top, tf.margin_bottom = round(top*sy), round(bottom*sy)
        for i, line in enumerate(item['text'].replace('\r\n', '\n').split('\n')):
            para = tf.paragraphs[0] if i == 0 else tf.add_paragraph()
            para.alignment = ALIGN[style.get('align', 'left')]
            para.line_spacing = style.get('line_spacing', 1.0)
            para.space_before = Pt(style.get('space_before_pt', 0))
            para.space_after = Pt(style.get('space_after_pt', 0))
            run = para.add_run()
            run.text = line
            run.font.name = style['font']
            run.font.size = Pt(style['size_pt'])
            run.font.bold, run.font.italic = style.get('bold', False), style.get('italic', False)
            run.font.color.rgb = RGBColor.from_string(style['color'])
            for name in ['ea', 'cs']:
                node = OxmlElement('a:' + name)
                node.set('typeface', style['font'])
                run._r.get_or_add_rPr().append(node)
    if page.get('notes'):
        slide.notes_slide.notes_text_frame.text = page['notes']
    return slide


def diagnostics(doc):
    overlaps = []
    for page in doc['pages']:
        texts = page['texts']
        for i, a in enumerate(texts):
            x, y, w, h = a['box']
            for b in texts[i+1:]:
                u, v, z, t = b['box']
                if min(x+w, u+z) > max(x, u) and min(y+h, v+t) > max(y, v):
                    overlaps.append({'page': page['id'], 'objects': [a['id'], b['id']]})
    return {'box_overlap_candidates': overlaps,
            'unreviewed_ocr_pages': [p['id'] for p in doc['pages'] if p.get('source_status') == 'unreviewed_ocr'],
            'limitations': ['Frame bounds do not establish actual glyph fit, correct region ownership or legibility.',
                            'Canonical copy inventory must itself be checked against the supplied manuscript and design.',
                            'Overlap candidates need pixel review; blank backgrounds can retain raster text.'],
            'visual_quality_verified': False}


def build(spec, output, page_id=None):
    doc, deps = load_spec(spec, page_id=page_id)
    identity = {'version': VERSION, 'engine_sha256': sha(Path(__file__)),
                'geometry_engine_sha256': sha(Path(__file__).with_name('label_geometry.py')),
                'blank_engine_sha256': sha(Path(__file__).with_name('paired_deck.py')),
                'package_engine_sha256': sha(Path(__file__).with_name('render_cache.py')),
                'python_pptx': importlib.metadata.version('python-pptx'),
                'document': doc, 'dependencies': deps}
    fingerprint = hashlib.sha256(json.dumps(identity, sort_keys=True, ensure_ascii=False,
                                           allow_nan=False).encode()).hexdigest()
    output = Path(output).resolve()
    record = output / 'build.json'
    if output.exists():
        require(record.is_file(), 'Output exists without a complete build record; use a fresh attempt')
        report = read_json(record)
        require(report['fingerprint'] == fingerprint, 'Inputs changed; use a fresh output directory')
        for item in report['artifacts'].values():
            p = (output / item['path']).resolve()
            require(p.is_relative_to(output) and p.is_file() and sha(p) == item['sha256'], 'Cached output changed')
        return report
    output.mkdir(parents=True)
    prs = Presentation()
    prs.slide_width = Inches(doc['canvas']['width_in'])
    prs.slide_height = Inches(doc['canvas']['height_in'])
    for page in doc['pages']:
        build_slide(prs, doc, page)
    full, blank = output/'editable.pptx', output/'blank.pptx'
    prs.save(full)
    derive_blank(full, blank, PREFIX)
    # Preserve every package part byte; normalize only ZIP serialization so an
    # unchanged blank layer can reuse an exact-source render across attempts.
    canonicalize_pptx(full)
    canonicalize_pptx(blank)
    pair = check_pair(full, blank, PREFIX)
    require(pair['technicalPassed'], 'Full/blank layer verification failed')
    atomic_json(output/'resolved.json', doc)
    artifacts = {k: {'path': p.name, 'sha256': sha(p)} for k, p in
                 [('editable_pptx', full), ('blank_pptx', blank), ('resolved_spec', output/'resolved.json')]}
    if len(doc['pages']) == 1:
        # Mechanical adapter, not a solved page. Snapshot the exact engine so a
        # future skill update cannot change the accepted proof's implementation.
        for name in ['native_deck.py', 'paired_deck.py', 'render_cache.py']:
            target = output / name
            shutil.copyfile(Path(__file__).with_name(name), target)
            artifacts[name.removesuffix('.py')] = {'path': name, 'sha256': sha(target)}
        source = output/'page.py'
        source.write_text('''from pathlib import Path
import importlib.util
import sys

def build_slide(presentation, job):
    root = Path(__file__).resolve().parent
    sys.path.insert(0, str(root))
    try:
        spec = importlib.util.spec_from_file_location('_native_page_engine', root/'native_deck.py')
        engine = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(engine)
        doc, _ = engine.load_spec(root/'resolved.json')
        if len(doc['pages']) != 1:
            raise ValueError('Expected a one-page proof source')
        return engine.build_slide(presentation, doc, doc['pages'][0])
    finally:
        sys.path.pop(0)
''', encoding='utf-8')
        artifacts['source'] = {'path': source.name, 'sha256': sha(source)}
    report = {'schema_version': 1, 'engine_version': VERSION, 'fingerprint': fingerprint,
              'source_spec': str(Path(spec).resolve()), 'dependencies': deps,
              'page_order': [p['id'] for p in doc['pages']],
              'native_text_objects': sum(len(p['texts']) for p in doc['pages']),
              'artifacts': artifacts,
              'pair_checks': pair, 'diagnostics': diagnostics(doc), 'visual_quality_verified': False}
    atomic_json(record, report)
    return report


def assemble(jobs_path, output):
    """Rebuild all validated one-page sources in their declared order."""
    from validate_page_handoff import validate_many
    from compare_slide_proof import compare
    receipt = read_json(jobs_path)
    entries = receipt['jobs']
    require([e['page_id'] for e in entries] == receipt['page_order'], 'Receipt page order differs')
    checked = validate_many([(e['job'], e['result']) for e in entries], complete_deck=True)
    require([p['page_id'] for p in checked['pages']] == receipt['page_order'], 'Contract order differs')
    output = Path(output).resolve()
    require(not output.exists(), 'Use a fresh assembly output directory')
    first = Presentation(checked['pages'][0]['files']['editable_pptx'])
    prs = Presentation()
    prs.slide_width, prs.slide_height = first.slide_width, first.slide_height
    for i, (entry, page) in enumerate(zip(entries, checked['pages'])):
        source = Path(page['files']['source'])
        module_spec = importlib.util.spec_from_file_location(f'_accepted_page_{i}', source)
        module = importlib.util.module_from_spec(module_spec)
        module_spec.loader.exec_module(module)
        before = len(prs.slides)
        module.build_slide(prs, read_json(entry['job']))
        require(len(prs.slides) == before+1, 'Page source did not add exactly one slide')
    output.mkdir(parents=True)
    full, blank = output/'editable.pptx', output/'blank.pptx'
    prs.save(full)
    derive_blank(full, blank, PREFIX)
    canonicalize_pptx(full)
    canonicalize_pptx(blank)
    comparisons = [compare(page['files']['editable_pptx'], full, i+1)
                   for i, page in enumerate(checked['pages'])]
    require(all(v['native_assembly_unchanged'] for v in comparisons), 'Assembly changed a page proof')
    report = {'page_order': receipt['page_order'], 'proof_comparisons': comparisons,
              'editable_pptx': {'path': str(full), 'sha256': sha(full)},
              'blank_pptx': {'path': str(blank), 'sha256': sha(blank)},
              'visual_quality_verified': False, 'final_render_required': True}
    atomic_json(output/'assembly.json', report)
    return report


def patch_spec(spec, edits, output):
    """Apply all identified object changes atomically; never rewrite a builder."""
    output = Path(output).resolve()
    require(not output.exists(), 'Refusing to replace an existing spec')
    # Broken geometry must be repairable; only the final candidate must validate.
    doc, _ = load_spec(spec, validate_input=False)
    changes = read_json(edits)
    require(isinstance(changes, list) and changes, 'Edits must be a nonempty list')
    seen = set()
    for edit in changes:
        key = (edit['page'], edit['id'])
        require(key not in seen, 'Combine changes for one object in a single edit entry')
        seen.add(key)
        pages = [p for p in doc['pages'] if p['id'] == edit['page']]
        require(len(pages) == 1, f'Unknown page: {edit["page"]}')
        items = [t for t in pages[0]['texts'] if t['id'] == edit['id']]
        require(len(items) == 1, f'Unknown object: {edit["id"]}')
        update = edit['set']
        require(isinstance(update, dict) and set(update) <= TEXT_KEYS-{'id', 'copy_id'}, 'Invalid patch fields')
        items[0].update(update)
    validate(doc)
    original_path = Path(spec).resolve(strict=True)
    original = read_json(original_path)
    changed_pages = {edit['page'] for edit in changes}
    page_dir = output.parent/(output.stem+'-pages')
    writes = []
    for index, (old, page) in enumerate(zip(original['pages'], doc['pages'])):
        if isinstance(old, dict) and set(old) == {'path'}:
            if page['id'] not in changed_pages:
                doc['pages'][index] = {'path': os.path.relpath(
                    (original_path.parent/old['path']).resolve(strict=True), output.parent)}
                continue
            target = page_dir/f'page-{index+1}.json'
            doc['pages'][index] = {'path': os.path.relpath(target, output.parent)}
            writes.append((target, page))
        else:
            target = output
        for key in ['background', 'design']:
            if key in page:
                page[key]['path'] = os.path.relpath(page[key]['path'], target.parent)
    require(not writes or not page_dir.exists(), 'Patch page-output directory already exists')
    created = []
    try:
        for target, page in writes:
            atomic_json(target, page)
            created.append(target)
        atomic_json(output, doc)
    except OSError:
        for target in created:
            target.unlink(missing_ok=True)
        if writes and page_dir.exists() and not any(page_dir.iterdir()):
            page_dir.rmdir()
        raise
    return {'output': str(output), 'objects_updated': len(changes),
            'changed_page_files': [str(p) for p, _ in writes], 'visual_quality_verified': False}


def draft_ocr(plan, output):
    """Mechanical OCR starting points only; preserve all detections for review."""
    plan = Path(plan).resolve(strict=True)
    output = Path(output).resolve()
    require(not output.exists(), 'Refusing to replace an existing draft')
    cfg = read_json(plan)
    doc = {k: copy.deepcopy(cfg[k]) for k in ['schema_version', 'canvas', 'styles']}
    doc['pages'] = []
    style = cfg['ocr_style']
    require(style in doc['styles'], 'ocr_style must name an explicit shared style')
    c = doc['canvas']
    for item in cfg['pages']:
        path = (plan.parent / item['ocr']).resolve(strict=True)
        data = read_json(path)
        if isinstance(data, dict) and 'result' in data:
            require(data.get('status') == 'completed', 'OCR receipt is not completed')
            data = data['result']
        require(isinstance(data, dict) and isinstance(data.get('detections'), list), 'Expected OCR detections')
        design = asset(item['design'], plan.parent)
        require(data.get('image', {}).get('sha256') == design['sha256'], 'OCR does not bind to the supplied design')
        with Image.open(design['path']) as im:
            width, height = im.size
        background = asset(item['background'], plan.parent)
        page = {'id': item['id'], 'design': design, 'background': background,
                'source_status': 'unreviewed_ocr', 'ocr_source_sha256': sha(path),
                'copy': {}, 'texts': [], 'ocr_candidates': []}
        for i, detection in enumerate(data['detections']):
            text = detection.get('text')
            require(isinstance(text, str), 'OCR detection text must be a string')
            page['ocr_candidates'].append(copy.deepcopy(detection))
            if not normalized(text):
                continue
            values = [detection[k] for k in ['x', 'y', 'w', 'h']]
            require(all(type(v) in (int, float) and math.isfinite(v) for v in values), 'Non-finite OCR box')
            x, y, w, h = values
            # Small floating-point edge drift is clipped; no semantic filtering.
            x0, y0 = max(0, x), max(0, y)
            x1, y1 = min(width, x+w), min(height, y+h)
            require(x1 > x0 and y1 > y0, 'OCR detection lies outside the image')
            tid = f'ocr-{i+1}'
            page['copy'][tid] = text
            page['texts'].append({'id': tid, 'text': text, 'style': style,
                                  'box': [x0/width*c['width_px'], y0/height*c['height_px'],
                                          (x1-x0)/width*c['width_px'], (y1-y0)/height*c['height_px']]})
        doc['pages'].append(page)
    validate(doc)
    for page in doc['pages']:
        for key in ['background', 'design']:
            page[key]['path'] = os.path.relpath(page[key]['path'], output.parent)
    atomic_json(output, doc)
    return {'output': str(output), 'pages': len(doc['pages']), 'status': 'unreviewed_ocr',
            'warning': 'OCR text is not canonical. Reconcile every occurrence, font and placement before acceptance.'}


def doctor():
    optional = {}
    for package in ['PyMuPDF', 'numpy']:
        try:
            optional[package] = importlib.metadata.version(package)
        except importlib.metadata.PackageNotFoundError:
            optional[package] = None
    return {'engine_version': VERSION, 'engine_sha256': sha(Path(__file__)),
            'dependencies': {p: importlib.metadata.version(p) for p in ['python-pptx', 'Pillow', 'lxml']},
            'optional_dependencies': optional,
            'capabilities': {'native_inspect': optional['PyMuPDF'] is not None,
                             'paired_pdf_pixel_audit': all(optional.values())},
            'notes': ['Native inspection requires PyMuPDF; paired PDF pixel auditing also requires NumPy.',
                      'Service availability, installed fonts and rendering behavior are separate checks.'],
            'layout_or_font_calibration_performed': False, 'visual_quality_verified': False}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('doctor')
    p = sub.add_parser('build'); p.add_argument('--spec', required=True); p.add_argument('--out', required=True)
    p.add_argument('--page')
    p = sub.add_parser('patch'); p.add_argument('--spec', required=True); p.add_argument('--edits', required=True)
    p.add_argument('--out', required=True)
    p = sub.add_parser('draft-ocr'); p.add_argument('--plan', required=True); p.add_argument('--out', required=True)
    p = sub.add_parser('assemble'); p.add_argument('--jobs', required=True); p.add_argument('--out', required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == 'build':
            report = build(args.spec, args.out, args.page)
        elif args.command == 'patch':
            report = patch_spec(args.spec, args.edits, args.out)
        elif args.command == 'draft-ocr':
            report = draft_ocr(args.plan, args.out)
        elif args.command == 'assemble':
            report = assemble(args.jobs, args.out)
        else:
            report = doctor()
        if args.command == 'build':
            report = {k: v for k, v in report.items() if k not in ['pair_checks', 'diagnostics']}
            detailed = read_json(Path(args.out)/'build.json')
            report['build_record'] = str((Path(args.out)/'build.json').resolve())
            report['diagnostic_counts'] = {
                'box_overlap_candidates': len(detailed['diagnostics']['box_overlap_candidates']),
                'unreviewed_ocr_pages': len(detailed['diagnostics']['unreviewed_ocr_pages'])}
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(json.dumps({'status': 'error', 'error': str(error)}, ensure_ascii=False))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
