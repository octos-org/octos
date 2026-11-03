#!/usr/bin/env python3
"""Immutable page revisions: allocate once, resume work, publish after model review."""
import copy
import os
from pathlib import Path
import tempfile

from native_deck import atomic_json, load_spec, patch_spec, read_json, require, sha
from validate_page_handoff import bound_file, validate_pair


def bound(path, digest):
    path = Path(path).resolve(strict=True)
    require(sha(path) == digest, f'Revision input changed: {path}')
    return path


def prepare(cycle_path, edits_path, workspace, job_path=None):
    root = Path(workspace).resolve(strict=True)
    cycle_path, edits_path = Path(cycle_path).resolve(strict=True), Path(edits_path).resolve(strict=True)
    cycle = read_json(cycle_path)
    require(cycle['state'] == 'ready_for_visual_review', 'Finish the current render before revising it')
    require(Path(cycle['workspace']).resolve() == root and cycle_path.is_relative_to(root), 'Wrong revision workspace')
    record_path = bound(cycle['build_record'], cycle['build_record_sha256'])
    record = read_json(record_path)
    for p, digest in record['dependencies'].items(): bound(p, digest)
    files = {name: bound(record_path.parent/value['path'], value['sha256'])
             for name, value in record['artifacts'].items()}
    original, _ = load_spec(files['resolved_spec'])
    changes = read_json(edits_path)
    require(isinstance(changes, list) and changes, 'Supply a nonempty batch of object changes')
    job, parent_job = None, None
    if job_path:
        parent_job = Path(job_path).resolve(strict=True); job = read_json(parent_job)
        require(record['page_order'] == [job['page_id']], 'Job and cycle page differ')
        base = (parent_job.parent/job['output_dir']).resolve(strict=True)
        require(cycle_path.is_relative_to(base), 'Cycle is outside the assigned output directory')
        bound_file(parent_job.parent, job['shared_contract'])
        for item in job['inputs']: bound_file(parent_job.parent, item)
        if (base/'result.json').exists(): validate_pair(parent_job, base/'result.json')
        require(all(e['page'] == job['page_id'] for e in changes), 'Revision may only edit its assigned page')
    else:
        base = cycle_path.parent
    require(base.is_relative_to(root), 'Revision output is outside workspace')
    folder = base/'revisions'; folder.mkdir(exist_ok=True)
    require(folder.resolve().is_relative_to(base), 'Revision directory escapes the owned output')
    target = Path(tempfile.mkdtemp(prefix='attempt-', dir=folder))
    # Source and accepted artifacts remain untouched even if a later build fails.
    try:
        atomic_json(target/'edits.json', changes)
        patch_spec(files['resolved_spec'], target/'edits.json', target/'source.json')
        new_job = None
        if job:
            updated = copy.deepcopy(job)
            updated['attempt_id'] = target.name
            updated['output_dir'] = 'output'
            for item in [updated['shared_contract'], *updated['inputs']]:
                item['path'] = os.path.relpath((parent_job.parent/item['path']).resolve(strict=True), target)
            updated['inputs'].append({'role': 'revision-spec', 'path': 'source.json', 'sha256': sha(target/'source.json')})
            atomic_json(target/'job.json', updated)
            new_job = str(target/'job.json')
        (target/'output').mkdir()
        dependencies = {str(cycle_path): sha(cycle_path), str(record_path): sha(record_path),
                        str(target/'source.json'): sha(target/'source.json'),
                        str(target/'edits.json'): sha(target/'edits.json')}
        if new_job: dependencies[new_job] = sha(new_job)
        if parent_job: dependencies[str(parent_job)] = sha(parent_job)
        revision = {'schema_version': 1, 'workspace': str(root), 'source_cycle': str(cycle_path),
                    'parent_job': str(parent_job) if parent_job else None, 'job': new_job,
                    'source_spec': str(target/'source.json'), 'cycle_output': str(target/'output/cycle'),
                    'result': str(target/'output/result.json') if job else None,
                    'dependencies': dependencies, 'page_order': record['page_order'],
                    'objects_changed': [{'page': e['page'], 'id': e['id']} for e in changes],
                    'visual_quality_verified': False}
        atomic_json(target/'revision.json', revision)
        return target/'revision.json'
    except Exception:
        # Preserve an inspectable failed preparation; never publish a usable revision record.
        atomic_json(target/'preparation-failed.json', {'state': 'failed', 'source_unchanged': True})
        raise


def load_revision(path):
    path = Path(path).resolve(strict=True); value = read_json(path)
    require(value.get('schema_version') == 1, 'Unknown revision version')
    for p, digest in value['dependencies'].items(): bound(p, digest)
    return path, value


def run(path, seconds=35, size=None, inspect=True):
    from native_cycle import run as run_cycle, summarize
    path, revision = load_revision(path)
    result = run_cycle(revision['source_spec'], revision['cycle_output'], revision['workspace'],
                       seconds=seconds, size=size)
    display = summarize(result, revision['cycle_output'], inspect=inspect,
                        previous_cycle=revision['source_cycle'])
    display.update(revision=str(path), job=revision['job'], result=revision['result'],
                   objects_changed=revision['objects_changed'])
    return display


def publish(path, review, jobs=None):
    """An explicit model review is still required. Publish a new receipt, never rewrite it."""
    from native_cycle import handoff
    path, revision = load_revision(path)
    require(revision['job'] is not None, 'Solo revisions have no page-job handoff')
    receipt, selected = None, None
    if jobs:
        receipt = read_json(jobs)
        matches = [e for e in receipt['jobs'] if e['page_id'] == revision['page_order'][0]]
        require(len(matches) == 1, 'Receipt does not contain exactly one assigned page')
        selected = matches[0]
        require(Path(selected['job']).resolve() == Path(revision['parent_job']).resolve(),
                'Receipt already refers to a different attempt; merge that revision explicitly')
        require(not (path.parent/'updated-jobs.json').exists(), 'Updated receipt already exists')
    result_path = Path(revision['result'])
    if result_path.exists():
        # Safe retry after a successful publish: accept only the same original review bytes.
        checked = validate_pair(revision['job'], result_path)
        qa = read_json(checked['files']['qa'])
        require(qa['inspection_declaration_source']['sha256'] == sha(review), 'Published review differs')
        result = {'result': str(result_path), 'handoff_consistent': True, 'visual_quality_verified': False}
    else:
        result = handoff(revision['job'], Path(revision['cycle_output'])/'cycle.json', review)
    if receipt:
        selected.update(job=revision['job'], result=revision['result'])
        selected.pop('spawn_request', None)  # This revised job is already completed.
        selected['input_roles'] = [i['role'] for i in read_json(revision['job'])['inputs']]
        out = path.parent/'updated-jobs.json'; atomic_json(out, receipt)
        result['updated_jobs'] = str(out)
    return result
