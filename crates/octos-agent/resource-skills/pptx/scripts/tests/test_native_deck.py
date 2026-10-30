"""Synthetic behavioral checks for reusable page construction; no deck answers."""
import copy
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from PIL import Image
from pptx import Presentation
from pptx.util import Inches

sys.path.insert(0, str(Path(__file__).parents[1]))
import native_deck as engine
import native_cycle
from checked_jobs import JobClient, pptx_page_count
from prepare_page_jobs import prepare
from compare_slide_proof import compare


class Fixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        Image.new('RGB', (1000, 700), '#ece7df').save(self.root/'art.png')
        self.doc = {
            'schema_version': 1,
            'canvas': {'width_px': 1000, 'height_px': 700, 'width_in': 10, 'height_in': 7},
            'styles': {'heading': {'font': 'Arial', 'size_pt': 28, 'bold': True, 'locked': True},
                       'body': {'font': 'Arial', 'size_pt': 17, 'color': '123456'}},
            'pages': [self.page('alpha'), self.page('beta')]}
        self.spec = self.root/'deck.json'
        self.save()

    def page(self, name):
        return {'id': name, 'background': 'art.png', 'design': 'art.png',
                'copy': {'title': '示例 / Example', 'first': 'Repeat 7', 'second': 'Repeat 7'},
                'regions': {'left': [40, 200, 400, 350], 'right': [550, 200, 400, 350]},
                'texts': [{'id': 'title', 'text': '示例 / Example', 'box': [40, 40, 900, 80], 'style': 'heading'},
                          {'id': 'first', 'text': 'Repeat\n7', 'box': [60, 230, 300, 150], 'style': 'body', 'region': 'left'},
                          {'id': 'second', 'text': 'Repeat 7', 'box': [600, 230, 300, 150], 'style': 'body', 'region': 'right'}]}

    def save(self):
        engine.atomic_json(self.spec, self.doc)


