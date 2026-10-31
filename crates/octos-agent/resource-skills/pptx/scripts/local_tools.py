#!/usr/bin/env python3
"""Portable PPTX rendering and optional image API bridge, without a QA policy."""
import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from socketserver import TCPServer
import json
import mimetypes
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import threading
import uuid

from PIL import Image
import requests

from native_deck import atomic_json
from operation_receipts import attach_job_metadata, publish_operation


class LoopbackHTTPServer(ThreadingHTTPServer):
    """Bind the local service without a reverse-DNS dependency at startup."""
    def server_bind(self):
        TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def now():
    return datetime.now(timezone.utc).isoformat()


@contextmanager
def workspace_lock(root):
    path = root / 'tools/.local-tools.lock'
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+b') as handle:
        try:
            if os.name == 'nt':
                import msvcrt
                handle.seek(0); handle.write(b'0'); handle.flush(); handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            raise RuntimeError('A local tool service already owns this workspace') from error
        yield


class Service:
    def __init__(self, workspace, workers=3, soffice=None):
        self.root = Path(workspace).resolve(strict=True)
        self.jobs = self.root / 'requests/tool-jobs'
        self.jobs.mkdir(parents=True, exist_ok=True)
        self.pool = ThreadPoolExecutor(max_workers=workers)
        self.lock = threading.Lock()
        self.soffice = soffice
        self.image_key = os.environ.get('OPENAI_API_KEY', '')
        self.image_model = os.environ.get('PPTX_IMAGE_MODEL', 'gpt-image-2')
        self.image_base = os.environ.get('PPTX_IMAGE_BASE_URL', 'https://api.openai.com/v1').rstrip('/')
        self.capabilities = ['render'] + (['image'] if self.image_key else [])
        for path in self.jobs.glob('*.json'):
            job = json.loads(path.read_text())
            if job.get('status') in ('queued', 'running'):
                job.update(status='interrupted', error='Service restarted; inspect artifacts before submitting a new job')
                self.save(job)

    def confined(self, value):
        path = Path(value)
        if path.is_absolute():
            raise ValueError('Use workspace-relative paths')
        path = (self.root / path).resolve()
        if not path.is_relative_to(self.root):
            raise ValueError('Path leaves workspace')
        return path

    def save(self, job):
        receipt = f'requests/tool-jobs/{job["id"]}.json'
        attach_job_metadata(job, receipt)
        atomic_json(self.root / receipt, job)

    def submit(self, request):
        job_id = request.get('id', '')
        if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,95}', job_id):
            raise ValueError('Invalid job id')
        if request.get('op') not in self.capabilities:
            raise ValueError('Available operations: ' + ', '.join(self.capabilities))
        fingerprint = hashlib.sha256(json.dumps(request, sort_keys=True, ensure_ascii=False).encode()).hexdigest()
        path = self.jobs / (job_id + '.json')
        with self.lock:
            if path.exists():
                job = json.loads(path.read_text())
                if job['request_sha256'] != fingerprint:
                    raise ValueError('Job id already belongs to different input')
                return job
            job = dict(id=job_id, request=request, request_sha256=fingerprint,
                       status='queued', queued_at=now())
            self.save(job)
            self.pool.submit(self.execute, job)
            return json.loads(path.read_text())

    def execute(self, job):
        job.update(status='running', started_at=now())
        self.save(job)
        try:
            method = self.render if job['request']['op'] == 'render' else self.image
            job['result'] = method(job['request'])
            job['status'] = 'completed'
        except Exception as error:
            message = str(error)
            if self.image_key:
                message = message.replace(self.image_key, '[redacted]')
            job.update(status='failed', error=message[:2000])
        job['finished_at'] = now()
        self.save(job)
        try:
            publish_operation(self.root, job, f'requests/tool-jobs/{job["id"]}.json')
        except Exception:
            # Durable job status remains authoritative if progress annotation fails.
            pass

    def render(self, request):
        source = self.confined(request['pptx'])
        output = self.confined(request['out'])
        if source.suffix.lower() != '.pptx' or not source.is_file() or output == self.root:
            raise ValueError('A readable PPTX and fresh render directory are required')
        output.mkdir(parents=True, exist_ok=False)
        snapshot = output / '.input' / source.name
        snapshot.parent.mkdir()
        shutil.copyfile(source, snapshot)
        source_hash = digest(snapshot)
        if request.get('input_sha256') != source_hash:
            raise ValueError('PPTX hash changed after submission')
        command = [sys.executable, str(Path(__file__).with_name('render_deck.py')),
                   str(snapshot), '--out', str(output), '--size', request.get('size', '3840x2160')]
        if self.soffice:
            command += ['--soffice', self.soffice]
        run = subprocess.run(command, capture_output=True, text=True, timeout=360)
        (output / 'service-render.log').write_text(run.stdout + run.stderr)
        if run.returncode:
            raise RuntimeError('Render failed; see ' + str((output / 'service-render.log').relative_to(self.root)))
        return {'out': str(output.relative_to(self.root)), 'snapshot_sha256': source_hash,
                'report': json.loads((output / 'render-report.json').read_text())}

    def image(self, request):
        prompt = request.get('prompt', '')
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError('Image prompt is required')
        output = self.confined(request['output'])
        if output.suffix.lower() != '.png':
            raise ValueError('Use a PNG output path')
        references = [self.confined(p) for p in request.get('reference_paths', [])]
        if len(references) > 5:
            raise ValueError('At most five reference images')
        payload = dict(model=self.image_model, prompt=prompt,
                       size=request.get('size', '3840x2160'), quality=request.get('quality', 'high'),
                       output_format='png', n=1,
                       background='transparent' if request.get('transparent_background') else 'opaque')
        output.parent.mkdir(parents=True, exist_ok=True)
        # Reserve the output before a billed call; a second job cannot replace it.
        reservation = output.with_suffix('.image-pending')
        with self.lock:
            if output.exists():
                raise ValueError('Image output already exists')
            with reservation.open('x') as handle:
                handle.write(request['id'])
        headers = {'Authorization': 'Bearer ' + self.image_key}
        if references:
            files = [('image[]', (p.name, p.read_bytes(), mimetypes.guess_type(p)[0] or 'application/octet-stream'))
                     for p in references]
            response = requests.post(self.image_base + '/images/edits', headers=headers,
                                     data=payload, files=files, timeout=(20, 900))
        else:
            response = requests.post(self.image_base + '/images/generations', headers=headers,
                                     json=payload, timeout=(20, 900))
        if not response.ok:
            raise RuntimeError(f'Image API HTTP {response.status_code}: {response.text[:1200]}')
        data = response.json()
        raw = base64.b64decode(data['data'][0]['b64_json'], validate=True)
        temporary = output.with_suffix('.pending.png')
        temporary.write_bytes(raw)
        with Image.open(temporary) as im:
            im.verify()
        with Image.open(temporary) as im:
            pixels = list(im.size)
        temporary.replace(output)
        reservation.unlink()
        return dict(model=self.image_model, requested_size=payload['size'],
                    image=dict(path=str(output.relative_to(self.root)), pixels=pixels,
                               sha256=digest(output), bytes=len(raw)),
                    references=[dict(path=str(p.relative_to(self.root)), sha256=digest(p)) for p in references],
                    usage=data.get('usage'), provider_request_id=response.headers.get('x-request-id'))

    def close(self):
        self.pool.shutdown(wait=True)


