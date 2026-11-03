#!/usr/bin/env python3
"""Propose text fitting from explicit font files; never change a deck or approve it."""
import argparse
import math
from pathlib import Path
import unicodedata

from PIL import ImageFont, features

from native_deck import atomic_json, effective_style, load_spec, normalized, read_json, require, sha, rect, within
from label_geometry import region as label_region

VERSION = '1.1'
EM = 1000


def tokens(text):
    """Keep Latin identifiers/numbers intact; allow CJK boundaries, not combining splits."""
    result, word = [], ''
    for char in text:
        if unicodedata.combining(char) and (word or result):
            if word:
                word += char
            else:
                result[-1] += char
        elif (char.isalnum() or char in '_./:+-') and unicodedata.east_asian_width(char) not in 'WF':
            word += char
        else:
            if word:
                result.append(word); word = ''
            result.append(char)
    if word:
        result.append(word)
    return result


def layout(text, font, size, available_width, wrap):
    measure = lambda s: font.getlength(s) * size / EM
    lines = []
    for paragraph in text.replace('\r\n', '\n').split('\n'):
        if not wrap:
            lines.append(paragraph)
            continue
        line = ''
        for part in tokens(paragraph):
            if line and measure(line + part) > available_width:
                # Leave punctuation with its phrase; oversized units remain infeasible.
                if part in '，。！？；：、）》」』】,.!?;:)]}':
                    line += part
                    continue
                lines.append(line.rstrip()); line = part.lstrip()
            else:
                line += part
        lines.append(line.rstrip())
    require(normalized('\n'.join(lines)) == normalized(text), 'Line layout changed canonical content')
    return lines, max((measure(s) for s in lines), default=0)


def font_entry(config, config_path, style, cache):
    key = (style['font'], style.get('bold', False), style.get('italic', False))
    entries = [e for e in config['fonts'] if
               (e['family'], e.get('bold', False), e.get('italic', False)) == key]
    require(len(entries) == 1, f'Need one explicit font file for family/bold/italic {key}')
    entry = entries[0]
    path = (config_path.parent / entry['path']).resolve(strict=True)
    index = entry.get('index', 0)
    require(type(index) is int and index >= 0, 'Font collection index must be nonnegative')
    digest_key = ('font_digest', str(path))
    if digest_key not in cache:
        cache[digest_key] = sha(path)
    digest = cache[digest_key]
    require(not entry.get('sha256') or entry['sha256'] == digest, 'Font file hash changed')
    cache_key = (str(path), digest, index)
    if cache_key not in cache:
        from fontTools.ttLib import TTFont, TTLibError
        try:
            with path.open('rb') as stream:
                with TTFont(stream, fontNumber=index, lazy=True) as tt:
                    cmap = tt.getBestCmap() or {}
                    require('fvar' not in tt, 'Instantiate variable fonts before measuring; axes are otherwise ambiguous')
        except (TTLibError, IndexError) as error:
            raise ValueError(f'Cannot read selected font face: {error}') from error
        cache[cache_key] = (ImageFont.truetype(str(path), EM, index=index), set(cmap))
    font, glyphs = cache[cache_key]
    return font, glyphs, {'path': str(path), 'sha256': digest, 'index': index,
                         'requested_family': style['font'], 'actual_family_style': list(font.getname())}


def ink_layout(lines, font, size, width, style):
    """Predict visible glyph offsets in points, not just the nominal line boxes."""
    ascent, descent = font.getmetrics()
    line_height = (ascent+descent)/EM*size*style.get('line_spacing', 1.0)
    before, after = style.get('space_before_pt', 0), style.get('space_after_pt', 0)
    step = line_height+before+after
    bounds = []
    for i, line in enumerate(lines):
        if not line.strip(): continue
        advance = font.getlength(line)/EM*size
        x = {'left': 0, 'center': (width-advance)/2, 'right': width-advance}[style.get('align', 'left')]
        baseline = i*step+before+ascent/EM*size
        a, b, c, d = (value/EM*size for value in font.getbbox(line, anchor='ls'))
        bounds.append([x+a, baseline+b, x+c, baseline+d])
    require(bounds, 'No visible glyph bounds available')
    return [min(b[0] for b in bounds), min(b[1] for b in bounds),
            max(b[2] for b in bounds), max(b[3] for b in bounds)], len(lines)*step


