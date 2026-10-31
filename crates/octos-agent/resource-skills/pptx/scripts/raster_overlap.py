"""Propose raster/native text collisions from actual PPTX layers, never page IDs.

OCR only proposes regions. An independent pixel review must decide whether a
proposal is real duplicated text, unrelated artwork, or unresolved uncertainty.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile
from lxml import etree
from PIL import Image
from review_cache import content_lock

PROMPT = '''Inspect the supplied raster/native collision candidates from an actual editable presentation. Each candidate includes original design, actual raster-only layer and actual rendered slide at the SAME crop. OCR text and native frame overlap are proposals, not proof. Determine whether old raster text remains under or visibly collides with native text, causing duplicate words, mixed languages or ghost glyphs. Independently legible labels and diagram symbols near a text frame are not defects. Ignore OCR hallucinations on icons. Do not invent replacement copy or coordinates. Use unknown when the pixels cannot establish the relationship. Return ONLY JSON with every candidate ID once: {"candidates":[{"id":"candidate id","status":"pass|fail|unknown","detail":"specific visible evidence"}]}. A pass requires that this candidate contains no visible duplicate or residual text collision. This is an additional layer check, not a replacement for full-page inspection.'''

def digest(data):
    return hashlib.sha256(data).hexdigest()

def write_json(path, data):
    path = Path(path); path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix('.tmp'); tmp.write_text(json.dumps(data, ensure_ascii=False, indent=2)); tmp.replace(path)

def textless_snapshot(source, target):
    """Host analysis derivative only; retain every picture/shape and erase text."""
    namespace = {'a': 'http://schemas.openxmlformats.org/drawingml/2006/main'}
    with zipfile.ZipFile(source) as src, zipfile.ZipFile(target, 'w', zipfile.ZIP_DEFLATED) as dst:
        for item in src.infolist():
            raw = src.read(item.filename)
            if item.filename.startswith('ppt/slides/slide') and item.filename.endswith('.xml'):
                xml = etree.fromstring(raw)
                for element in xml.findall('.//a:t', namespace):
                    element.text = ''
                raw = etree.tostring(xml, encoding='UTF-8', xml_declaration=True, standalone=True)
            dst.writestr(item, raw)

def proposals(detections, size, objects):
    width, height = size; found = []
    for detection in detections:
        if not isinstance(detection.get('text'), str) or not detection['text'].strip():
            continue
        x, y, w, h = [float(detection[k]) for k in ['x', 'y', 'w', 'h']]
        if w <= 0 or h <= 0: continue
        box = [max(0, x/width), max(0, y/height), min(1, (x+w)/width), min(1, (y+h)/height)]
        if box[0] >= box[2] or box[1] >= box[3]: continue
        area = (box[2]-box[0])*(box[3]-box[1]); overlaps = []
        for obj in objects:
            b = obj['bbox']
            intersection = max(0, min(box[2], b[2])-max(box[0], b[0])) * max(0, min(box[3], b[3])-max(box[1], b[1]))
            denominator = min(area, max(0, b[2]-b[0])*max(0, b[3]-b[1]))
            if denominator > 0 and intersection/denominator >= .15:
                overlaps.append({'id': obj['id'], 'text': obj['text'], 'bbox': b})
        if overlaps:
            bounds = [box]+[o['bbox'] for o in overlaps]
            crop = [max(0, min(b[0] for b in bounds)-.015), max(0, min(b[1] for b in bounds)-.015),
                    min(1, max(b[2] for b in bounds)+.015), min(1, max(b[3] for b in bounds)+.015)]
            found.append({'id': str(len(found)+1), 'raster_text': detection['text'], 'raster_bbox': box,
                          'ocr_confidence': detection.get('confidence'), 'native': overlaps, 'crop': crop})
    return found

def parse_review(answer, candidates):
    raw = answer.strip()
    if raw.startswith('```'):
        raw = raw.removeprefix('```json').removeprefix('```').removesuffix('```').strip()
    data = json.loads(raw); rows = data.get('candidates')
    expected = {c['id'] for c in candidates}
    if not isinstance(rows, list) or len(rows) != len(expected) or {r.get('id') for r in rows} != expected:
        raise ValueError('Incomplete raster/native collision review coverage')
    for row in rows:
        if row.get('status') not in ['pass', 'fail', 'unknown'] or not isinstance(row.get('detail'), str) or not row['detail'].strip():
            raise ValueError('Unsubstantiated raster/native collision verdict')
    data['status'] = 'pass' if all(r['status'] == 'pass' for r in rows) else 'fail'
    return data

def cached_ocr(image, executable, cache_root):
    key = digest(Path(image).read_bytes()+Path(executable).read_bytes())
    cache = Path(cache_root)/key/'ocr.json'
    with content_lock('raster-ocr:'+key):
        if cache.exists(): return json.loads(cache.read_text())
        result = subprocess.run([str(executable), str(image)], capture_output=True, text=True, timeout=90)
        if result.returncode: raise RuntimeError('Raster OCR failed: '+result.stderr[-500:])
        data = json.loads(result.stdout)
        if not isinstance(data, list): raise ValueError('Raster OCR did not return detections')
        write_json(cache, data); return data
