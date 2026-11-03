#!/usr/bin/env python3
"""Derive and check a blank deck from an explicitly layered PPTX.

Editable labels must be separate TEXT__ textboxes. Other shapes are preserved.
Requires python-pptx and lxml. PDF checks additionally require PyMuPDF, Pillow,
and NumPy. This verifies technical invariants, not semantic or aesthetic quality.
"""
import argparse
from collections import Counter
from hashlib import sha256
import json
import os
from pathlib import Path
import tempfile
import unicodedata

from lxml import etree
from pptx import Presentation
from pptx.enum.shapes import MSO_SHAPE_TYPE
from pptx.oxml.ns import qn


def text_of(shape):
    return ''.join(el.text or '' for el in shape._element.iter(qn('a:t')))


def layer_errors(prs, prefix):
    errors = []
    for index, slide in enumerate(prs.slides, 1):
        for shape in slide.shapes:
            if shape.name.startswith(prefix):
                if shape.shape_type != MSO_SHAPE_TYPE.TEXT_BOX:
                    errors.append({'slide': index, 'shape': shape.name,
                                   'reason': 'Text layer must be a separate textbox; mixed objects need review.'})
            elif text_of(shape).strip():
                errors.append({'slide': index, 'shape': shape.name,
                               'reason': 'Unmarked text; separate and explicitly mark its text layer first.'})
    return errors


def derive_blank(master, output, prefix, force=False):
    if master.resolve() == output.resolve():
        raise ValueError('Master and blank output must be different files.')
    if output.exists() and not force:
        raise ValueError('Output exists; use --force to replace that derivative intentionally.')
    prs = Presentation(master)
    errors = layer_errors(prs, prefix)
    if errors:
        raise ValueError(json.dumps(errors, ensure_ascii=False))
    removed = 0
    for slide in prs.slides:
        for shape in list(slide.shapes):
            if shape.name.startswith(prefix):
                element = shape._element
                element.getparent().remove(element)
                removed += 1
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix='.blank-', suffix='.pptx', dir=output.parent)
    os.close(fd)
    temporary = Path(name)
    try:
        prs.save(temporary)
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)
    return {'output': str(output.resolve()), 'slides': len(prs.slides),
            'removedNativeTextBoxes': removed, 'notesRetained': True}


def canonical(element):
    return etree.tostring(element, method='c14n') if element is not None else b''


def visual_signature(slide, prefix):
    """Include geometry, paint, background, and directly referenced payloads."""
    entries = []
    r_namespace = '{http://schemas.openxmlformats.org/officeDocument/2006/relationships}'
    for shape in slide.shapes:
        if shape.name.startswith(prefix):
            continue
        relations = {}
        for element in shape._element.iter():
            for attr, value in element.attrib.items():
                if attr.startswith(r_namespace):
                    relation = slide.part.rels[value]
                    payload = (str(relation.target_ref).encode() if relation.is_external
                               else relation.target_part.blob)
                    relations[value] = sha256(payload).hexdigest()
        entries.append({'xml': sha256(canonical(shape._element)).hexdigest(),
                        'relations': relations})
    bg = slide._element.cSld.find(qn('p:bg'))
    return {'shapes': entries, 'background': sha256(canonical(bg)).hexdigest()}


def normalize(text):
    return ''.join(c for c in unicodedata.normalize('NFKC', text) if not c.isspace())


def size_arg(value):
    try:
        width, height = map(int, value.lower().split('x'))
        if min(width, height) <= 0:
            raise ValueError
        return width, height
    except ValueError as exc:
        raise argparse.ArgumentTypeError('Expected positive WIDTHxHEIGHT.') from exc


