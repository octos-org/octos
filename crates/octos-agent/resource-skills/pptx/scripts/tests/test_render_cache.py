"""Synthetic cache invariants; fake render pixels are not a visual-quality test."""
from concurrent.futures import ThreadPoolExecutor
import copy
import json
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import zipfile

from PIL import Image
from pptx import Presentation
from pptx.util import Inches

sys.path.insert(0, str(Path(__file__).parents[1]))
from checked_jobs import JobClient, JobError, JobPending, pptx_page_count
from render_cache import cached_render, canonicalize_pptx, digest


class Service:
    def __init__(self, root):
        self.root = root
        self.jobs = {}
        self.complete = True
        self.executions = 0
        self.lock = threading.Lock()

    def transport(self, path, request=None):
        with self.lock:
            ident = request['id'] if request is not None else path.rsplit('/', 1)[1]
            if request is not None:
                self.jobs.setdefault(ident, {'id': ident, 'status': 'queued', 'request': request})
            job = self.jobs[ident]
            if self.complete and job['status'] != 'completed':
                self.executions += 1
                r = job['request']; source = self.root/r['pptx']; output = self.root/r['out']
                (output/'png').mkdir(parents=True, exist_ok=True)
                dimensions = list(map(int, r['size'].split('x')))
                pages = pptx_page_count(source)
                images = []
                for n in range(1, pages+1):
                    p = output/'png'/f'page-{n}.png'
                    Image.new('RGB', dimensions, '#aabbcc').save(p)
                    images.append({'page': n, 'path': str(p.relative_to(output)), 'sha256': digest(p)})
                pdf = output/'source.pdf'; pdf.write_bytes(b'%PDF synthetic lifecycle fixture\n')
                report = {'input_sha256': digest(source), 'pdf_sha256': digest(pdf),
                          'pages': pages, 'rendered_pages': list(range(1, pages+1)),
                          'page_images': images, 'export_pixels': dimensions}
                (output/'render-report.json').write_text(json.dumps(report))
                job.update(status='completed', result={'out': r['out'],
                           'snapshot_sha256': digest(source), 'report': report})
            return copy.deepcopy(job)


class RenderCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(); self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root/'page.pptx'
        p = Presentation(); p.slide_width = Inches(10); p.slide_height = Inches(7)
        slide = p.slides.add_slide(p.slide_layouts[6])
        shape = slide.shapes.add_textbox(Inches(1), Inches(1), Inches(5), Inches(1))
        shape.text = 'Independent reusable fixture'
        Image.new('RGB', (50, 35), '#777777').save(self.root/'art.png')
        slide.shapes.add_picture(str(self.root/'art.png'), 0, Inches(3), width=Inches(5))
        p.save(self.source); canonicalize_pptx(self.source)
        self.service = Service(self.root)
        self.client = JobClient('http://synthetic-service', transport=self.service.transport)

    def render(self, owner='owner/a1', source=None, size='400x280', backend='lifetime-a'):
        path = source or self.source
        return cached_render(self.client, self.root, str(path.relative_to(self.root)),
                             owner+'/renders', 'candidate', size=size, seconds=0,
                             cache_root=self.root/'shared-cache', backend_identity=backend)

    def rewrite_member(self, target, member, replacement):
        with zipfile.ZipFile(self.source) as src, zipfile.ZipFile(target, 'w') as dst:
            for name in src.namelist():
                value = replacement if name == member else src.read(name)
                dst.writestr(name, value)
        canonicalize_pptx(target)

    def test_zip_timestamps_order_and_permissions_do_not_change_canonical_bytes(self):
        other = self.root/'other.pptx'
        with zipfile.ZipFile(self.source) as src, zipfile.ZipFile(other, 'w') as dst:
            original = {n: src.read(n) for n in src.namelist()}
            for name in reversed(src.namelist()):
                info = zipfile.ZipInfo(name, (2026, 10, 1, 1, 2, 4))
                info.external_attr = 0o777 << 16
                dst.writestr(info, src.read(name))
        self.assertNotEqual(digest(other), digest(self.source))
        canonicalize_pptx(other)
        self.assertEqual(digest(other), digest(self.source))
        with zipfile.ZipFile(other) as z:
            self.assertEqual(original, {n: z.read(n) for n in z.namelist()})

    def test_exact_input_reuses_one_backend_job_with_separate_owned_artifacts(self):
        a = self.render(); b = self.render('another-owner/a2')
        self.assertEqual(self.service.executions, 1)
        self.assertFalse(a['render_reuse']['cache_hit'])
        self.assertTrue(b['render_reuse']['cache_hit'])
        self.assertEqual(a['id'], b['id'])
        self.assertNotEqual(a['result']['out'], b['result']['out'])
        self.assertEqual(b['verified_input_sha256'], digest(self.source))
        self.assertFalse(b['render_reuse']['visual_approval_reused'])
        pa = self.root/a['result']['out']/a['result']['report']['page_images'][0]['path']
        pb = self.root/b['result']['out']/b['result']['report']['page_images'][0]['path']
        self.assertEqual(digest(pa), digest(pb))
        self.assertNotEqual(pa.stat().st_ino, pb.stat().st_ino)

    def test_geometry_artwork_and_theme_changes_invalidate(self):
        self.render()
        changed = self.root/'geometry.pptx'
        p = Presentation(self.source); p.slides[0].shapes[0].left += Inches(.2)
        p.save(changed); canonicalize_pptx(changed)
        self.render('geometry', changed)
        with zipfile.ZipFile(self.source) as z:
            media = next(n for n in z.namelist() if n.startswith('ppt/media/'))
            theme = z.read('ppt/theme/theme1.xml')
        Image.new('RGB', (50, 35), '#888888').save(self.root/'changed-art.png')
        self.rewrite_member(self.root/'art-change.pptx', media, (self.root/'changed-art.png').read_bytes())
        self.render('art', self.root/'art-change.pptx')
        self.rewrite_member(self.root/'theme-change.pptx', 'ppt/theme/theme1.xml',
                            theme.replace(b'val="4F81BD"', b'val="4F81BE"'))
        self.assertNotEqual(digest(self.source), digest(self.root/'theme-change.pptx'))
        self.render('theme', self.root/'theme-change.pptx')
        self.assertEqual(self.service.executions, 4)

    def test_size_and_backend_lifetime_invalidate(self):
        self.render(); self.render('larger', size='800x560'); self.render('restart', backend='lifetime-b')
        self.assertEqual(self.service.executions, 3)

    def test_pending_resume_and_concurrent_owners_share_same_durable_job(self):
        self.service.complete = False
        with self.assertRaises(JobPending) as pending:
            self.render()
        ident = pending.exception.receipt['id']
        with self.assertRaises(JobPending) as second:
            self.render('other-pending')
        self.assertEqual(second.exception.receipt['id'], ident)
        self.assertEqual(len(self.service.jobs), 1)
        self.service.complete = True
        with ThreadPoolExecutor(max_workers=6) as pool:
            results = list(pool.map(lambda i: self.render('worker-'+str(i)), range(6)))
        self.assertEqual(self.service.executions, 1)
        self.assertEqual(len({r['id'] for r in results}), 1)
        self.assertEqual(len({r['result']['out'] for r in results}), 6)

    def test_corrupt_cache_image_fails_without_submitting_another_job(self):
        a = self.render()
        source_out = self.root/a['render_reuse']['source_out']
        path = source_out/a['result']['report']['page_images'][0]['path']
        path.write_bytes(b'corrupt')
        with self.assertRaisesRegex(JobError, 'artifact changed'):
            self.render('new')
        self.assertEqual(self.service.executions, 1)

    def test_corrupt_report_pdf_and_owner_copy_are_detected(self):
        a = self.render()
        local = self.root/a['result']['out']/'source.pdf'; old = local.read_bytes()
        local.write_bytes(b'corrupt')
        with self.assertRaisesRegex(JobError, 'artifact changed'):
            self.render()
        local.write_bytes(old)
        report = self.root/a['render_reuse']['source_out']/'render-report.json'
        report.write_text('{}')
        with self.assertRaisesRegex(JobError, 'artifact changed'):
            self.render('new')

    def test_document_properties_are_not_ignored(self):
        with zipfile.ZipFile(self.source) as z:
            data = z.read('docProps/core.xml')
        changed = data.replace(b'2013-01-27', b'2014-01-27')
        self.assertNotEqual(data, changed)
        self.rewrite_member(self.root/'core-change.pptx', 'docProps/core.xml', changed)
        self.assertNotEqual(digest(self.source), digest(self.root/'core-change.pptx'))
        self.render(); self.render('core', self.root/'core-change.pptx')
        self.assertEqual(self.service.executions, 2)

    def test_cycle_text_repair_reuses_blank_and_preserves_handoff_binding(self):
        import native_cycle
        import native_deck
        doc = {'schema_version': 1,
               'canvas': {'width_px': 1000, 'height_px': 700, 'width_in': 10, 'height_in': 7},
               'styles': {'body': {'font': 'Arial', 'size_pt': 18}},
               'pages': [{'id': 'demo', 'background': 'art.png', 'copy': {'label': 'Example'},
                          'texts': [{'id': 'label', 'text': 'Example', 'style': 'body',
                                     'box': [100, 100, 500, 100]}]}]}
        spec = self.root/'spec.json'; native_deck.atomic_json(spec, doc)
        native_deck.atomic_json(self.root/'tools/service.json',
                                {'url': self.client.url, 'render_cache_identity': 'test-lifetime'})
        owner = self.root/'worker/out'
        with patch.object(native_cycle, 'JobClient', lambda url: self.client):
            first = native_cycle.run(spec, owner/'a1', self.root, seconds=0, size='400x280')
            self.assertEqual(first['state'], 'ready_for_visual_review')
            self.assertEqual(self.service.executions, 2)
            cycle_hash = digest(owner/'a1/cycle.json')
            resumed = native_cycle.run(spec, owner/'a1', self.root, seconds=0, size='400x280')
            self.assertEqual(resumed, first)
            self.assertEqual(digest(owner/'a1/cycle.json'), cycle_hash)
            self.assertEqual(self.service.executions, 2)
            doc['pages'][0]['texts'][0]['box'][0] += 15
            native_deck.atomic_json(spec, doc)
            second = native_cycle.run(spec, owner/'a2', self.root, seconds=0, size='400x280')
        self.assertEqual(self.service.executions, 3)
        self.assertTrue(second['renders']['blank']['receipt']['render_reuse']['cache_hit'])
        self.assertFalse(second['renders']['editable']['receipt']['render_reuse']['cache_hit'])
        self.assertEqual(digest(owner/'a1/build/blank.pptx'), digest(owner/'a2/build/blank.pptx'))
        self.assertNotEqual(digest(owner/'a1/build/editable.pptx'), digest(owner/'a2/build/editable.pptx'))
        (self.root/'copy.txt').write_text('Example')
        native_deck.atomic_json(self.root/'contract.json', {'schema_version': 1, 'page_order': ['demo']})
        native_deck.atomic_json(self.root/'worker/job.json', {
            'schema_version': 1, 'page_id': 'demo', 'attempt_id': 'test', 'output_dir': 'out',
            'shared_contract': {'path': '../contract.json', 'sha256': digest(self.root/'contract.json')},
            'inputs': [{'role': 'canonical-copy', 'path': '../copy.txt', 'sha256': digest(self.root/'copy.txt')}]})
        native_deck.atomic_json(owner/'review.json', {
            'inspection': {'source': True, 'editable_render': True, 'blank_render': True},
            'open_findings': [], 'observations': ['Synthetic lifecycle declaration only; not visual QA.']})
        result = native_cycle.handoff(self.root/'worker/job.json', owner/'a2/cycle.json', owner/'review.json')
        self.assertTrue(result['handoff_consistent'])
        self.assertFalse(result['visual_quality_verified'])


if __name__ == '__main__':
    unittest.main()