def position_ink(target, bounds, used_height, style, canvas, region, vertical):
    """Return a frame proposal aligning visible ink to an explicitly selected target."""
    sx = canvas['width_px']/(canvas['width_in']*72)
    sy = canvas['height_px']/(canvas['height_in']*72)
    x, y, w, h = target
    left, top, right, bottom = style.get('margins_px', [0, 0, 0, 0])
    a, b, c, d = bounds
    # Margins belong to the native frame; target denotes the desired visible region.
    ax = {'left': 0, 'center': .5, 'right': 1}[style.get('align', 'left')]
    ay = {'top': 0, 'middle': .5, 'bottom': 1}[vertical]
    px = x+ax*(w-(c-a)*sx)-left-a*sx
    base_y = y+ay*(h-(d-b)*sy)-top-b*sy
    logical_height = top+bottom+used_height*sy
    anchor = {'top': 0, 'middle': .5, 'bottom': 1}[style.get('anchor', 'top')]
    boundary = region or [0, 0, canvas['width_px'], canvas['height_px']]
    # Retain available frame space rather than squeezing to nominal font metrics.
    # Office engines may use different line metrics. Extra room changes the origin
    # for middle/bottom anchors, so account for that instead of overriding the style.
    max_height = float('inf')
    if anchor:
        max_height = min(max_height, (base_y+anchor*logical_height-boundary[1])/anchor)
    if anchor < 1:
        max_height = min(max_height, (boundary[1]+boundary[3]-base_y-anchor*logical_height)/(1-anchor))
    frame_height = min(max(h, logical_height), max_height)
    require(frame_height >= logical_height-1e-6, 'Predicted native frame crosses declared owner or canvas; choose a target with line-box room')
    py = base_y-anchor*(frame_height-logical_height)
    frame = [px, py, w, frame_height]
    require(within(frame, [0, 0, canvas['width_px'], canvas['height_px']]),
            'Predicted native frame crosses canvas; select a target with room for font line metrics')
    require(region is None or within(frame, region),
            'Predicted native frame crosses declared owner; review target/owner geometry instead of moving borders silently')
    predicted = [px+left+a*sx, py+top+b*sy+anchor*(frame_height-logical_height), (c-a)*sx, (d-b)*sy]
    return [round(v, 6) for v in frame], [round(v, 6) for v in predicted]


