#!/usr/bin/env python3
"""Explicit checked-cycle wiring and receipts for Octos; never approves artwork."""
import argparse
import hashlib
import json
from pathlib import Path
from native_deck import atomic_json, read_json, require, sha

DOMAINS = ('copy', 'artwork', 'readability')


def inside(root, path):
    path = Path(path)
    if not path.is_absolute(): path = root / path
    path = path.resolve(strict=True)
    require(path.is_file() and path.is_relative_to(root), 'Evidence must be a workspace file')
    return path


def init(workspace, task_id, owner_agent, deliverables, window=3):
    root = Path(workspace).resolve(strict=True)
    require(task_id and len(task_id) <= 256 and owner_agent and 1 <= window <= 2**32-1, 'Invalid task/owner/window')
    require(deliverables and all(not Path(p).is_absolute() and '..' not in Path(p).parts for p in deliverables), 'Declare relative delivery paths')
    target = root/'.octos/continuation.json'
    target.parent.mkdir(exist_ok=True)
    require(target.parent.resolve().is_relative_to(root), 'Policy directory escapes workspace')
    require(not target.exists() and not (root/'.octos/continuation-state.json').exists(),
            'Preserve the current task policy and history; use a new task workspace')
    policy = {'schema_version': 1, 'task_id': task_id, 'owner_agent': owner_agent,
              'evidence_file': '.octos/iteration-evidence.json', 'stagnant_attempts': window,
              'deliverables': deliverables,
              'required_checks': [{'id': c, 'epsilon': 0} for c in ('package', *DOMAINS)]}
    atomic_json(target, policy)
    return {'policy': str(target), 'quality_approved': False}


def status(workspace, owner_agent):
    """Read persisted observer evidence; policy existence alone is not activation."""
    root = Path(workspace).resolve(strict=True)
    result = {'configured': False, 'runtime_consumed_current_evidence': False,
              'expected_owner': owner_agent, 'quality_approved': False,
              'process_liveness_verified': False, 'errors': []}
    try:
        policy_path = inside(root, '.octos/continuation.json')
        policy = read_json(policy_path)
        require(isinstance(policy, dict), 'Policy must be an object')
        require(policy.get('schema_version') == 1, 'Unsupported policy version')
        require(owner_agent and policy.get('owner_agent') == owner_agent,
                'Policy owner does not match the actual execution identity')
        require(policy.get('task_id') and policy.get('stagnant_attempts', 0) > 0,
                'Missing task or invalid stagnation window')
        require(policy.get('required_checks') and policy.get('deliverables'),
                'Missing required checks or deliverables')
        result.update(configured=True, task_id=policy['task_id'],
                      stagnant_attempts=policy['stagnant_attempts'])
        evidence_file = root / policy['evidence_file']
        require(evidence_file.resolve().is_relative_to(root), 'Evidence path escapes workspace')
        state_file = root / '.octos/continuation-state.json'
        if not evidence_file.exists() or not state_file.exists():
            result['note'] = 'Configured; current checked evidence has not been observed in runtime state'
            return result
        evidence_path = inside(root, evidence_file)
        evidence = read_json(evidence_path)
        state = read_json(inside(root, state_file))
        require(isinstance(evidence, dict) and isinstance(state, dict), 'Evidence/state must be objects')
        require(evidence.get('schema_version') == 1 and evidence.get('task_id') == policy['task_id'],
                'Evidence belongs to another task/version')
        result.update(evidence_attempt=evidence.get('attempt'), runtime_attempt=state.get('attempt'),
                      flat_attempts=state.get('flat_attempts'), warned=state.get('warned'))
        result['runtime_consumed_current_evidence'] = (
            evidence.get('phase') == 'checked'
            and isinstance(evidence.get('attempt'), int) and not isinstance(evidence['attempt'], bool)
            and evidence['attempt'] > 0
            and state.get('policy_digest') == sha(policy_path)
            and state.get('snapshot_digest') == sha(evidence_path)
            and state.get('attempt') == evidence['attempt'])
        result['note'] = ('Matching persisted runtime observation; not a visual pass or live-process check'
                          if result['runtime_consumed_current_evidence']
                          else 'Runtime state is absent, stale or does not match this checked evidence')
    except (OSError, ValueError, KeyError, TypeError) as exc:
        result['errors'].append(str(exc))
    return result


def review_checks(pages, review, previous_findings):
    """Interpret explicit self-review, retaining omitted unresolved finding IDs."""
    findings = dict(previous_findings)
    seen = set()
    for finding in review.get('findings', []):
        fid = finding.get('id')
        require(isinstance(fid, str) and fid and fid not in seen, 'Duplicate/missing finding ID')
        seen.add(fid)
        require(finding.get('check') in DOMAINS, 'Unknown review category')
        require(finding.get('state') in ('open', 'unknown', 'resolved', 'dismissed'), 'Unknown finding state')
        require(finding.get('note') and finding.get('evidence'), 'Finding needs reason and current visual evidence')
        if fid in findings: require(findings[fid]['check'] == finding['check'], 'Preserve finding category')
        findings[fid] = finding
    inspected = review.get('inspected', {})
    checks = []
    for check in DOMAINS:
        coverage = inspected.get(check, [])
        require(isinstance(coverage, list) and len(set(coverage)) == len(coverage)
                and set(coverage) <= set(pages), 'Invalid/duplicate inspected page IDs')
        missing = len(set(pages) - set(coverage))
        active = [f for f in findings.values() if f['check'] == check and f['state'] in ('open', 'unknown')]
        unknown = missing or any(f['state'] == 'unknown' for f in active)
        failed = any(f['state'] == 'open' for f in active)
        checks.append({'id': check, 'status': 'fail' if failed else 'unknown' if unknown else 'pass',
                       'remaining': len(active) + missing})
    return checks, findings