class BuilderTests(Fixture):
    def test_native_objects_repeated_copy_dimensions_and_blank_art(self):
        report = engine.build(self.spec, self.root/'candidate')
        full = Presentation(self.root/'candidate/editable.pptx')
        blank = Presentation(self.root/'candidate/blank.pptx')
        self.assertEqual((full.slide_width, full.slide_height), (Inches(10), Inches(7)))
        self.assertEqual(len(full.slides), 2)
        for a, b in zip(full.slides, blank.slides):
            self.assertEqual(len(a.shapes), 4)
            self.assertEqual(len(b.shapes), 1)
            self.assertEqual(a.shapes[0].image.blob, b.shapes[0].image.blob)
            self.assertEqual(a.shapes[0].image.blob, (self.root/'art.png').read_bytes())
            self.assertEqual(a.shapes[1].text, '示例 / Example')
            self.assertEqual(a.shapes[1].text_frame.paragraphs[0].runs[0].font.size.pt, 28)
            self.assertEqual(a.shapes[2].text, 'Repeat\n7')
            self.assertEqual(a.shapes[3].text, 'Repeat 7')
        self.assertTrue(report['pair_checks']['technicalPassed'])
        self.assertFalse(report['visual_quality_verified'])

    def test_unchanged_build_reuses_artifacts_and_changed_input_requires_new_attempt(self):
        out = self.root/'candidate'
        first = engine.build(self.spec, out)
        time_before = (out/'editable.pptx').stat().st_mtime_ns
        self.assertEqual(first, engine.build(self.spec, out))
        self.assertEqual(time_before, (out/'editable.pptx').stat().st_mtime_ns)
        self.doc['pages'][0]['texts'][0]['box'][0] = 45
        self.save()
        with self.assertRaisesRegex(ValueError, 'Inputs changed'):
            engine.build(self.spec, out)
        self.assertNotEqual(first['fingerprint'], engine.build(self.spec, self.root/'changed')['fingerprint'])

    def test_cached_output_tampering_fails(self):
        out = self.root/'candidate'
        engine.build(self.spec, out, 'alpha')
        (out/'resolved.json').write_text('{}')
        with self.assertRaisesRegex(ValueError, 'Cached output changed'):
            engine.build(self.spec, out, 'alpha')

    def test_copy_numbers_and_occurrence_counts_are_not_silently_changed(self):
        for mutate in [lambda p: p['texts'].pop(),
                       lambda p: p['texts'][1].update(text='Repeat 8'),
                       lambda p: p['texts'][2].update(copy_id='first')]:
            doc = copy.deepcopy(self.doc)
            mutate(doc['pages'][0])
            engine.atomic_json(self.spec, doc)
            with self.assertRaises(ValueError):
                engine.build(self.spec, self.root/'invalid')
            self.assertFalse((self.root/'invalid').exists())

    def test_owner_bounds_locked_styles_and_invalid_coordinates(self):
        for change in [{'box': [500, 230, 300, 150]}, {'box': [60, 230, float('nan'), 150]},
                       {'style': 'unknown'}, {'format': {'word_wrapp': False}}]:
            doc, _ = engine.load_spec(self.spec)
            doc['pages'][0]['texts'][1].update(change)
            with self.assertRaises(ValueError):
                engine.validate(doc)
        doc, _ = engine.load_spec(self.spec)
        doc['pages'][0]['texts'][0]['format'] = {'size_pt': 12}
        with self.assertRaisesRegex(ValueError, 'locked'):
            engine.validate(doc)

    def test_no_artwork_stretch_or_hash_substitution(self):
        self.doc['pages'][0]['background'] = {'path': 'art.png', 'sha256': '0'*64}
        self.save()
        with self.assertRaisesRegex(ValueError, 'hash mismatch'):
            engine.load_spec(self.spec)
        self.doc['pages'][0]['background'] = 'art.png'
        self.doc['canvas']['width_in'] = 13
        self.save()
        with self.assertRaisesRegex(ValueError, 'aspect ratios'):
            engine.load_spec(self.spec)

    def test_independent_page_file_paths_and_batch_patch_are_atomic(self):
        pages = self.root/'pages'; pages.mkdir()
        page = copy.deepcopy(self.doc['pages'][0])
        page['background'] = '../art.png'; page['design'] = '../art.png'
        engine.atomic_json(pages/'one.json', page)
        self.doc['pages'][0] = {'path': 'pages/one.json'}
        self.save()
        changes = self.root/'changes.json'
        engine.atomic_json(changes, [{'page': 'alpha', 'id': 'first', 'set': {'box': [60, 240, 300, 160]}},
                                    {'page': 'beta', 'id': 'second', 'set': {'box': [600, 250, 300, 180]}}])
        out = self.root/'nested/new.json'
        original = self.spec.read_bytes()
        engine.patch_spec(self.spec, changes, out)
        revised, _ = engine.load_spec(out)
        self.assertEqual(set(engine.read_json(out)['pages'][0]), {'path'})
        self.assertEqual(engine.read_json(pages/'one.json'), page)
        self.assertEqual(revised['pages'][0]['texts'][1]['box'][1], 240)
        self.assertEqual(revised['pages'][1]['texts'][2]['box'][1], 250)
        self.assertEqual(self.spec.read_bytes(), original)
        engine.atomic_json(changes, [{'page': 'alpha', 'id': 'first', 'set': {'text': 'Wrong'}}])
        with self.assertRaises(ValueError):
            engine.patch_spec(self.spec, changes, self.root/'bad.json')
        self.assertFalse((self.root/'bad.json').exists())

    def test_patch_can_repair_invalid_owner_geometry(self):
        self.doc['pages'][0]['texts'][1]['box'] = [500, 230, 300, 150]
        self.save()
        with self.assertRaisesRegex(ValueError, 'owner region'):
            engine.load_spec(self.spec)
        engine.atomic_json(self.root/'fix.json', [
            {'page': 'alpha', 'id': 'first', 'set': {'box': [60, 230, 300, 150]}}])
        engine.patch_spec(self.spec, self.root/'fix.json', self.root/'fixed.json')
        fixed, _ = engine.load_spec(self.root/'fixed.json')
        self.assertEqual(fixed['pages'][0]['texts'][1]['box'][0], 60)

    def test_numeric_color_returns_validation_error(self):
        self.doc['styles']['body']['color'] = 123456
        self.save()
        with self.assertRaisesRegex(ValueError, 'RGB string'):
            engine.load_spec(self.spec)

    def test_generated_adapter_rebuilds_exact_page_in_different_slide_order(self):
        out = self.root/'one'
        engine.build(self.spec, out, 'alpha')
        spec = importlib.util.spec_from_file_location('test_adapter', out/'page.py')
        adapter = importlib.util.module_from_spec(spec); spec.loader.exec_module(adapter)
        prs = Presentation(); prs.slide_width = Inches(10); prs.slide_height = Inches(7)
        prs.slides.add_slide(prs.slide_layouts[6])
        adapter.build_slide(prs, {})
        combined = self.root/'combined.pptx'; prs.save(combined)
        self.assertTrue(compare(out/'editable.pptx', combined, 2)['native_assembly_unchanged'])

    def test_selected_page_does_not_validate_or_load_unrelated_page_art(self):
        self.doc['pages'][1]['texts'][1]['box'] = [500, 230, 300, 150]
        self.doc['pages'][1]['background'] = 'not-ready-yet.png'
        self.save()
        report = engine.build(self.spec, self.root/'only-alpha', 'alpha')
        self.assertEqual(report['page_order'], ['alpha'])
        with self.assertRaises(OSError):
            engine.build(self.spec, self.root/'whole-deck')

    def test_draft_requires_matching_completed_ocr_and_preserves_uncertain_duplicates(self):
        plan = {k: copy.deepcopy(self.doc[k]) for k in ['schema_version', 'canvas', 'styles']}
        plan.update(ocr_style='body', pages=[{'id': 'synthetic', 'background': 'art.png',
                                           'design': 'art.png', 'ocr': 'ocr.json'}])
        detections = [{'text': '9?', 'x': 100, 'y': y, 'w': 90, 'h': 35, 'confidence': .2} for y in [150, 300]]
        data = {'status': 'completed', 'result': {'image': {'sha256': engine.sha(self.root/'art.png')},
                                                'detections': detections}}
        engine.atomic_json(self.root/'ocr.json', data); engine.atomic_json(self.root/'plan.json', plan)
        engine.draft_ocr(self.root/'plan.json', self.root/'draft.json')
        doc, _ = engine.load_spec(self.root/'draft.json')
        self.assertEqual(list(doc['pages'][0]['copy'].values()), ['9?', '9?'])
        self.assertEqual(len(doc['pages'][0]['ocr_candidates']), 2)
        self.assertEqual(doc['pages'][0]['source_status'], 'unreviewed_ocr')
        data['status'] = 'queued'; engine.atomic_json(self.root/'ocr.json', data)
        with self.assertRaisesRegex(ValueError, 'not completed'):
            engine.draft_ocr(self.root/'plan.json', self.root/'bad-draft.json')