def propose(spec, fonts, objects=None, min_size=None, max_size=None, max_lines=None,
            ink_align=None, targets=None):
    """Only explicit size bounds permit resizing. A proposal is not render evidence."""
    doc, dependencies = load_spec(spec, validate_input=False)
    font_path = Path(fonts).resolve(strict=True)
    config = read_json(font_path)
    require(config.get('schema_version') == 1 and isinstance(config.get('fonts'), list),
            'Font map needs schema_version 1 and fonts list')
    for name, value in [('min_size', min_size), ('max_size', max_size)]:
        require(value is None or (type(value) in (int, float) and math.isfinite(value) and value > 0),
                name + ' must be finite and positive')
    require(max_lines is None or (type(max_lines) is int and max_lines > 0), 'max_lines must be a positive integer')
    require(ink_align in [None, 'top', 'middle', 'bottom'], 'ink_align must be top, middle or bottom')
    require(not targets or ink_align, 'Explicit ink targets require ink_align')
    keys = {(p['id'], t['id']) for p in doc['pages'] for t in p['texts']}
    selected = set()
    for value in objects or []:
        matches = {k for k in keys if value == k[1] or value == '/'.join(k)}
        require(matches, f'Unknown object selection: {value}')
        selected |= matches
    if objects is None:
        selected = keys
    require(selected, 'No objects selected')
    target_map = {}
    if targets:
        target_path = Path(targets).resolve(strict=True); config_targets = read_json(target_path)
        require(config_targets.get('schema_version') == 1, 'Ink targets need schema_version 1')
        for target in config_targets['objects']:
            key = (target['page'], target['id'])
            require(key in keys and key not in target_map, 'Unknown or duplicate ink target')
            target_map[key] = rect(target['box'], 'ink target')
        dependencies[str(target_path)] = sha(target_path)
    c = doc['canvas']; sx = c['width_px'] / (c['width_in'] * 72); sy = c['height_px'] / (c['height_in'] * 72)
    rows, edits, cache = [], [], {}
    for page in doc['pages']:
        for item in page['texts']:
            if (page['id'], item['id']) not in selected:
                continue
            row = {'page': page['id'], 'id': item['id'], 'state': 'unresolved', 'box': item['box']}
            rows.append(row)
            try:
                style = effective_style(doc, item)
                font, glyphs, provenance = font_entry(config, font_path, style, cache)
                row['font'] = provenance
                missing = sorted({ch for ch in item['text'] if not ch.isspace() and ord(ch) not in glyphs})
                require(not missing, 'Font lacks glyphs: ' + repr(missing))
                # BASIC layout cannot predict shaping of contextual scripts.
                needs_shaping = any(0x0590 <= ord(ch) <= 0x0DFF for ch in item['text'])
                require(not needs_shaping or features.check('raqm'), 'Contextual script requires Pillow RAQM shaping')
                left, top, right, bottom = style.get('margins_px', [0, 0, 0, 0])
                require(not ink_align or not item.get('rotation'),
                        'Axis-aligned ink placement cannot position a rotated label; use label_geometry and inspect actual pixels')
                target = target_map.get((page['id'], item['id']), item['box'])
                require(within(target, [0, 0, c['width_px'], c['height_px']]), 'Ink target outside canvas')
                width = (target[2] - left - right) / sx
                height = (target[3] if ink_align else target[3]-top-bottom) / sy
                require(width > 0 and height > 0, 'Margins consume text box')
                original = style['size_pt']
                lo, hi = min_size if min_size is not None else original, max_size if max_size is not None else original
                require(lo <= hi, 'Minimum size exceeds maximum size')
                locked = doc['styles'][item['style']].get('locked', False)
                if locked:
                    require(lo <= original <= hi, 'Locked style conflicts with the requested size range')
                    lo = hi = original
                ascent, descent = font.getmetrics()
                def candidate(size):
                    lines, widest = layout(item['text'], font, size, width, style.get('wrap', True))
                    # Conservative line boxes. Actual Office shaping is checked after rendering.
                    line_height = (ascent + descent) / EM * size * style.get('line_spacing', 1.0)
                    used_height = len(lines) * (line_height + style.get('space_before_pt', 0) + style.get('space_after_pt', 0))
                    bounds, _ = ink_layout(lines, font, size, width, style)
                    occupied = bounds[3]-bounds[1] if ink_align else used_height
                    fits = widest <= width + .001 and occupied <= height + .001 and (max_lines is None or len(lines) <= max_lines)
                    if ink_align: fits = fits and bounds[2]-bounds[0] <= target[2]/sx+.001
                    return fits, lines, widest, used_height
                fits, lines, widest, used_height = candidate(lo)
                row.update(available_pt=[width, height], allowed_size_pt=[lo, hi], locked_style=bool(locked))
                if not fits:
                    row.update(state='no_fit_within_bounds', measured_at_min_pt=lo,
                               measured_pt=[widest, used_height], lines=len(lines),
                               reason='Reflow or change the declared region/box; no out-of-policy shrinking proposed.')
                    continue
                # Binary search, then round downward; verify the exact proposed line layout.
                lower, upper = lo, hi
                if candidate(hi)[0]:
                    size = hi
                else:
                    for _ in range(18):
                        mid = (lower + upper) / 2
                        if candidate(mid)[0]: lower = mid
                        else: upper = mid
                    size = max(lo, math.floor(lower * 100) / 100)
                fits, lines, widest, used_height = candidate(size)
                require(fits, 'Proposed size failed its own metrics')
                row.update(state='proposal', size_pt=size, measured_pt=[widest, used_height],
                           lines=lines, ascent_pt=ascent / EM * size, descent_pt=descent / EM * size,
                           owner_status='declared_not_semantically_verified' if item.get('region') else 'not_declared')
                proposed_text = '\n'.join(lines)
                update = {}
                if proposed_text != item['text']:
                    update['text'] = proposed_text
                if ink_align:
                    bounds, used_height = ink_layout(lines, font, size, width, style)
                    frame, predicted = position_ink(target, bounds, used_height, style, c,
                                                    (label_region(page['regions'][item['region']])['box']
                                                     if item.get('region') else None), ink_align)
                    row['ink_placement'] = {'target_box_px': target, 'proposed_frame_px': frame,
                                            'predicted_ink_box_px': predicted, 'vertical': ink_align,
                                            'basis': 'Actual font glyph bounds and predicted baseline; confirm in the target renderer'}
                    if frame != item['box']: update['box'] = frame
                if locked and style.get('wrap', True) and (ink_align or proposed_text != item['text']):
                    row.update(state='needs_style_decision',
                               required_style_changes={'wrap': False},
                               reason='Explicit lines can wrap again. Locked style is preserved; choose a no-wrap shared style to apply this layout.')
                    continue
                if not locked:
                    fmt = dict(item.get('format', {}))
                    if size != original: fmt['size_pt'] = size
                    # Explicit lines avoid renderer-specific auto wrapping of short identifiers.
                    if style.get('wrap', True) and (ink_align or size != original or proposed_text != item['text']):
                        fmt['wrap'] = False
                    if fmt != item.get('format', {}): update['format'] = fmt
                if update:
                    edits.append({'page': page['id'], 'id': item['id'], 'set': update})
            except (ValueError, KeyError, OSError, ImportError) as error:
                row.update(state='unresolved', reason=str(error))
    font_bindings = {key[1]: digest for key, digest in cache.items() if key[0] == 'font_digest'}
    for path, digest in font_bindings.items():
        require(sha(path) == digest, f'Font changed while measuring: {path}')
    return {'schema_version': 1, 'helper': 'native_fit', 'version': VERSION,
            'source_spec': str(Path(spec).resolve()), 'dependencies': dependencies,
            'font_map': {'path': str(font_path), 'sha256': sha(font_path)},
            'font_bindings': font_bindings,
            'objects': rows, 'proposed_edits': edits, 'mutated_source': False,
            'visual_quality_verified': False,
            'limitations': ['Font metrics propose placement; actual PowerPoint/LibreOffice output must still be inspected.',
                            'No owner/card inference, automatic approval or silent font fallback.',
                            'Font-family aliases and fallback in the target renderer require independent verification.']}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ['spec', 'fonts', 'out']: p.add_argument('--' + name, required=True)
    p.add_argument('--objects', nargs='+'); p.add_argument('--min-size', type=float)
    p.add_argument('--max-size', type=float); p.add_argument('--max-lines', type=int)
    p.add_argument('--ink-align', choices=['top', 'middle', 'bottom'], help='Align visible glyph ink inside current boxes or explicit targets')
    p.add_argument('--targets', help='JSON schema_version=1, objects=[{page,id,box}] in canvas pixels')
    a = p.parse_args()
    try:
        out = Path(a.out).resolve(); require(not out.exists(), 'Use a fresh proposal output')
        report = propose(a.spec, a.fonts, a.objects, a.min_size, a.max_size, a.max_lines, a.ink_align, a.targets)
        atomic_json(out/'report.json', report)
        atomic_json(out/'edits.json', report['proposed_edits'])
        import json
        from collections import Counter
        print(json.dumps({'report': str(out/'report.json'), 'proposed_edits': str(out/'edits.json'),
                          'states': dict(Counter(x['state'] for x in report['objects'])),
                          'visual_quality_verified': False}))
        return 0
    except (ValueError, KeyError, OSError, TypeError) as error:
        import json
        print(json.dumps({'state': 'error', 'error': str(error)})); return 1


if __name__ == '__main__':
    raise SystemExit(main())