def record(workspace, cycle, review_path=None):
    from native_inspect import bound_cycle
    from check_pptx_compat import inspect
    root = Path(workspace).resolve(strict=True)
    policy = read_json(inside(root, '.octos/continuation.json'))
    cycle_path = inside(root, cycle)
    # This helper verifies full/blank artifacts, renderer inputs, rendered pages,
    # source dependencies and document page order before reporting a checked cycle.
    doc, renders, bindings = bound_cycle(cycle_path)
    pages = [p['id'] for p in doc['pages']]
    require(pages and len(set(pages)) == len(pages), 'Missing/duplicate page IDs')
    book_path = root/'.octos/pptx-evidence-history.json'
    book = read_json(inside(root, book_path)) if book_path.exists() else {}
    require(not book or book.get('task_id') == policy['task_id'], 'Task changed without new history')
    require(not book or book.get('pages') == pages, 'Page inventory changed; reconcile task scope explicitly')
    review = read_json(inside(root, review_path)) if review_path else {}
    if review_path:
        require(review.get('cycle_sha256') == sha(cycle_path), 'Review belongs to another candidate')
        bindings[str(inside(root, review_path))] = sha(inside(root, review_path))
        # Human/model judgment stays explicit. Hashes prove which images were
        # referenced, not that the reviewer interpreted them correctly.
        rendered_images = {str(p) for render in renders.values() for p in render['images']}
        for finding in review.get('findings', []):
            image = inside(root, finding.get('evidence', ''))
            require(str(image) in rendered_images, 'Finding must reference this candidate\'s rendered image')
    checks, findings = review_checks(pages, review, book.get('findings', {}))
    package = inspect(renders['editable']['pptx'])
    package_bad = package['issue_count'] + (1 if package['counts'].get('slides', 0) == 0 else 0)
    checks.insert(0, {'id': 'package', 'status': 'fail' if package_bad else 'pass', 'remaining': package_bad})
    # Preserve the full scoped diagnostics alongside, without promoting detector
    # candidates into confirmed visual defects or silently clearing unknowns.
    detail = root/'.octos/pptx-evidence-detail.json'
    atomic_json(detail, {'task_id': policy['task_id'], 'cycle': str(cycle_path),
                        'package_preflight': package, 'self_review': review,
                        'findings': findings, 'visual_quality_verified_by_tool': False})
    # All kernel-visible bindings are confined to the task workspace. Shared
    # builder dependencies were checked above but do not grant kernel file access.
    artifacts = [{'path': str(Path(p).relative_to(root)), 'sha256': digest}
                 for p, digest in sorted(bindings.items()) if Path(p).is_relative_to(root)]
    # Only bind a published delivery when it matches the inspected PPTX bytes.
    # Publishing remains the producer's action; this helper never copies/edits it.
    for relative in policy['deliverables']:
        delivery = root / relative
        if delivery.is_file():
            delivery = inside(root, delivery)
            if sha(delivery) == sha(renders['editable']['pptx']):
                item = {'path': str(delivery.relative_to(root)), 'sha256': sha(delivery)}
                if item not in artifacts: artifacts.append(item)
    payload = {'schema_version': 1, 'task_id': policy['task_id'], 'phase': 'checked',
               'artifacts': artifacts, 'checks': checks}
    key = hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest()
    out = root/policy['evidence_file']
    require(out.parent.resolve().is_relative_to(root), 'Evidence output escapes workspace')
    old = read_json(inside(root, out)) if out.exists() else {}
    attempt = book.get('attempt', 0) if book.get('key') == key else max(book.get('attempt', 0), old.get('attempt', 0)) + 1
    payload['attempt'] = attempt
    atomic_json(book_path, {'task_id': policy['task_id'], 'pages': pages, 'attempt': attempt,
                           'key': key, 'findings': findings})
    atomic_json(out, payload)
    return {'evidence': str(out), 'attempt': attempt, 'checks': checks,
            'visual_quality_verified_by_tool': False,
            'review_provenance': 'package: deterministic preflight; copy/artwork/readability: producer self-review'}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='action', required=True)
    i = sub.add_parser('init'); i.add_argument('--workspace', required=True); i.add_argument('--task-id', required=True); i.add_argument('--owner-agent', required=True); i.add_argument('--deliverable', dest='deliverables', action='append', required=True); i.add_argument('--window', type=int, default=3)
    r = sub.add_parser('record'); r.add_argument('--workspace', required=True); r.add_argument('--cycle', required=True); r.add_argument('--review')
    s = sub.add_parser('status'); s.add_argument('--workspace', required=True); s.add_argument('--owner-agent', required=True); s.add_argument('--require-consumed', action='store_true')
    args = vars(p.parse_args()); action = args.pop('action')
    if action == 'status':
        require_consumed = args.pop('require_consumed')
        result = status(**args)
        print(json.dumps(result, ensure_ascii=False, indent=2))
        if result['errors'] or not result['configured'] or (require_consumed and not result['runtime_consumed_current_evidence']):
            raise SystemExit(2)
        return
    if action == 'record': args['review_path'] = args.pop('review')
    print(json.dumps((init if action == 'init' else record)(**args), ensure_ascii=False, indent=2))


if __name__ == '__main__': main()