def handler_for(service):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send(self, code, value):
            raw = json.dumps(value, ensure_ascii=False).encode()
            self.send_response(code)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def allowed(self):
            return (not self.headers.get('Origin') and
                    self.headers.get('Host') == f'127.0.0.1:{self.server.server_port}')

        def do_GET(self):
            if not self.allowed():
                return self.send(403, {'error': 'Local tool clients only'})
            if self.path == '/health':
                return self.send(200, dict(service='portable-pptx-tools', capabilities=service.capabilities,
                                           workspace=str(service.root)))
            match = re.fullmatch(r'/jobs/([A-Za-z0-9][A-Za-z0-9_.-]{0,95})', self.path)
            if match and (service.jobs / (match[1] + '.json')).is_file():
                return self.send(200, json.loads((service.jobs / (match[1] + '.json')).read_text()))
            self.send(404, {'error': 'Not found'})

        def do_POST(self):
            if not self.allowed():
                return self.send(403, {'error': 'Local tool clients only'})
            try:
                if self.path != '/jobs':
                    return self.send(404, {'error': 'Use /jobs'})
                length = int(self.headers.get('Content-Length', 0))
                if not 0 < length <= 100000:
                    raise ValueError('Request must be 1–100000 bytes')
                self.send(202, service.submit(json.loads(self.rfile.read(length))))
            except (ValueError, KeyError, TypeError) as error:
                self.send(400, {'error': str(error)})
    return Handler


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--workspace', type=Path, required=True)
    parser.add_argument('--port', type=int, default=0)
    parser.add_argument('--workers', type=int, default=3)
    parser.add_argument('--soffice')
    args = parser.parse_args()
    args.workspace.mkdir(parents=True, exist_ok=True)
    if args.workers < 1:
        parser.error('--workers must be positive')
    with workspace_lock(args.workspace.resolve()):
        service = Service(args.workspace, args.workers, args.soffice)
        server = LoopbackHTTPServer(('127.0.0.1', args.port), handler_for(service))
        config = dict(url=f'http://127.0.0.1:{server.server_port}', capabilities=service.capabilities,
                      render_cache_identity={'lifetime': uuid.uuid4().hex,
                                             'renderer_sha256': digest(Path(__file__).with_name('render_deck.py'))},
                      workspace_only=True)
        atomic_json(service.root / 'tools/service.json', config)
        print(json.dumps(config), flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            server.server_close()
            service.close()


if __name__ == '__main__':
    main()