class FakeRenderService:
    """Mechanical lifecycle fixture; generated pixels are not visual evidence."""
    def __init__(self, root):
        self.root = root
        self.jobs = {}
        self.complete = False

    def transport(self, path, request=None):
        if request is not None:
            ident = request['id']
            self.jobs.setdefault(ident, {'id': ident, 'status': 'queued', 'request': request})
        else:
            ident = path.rsplit('/', 1)[1]
        job = self.jobs[ident]
        if self.complete and job['status'] != 'completed':
            r = job['request']; source = self.root/r['pptx']; out = self.root/r['out']
            (out/'png').mkdir(parents=True, exist_ok=True)
            size = list(map(int, r['size'].split('x'))); pages = []
            for n in range(1, pptx_page_count(source)+1):
                p = out/'png'/f'slide-{n:02}.png'; Image.new('RGB', size, '#eeeeee').save(p)
                pages.append({'page': n, 'path': str(p.relative_to(out)), 'sha256': engine.sha(p)})
            report = {'input_sha256': engine.sha(source), 'pages': len(pages),
                      'rendered_pages': list(range(1, len(pages)+1)), 'page_images': pages, 'export_pixels': size}
            job.update(status='completed', result={'out': r['out'], 'snapshot_sha256': engine.sha(source), 'report': report})
        return job


