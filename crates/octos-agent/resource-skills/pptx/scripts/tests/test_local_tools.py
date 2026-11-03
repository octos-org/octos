import base64
import io
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch, Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from PIL import Image
from local_tools import Service, LoopbackHTTPServer, handler_for


class LocalToolsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.service = Service(self.root, workers=1)

    def tearDown(self):
        self.service.close()
        self.directory.cleanup()

    def test_loopback_startup_and_health_do_not_require_reverse_dns(self):
        import http.client
        with patch('socket.getfqdn', side_effect=AssertionError('reverse DNS is unavailable')):
            server = LoopbackHTTPServer(('127.0.0.1', 0), handler_for(self.service))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                connection.request('GET', '/health')
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(response.read())['workspace'], str(self.root.resolve()))
                connection.close()
            finally:
                server.shutdown(); server.server_close(); thread.join(timeout=2)

    def test_paths_cannot_escape_through_parent_or_symlink(self):
        with self.assertRaises(ValueError):
            self.service.confined('../outside')
        (self.root / 'alias').symlink_to(self.root.parent, target_is_directory=True)
        with self.assertRaises(ValueError):
            self.service.confined('alias/outside')

    def test_same_job_resumes_once_and_conflicting_request_is_rejected(self):
        gate = threading.Event()
        calls = []
        def render(request):
            calls.append(request)
            gate.wait(2)
            return {'artifact': 'example'}
        self.service.render = render
        request = dict(id='render-one', op='render', pptx='example.pptx', out='render')
        try:
            self.service.submit(request)
            self.service.submit(request)
            with self.assertRaises(ValueError):
                self.service.submit({**request, 'out': 'different'})
        finally:
            gate.set()
        self.service.close()
        self.assertEqual(len(calls), 1)
        job = self.service.submit(request)
        self.assertEqual(job['status'], 'completed')
        self.assertEqual(job['octos_external_job']['receipt_path'], 'requests/tool-jobs/render-one.json')

    def test_restart_marks_ambiguous_job_interrupted_without_resubmission(self):
        request = dict(id='unfinished', op='render')
        self.service.save(dict(id='unfinished', request=request,
                               request_sha256='unknown', status='running'))
        other = Service(self.root)
        try:
            job = json.loads((other.jobs / 'unfinished.json').read_text())
            self.assertEqual(job['status'], 'interrupted')
        finally:
            other.close()

    def test_image_records_returned_pixels_and_reserves_output(self):
        buffer = io.BytesIO()
        Image.new('RGB', (32, 18), 'white').save(buffer, format='PNG')
        response = Mock(ok=True, headers={'x-request-id': 'example'})
        response.json.return_value = {'data': [{'b64_json': base64.b64encode(buffer.getvalue()).decode()}]}
        request = dict(id='art-one', op='image', prompt='example', output='art/one.png', size='3840x2160')
        with patch('local_tools.requests.post', return_value=response) as post:
            result = self.service.image(request)
            self.assertEqual(result['image']['pixels'], [32, 18])
            with self.assertRaises(ValueError):
                self.service.image({**request, 'id': 'art-two'})
            self.assertEqual(post.call_count, 1)

    def test_failed_provider_call_does_not_leak_key_or_repeat(self):
        self.service.image_key = 'secret-fixture-value'
        self.service.capabilities.append('image')
        response = Mock(ok=False, status_code=429, text='quota secret-fixture-value')
        request = dict(id='quota', op='image', prompt='example', output='art/quota.png')
        with patch('local_tools.requests.post', return_value=response) as post:
            self.service.submit(request)
            self.service.close()
            again = self.service.submit(request)
            self.assertEqual(again['status'], 'failed')
            self.assertNotIn('secret-fixture-value', json.dumps(again))
            self.assertEqual(post.call_count, 1)


if __name__ == '__main__':
    unittest.main()
