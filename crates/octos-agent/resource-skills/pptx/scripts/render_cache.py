"""Exact-input render reuse. Cache identity is a host-owned backend contract.

Only ZIP serialization is canonicalized; every uncompressed package member stays
byte-identical, including metadata XML. No layout or rendering dependency is
discarded. A backend identity must change whenever its service lifetime, renderer,
fonts or rendering configuration changes. Without it native_cycle does not cache.

Default caches live under the worker's owned output parent. A shared cache is an
explicit host option, not permission to write to another worker's output tree.
"""
from contextlib import contextmanager
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import time
import zipfile

CACHE_VERSION = 1


def digest(path):
    value = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False,
                      separators=(',', ':'), allow_nan=False).encode()


def atomic_json(path, value):
    path = Path(path)
    fd, name = tempfile.mkstemp(prefix='.record-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(encoded(value) + b'\n')
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def canonicalize_pptx(path):
    """Normalize ZIP packaging without changing any package member bytes."""
    path = Path(path)
    fd, name = tempfile.mkstemp(prefix='.pptx-', suffix='.pptx', dir=path.parent)
    os.close(fd)
    try:
        with zipfile.ZipFile(path) as source, zipfile.ZipFile(name, 'w') as target:
            names = source.namelist()
            if len(names) != len(set(names)):
                raise ValueError('Duplicate PPTX ZIP members cannot be canonicalized safely')
            for member in sorted(names):
                item = zipfile.ZipInfo(member, (1980, 1, 1, 0, 0, 0))
                item.create_system = 3
                item.compress_type = zipfile.ZIP_DEFLATED
                item.external_attr = (0o755 if member.endswith('/') else 0o600) << 16
                target.writestr(item, source.read(member), compresslevel=6)
        if digest(path) != digest(name):
            os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


@contextmanager
def locked(path, seconds=10):
    """A short publication lock, never held while a backend job is running."""
    from checked_jobs import JobError
    with path.open('a+b') as stream:
        if os.name == 'nt':
            import msvcrt
            if stream.tell() == 0:
                stream.write(b'\0'); stream.flush()
            def acquire():
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
            def release():
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            def acquire():
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            def release():
                fcntl.flock(stream, fcntl.LOCK_UN)
        deadline = time.monotonic() + seconds
        while True:
            try:
                acquire()
                break
            except (BlockingIOError, PermissionError):
                if time.monotonic() >= deadline:
                    raise JobError('Render cache publication is busy; resume the same cycle')
                time.sleep(.05)
        try:
            yield
        finally:
            release()


def confined(base, relative):
    from checked_jobs import JobError
    value = Path(relative)
    result = (base / value).resolve()
    if value.is_absolute() or not result.is_relative_to(base):
        raise JobError('Render cache artifact escapes its bound directory')
    return result


def inventory(output):
    """Keep public renderer results; omit transient LO caches and input copies."""
    files = {}
    for path in output.rglob('*'):
        rel = path.relative_to(output)
        if any(part.startswith('.') for part in rel.parts):
            continue
        if path.is_symlink():
            raise ValueError('Render cache does not accept symlink outputs')
        if path.is_file():
            files[str(rel)] = digest(path)
    return files


def verify_outputs(output, files, receipt, source_hash, size):
    from checked_jobs import JobError, png_dimensions, pptx_page_count
    if not isinstance(files, dict) or not files:
        raise JobError('Render cache has no artifact inventory')
    for relative, expected in files.items():
        path = confined(output, relative)
        if not path.is_file() or path.is_symlink() or digest(path) != expected:
            raise JobError('Render cache artifact changed: ' + relative)
    result = receipt.get('result', {})
    report = result.get('report', {})
    if (receipt.get('status') != 'completed' or
            receipt.get('verified_input_sha256') != source_hash or
            result.get('snapshot_sha256') != source_hash or
            report.get('input_sha256') != source_hash):
        raise JobError('Render cache source binding changed')
    report_path = confined(output, 'render-report.json')
    if 'render-report.json' not in files or json.loads(report_path.read_text()) != report:
        raise JobError('Render cache report differs from its completed receipt')
    dimensions = list(map(int, size.split('x')))
    images = report.get('page_images', [])
    expected_pages = list(range(1, report.get('pages', 0) + 1))
    if (not expected_pages or report.get('export_pixels') != dimensions or
            report.get('rendered_pages') != expected_pages or
            [item.get('page') for item in images] != expected_pages):
        raise JobError('Render cache page or dimension coverage changed')
    for item in images:
        path = confined(output, item['path'])
        if files.get(item['path']) != item.get('sha256') or png_dimensions(path) != dimensions:
            raise JobError('Render cache image binding changed')
    pdf_hash = report.get('pdf_sha256')
    if pdf_hash and sum(d == pdf_hash for p, d in files.items() if p.lower().endswith('.pdf')) != 1:
        raise JobError('Render cache PDF binding changed')


def cached_render(client, workspace, pptx, output_base, attempt, *, size, seconds,
                  cache_root, backend_identity):
    """Reuse identical source bytes, dimensions and explicitly versioned backend.

    Concurrent requests submit the SAME durable job ID. Pending attempts keep
    that ID; finished results are copied into each owner's output with provenance.
    Cache hits never transfer another page's visual-approval declaration.
    """
    from checked_jobs import JobError, checked_render, confined as workspace_path, pptx_page_count
    root = Path(workspace).resolve()
    source = workspace_path(root, pptx)
    base = workspace_path(root, output_base)
    cache_root = Path(cache_root).resolve()
    if not cache_root.is_relative_to(root):
        raise JobError('Render cache must remain inside workspace')
    if not backend_identity:
        raise JobError('Render reuse requires an explicit backend identity')
    source_hash = digest(source)
    identity = {'cache_version': CACHE_VERSION, 'service_url': client.url,
                'backend_identity': backend_identity, 'input_sha256': source_hash, 'size': size}
    key = hashlib.sha256(encoded(identity)).hexdigest()
    entry = cache_root/key
    entry.mkdir(parents=True, exist_ok=True)
    record_path = entry/'completed.json'
    input_path = entry/'source.pptx'
    with locked(entry/'.publication.lock'):
        if input_path.exists():
            if input_path.is_symlink() or digest(input_path) != source_hash:
                raise JobError('Render cache immutable input changed')
        else:
            fd, temporary = tempfile.mkstemp(prefix='.input-', dir=entry)
            os.close(fd)
            try:
                shutil.copyfile(source, temporary)
                if digest(temporary) != source_hash:
                    raise JobError('Source changed while entering render cache')
                os.replace(temporary, input_path)
            finally:
                Path(temporary).unlink(missing_ok=True)
        record = json.loads(record_path.read_text()) if record_path.exists() else None
    reused = record is not None
    if record is None:
        # Repeated/concurrent submissions have identical paths, bytes, size and ID.
        receipt = checked_render(client, root, str(input_path.relative_to(root)),
                                 str((entry/'render').relative_to(root)), 'cache',
                                 size=size, seconds=seconds)
        rendered = workspace_path(root, receipt['result']['out'])
        files = inventory(rendered)
        verify_outputs(rendered, files, receipt, source_hash, size)
        if receipt['result']['report']['pages'] != pptx_page_count(input_path):
            raise JobError('Render cache page count differs from source')
        proposed = {'identity': identity, 'receipt': receipt, 'files': files}
        with locked(entry/'.publication.lock'):
            if record_path.exists():
                record = json.loads(record_path.read_text())
                reused = True
            else:
                atomic_json(record_path, proposed)
                record = proposed
    if record.get('identity') != identity:
        raise JobError('Render cache identity changed')
    origin = record['receipt']
    rendered = workspace_path(root, origin['result']['out'])
    if not rendered.is_relative_to(entry):
        raise JobError('Render cache result is outside its cache entry')
    verify_outputs(rendered, record['files'], origin, source_hash, size)
    if digest(source) != source_hash or digest(input_path) != source_hash:
        raise JobError('Render cache source changed during verification')
    if origin['result']['report']['pages'] != pptx_page_count(source):
        raise JobError('Render cache source page count changed')

    # Each page handoff keeps its own immutable copy, never a shared writable link.
    base.mkdir(parents=True, exist_ok=True)
    output = base/('reuse-'+key[:24])
    provenance = {'cache_key': key, 'cache_record_sha256': digest(record_path),
                  'source_job_id': origin['id'], 'source_out': origin['result']['out'],
                  'input_sha256': source_hash, 'files': record['files']}
    with locked(entry/'.publication.lock'):
        if output.exists():
            marker = output/'.reuse-record.json'
            if not marker.is_file() or json.loads(marker.read_text()) != provenance:
                raise JobError('Existing render copy has different provenance')
            verify_outputs(output, record['files'], origin, source_hash, size)
        else:
            temporary = Path(tempfile.mkdtemp(prefix='.render-copy-', dir=base))
            try:
                for relative in record['files']:
                    target = confined(temporary, relative)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(confined(rendered, relative), target)
                verify_outputs(temporary, record['files'], origin, source_hash, size)
                atomic_json(temporary/'.reuse-record.json', provenance)
                os.replace(temporary, output)
            finally:
                if temporary.exists():
                    shutil.rmtree(temporary)
    result = copy.deepcopy(origin)
    result['result']['out'] = str(output.relative_to(root))
    result['result']['files'] = [str((output/rel).relative_to(root)) for rel in sorted(record['files'])]
    result['render_reuse'] = {
        'cache_hit': reused, 'source_job_id': origin['id'], 'source_out': origin['result']['out'],
        'source_receipt_sha256': hashlib.sha256(encoded(origin)).hexdigest(),
        'cache_record_sha256': digest(record_path), 'identity': identity,
        'materialization': 'verified artifact copies; identical source bytes',
        'backend_render_executed_for_this_copy': False,
        'visual_approval_reused': False}
    return result
