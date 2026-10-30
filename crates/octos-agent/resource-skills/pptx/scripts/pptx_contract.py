"""Read-only, configuration-driven PPTX checks; no project or model defaults.

Formatting is direct/paragraph formatting, not resolved layout/master/theme
inheritance. Unresolved required properties are reported as unknown, never pass.
Text inventory covers text frames, groups and table cells; charts and SmartArt
still need rendered review. Rotated/flipped groups are not measured geometrically.
"""
import copy
import math
import re
import copy_glossary
from pathlib import Path

from pptx import Presentation
from pptx.enum.dml import MSO_COLOR_TYPE
from pptx.enum.shapes import MSO_SHAPE_TYPE
from pptx.enum.text import PP_ALIGN
from check_pptx_compat import inspect as package_preflight


def normalized(text):
    # Preserve word boundaries. OCR whitespace removal can change meaning.
    return ' '.join(text.split())


def keys(value, allowed, where):
    if not isinstance(value, dict) or set(value) - set(allowed):
        raise ValueError('Invalid or unknown fields in ' + where)


def positive(value, where, zero=False):
    if type(value) not in (int, float) or not math.isfinite(value) or value < 0 or (value == 0 and not zero):
        raise ValueError('Invalid numeric value: ' + where)


def validate_contract(c):
    keys(c, ['version', 'artifact', 'pages', 'checks', 'rules', 'visual', 'glossary'], 'contract')
    copy_glossary.validate(c.get('glossary', {}))
    if c.get('version') != 1 or not isinstance(c.get('artifact'), str) or not c['artifact']:
        raise ValueError('Contract version 1 and artifact path required')
    pages = c.get('pages', [])
    if not isinstance(pages, list) or ('pages' in c and not pages):
        raise ValueError('Configured pages must be a nonempty ordered list')
    ids = []
    for p in pages:
        keys(p, ['id', 'title', 'brief', 'required_text', 'reference', 'blank'], 'page')
        if not isinstance(p.get('id'), str) or not p['id'].strip():
            raise ValueError('Page ID required')
        ids.append(p['id'])
        for name in ('title', 'brief', 'reference', 'blank'):
            if name in p and (not isinstance(p[name], str) or not p[name].strip()):
                raise ValueError('Invalid page ' + name)
        for t in p.get('required_text', []):
            keys(t, ['text', 'count'], 'required_text')
            if not isinstance(t.get('text'), str) or not t['text'].strip() or type(t.get('count', 1)) is not int or t.get('count', 1) < 0:
                raise ValueError('required_text needs nonempty text and nonnegative count')
    if len(ids) != len(set(ids)):
        raise ValueError('Duplicate page IDs')
    checks = c.get('checks', {})
    keys(checks, ['text_in_canvas', 'canvas_tolerance', 'min_native_text_objects'], 'checks')
    if 'text_in_canvas' in checks and type(checks['text_in_canvas']) is not bool:
        raise ValueError('text_in_canvas must be boolean')
    if 'canvas_tolerance' in checks:
        positive(checks['canvas_tolerance'], 'canvas tolerance', zero=True)
    if 'min_native_text_objects' in checks and (type(checks['min_native_text_objects']) is not int or checks['min_native_text_objects'] < 0):
        raise ValueError('min_native_text_objects must be a nonnegative integer')
    rule_ids = []
    for rule in c.get('rules', []):
        keys(rule, ['id', 'page_ids', 'select', 'count', 'style', 'consistent', 'tolerance'], 'rule')
        if not isinstance(rule.get('id'), str) or not rule['id']:
            raise ValueError('Rule ID required')
        rule_ids.append(rule['id'])
        sel = rule.get('select', {})
        keys(sel, ['role', 'name', 'text', 'all_text'], 'selector')
        if len(sel) != 1 or ('role' in sel and sel['role'] != 'title') or ('all_text' in sel and sel['all_text'] is not True):
            raise ValueError('Select one of title role, exact name, exact text, all_text')
        if any(k in sel and (not isinstance(sel[k], str) or not sel[k]) for k in ('name', 'text')):
            raise ValueError('Empty selector')
        if 'page_ids' in rule and (not rule['page_ids'] or not set(rule['page_ids']) <= set(ids)):
            raise ValueError('Rule page_ids must refer to authoritative pages')
        if 'count' in rule and (type(rule['count']) is not int or rule['count'] < 0):
            raise ValueError('Rule count must be a nonnegative integer per page')
        style = rule.get('style', {})
        keys(style, ['font_names', 'font_size_pt', 'bold', 'color_rgb', 'alignment'], 'style')
        if 'font_names' in style and (not isinstance(style['font_names'], list) or not style['font_names'] or not all(isinstance(s, str) and s for s in style['font_names'])):
            raise ValueError('font_names must be nonempty names')
        if 'font_size_pt' in style:
            positive(style['font_size_pt'], 'font size')
        if 'bold' in style and type(style['bold']) is not bool:
            raise ValueError('bold must be boolean')
        if 'color_rgb' in style and not re.fullmatch('[0-9a-fA-F]{6}', style['color_rgb']):
            raise ValueError('color_rgb must be six hex digits')
        if 'alignment' in style and style['alignment'] not in ('left', 'right', 'center', 'justify'):
            raise ValueError('Unknown paragraph alignment')
        if not set(rule.get('consistent', [])) <= {'left_origin', 'top_origin'}:
            raise ValueError('Unsupported consistency property')
        if 'tolerance' in rule:
            positive(rule['tolerance'], 'consistency tolerance', zero=True)
        if not style and not rule.get('consistent') and 'count' not in rule:
            raise ValueError('Rule must assert something')
    if len(rule_ids) != len(set(rule_ids)):
        raise ValueError('Duplicate rule IDs')
    visual = c.get('visual', {})
    keys(visual, ['required', 'instructions'], 'visual')
    if 'required' in visual and type(visual['required']) is not bool:
        raise ValueError('visual.required must be boolean')
    if 'instructions' in visual and not isinstance(visual['instructions'], str):
        raise ValueError('visual.instructions must be text')
    return c