class CycleTests(Fixture):
    def setUp(self):
        super().setUp()
        engine.atomic_json(self.root/'tools/service.json', {'url': 'http://fixture.invalid'})
        self.service = FakeRenderService(self.root)
        self.mock = patch.object(native_cycle, 'JobClient', lambda url: JobClient(url, self.service.transport))
        self.mock.start(); self.addCleanup(self.mock.stop)

    def test_pending_resume_preserves_build_and_job_identity(self):
        out = self.root/'cycle'
        first = native_cycle.run(self.spec, out, self.root, page='alpha', seconds=0)
        self.assertEqual(first['state'], 'pending')
        identities = set(self.service.jobs)
        before = (out/'build/editable.pptx').stat().st_mtime_ns
        self.service.complete = True
        second = native_cycle.run(self.spec, out, self.root, page='alpha', seconds=0)
        self.assertEqual(second['state'], 'ready_for_visual_review')
        self.assertEqual(set(self.service.jobs), identities)
        self.assertEqual(before, (out/'build/editable.pptx').stat().st_mtime_ns)
        self.assertFalse(second['visual_quality_verified'])

    def test_page_handoff_and_complete_assembly_with_generated_adapters(self):
        self.service.complete = True
        engine.atomic_json(self.root/'contract.json', {'schema_version': 1, 'page_order': ['alpha', 'beta']})
        (self.root/'copy.md').write_text('Synthetic test authority; no real QA assertion')
        (self.root/'TOOLS.md').write_text('Synthetic tool instructions')
        plan = {'shared_contract': 'contract.json', 'pages': [
            {'page_id': p, 'inputs': [{'role': 'canonical-copy', 'path': 'copy.md'},
                                     {'role': 'design', 'path': 'art.png'},
                                     {'role': 'blank-art', 'path': 'art.png'}]} for p in ['alpha', 'beta']]}
        engine.atomic_json(self.root/'plan.json', plan)
        skill = Path(__file__).parents[2]/'SKILL.md'
        receipt = prepare(self.root/'plan.json', self.root/'jobs', skill, self.root/'TOOLS.md')
        for entry in receipt['jobs']:
            root = Path(entry['result']).parent
            out = root/'cycle'
            native_cycle.run(self.spec, out, self.root, page=entry['page_id'], seconds=0)
            review = root/'review.json'
            engine.atomic_json(review, {'inspection': {'source': True, 'editable_render': False, 'blank_render': True},
                                       'open_findings': [], 'observations': ['Test-only declarations; fake renderer.']})
            with self.assertRaisesRegex(ValueError, 'incomplete'):
                native_cycle.handoff(entry['job'], out/'cycle.json', review)
            self.assertFalse(Path(entry['result']).exists())
            value = engine.read_json(review); value['inspection']['editable_render'] = True
            engine.atomic_json(review, value)
            native_cycle.handoff(entry['job'], out/'cycle.json', review)
        report = engine.assemble(self.root/'jobs/prepared-jobs.json', self.root/'assembled')
        self.assertEqual(report['page_order'], ['alpha', 'beta'])
        self.assertFalse(report['visual_quality_verified'])
        self.assertTrue(all(r['native_assembly_unchanged'] for r in report['proof_comparisons']))


if __name__ == '__main__':
    unittest.main()
