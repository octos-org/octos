#!/usr/bin/env python3
"""Inventory native text and embedded images before attributing a visual defect.

Read-only; inventories every slide and speaker-note body by default, with no PDF dependency. Coordinates
inside groups are local, not slide-space. Formatting is raw OOXML, not resolved
theme inheritance. This does not detect clipping, visibility, raster text, or
prove a visual region is clean.
"""
import argparse
import hashlib
import json
from pathlib import Path
import posixpath
import xml.etree.ElementTree as ET
import zipfile

NS = {'p': 'http://schemas.openxmlformats.org/presentationml/2006/main',
      'a': 'http://schemas.openxmlformats.org/drawingml/2006/main',
      'r': 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'}


def relationship_map(z, owner):
    parent, name = posixpath.split(owner)
    rel = posixpath.join(parent, '_rels', name + '.rels')
    if rel not in z.namelist():
        return {}
    return {e.get('Id'): e.attrib for e in ET.fromstring(z.read(rel))}


def target_part(owner, relation):
    if relation.get('TargetMode') == 'External':
        return None
    target = relation['Target']
    return target.lstrip('/') if target.startswith('/') else posixpath.normpath(posixpath.join(posixpath.dirname(owner), target))


def properties(element):
    if element is None:
        return None
    return {'attributes': dict(element.attrib),
            'children': [{'tag': e.tag.split('}')[-1], 'attributes': dict(e.attrib)} for e in element]}


def paragraph_record(paragraph):
    runs = []
    for child in paragraph:
        tag = child.tag.split('}')[-1]
        if tag in ('r', 'fld', 'br'):
            runs.append({'kind': tag, 'text': ''.join(t.text or '' for t in child.findall('a:t', NS)),
                         'properties': properties(child.find('a:rPr', NS))})
    return {'properties': properties(paragraph.find('a:pPr', NS)),
            'default_run_properties': properties(paragraph.find('a:pPr/a:defRPr', NS)),
            'runs': runs}


def speaker_notes(z, slide_part, relations):
    """Read note bodies/custom text, excluding master/header/footer/slide-number fields."""
    parts, texts = [], []
    for relation in relations.values():
        if relation.get('Type', '').rsplit('/', 1)[-1] != 'notesSlide':
            continue
        part = target_part(slide_part, relation)
        if part is None:
            raise ValueError('External speaker notes are unsupported')
        parts.append(part)
        root = ET.fromstring(z.read(part))
        for shape in root.findall('.//p:sp', NS):
            placeholder = shape.find('p:nvSpPr/p:nvPr/p:ph', NS)
            if placeholder is not None and placeholder.get('type') != 'body':
                continue
            text = '\n'.join(''.join(t.text or '' for t in p.findall('.//a:t', NS))
                             for p in shape.findall('p:txBody/a:p', NS)).strip()
            if text:
                texts.append(text)
    return {'present': bool(parts), 'parts': parts, 'text': '\n'.join(texts),
            'scope': 'Note bodies and custom text only; no note-master/header/footer/slide-number text.'}


def slide_record(z, page, slide_id, presentation_rels):
    rid = slide_id.get('{%s}id' % NS['r'])
    part = target_part('ppt/presentation.xml', presentation_rels[rid])
    root = ET.fromstring(z.read(part)); rels = relationship_map(z, part)
    text, images = [], []
    for tag in ('sp', 'graphicFrame'):
        for shape in root.findall('.//p:' + tag, NS):
            paragraphs = shape.findall('.//a:p', NS)
            strings = [''.join(t.text or '' for t in p.findall('.//a:t', NS)) for p in paragraphs]
            if not any(strings):
                continue
            identity = shape.find('.//p:cNvPr', NS)
            transform = shape.find('.//a:xfrm', NS)
            if transform is None:
                transform = shape.find('p:xfrm', NS)
            text.append({'id': identity.get('id') if identity is not None else None,
                         'name': identity.get('name') if identity is not None else None,
                         'text': '\n'.join(strings),
                         'body_properties': [properties(e) for e in shape.findall('.//a:bodyPr', NS)],
                         'paragraphs': [paragraph_record(p) for p in paragraphs],
                         'transform_local_emu': [{e.tag.split('}')[-1]: e.attrib} for e in transform] if transform is not None else None})
    for blip in root.findall('.//a:blip', NS):
        rid = blip.get('{%s}embed' % NS['r'])
        if rid not in rels:
            continue
        media = target_part(part, rels[rid])
        if media:
            images.append({'relationship': rid, 'part': media,
                           'sha256': hashlib.sha256(z.read(media)).hexdigest()})
    return {'page': page, 'slide_id': slide_id.get('id'), 'slide_part': part,
            'native_text': text, 'embedded_images': images,
            'speaker_notes': speaker_notes(z, part, rels)}


def inventory(pptx, page=None):
    pptx = Path(pptx)
    with zipfile.ZipFile(pptx) as z:
        slides = ET.fromstring(z.read('ppt/presentation.xml')).findall('p:sldIdLst/p:sldId', NS)
        if page is not None and not 1 <= page <= len(slides):
            raise ValueError('Page outside presentation order')
        rels = relationship_map(z, 'ppt/presentation.xml')
        records = [slide_record(z, i, slide, rels) for i, slide in enumerate(slides, 1)
                   if page is None or page == i]
    return {'pptx_sha256': hashlib.sha256(pptx.read_bytes()).hexdigest(),
            'slide_count': len(slides), 'inspected_slide_count': len(records),
            'pages': records, 'scope': __doc__.strip(), 'visual_pass': None}


def inspect(pptx, page):
    # Preserve the established single-page interface for existing callers.
    result = inventory(pptx, page)
    return {'pptx_sha256': result['pptx_sha256'], **result['pages'][0], 'scope': result['scope']}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('pptx', type=Path)
    p.add_argument('--page', type=int, help='Optional focused inventory; omitted means every slide')
    p.add_argument('--out', type=Path, required=True)
    a = p.parse_args()
    result = inventory(a.pptx) if a.page is None else inspect(a.pptx, a.page)
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    pages = result['pages'] if a.page is None else [result]
    print(json.dumps({'inspected_slides': len(pages),
                      'native_objects': sum(len(s['native_text']) for s in pages),
                      'embedded_images': sum(len(s['embedded_images']) for s in pages), 'out': str(a.out)}))


if __name__ == '__main__':
    main()