def select_pages(contract, ids):
    validate_contract(contract)
    mapping = {s['id']: s for s in contract.get('pages', [])}
    if not ids or len(ids) != len(set(ids)) or not set(ids) <= set(mapping):
        raise ValueError('Explicit unique authoritative page IDs required')
    result = copy.deepcopy(contract)
    result['pages'] = [mapping[i] for i in ids]
    result['rules'] = [r for r in result.get('rules', []) if 'page_ids' not in r or set(r['page_ids']) & set(ids)]
    for rule in result['rules']:
        if 'page_ids' in rule:
            rule['page_ids'] = [i for i in rule['page_ids'] if i in ids]
    return validate_contract(result)


def font_value(font, name):
    if name == 'font_names':
        return font.name
    if name == 'font_size_pt':
        return font.size.pt if font.size is not None else None
    if name == 'color_rgb':
        return str(font.color.rgb) if font.color.type == MSO_COLOR_TYPE.RGB else None
    return font.bold


def text_record(tf, identity, name, rect, origin, role=None):
    runs, alignments = [], []
    names = {PP_ALIGN.LEFT: 'left', PP_ALIGN.CENTER: 'center', PP_ALIGN.RIGHT: 'right', PP_ALIGN.JUSTIFY: 'justify'}
    for p in tf.paragraphs:
        if not p.text.strip():
            continue
        alignments.append(names.get(p.alignment))
        for r in p.runs:
            if not r.text.strip():
                continue
            values = {}
            for field in ('font_names', 'font_size_pt', 'bold', 'color_rgb'):
                v = font_value(r.font, field)
                values[field] = font_value(p.font, field) if v is None else v
            runs.append({'text': r.text, **values})
    return {'id': identity, 'name': name, 'text': tf.text, 'bbox': rect,
            'left_origin': origin[0], 'top_origin': origin[1], 'role': role,
            'runs': runs, 'alignment': alignments}


def inventory(prs):
    w, h = prs.slide_width, prs.slide_height
    pages = []
    for slide in prs.slides:
        objects = []
        native_title = slide.shapes.title
        title_id = native_title.shape_id if native_title is not None else None
        def walk(shapes, transform=(1, 1, 0, 0), prefix=''):
            sx, sy, dx, dy = transform if transform else (1, 1, 0, 0)
            for s in shapes:
                sid = prefix + str(s.shape_id)
                unrotated = transform is not None and not s.rotation
                xfrm = s._element.find('.//{http://schemas.openxmlformats.org/drawingml/2006/main}xfrm')
                if xfrm is not None and (xfrm.get('flipH') in ('1', 'true') or xfrm.get('flipV') in ('1', 'true')):
                    unrotated = False
                if s.shape_type == MSO_SHAPE_TYPE.GROUP:
                    t = s._element.grpSpPr.xfrm
                    if unrotated and t is not None and t.chExt is not None and t.chExt.cx and t.chExt.cy:
                        ax, ay = s.width / t.chExt.cx, s.height / t.chExt.cy
                        child = (sx * ax, sy * ay, dx + sx * (s.left - ax * t.chOff.x), dy + sy * (s.top - ay * t.chOff.y))
                    else:
                        child = None
                    walk(s.shapes, child, sid + '/')
                    continue
                def geometry(left, top, width, height, mx, my):
                    if not unrotated:
                        return None, (None, None)
                    return ([(dx + sx * left) / w, (dy + sy * top) / h,
                             (dx + sx * (left + width)) / w, (dy + sy * (top + height)) / h],
                            ((dx + sx * (left + mx)) / w, (dy + sy * (top + my)) / h))
                if s.has_text_frame and s.text.strip():
                    tf = s.text_frame
                    rect, origin = geometry(s.left, s.top, s.width, s.height, tf.margin_left, tf.margin_top)
                    objects.append(text_record(tf, sid, s.name, rect, origin, 'title' if not prefix and s.shape_id == title_id else None))
                if s.has_table:
                    table = s.table; y = s.top
                    for row_index, row in enumerate(table.rows):
                        x = s.left
                        for col_index, cell in enumerate(row.cells):
                            if cell.text.strip() and not cell.is_spanned:
                                width = sum(table.columns[k].width for k in range(col_index, col_index + cell.span_width))
                                height = sum(table.rows[k].height for k in range(row_index, row_index + cell.span_height))
                                rect, origin = geometry(x, y, width, height, cell.margin_left, cell.margin_top)
                                objects.append(text_record(cell.text_frame, f'{sid}/cell/{row_index}/{col_index}', s.name, rect, origin))
                            x += table.columns[col_index].width
                        y += row.height
        walk(slide.shapes)
        pages.append(objects)
    return pages


