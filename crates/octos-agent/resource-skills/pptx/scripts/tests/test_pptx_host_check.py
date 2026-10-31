import copy
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import pptx_host_check as host
from pptx import Presentation
from pptx.util import Inches
from PIL import Image


class HostChecks(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name); self.work = self.root / 'producer'; self.work.mkdir()
        self.pptx = self.work / 'proposal.pptx'
        prs = Presentation(); slide = prs.slides.add_slide(prs.slide_layouts[6])
        slide.shapes.add_textbox(Inches(1), Inches(1), Inches(5), Inches(1)).text = 'A different project'
        prs.save(self.pptx)
        self.contract = {'version': 1, 'artifact': 'proposal.pptx', 'pages': [{'id': 'intro', 'title': 'A different project'}]}
        self.cfg = {'workspace': str(self.work), 'contract': 'contract.json', 'evidence_dir': str(self.root / 'evidence')}

    def checker(self):
        (self.root / 'contract.json').write_text(json.dumps(self.contract))
        (self.root / 'host.json').write_text(json.dumps(self.cfg))
        return host.Checker(self.root / 'host.json')

    def render_stub(self):
        # This adapter tests protocol binding, not visual accuracy.
        file = self.root / 'render.py'
        file.write_text('''import sys,json,hashlib
from pathlib import Path
from PIL import Image
p,out=map(Path,sys.argv[1:]);out.mkdir(exist_ok=True)
im=out/'page.png';Image.new('RGB',(160,90),'white').save(im)
h=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
(out/'render-report.json').write_text(json.dumps({'input_sha256':h(p),'pages':1,'page_images':[{'page':1,'path':im.name,'sha256':h(im)}]}))
''')
        self.cfg['renderer'] = {'identity': 'test-adapter-v1', 'command': [sys.executable, str(file), '{pptx}', '{out}']}
        return file

    def test_missing_and_changed_actual_artifacts(self):
        checker = self.checker(); result = checker.check({'mode': 'delivery'})
        self.assertEqual(result['status'], 'pass'); self.assertEqual(result['visual'], 'not_requested')
        self.assertTrue(checker.check({'mode': 'delivery'})['reused'])
        prs = Presentation(self.pptx); prs.slides[0].shapes[0].text = 'Wrong'; prs.save(self.pptx)
        changed = checker.check({'mode': 'delivery'})
        self.assertEqual(changed['status'], 'fail'); self.assertNotEqual(changed['sha256'], result['sha256'])
        self.pptx.unlink(); self.assertEqual(checker.check({'mode': 'delivery'})['status'], 'fail')

    def test_contract_and_references_cannot_be_producer_controlled(self):
        (self.work / 'contract.json').write_text(json.dumps(self.contract))
        self.cfg['contract'] = 'producer/contract.json'
        with self.assertRaises(ValueError): self.checker()
        self.cfg['contract'] = 'contract.json'
        self.contract['pages'][0]['reference'] = str(self.work / 'ref.png')
        with self.assertRaises(ValueError): self.checker()

    def test_visual_requirement_cannot_be_silently_skipped(self):
        self.contract['visual'] = {'required': True}
        result = host.safe_check(self.checker(), {'mode': 'delivery'})
        self.assertEqual(result['status'], 'error')
        self.assertIn('renderer', result['summary'])
        self.render_stub()
        result = host.safe_check(self.checker(), {'mode': 'delivery'})
        self.assertEqual(result['status'], 'error'); self.assertIn('reviewer', result['summary'])

    def test_visual_review_works_without_source_or_blank(self):
        self.render_stub(); self.contract['visual'] = {'required': True}
        self.cfg['reviewer'] = {'model': 'host-selected-vision-model', 'base_url': 'https://unused.invalid/v1'}
        def measured(spec, objects, render, contract, reviewer, evidence):
            self.assertNotIn('reference', spec); self.assertNotIn('blank', spec)
            return {'status': 'pass', 'checks': {'content': 'pass', 'layout': 'pass', 'legibility': 'pass'}, 'objects': [], 'findings': []}
        with patch.object(host, 'review_page', side_effect=measured) as review:
            result = self.checker().check({'mode': 'delivery'})
        self.assertEqual(result['visual'], 'pass'); self.assertEqual(review.call_count, 1)
        self.assertEqual(result['native_powerpoint'], 'not_verified')

    def test_layer_failure_blocks_a_full_page_pass_without_changing_artifact(self):
        self.render_stub(); self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'host-model','base_url':'https://unused.invalid/v1'}
        self.cfg['raster_overlap']={'ocr_executable':sys.executable}
        before=self.pptx.read_bytes()
        base={'status':'pass','checks':{'content':'pass','layout':'pass','legibility':'pass'},'objects':[],'findings':[]}
        layer={'status':'fail','findings':[{'check':'raster-native-text-collision','detail':'Two visible labels overlap'}]}
        with patch.object(host,'review_page',return_value=base),patch.object(host,'review_raster_layer',return_value=layer):
            result=self.checker().check({'mode':'delivery'})
        self.assertEqual(result['status'],'fail')
        self.assertEqual(result['findings'][0]['check'],'raster-native-text-collision')
        self.assertEqual(self.pptx.read_bytes(),before)

    def test_layer_checker_error_preserves_confirmed_page_findings(self):
        self.render_stub(); self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'host-model','base_url':'https://unused.invalid/v1'}
        self.cfg['raster_overlap']={'ocr_executable':sys.executable}
        base={'status':'fail','checks':{'content':'fail','layout':'pass','legibility':'pass'},'objects':[],
              'findings':[{'check':'content','detail':'Required label is absent'}]}
        with patch.object(host,'review_page',return_value=base),patch.object(host,'review_raster_layer',side_effect=RuntimeError('OCR unavailable')):
            result=self.checker().check({'mode':'delivery'})
        self.assertEqual(result['status'],'error')
        self.assertTrue(result['checker_errors'])
        self.assertEqual(result['findings'][0]['detail'],'Required label is absent')

    def test_prior_defect_blocks_later_page_pass_until_explicitly_resolved(self):
        self.render_stub(); self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'host-model','base_url':'https://unused.invalid/v1'}
        base={'status':'pass','checks':{'content':'pass','layout':'pass','legibility':'pass'},'objects':[],'findings':[]}
        closure={'status':'fail','findings':[{'check':'known-defect-not-resolved','detail':'Current pixels still cross the card border'}]}
        before=self.pptx.read_bytes()
        with patch.object(host,'review_page',return_value=base),patch.object(host,'review_known_defects',return_value=closure):
            result=self.checker().check({'mode':'delivery'})
        self.assertEqual(result['status'],'fail')
        self.assertEqual(result['findings'][0]['check'],'known-defect-not-resolved')
        self.assertEqual(self.pptx.read_bytes(),before)

    def test_closure_error_keeps_other_confirmed_findings(self):
        self.render_stub();self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'host-model','base_url':'https://unused.invalid/v1'}
        base={'status':'fail','checks':{'content':'fail','layout':'pass','legibility':'pass'},'objects':[],
              'findings':[{'check':'content','detail':'Required label absent'}]}
        with patch.object(host,'review_page',return_value=base),patch.object(host,'review_known_defects',side_effect=RuntimeError('Evidence unavailable')):
            result=self.checker().check({'mode':'delivery'})
        self.assertEqual(result['status'],'error')
        self.assertEqual(result['findings'][0]['detail'],'Required label absent')

    def test_reference_renderer_and_policy_changes_invalidate_evidence(self):
        script = self.render_stub(); checker = self.checker(); a = checker.revision(checker.contract)
        script.write_text(script.read_text() + '\n# renderer change\n')
        self.assertNotEqual(a, checker.revision(checker.contract))
        b = checker.revision(checker.contract)
        checker.cfg['renderer']['identity'] = 'different-fonts'
        self.assertNotEqual(b, checker.revision(checker.contract))
        ref = self.root / 'ref.png'; Image.new('RGB', (4, 4), 'white').save(ref)
        self.contract['pages'][0]['reference'] = str(ref)
        checker = self.checker(); a = checker.revision(checker.contract)
        Image.new('RGB', (4, 4), 'black').save(ref)
        self.assertNotEqual(a, checker.revision(checker.contract))

    def test_unauthorized_page_or_path_cannot_select_easier_checks(self):
        checker = self.checker()
        for request in ({'mode': 'pages', 'pages': ['invented'], 'pptx': 'proposal.pptx'},
                        {'mode': 'pages', 'pages': ['intro'], 'pptx': '../outside.pptx'}):
            self.assertEqual(host.safe_check(checker, request)['status'], 'error')

    def test_renderer_must_prove_page_order_hash_and_current_bytes(self):
        folder = self.root / 'render'; folder.mkdir()
        p = folder / 'page.png'; Image.new('RGB', (8, 8)).save(p)
        report = {'input_sha256': 'input', 'pages': 1, 'page_images': [{'page': 1, 'path': 'page.png', 'sha256': host.sha(p.read_bytes())}]}
        self.assertEqual(host.verify_render(report, folder, 'input', 1), [p.resolve()])
        for change in ({'input_sha256': 'old'}, {'pages': 2}, {'page_images': []}):
            with self.assertRaises(ValueError): host.verify_render({**report, **change}, folder, 'input', 1)
        Image.new('RGB', (8, 8), 'white').save(p)
        with self.assertRaises(ValueError): host.verify_render(report, folder, 'input', 1)

    def test_producer_report_symlink_cannot_overwrite_host_files(self):
        checker = self.checker(); value = checker.check({'mode': 'delivery'})
        report = self.work / value['report']; report.unlink()
        protected = self.root / 'protected'; protected.write_text('preserve')
        report.symlink_to(protected)
        self.assertEqual(host.safe_check(checker, {'mode': 'delivery'})['status'], 'error')
        self.assertEqual(protected.read_text(), 'preserve')

    def test_verdict_requires_all_objects_and_no_unknown_or_findings(self):
        objects = [{'id': '2'}]; checks = ['content', 'layout', 'legibility']
        v = {'checks': dict.fromkeys(checks, 'pass'), 'objects': [{'id': '2', 'status': 'pass', 'evidence': 'Title readable inside its header'}], 'findings': []}
        self.assertEqual(host.parse_verdict(json.dumps(v), objects, checks)['status'], 'pass')
        missing = copy.deepcopy(v); missing['objects'] = []
        with self.assertRaises(ValueError): host.parse_verdict(json.dumps(missing), objects, checks)
        v['objects'][0]['status'] = 'unknown'
        self.assertEqual(host.parse_verdict(json.dumps(v), objects, checks)['status'], 'fail')

    def test_unchanged_page_review_reused_after_package_bytes_change(self):
        self.render_stub(); self.contract['visual'] = {'required': True}
        self.cfg['reviewer'] = {'model': 'host-model', 'base_url': 'https://unused.invalid/v1'}
        verdict = {'status': 'pass', 'checks': {'content': 'pass', 'layout': 'pass', 'legibility': 'pass'}, 'objects': [], 'findings': []}
        with patch.object(host, 'review_page', return_value=verdict) as review:
            checker = self.checker(); first = checker.check({'mode': 'delivery'})
            prs = Presentation(self.pptx); prs.core_properties.subject = 'Metadata only'; prs.save(self.pptx)
            second = checker.check({'mode': 'delivery'})
        self.assertNotEqual(first['sha256'], second['sha256'])
        self.assertEqual(review.call_count, 1)
        self.assertTrue(second['pages'][0]['reused'])

    def test_provider_receives_actual_images_without_project_or_model_defaults(self):
        image = self.root / 'render.png'; Image.new('RGB', (160, 120), 'white').save(image)
        objects = [{'id': '7', 'text': 'Hello'}]
        verdict = {'checks': {'content': 'pass', 'layout': 'pass', 'legibility': 'pass'},
                   'objects': [{'id': '7', 'status': 'pass', 'evidence': 'Hello is readable'}], 'findings': []}
        response = {'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(verdict)}}]}
        def endpoint(url, headers, payload, timeout):
            self.assertEqual(payload['model'], 'another-vision-model')
            self.assertNotIn('reasoning_effort', payload)
            content = payload['messages'][1]['content']
            self.assertEqual(sum(p['type'] == 'image_url' for p in content), 5)
            self.assertNotIn('reference_fidelity', content[0]['text'])
            return response
        with patch.object(host, 'chat_completion', side_effect=endpoint):
            result = host.review_page({'id': 'intro'}, objects, image, {},
                                      {'model': 'another-vision-model', 'base_url': 'https://unused.invalid/v1'}, self.root)
        self.assertEqual(result['status'], 'pass')

    def test_transport_retry_changes_preserve_visual_evidence(self):
        self.render_stub();self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'host-model','base_url':'https://unused.invalid/v1'}
        verdict={'status':'pass','checks':{'content':'pass','layout':'pass','legibility':'pass'},'objects':[],'findings':[]}
        with patch.object(host,'review_page',return_value=verdict) as review:
            checker=self.checker();checker.check({'mode':'delivery'})
            original=checker.revision
            with patch.object(checker,'revision',side_effect=lambda c:'transport-only-'+original(c)):
                result=checker.check({'mode':'delivery'})
        self.assertEqual(review.call_count,1)
        self.assertTrue(result['pages'][0]['reused'])

    def test_bad_reviewer_never_becomes_structure_only_pass(self):
        self.render_stub();self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'host-model','base_url':'https://unused.invalid/v1'}
        with patch.object(host,'review_page',side_effect=ValueError('Malformed review coverage')):
            result=self.checker().check({'mode':'delivery'})
        self.assertEqual(result['status'],'error')
        self.assertEqual(result['visual'],'error')
        self.assertEqual(result['checker_errors'][0]['page'],'intro')
        self.assertTrue((self.work/result['report']).is_file())

    def test_changed_model_cannot_reuse_prior_visual_evidence(self):
        self.render_stub();self.contract['visual']={'required':True}
        self.cfg['reviewer']={'model':'first','base_url':'https://unused.invalid/v1'}
        verdict={'status':'fail','checks':{'content':'pass','layout':'fail','legibility':'pass'},'objects':[],'findings':[{'detail':'A visible overlap'}]}
        with patch.object(host,'review_page',return_value=verdict) as review:
            checker=self.checker();checker.check({'mode':'delivery'})
            checker.cfg['reviewer']['model']='second'
            checker.check({'mode':'delivery'})
        self.assertEqual(review.call_count,2)


if __name__ == '__main__':
    unittest.main()
