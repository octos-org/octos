"""Legacy metadata helper for explicitly host-managed application workflows.

The native Octos PPTX peer workflow does not use this as a scheduler: its Octos
coordinator gathers publications, assigns review and routes repairs itself. Do
not execute the returned prompt or interrupt a writer automatically in that flow.

Call after a maker tool/completion event from an already-owned peer connection.
The caller queues the returned job on the existing reviewer's owned connection
and persists its id once enqueued. This module has no polling loop or transport.
Native page/review admission remains authoritative.
"""
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import stat

MAX_BYTES = 64 * 1024 * 1024

def execution_state(root_active, peer_states, verified_complete, blocked, task_status, terminal_control):
    """Host liveness only; a task's `complete` text is not completion evidence."""
    states = set(peer_states)
    active = root_active or bool(states & {'running','opening','starting'})
    if active:
        return 'running'
    if verified_complete:
        return 'complete'
    if blocked or task_status == 'blocked' or terminal_control in {'evidence_stalled','replan','error','errored','failed','interrupted'}:
        return 'blocked'
    if 'queued' in states:
        return 'queued'
    return 'idle_incomplete'

def _read(root, relative):
    path = PurePosixPath(relative)
    # Native resolved-spec asset bindings use absolute paths. Convert only
    # lexical descendants; open-at below still rejects every symlink component.
    if path.is_absolute():
        path = path.relative_to(PurePosixPath(root))
    if not path.parts or any(p in ('..', '.') for p in path.parts):
        raise ValueError('expected a file inside the workspace')
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for component in path.parts[:-1]:
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd); fd = child
        child = os.open(path.parts[-1], os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd)
        try:
            info = os.fstat(child)
            if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_BYTES:
                raise ValueError('not a bounded regular file')
            with os.fdopen(child, 'rb', closefd=False) as stream:
                data = stream.read(MAX_BYTES + 1)
            if len(data) > MAX_BYTES:
                raise ValueError('file exceeds handoff read bound')
            return data
        finally:
            os.close(child)
    finally:
        os.close(fd)

def _bound(root, binding):
    data = _read(root, binding['path'])
    if hashlib.sha256(data).hexdigest() != binding['sha256']:
        raise ValueError('candidate binding changed')
    return data

def prepare_review_handoff(workspace, producer_session, assignments, peers, seen):
    """Return one independent-review job or None when no fresh handoff is ready.

    producer_session and peers must come from the host's actual peer bindings,
    not model-authored result text. A valid hash is readiness to request review,
    not proof of editability, visual correctness or peer execution.
    """
    try:
        root = Path(workspace).resolve(strict=True)
        plan = json.loads(_read(root, '.octos/pptx-page-plan.json'))
        rows = [r for r in plan['pages'] if r['maker_session'] == producer_session]
        if len(rows) != 1:
            return None
        row = rows[0]; page = row['id']
        raw = _read(root, row['candidate']); digest = hashlib.sha256(raw).hexdigest()
        candidate = json.loads(raw)
        if candidate['page_id'] != page or candidate['maker_session'] != producer_session:
            return None
        for key in ('source','build_receipt','artifact','render','render_receipt'):
            _bound(root, candidate[key])
        for key in ('dependencies','receipts'):
            for binding in candidate[key]:
                _bound(root, binding)
        cycle = json.loads(_bound(root, candidate['build_receipt']))
        if cycle['pages'] != [page]:
            return None
        reviewers = [(slug, r) for slug, r in assignments['reviewers'].items() if page in r['pages']]
        if len(reviewers) != 1:
            return None
        slug, reviewer = reviewers[0]; session = reviewer['session_id']
        makers = {r['maker_session'] for r in plan['pages']}
        if session in makers or session == plan['coordinator_session']:
            return None
        if peers.get(slug, {}).get('session_id') != session:
            return None
        score = float(plan.get('min_score',95))
        if not math.isfinite(score) or not 0 <= score <= 100:
            return None
        job_id = session + ':' + page + ':' + digest
        if job_id in seen:
            return None
        return dict(id=job_id, page_id=page, reviewer_slug=slug, reviewer_session=session,
            maker_session=producer_session, candidate=dict(path=row['candidate'],sha256=digest),
            cycle=candidate['build_receipt'], review_path=row['review'],
            prompt=(f"Independently review {page}. Candidate: {row['candidate']}; "
                f"cycle: {candidate['build_receipt']['path']}. Use page-scoped native review evidence, "
                f"actual source/render pixels in bounded views, and the existing rubric. Acceptance requires "
                f">={score:g}/100, no critical defects or open required findings. Preserve unknowns. "
                f"Write the bound page verdict to {row['review']} with real review_submit evidence; "
                f"publish the complete defect batch. Do not edit the candidate or award an unsupported pass."))
    except (OSError, ValueError, TypeError, KeyError):
        return None