def matches(obj, selector):
    if 'all_text' in selector:
        return True
    key, value = next(iter(selector.items()))
    return normalized(obj['text']) == normalized(value) if key == 'text' else obj.get(key) == value


def inspect(path, contract):
    validate_contract(contract)
    preflight = package_preflight(path)
    issues = [{'check': 'package', 'detail': f} for f in preflight['issues']]
    if issues:
        return {'findings': issues, 'pages': [], 'preflight': preflight}
    prs = Presentation(path)
    pages = inventory(prs)
    specs = contract.get('pages', [{'id': str(i + 1)} for i in range(len(pages))])
    if not pages or len(pages) != len(specs):
        issues.append({'check': 'coverage', 'detail': f'Expected {len(specs)} slides; found {len(pages)}'})
        return {'findings': issues, 'pages': pages, 'preflight': preflight}
    consistent = {}
    def finding(check, detail, pid, obj=None, status='fail'):
        issues.append({'check': check, 'page': pid, 'object': obj, 'status': status, 'detail': detail})
    for spec, objects in zip(specs, pages):
        pid = spec['id']
        issues.extend({'page':pid, **issue} for issue in copy_glossary.findings(objects,contract.get('glossary', {})))
        if 'title' in spec:
            titles = [o for o in objects if normalized(o['text']) == normalized(spec['title'])]
            if len(titles) != 1:
                finding('title', f'Expected one native title {spec["title"]!r}; found {len(titles)}', pid)
            for o in objects:
                o['role'] = 'title' if o in titles else None
        for expected in spec.get('required_text', []):
            count = sum(normalized(o['text']) == normalized(expected['text']) for o in objects)
            if count != expected.get('count', 1):
                finding('required-text', {'expected': expected, 'actual_count': count}, pid)
        checks = contract.get('checks', {})
        if len(objects) < checks.get('min_native_text_objects', 0):
            finding('editable-text', 'Insufficient native text objects for configured requirement', pid)
        if checks.get('text_in_canvas'):
            tol = checks.get('canvas_tolerance', 0)
            for o in objects:
                box = o['bbox']
                if box is None:
                    finding('text-in-canvas', 'Transform cannot be resolved by this checker; inspect rendered placement', pid, o['id'], 'unknown')
                elif box[0] < -tol or box[1] < -tol or box[2] > 1 + tol or box[3] > 1 + tol or box[2] <= box[0] or box[3] <= box[1]:
                    finding('text-in-canvas', {'bbox': box}, pid, o['id'])
        for rule in contract.get('rules', []):
            if pid not in rule.get('page_ids', [pid]):
                continue
            selected = [o for o in objects if matches(o, rule['select'])]
            if ('count' in rule and len(selected) != rule['count']) or (not selected and rule.get('count') != 0):
                finding(rule['id'], f'Selector matched {len(selected)} objects; expected {rule.get("count", "at least one")}', pid)
            for o in selected:
                for prop, expected in rule.get('style', {}).items():
                    actuals = o['alignment'] if prop == 'alignment' else [r[prop] for r in o['runs']]
                    if not actuals or any(v is None for v in actuals):
                        finding(rule['id'], f'{prop} is inherited or unresolved; cannot verify configured requirement', pid, o['id'], 'unknown')
                    for value in [v for v in actuals if v is not None]:
                        ok = value.casefold() in {f.casefold() for f in expected} if prop == 'font_names' else abs(value - expected) <= .01 if prop == 'font_size_pt' else str(value).upper() == expected.upper() if prop == 'color_rgb' else value == expected
                        if not ok:
                            finding(rule['id'], f'{prop}: expected {expected!r}, observed {value!r}', pid, o['id'])
                            break
                for prop in rule.get('consistent', []):
                    if o[prop] is None:
                        finding(rule['id'], f'{prop} cannot be resolved', pid, o['id'], 'unknown')
                    else:
                        consistent.setdefault((rule['id'], prop), []).append((pid, o['id'], o[prop]))
    for rule in contract.get('rules', []):
        for prop in rule.get('consistent', []):
            values = consistent.get((rule['id'], prop), [])
            if values and max(v[2] for v in values) - min(v[2] for v in values) > rule.get('tolerance', .001):
                issues.append({'check': rule['id'], 'detail': {'inconsistent': prop, 'observed': values}})
    return {'findings': issues, 'pages': pages, 'specs': specs, 'preflight': preflight,
            'limitations': __doc__.strip()}