def pdf_checks(prs, full_path, blank_path, prefix, pixels, margin):
    import fitz
    import numpy as np
    from PIL import Image, ImageDraw

    results = []
    with fitz.open(full_path) as full, fitz.open(blank_path) as blank:
        if len(full) != len(prs.slides) or len(blank) != len(prs.slides):
            return {'passed': False, 'error': 'PDF page count differs from PPTX.'}
        for index, slide in enumerate(prs.slides):
            labels = [s for s in slide.shapes if s.name.startswith(prefix)]
            expected = Counter(normalize(s.text) for s in labels if s.text.strip())
            extracted = normalize(full[index].get_text())
            missing = [{'text': text, 'expectedCount': count,
                        'extractedOccurrenceCount': extracted.count(text)}
                       for text, count in expected.items() if extracted.count(text) < count]
            arrays = []
            size_ok = full[index].rect == blank[index].rect
            for doc in (full, blank):
                page = doc[index]
                pix = page.get_pixmap(matrix=fitz.Matrix(pixels[0] / page.rect.width,
                                                       pixels[1] / page.rect.height), alpha=False)
                size_ok = size_ok and (pix.width, pix.height) == pixels
                arrays.append(np.frombuffer(pix.samples, dtype=np.uint8).reshape(pix.height, pix.width, pix.n))
            if not size_ok or arrays[0].shape != arrays[1].shape:
                results.append({'slide': index + 1, 'passed': False, 'error': 'Rendered dimensions differ.'})
                continue
            mask = Image.new('1', pixels)
            draw = ImageDraw.Draw(mask)
            for shape in labels:
                x = shape.left / prs.slide_width * pixels[0]
                y = shape.top / prs.slide_height * pixels[1]
                right = (shape.left + shape.width) / prs.slide_width * pixels[0]
                bottom = (shape.top + shape.height) / prs.slide_height * pixels[1]
                from label_geometry import frame_quad
                # Rotate in slide units before scaling; output pixels need not be square.
                q = frame_quad([shape.left,shape.top,shape.width,shape.height],shape.rotation)
                points = [(p[0]/prs.slide_width*pixels[0],p[1]/prs.slide_height*pixels[1]) for p in q]
                draw.polygon(points, fill=1)
                if margin:
                    draw.line(points+[points[0]], fill=1, width=2*margin+1, joint='curve')
            changed = np.any(arrays[0] != arrays[1], axis=2)
            outside = int(np.count_nonzero(changed & ~np.asarray(mask, dtype=bool)))
            blank_text = blank[index].get_text().strip()
            results.append({'slide': index + 1, 'missingNativeText': missing,
                            'blankExtractableText': blank_text,
                            'changedPixelsOutsideTextBoxes': outside,
                            'passed': not missing and not blank_text and outside == 0})
    return {'passed': all(row['passed'] for row in results), 'pixels': list(pixels),
            'maskMarginPixels': margin, 'slides': results}


def check(master, blank_path, prefix, full_pdf=None, blank_pdf=None, pixels=(3840, 2160), margin=2):
    full, blank = Presentation(master), Presentation(blank_path)
    errors = layer_errors(full, prefix)
    equal_size = (full.slide_width, full.slide_height) == (blank.slide_width, blank.slide_height)
    equal_count = len(full.slides) == len(blank.slides)
    slides = []
    for index, (a, b) in enumerate(zip(full.slides, blank.slides), 1):
        residual = [s.name for s in b.shapes if text_of(s).strip() or s.name.startswith(prefix)]
        equal_art = visual_signature(a, prefix) == visual_signature(b, prefix)
        slides.append({'slide': index, 'nativeTextBoxes': sum(s.name.startswith(prefix) for s in a.shapes),
                       'visualObjectsIdentical': equal_art, 'residualTextObjects': residual,
                       'passed': equal_art and not residual})
    result = {'scope': 'Technical layered-deck checks; raster text, canonical copy, fonts and visual UX require separate review.',
              'masterSlides': len(full.slides), 'blankSlides': len(blank.slides),
              'pageSizeMatches': equal_size, 'pageCountMatches': equal_count,
              'layerErrors': errors, 'slides': slides}
    result['renderChecked'] = bool(full_pdf)
    passed = equal_size and equal_count and not errors and all(row['passed'] for row in slides)
    if full_pdf:
        result['pdf'] = pdf_checks(full, full_pdf, blank_pdf, prefix, pixels, margin)
        passed = passed and result['pdf']['passed']
    result['technicalPassed'] = bool(passed)
    result['visualUXAssessed'] = False
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    derive = commands.add_parser('blank', help='Remove explicitly marked textboxes from a separate derivative.')
    derive.add_argument('master', type=Path)
    derive.add_argument('output', type=Path)
    derive.add_argument('--force', action='store_true')
    verify = commands.add_parser('check', help='Compare layers, optionally including rendered PDFs.')
    verify.add_argument('master', type=Path)
    verify.add_argument('blank', type=Path)
    verify.add_argument('--full-pdf', type=Path)
    verify.add_argument('--blank-pdf', type=Path)
    verify.add_argument('--render-size', type=size_arg, default=(3840, 2160))
    verify.add_argument('--mask-margin', type=int, default=2)
    verify.add_argument('--out', type=Path)
    for sub in (derive, verify):
        sub.add_argument('--prefix', default='TEXT__')
    args = parser.parse_args()
    if not args.prefix:
        parser.error('Text prefix cannot be empty.')
    if args.command == 'check':
        if bool(args.full_pdf) != bool(args.blank_pdf):
            parser.error('Pass both --full-pdf and --blank-pdf, or neither.')
        if args.mask_margin < 0:
            parser.error('--mask-margin must be nonnegative.')
    try:
        if args.command == 'blank':
            report = derive_blank(args.master, args.output, args.prefix, args.force)
        else:
            report = check(args.master, args.blank, args.prefix, args.full_pdf, args.blank_pdf,
                           args.render_size, args.mask_margin)
    except (OSError, ValueError, KeyError) as exc:
        parser.error(str(exc))
    serialized = json.dumps(report, ensure_ascii=False, indent=2)
    if args.command == 'check' and args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(serialized + '\n')
    print(serialized)
    if args.command == 'check' and not report['technicalPassed']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
