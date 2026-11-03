#!/usr/bin/env python3
"""Build + checked full/blank renders in one call; the agent still judges pixels."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import time

from checked_jobs import JobClient, JobError, JobPending, checked_render
from native_deck import atomic_json, build, read_json, require, sha
from render_cache import cached_render


def run(spec, output, workspace, page=None, seconds=35, size=None):
    root = Path(workspace).resolve(strict=True)
    output = Path(output).resolve()
    require(output.is_relative_to(root), 'Cycle output must be inside workspace')
    build_info = build(spec, output/'build', page)
    doc = read_json(output/'build/resolved.json')
    size = size or f"{int(doc['canvas']['width_px'])}x{int(doc['canvas']['height_px'])}"
    service = read_json(root/'tools/service.json')
    client = JobClient(service['url'])
    backend_identity = service.get('render_cache_identity')
    cache_root = ((root/service['render_cache_dir']).resolve()
                  if service.get('render_cache_dir') else output.parent/'.render-cache')

    def render(role):
        path = output/'build'/build_info['artifacts'][role+'_pptx']['path']
        try:
            args = (client, root, str(path.relative_to(root)),
                    str((output/'renders'/role).relative_to(root)), role)
            options = {'size': size, 'seconds': min(max(seconds, 0), 40)}
            result = (cached_render(*args, **options, cache_root=cache_root,
                                    backend_identity=backend_identity)
                      if backend_identity else checked_render(*args, **options))
            return role, {'state': 'verified', 'receipt': result}
        except JobPending as error:
            return role, {'state': 'pending', 'receipt': error.receipt}
        except JobError as error:
            return role, {'state': 'failed', 'error': str(error), 'receipt': error.receipt}

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = dict(pool.map(render, ['editable', 'blank']))
    state = ('failed' if any(v['state'] == 'failed' for v in results.values()) else
             'pending' if any(v['state'] == 'pending' for v in results.values()) else 'ready_for_visual_review')
    report = {'schema_version': 1, 'state': state, 'workspace': str(root),
              'build_record': str(output/'build/build.json'), 'build_record_sha256': sha(output/'build/build.json'),
              'render_size': size, 'renders': results, 'visual_quality_verified': False,
              'diagnostics': build_info['diagnostics'],
              'render_cache': {'enabled': bool(backend_identity),
                               'directory': str(cache_root) if backend_identity else None,
                               'disabled_reason': None if backend_identity else
                               'No explicit backend render_cache_identity supplied'}}
    if state == 'ready_for_visual_review':
        report['inspection_pages'] = {
            role: [str(root/value['receipt']['result']['out']/item['path'])
                   for item in value['receipt']['result']['report']['page_images']]
            for role, value in results.items()}
    previous_path = output/'cycle.json'
    if state == 'ready_for_visual_review' and previous_path.is_file():
        previous = read_json(previous_path)
        def stable(value):
            value = json.loads(json.dumps(value))
            for render in value.get('renders', {}).values():
                reuse = render.get('receipt', {}).get('render_reuse', {})
                reuse.pop('cache_hit', None)
            return value
        if stable(previous) == stable(report):
            # Freshness was revalidated above. Preserve the first completed record
            # so an identical resume cannot invalidate its inspection or handoff.
            return previous
    atomic_json(output/'cycle.json', report)
    return report


def handoff(job_path, cycle_path, review_path):
    """Package real agent inspection declarations; no automatically passing QA."""
    from validate_page_handoff import validate_pair
    job_path, cycle_path, review_path = map(lambda p: Path(p).resolve(strict=True),
                                         [job_path, cycle_path, review_path])
    job, cycle, review = map(read_json, [job_path, cycle_path, review_path])
    output = (job_path.parent/job['output_dir']).resolve(strict=True)
    require(cycle_path.is_relative_to(output), 'Cycle outside assigned output directory')
    require(not (output/'result.json').exists(), 'Preserve accepted handoff; use a fresh attempt')
    require(cycle['state'] == 'ready_for_visual_review', 'Cycle is not ready for visual inspection')
    require(review.get('open_findings') == [] and
            all(review.get('inspection', {}).get(k) is True
                for k in ['source', 'editable_render', 'blank_render']), 'Agent inspection is incomplete')
    build_path = Path(cycle['build_record']).resolve(strict=True)
    require(build_path.is_relative_to(output) and sha(build_path) == cycle['build_record_sha256'],
            'Build record changed or outside output directory')
    build_info = read_json(build_path)
    require(build_info['page_order'] == [job['page_id']], 'Handoff must cover its assigned page only')
    # Revalidate all exact build inputs and artifact hashes without rebuilding.
    for path, digest in build_info['dependencies'].items():
        require(sha(path) == digest, 'Build input changed after rendering')
    artifacts = {}
    for key, value in build_info['artifacts'].items():
        p = (build_path.parent/value['path']).resolve(strict=True)
        require(p.is_relative_to(output) and sha(p) == value['sha256'], 'Build artifact changed')
        artifacts[key] = {'path': str(p.relative_to(output)), 'sha256': value['sha256']}
    root = Path(cycle['workspace'])
    for role, result_key in [('editable', 'render'), ('blank', 'blank_render')]:
        receipt = cycle['renders'][role]['receipt']
        report = receipt['result']['report']
        source_hash = artifacts[role+'_pptx']['sha256']
        require(report['input_sha256'] == source_hash and receipt['verified_input_sha256'] == source_hash,
                'Stale render source binding')
        require(len(report['page_images']) == 1, 'Expected one rendered page')
        image = report['page_images'][0]
        path = (root/receipt['result']['out']/image['path']).resolve(strict=True)
        require(path.is_relative_to(output) and sha(path) == image['sha256'], 'Render changed or outside output')
        artifacts[result_key] = {'path': str(path.relative_to(output)), 'sha256': image['sha256']}
    qa = dict(review)
    for field, key in [('pptx_sha256', 'editable_pptx'), ('blank_pptx_sha256', 'blank_pptx'),
                       ('render_sha256', 'render'), ('blank_render_sha256', 'blank_render')]:
        qa[field] = artifacts[key]['sha256']
    qa['inspection_declaration_source'] = {'path': str(review_path), 'sha256': sha(review_path)}
    qa_path = cycle_path.parent/'handoff-qa.json'
    atomic_json(qa_path, qa)
    artifacts['qa'] = {'path': str(qa_path.relative_to(output)), 'sha256': sha(qa_path)}
    for key, path in [('build_record', build_path), ('cycle_record', cycle_path)]:
        artifacts[key] = {'path': str(path.relative_to(output)), 'sha256': sha(path)}
    result = {'schema_version': 1, 'page_id': job['page_id'], 'attempt_id': job['attempt_id'],
              'job_sha256': sha(job_path), 'status': 'complete', 'artifacts': artifacts}
    pending = output/'.result-pending.json'
    try:
        atomic_json(pending, result)
        validate_pair(job_path, pending)
        os.replace(pending, output/'result.json')
    finally:
        pending.unlink(missing_ok=True)
    return {'result': str(output/'result.json'), 'handoff_consistent': True, 'visual_quality_verified': False}


def input_evidence(doc):
    """Report available references and real pixels, without asserting review quality."""
    from PIL import Image
    pages = []
    for page in doc['pages']:
        row = {'page': page['id'], 'labeled_design': 'not_supplied'}
        for role in ['design', 'background']:
            asset = page.get(role)
            if not asset:
                continue
            with Image.open(asset['path']) as image:
                row[role + '_pixels'] = list(image.size)
            row[role + '_path'] = asset['path']
            if role == 'design':
                row['labeled_design'] = 'present'
        pages.append(row)
    return {'page_inputs': pages,
            'layout_coordinates': [doc['canvas']['width_px'], doc['canvas']['height_px']],
            'limits': ['Reference presence does not prove same-design text removal or preservation.',
                       'Pixel dimensions do not prove native generation resolution.',
                       'Manuscript completeness, source/blank fidelity and rendered readability require model review.',
                       'A missing labeled reference is a gap when the chosen task requires one; native-only tasks may have none.']}


def summarize(result, output, inspect=False, previous_cycle=None):
    """Compact machine results; the model receives image paths and explicit unknowns."""
    from collections import Counter
    display = dict(result)
    display['cycle_record'] = str((Path(output)/'cycle.json').resolve())
    display['renders'] = {role: {'state': value['state'],
                                'job_id': (value.get('receipt') or {}).get('id'),
                                'cache_hit': (value.get('receipt') or {}).get('render_reuse', {}).get('cache_hit', False),
                                'error': value.get('error')}
                          for role, value in result['renders'].items()}
    display.pop('diagnostics', None)
    resolved = read_json(Path(result['build_record']).parent/'resolved.json')
    display['workflow_evidence'] = input_evidence(resolved)
    display['workflow_evidence']['object_measurements'] = (
        'not_requested' if not inspect else
        'pending_render' if result['state'] == 'pending' else
        'render_failed' if result['state'] == 'failed' else 'requested')
    display['diagnostic_counts'] = {
        'box_overlap_candidates': len(result['diagnostics']['box_overlap_candidates']),
        'unreviewed_ocr_pages': len(result['diagnostics']['unreviewed_ocr_pages'])}
    if inspect and result['state'] == 'ready_for_visual_review':
        from native_inspect import run as inspect_cycle
        from native_review import summarize as summarize_review
        inspection_dir = Path(output)/'inspection'
        checked = inspect_cycle(Path(output)/'cycle.json', inspection_dir, previous_cycle=previous_cycle)
        display['workflow_evidence']['object_measurements'] = 'produced'
        display['inspection'] = {
            'report': str((inspection_dir/'report.json').resolve()),
            'objects': len(checked['objects']),
            'findings': dict(Counter(f['code'] for f in checked['findings'])),
            'objects_without_declared_owner': len(checked['ownership_summary']['without_declared_owner']),
            'semantic_ownership_verified': False,
            'changed_objects': (checked.get('revision_comparison') or {}).get('changed_count'),
            'review': summarize_review(checked),
            'evidence': [a['path'] for a in checked['artifacts']],
            'visual_quality_verified': False}
    return display


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('run')
    for key in ['spec', 'out', 'workspace']:
        p.add_argument('--'+key, required=True)
    p.add_argument('--page'); p.add_argument('--seconds', type=float, default=35); p.add_argument('--size')
    p.add_argument('--inspect', action='store_true', help='After a ready render, produce shared measurements and comparison panels')
    p = sub.add_parser('handoff')
    p.add_argument('--review', required=True)
    p.add_argument('--job'); p.add_argument('--cycle')
    p.add_argument('--revision', help='Publish a prepared revision after your own visual review')
    p.add_argument('--jobs', help='With --revision, write a new prepared-jobs receipt for assembly')
    p = sub.add_parser('revise', help='Allocate a fresh attempt, apply a batch, build and inspect; old artifacts stay unchanged')
    p.add_argument('--cycle'); p.add_argument('--edits'); p.add_argument('--job'); p.add_argument('--workspace')
    p.add_argument('--resume', help='Resume the same revision.json without allocating another attempt')
    p.add_argument('--seconds', type=float, default=35); p.add_argument('--size')
    p.add_argument('--no-inspect', action='store_true', help='Collect render evidence only; model must still inspect before handoff')
    args = parser.parse_args(argv)
    revision = None
    try:
        if args.command == 'run':
            result = run(args.spec, args.out, args.workspace, args.page, args.seconds, args.size)
            display = summarize(result, args.out, args.inspect)
        elif args.command == 'revise':
            import native_revise
            if args.resume:
                require(not any([args.cycle, args.edits, args.job, args.workspace]),
                        'Use --resume alone with render options; revision inputs are already bound')
                revision = args.resume
            else:
                require(args.cycle and args.edits and args.workspace, 'Revision needs --cycle, --edits and --workspace')
                revision = native_revise.prepare(args.cycle, args.edits, args.workspace, args.job)
            result = display = native_revise.run(revision, args.seconds, args.size, not args.no_inspect)
        elif args.revision:
            import native_revise
            require(not args.job and not args.cycle, 'Revision already binds the job and cycle')
            result = display = native_revise.publish(args.revision, args.review, args.jobs)
        else:
            require(args.job and args.cycle and not args.jobs, 'Handoff needs --job and --cycle, or --revision')
            result = handoff(args.job, args.cycle, args.review)
            display = dict(result)
        print(json.dumps(display, ensure_ascii=False, indent=2))
        marker_source = result
        if args.command == 'revise':
            marker_source = read_json(result['cycle_record'])
        for value in marker_source.get('renders', {}).values():
            marker = (value.get('receipt') or {}).get('octos_external_job')
            if marker:
                print(json.dumps({'octos_external_job': marker}, ensure_ascii=False))
        return 2 if result.get('state') == 'pending' else 1 if result.get('state') == 'failed' else 0
    except (ValueError, KeyError, TypeError, OSError) as error:
        failure = {'state': 'error', 'error': str(error)}
        if revision is not None:
            failure['revision'] = str(revision)
            failure['note'] = 'The failed attempt is preserved. Diagnose it before resuming; do not overwrite the accepted predecessor.'
        print(json.dumps(failure, ensure_ascii=False))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
