"""Compact, object-level inspection evidence. No verdict or repair policy."""
from collections import Counter


def summarize(report, limit=12):
    objects = {(r['page'], r['id']): r for r in report['objects']}
    grouped = {}
    for finding in report['findings']:
        key = (finding['page'], finding['object'])
        grouped.setdefault(key, []).append(finding)
    comparison = report.get('comparison') or {}
    persistent = {tuple(k) for k in comparison.get('persisting', [])}
    new = {tuple(k) for k in comparison.get('new', [])}
    artifacts = report.get('artifacts', [])
    rows = []
    for (page, ident), findings in grouped.items():
        obj = objects.get((page, ident), {})
        owner = obj.get('owner')
        rows.append({
            'page': page, 'object': ident,
            'findings': [{'key': [page, ident, f['code']], 'severity': f['severity'],
                          'change': ('persisting' if (page, ident, f['code']) in persistent else
                                     'new' if (page, ident, f['code']) in new else 'uncompared'),
                          'details': f['details']} for f in findings],
            'text': obj.get('text', obj.get('canonical_text')),
            'owner': ({k: owner.get(k) for k in ['id', 'box_px', 'area_to_frame_ratio']} |
                      {'assigned_object_count': len(owner.get('assigned_objects', []))}) if owner else None,
            'overlay_index': obj.get('overlay_index'),
            'ink_attribution': obj.get('ink_attribution'),
            'frame_clearance_px': obj.get('frame_clearance_px'),
            'owner_clearance_px': obj.get('owner_clearance_px'),
            'evidence': [a['path'] for a in artifacts if a['page'] == page and
                         (ident in a.get('objects', []) or ident == '__page__' and a['kind'] == 'overview')],
        })
    # Present certain structural errors and measured owner overflow before ambiguous candidates.
    def priority(row):
        fs = row['findings']
        return (0 if any(f['severity'] == 'error' for f in fs) else
                1 if any(f['key'][2] == 'INK_OUTSIDE_OWNER_CANDIDATE' for f in fs) else
                2 if any(f['change'] == 'persisting' for f in fs) else 3, row['page'], row['object'])
    rows.sort(key=priority)
    pages = sorted({row['page'] for row in rows})
    return {
        'findings': dict(Counter(f['code'] for f in report['findings'])),
        'coordinate_units': 'canvas pixels; negative clearance means measured ink crosses the declared boundary',
        'finding_comparison': comparison or {'state': 'no_previous_inspection'},
        'affected_pages': pages, 'affected_objects': len(rows),
        'object_findings': rows[:limit], 'omitted_objects': max(0, len(rows)-limit),
        'more_findings_in_report': len(rows) > limit,
        'objects_without_declared_owner': report['ownership_summary']['without_declared_owner'],
        'semantic_ownership_verified': False, 'visual_quality_verified': False,
        'interpretation': 'Candidates need visual judgment. Unchanged findings remain candidates; no-longer-detected is not visual approval.',
    }
