"""Behavioral inspection tests on unrelated synthetic native pages and real PDFs."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

import fitz
from PIL import Image, ImageDraw
from pptx import Presentation

sys.path.insert(0, str(Path(__file__).parents[1]))
import native_deck
import native_inspect


class InspectionFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        image = Image.new('RGB', (1000, 700), '#f7f4ed')
        ImageDraw.Draw(image).rectangle((40, 180, 960, 620), outline='#777777', width=2)
        image.save(self.root/'art.png')
        self.doc = {'schema_version': 1,
                    'canvas': {'width_px': 1000, 'height_px': 700, 'width_in': 10, 'height_in': 7},
                    'styles': {'body': {'font': 'Arial', 'size_pt': 17}},
                    'pages': [{'id': 'sample', 'background': 'art.png', 'design': 'art.png',
                               'copy': {'heading': '示例 / Example', 'west': 'Repeat 7', 'east': 'Repeat 7'},
                               'texts': [{'id': 'heading', 'text': '示例 / Example', 'style': 'body', 'box': [80, 60, 800, 90]},
                                         {'id': 'west', 'text': 'Repeat 7', 'style': 'body', 'box': [80, 230, 330, 90]},
                                         {'id': 'east', 'text': 'Repeat 7', 'style': 'body', 'box': [600, 230, 330, 90]}]}]}

    def cycle(self, label='initial', render_offsets=None, render_colors=None):
        root = self.root/label; root.mkdir()
        spec = root/'deck.json'; doc = copy.deepcopy(self.doc)
        for page in doc['pages']:
            page['background'] = str(self.root/'art.png'); page['design'] = str(self.root/'art.png')
        native_deck.atomic_json(spec, doc)
        built = native_deck.build(spec, root/'build')
        c = doc['canvas']; renders = {}
        for role in ['editable', 'blank']:
            out = root/('render-'+role); out.mkdir()
            pdf = fitz.open()
            for definition in doc['pages']:
                page = pdf.new_page(width=c['width_in']*72, height=c['height_in']*72)
                page.insert_image(page.rect, filename=str(self.root/'art.png'))
                if role == 'editable':
                    for item in definition['texts']:
                        x, y, _, _ = item['box']; dx, dy = (render_offsets or {}).get(item['id'], [0, 0])
                        size = 17
                        # This is a real synthetic PDF, not a claim of LibreOffice rendering.
                        page.insert_text(((x+dx)*.72, (y+dy)*.72+size), item['text'],
                                         fontsize=size, fontname='china-s',
                                         color=(render_colors or {}).get(item['id'], (0, 0, 0)))
            pdf_path = out/(role+'.pdf'); pdf.save(pdf_path)
            images = []
            for i, page in enumerate(pdf):
                image_path = out/f'page-{i+1}.png'
                page.get_pixmap(matrix=fitz.Matrix(1000/720, 700/504), alpha=False).save(image_path)
                images.append({'page': i+1, 'path': image_path.name, 'sha256': native_deck.sha(image_path)})
            pdf.close()
            source_hash = built['artifacts'][role+'_pptx']['sha256']
            report = {'input_sha256': source_hash, 'pdf_sha256': native_deck.sha(pdf_path),
                      'pages': len(doc['pages']), 'rendered_pages': list(range(1, len(doc['pages'])+1)),
                      'page_images': images, 'export_pixels': [1000, 700]}
            renders[role] = {'state': 'verified', 'receipt': {
                'id': 'synthetic-'+role, 'status': 'completed', 'verified_input_sha256': source_hash,
                'result': {'snapshot_sha256': source_hash, 'out': str(out.relative_to(self.root)), 'report': report}}}
        cycle = {'schema_version': 1, 'state': 'ready_for_visual_review', 'workspace': str(self.root),
                 'build_record': str(root/'build/build.json'), 'build_record_sha256': native_deck.sha(root/'build/build.json'),
                 'renders': renders}
        native_deck.atomic_json(root/'cycle.json', cycle)
        return root/'cycle.json'

    def update_source_binding(self, cycle_path):
        cycle = native_deck.read_json(cycle_path); record_path = Path(cycle['build_record'])
        record = native_deck.read_json(record_path)
        source = record_path.parent/'editable.pptx'; digest = native_deck.sha(source)
        record['artifacts']['editable_pptx']['sha256'] = digest
        native_deck.atomic_json(record_path, record)
        cycle['build_record_sha256'] = native_deck.sha(record_path)
        receipt = cycle['renders']['editable']['receipt']
        receipt['verified_input_sha256'] = digest; receipt['result']['snapshot_sha256'] = digest
        receipt['result']['report']['input_sha256'] = digest
        native_deck.atomic_json(cycle_path, cycle)


class InspectionTests(InspectionFixture):
    def test_revision_panels_bind_before_after_and_keep_ownership_unknown(self):
        before = self.cycle('before')
        self.doc['pages'][0]['texts'][1]['box'][0] += 15
        after = self.cycle('after')
        report = native_inspect.run(after, self.root/'changes', previous_cycle=before)
        self.assertEqual(report['revision_comparison']['changed_count'], 1)
        self.assertEqual(report['revision_comparison']['changed_objects'][0]['id'], 'west')
        panels = [a for a in report['artifacts'] if a['kind'] == 'revision']
        self.assertEqual(len(panels), 1)
        self.assertTrue(Path(panels[0]['path']).is_file())
        self.assertFalse(report['ownership_summary']['semantic_ownership_verified'])
        self.assertEqual(len(report['ownership_summary']['without_declared_owner']), 3)
        old_png = before.parent/'render-editable/page-1.png'
        old_png.write_bytes(old_png.read_bytes()+b'changed')
        with self.assertRaisesRegex(ValueError, 'stale hash'):
            native_inspect.run(after, self.root/'bad-before', previous_cycle=before)

    def test_unchanged_revision_produces_no_new_object_repair_claim(self):
        before = self.cycle('before'); after = self.cycle('identical')
        report = native_inspect.run(after, self.root/'unchanged', previous_cycle=before)
        self.assertEqual(report['revision_comparison']['changed_count'], 0)
        self.assertFalse(any(a['kind'] == 'revision' for a in report['artifacts']))
        self.assertFalse(report['visual_quality_verified'])

    def test_sweep_covers_unflagged_pixels_without_claiming_review(self):
        cycle = self.cycle()
        report = native_inspect.run(cycle, self.root/'sweep', sweep_pages=['sample'])
        views = [a for a in report['artifacts'] if a['kind'] == 'sweep']
        self.assertEqual(len(views), 12)
        self.assertEqual({a['role'] for a in views}, {'source', 'editable', 'blank'})
        self.assertFalse(report['visual_quality_verified'])
        # Every pixel is covered by an editable viewport, including page edges.
        coverage = Image.new('L', (1000, 700))
        for a in views:
            self.assertFalse(a['visual_quality_verified'])
            self.assertLessEqual(max(a['output_size']), 1280)
            self.assertLessEqual(a['output_size'][0], a['crop_size'][0])
            self.assertLessEqual(a['output_size'][1], a['crop_size'][1])
            with Image.open(a['input_path']) as original, Image.open(a['path']) as actual:
                expected = original.convert('RGB').crop(a['crop_bounds_px'])
                self.assertEqual(actual.tobytes(), expected.tobytes())
            if a['role'] == 'editable':
                coverage.paste(255, tuple(a['crop_bounds_px']))
        self.assertEqual(coverage.getextrema(), (255, 255))
        # Review images cannot be silently mutated and reused.
        artifact = Path(views[-1]['path'])
        artifact.write_bytes(artifact.read_bytes()+b'tampered')
        with self.assertRaisesRegex(ValueError, 'evidence changed'):
            native_inspect.run(cycle, self.root/'sweep', sweep_pages=['sample'])

    def test_sweep_selection_is_explicit_and_fingerprint_bound(self):
        cycle = self.cycle()
        report = native_inspect.run(cycle, self.root/'normal')
        self.assertFalse(any(a['kind'] == 'sweep' for a in report['artifacts']))
        with self.assertRaisesRegex(ValueError, 'unknown page'):
            native_inspect.run(cycle, self.root/'unknown', sweep_pages=['missing'])
        with self.assertRaisesRegex(ValueError, 'Inputs changed'):
            native_inspect.run(cycle, self.root/'normal', sweep_pages=['sample'])

    def test_sweep_does_not_invent_labeled_source(self):
        doc = {'pages': [{'id': 'unlabeled', 'background': {'path': str(self.root/'art.png')}}]}
        render = {r: {'images': [self.root/'art.png']} for r in ['editable', 'blank']}
        views = native_inspect.sweep_views(doc, render, {'unlabeled'}, self.root)
        self.assertEqual(len(views), 8)
        self.assertEqual({v['role'] for v in views}, {'editable', 'blank'})

    def test_multilingual_repeated_labels_measure_separately(self):
        cycle = self.cycle()
        report = native_inspect.run(cycle, self.root/'inspection')
        self.assertEqual(len(report['objects']), 3)
        self.assertEqual([row['mapping'] for row in report['objects']], ['located']*3)
        by_id = {r['id']: r for r in report['objects']}
        self.assertLess(by_id['west']['ink_bounds_px'][0], by_id['east']['ink_bounds_px'][0])
        self.assertEqual(by_id['heading']['text'], '示例 / Example')
        self.assertFalse(report['visual_quality_verified'])
        self.assertEqual(report['coverage'][0]['without_declared_owner'], ['heading', 'west', 'east'])
        self.assertTrue(report['raster_difference'][0]['changed_pixels'] > 0)
        self.assertEqual([a['kind'] for a in report['artifacts']], ['overview', 'ownership'])

    def test_actual_pdf_glyphs_cross_frame_and_independent_owner(self):
        self.doc['pages'][0]['texts'][1]['box'][3] = 5
        cycle = self.cycle()
        regions = self.root/'regions.json'
        native_deck.atomic_json(regions, {'schema_version': 1, 'pages': [{
            'id': 'sample', 'basis': 'Synthetic narrow card drawn independently of the textbox.',
            'regions': {'small-card': [600, 220, 35, 110]},
            'objects': {'east': {'region': 'small-card'}}}]})
        report = native_inspect.run(cycle, self.root/'inspection', regions)
        keys = {(r['object'], r['code']) for r in report['findings']}
        self.assertIn(('west', 'INK_OUTSIDE_FRAME_CANDIDATE'), keys)
        self.assertIn(('east', 'INK_OUTSIDE_OWNER_CANDIDATE'), keys)
        self.assertTrue(any(a['kind'] == 'details' for a in report['artifacts']))

    def test_mutated_artifact_pdf_and_stale_cycle_are_rejected(self):
        for role in ['artwork', 'pdf', 'cycle']:
            with self.subTest(role=role):
                cycle = self.cycle(role)
                if role == 'pdf':
                    path = cycle.parent/'render-editable/editable.pdf'
                elif role == 'artwork':
                    path = self.root/'art.png'
                else:
                    path = cycle.parent/'build/resolved.json'
                previous = path.read_bytes(); path.write_bytes(previous+b'corrupt')
                with self.assertRaises(ValueError):
                    native_inspect.run(cycle, self.root/('inspection-'+role))
                self.assertFalse((self.root/('inspection-'+role)).exists())
                path.write_bytes(previous)

    def test_ambiguous_repeated_glyph_occurrences_stay_unknown(self):
        self.doc['pages'][0]['texts'][2]['box'] = list(self.doc['pages'][0]['texts'][1]['box'])
        cycle = self.cycle()
        report = native_inspect.run(cycle, self.root/'inspection')
        unknown = [r for r in report['findings'] if r['code'] == 'PDF_MAPPING_UNRESOLVED']
        self.assertEqual({r['object'] for r in unknown}, {'west', 'east'})
        self.assertTrue(all(r['severity'] == 'unknown' for r in unknown))

    def test_focused_scope_does_not_claim_missing_checks_resolved(self):
        self.doc['pages'][0]['texts'][1]['box'][3] = 5
        cycle = self.cycle()
        old = self.root/'all'; native_inspect.run(cycle, old)
        report = native_inspect.run(cycle, self.root/'focused', objects=['east'], previous=old/'report.json')
        self.assertEqual(len(report['objects']), 1)
        self.assertEqual(report['coverage'][0]['unmeasured_objects'], ['heading', 'west'])
        self.assertEqual(report['comparison']['no_longer_detected'], [])
        self.assertIn(('sample', 'west'), [tuple(x) for x in report['comparison']['not_rechecked']])
        with self.assertRaisesRegex(ValueError, 'Unknown requested object'):
            native_inspect.run(cycle, self.root/'wrong-id', objects=['not-present'])

    def test_whitespace_and_small_shifts_do_not_become_pixel_similarity_failures(self):
        self.doc['pages'][0]['texts'][1]['text'] = 'Repeat\n7'
        cycle = self.cycle(render_offsets={'west': [.2, .2]})
        report = native_inspect.run(cycle, self.root/'inspection')
        self.assertFalse(any(r['code'] == 'COPY_MISMATCH' for r in report['findings']))
        self.assertFalse(any('SIMILARITY' in r['code'] for r in report['findings']))
        self.assertEqual(next(r for r in report['objects'] if r['id'] == 'west')['rendered_lines'], 2)

    def test_font_minimum_is_explicit_policy_not_invented(self):
        cycle = self.cycle()
        baseline = native_inspect.run(cycle, self.root/'baseline')
        self.assertFalse(any(r['code'] == 'EXPLICIT_MINIMUM_FONT_SIZE' for r in baseline['findings']))
        regions = self.root/'policy.json'
        native_deck.atomic_json(regions, {'schema_version': 1, 'pages': [{
            'id': 'sample', 'basis': 'User expressly requests 18-point minimum.', 'policy': {'min_size_pt': 18}}]})
        report = native_inspect.run(cycle, self.root/'explicit', regions)
        self.assertEqual(sum(r['code'] == 'EXPLICIT_MINIMUM_FONT_SIZE' for r in report['findings']), 3)

    def test_native_copy_disagreement_and_missing_object_are_not_hidden_by_pdf(self):
        cycle = self.cycle()
        path = cycle.parent/'build/editable.pptx'; prs = Presentation(path)
        heading = next(s for s in prs.slides[0].shapes if s.name == 'TEXT__heading')
        heading.text_frame.paragraphs[0].runs[0].text = 'Changed 8'
        east = next(s for s in prs.slides[0].shapes if s.name == 'TEXT__east')
        east._element.getparent().remove(east._element)
        prs.save(path); self.update_source_binding(cycle)
        report = native_inspect.run(cycle, self.root/'inspection')
        keys = {(r['object'], r['code']) for r in report['findings']}
        self.assertIn(('heading', 'COPY_MISMATCH'), keys)
        self.assertIn(('east', 'NATIVE_OBJECT_UNRESOLVED'), keys)

    def test_same_bound_inspection_is_reused_and_tampering_is_rejected(self):
        cycle = self.cycle(); out = self.root/'inspection'
        first = native_inspect.run(cycle, out); before = (out/'report.json').stat().st_mtime_ns
        self.assertEqual(first, native_inspect.run(cycle, out))
        self.assertEqual(before, (out/'report.json').stat().st_mtime_ns)
        artifact = Path(first['artifacts'][0]['path']); artifact.write_bytes(artifact.read_bytes()+b'tampered')
        with self.assertRaisesRegex(ValueError, 'evidence changed'):
            native_inspect.run(cycle, out)

    def test_partial_render_cannot_serve_as_complete_evidence(self):
        cycle = self.cycle(); value = native_deck.read_json(cycle)
        value['renders']['editable']['receipt']['result']['report']['rendered_pages'] = []
        native_deck.atomic_json(cycle, value)
        with self.assertRaisesRegex(ValueError, 'Incomplete render coverage'):
            native_inspect.run(cycle, self.root/'inspection')

    def test_declared_artwork_collision_and_unknown_check_are_explicit(self):
        cycle = self.cycle(); regions = self.root/'regions.json'
        config = {'schema_version': 1, 'pages': [{
            'id': 'sample', 'basis': 'Synthetic arrow rectangle from artwork.',
            'forbidden': {'arrow': [80, 230, 250, 50]}}]}
        native_deck.atomic_json(regions, config)
        report = native_inspect.run(cycle, self.root/'inspection', regions)
        self.assertIn(('west', 'INK_OVER_ARTWORK_CANDIDATE'),
                      {(r['object'], r['code']) for r in report['findings']})
        config['pages'][0]['objects'] = {'west': {'unsupported_lines': 2}}
        native_deck.atomic_json(regions, config)
        with self.assertRaisesRegex(ValueError, 'Unknown object check'):
            native_inspect.run(cycle, self.root/'bad', regions)

    def test_nominal_font_ascender_is_informational_when_real_ink_fits(self):
        cycle = self.cycle('metrics')
        before = native_inspect.run(cycle, self.root/'metrics-inspection')
        west = next(r for r in before['objects'] if r['id'] == 'west')
        x, y, w, h = west['ink_bounds_px']
        item = self.doc['pages'][0]['texts'][1]
        # Put a legitimate native frame tightly around real rendered glyph ink.
        # Preserve the PDF glyph origin to isolate nominal ascenders from overflow.
        item['box'] = [x-.5, y-.5, w+1, h+1]
        cycle = self.cycle('ink-fit', render_offsets={'west': [80-(x-.5), 230-(y-.5)]})
        report = native_inspect.run(cycle, self.root/'ink-fit-inspection')
        west = next(r for r in report['objects'] if r['id'] == 'west')
        self.assertLess(west['font_metric_frame_clearance_px']['top'], -1)
        self.assertGreaterEqual(west['frame_clearance_px']['top'], 0)
        self.assertEqual(west['ink_attribution']['state'], 'attributed')
        self.assertNotIn('INK_OUTSIDE_FRAME_CANDIDATE', west['findings'])

    def test_real_overlapping_native_text_stays_unknown_for_ink_attribution(self):
        page = self.doc['pages'][0]
        page['copy']['west'] = page['texts'][1]['text'] = 'Alpha'
        page['copy']['east'] = page['texts'][2]['text'] = 'Beta'
        page['texts'][2]['box'] = list(page['texts'][1]['box'])
        report = native_inspect.run(self.cycle(), self.root/'inspection')
        for row in report['objects'][1:]:
            self.assertEqual(row['mapping'], 'located')
            self.assertEqual(row['ink_attribution']['state'], 'unknown')
            self.assertIn('INK_ATTRIBUTION_UNRESOLVED', row['findings'])
            self.assertNotIn('frame_clearance_px', row)

    def test_missing_or_low_contrast_ink_is_not_a_spatial_pass(self):
        cycle = self.cycle(render_colors={'west': (247/255, 244/255, 237/255)})
        report = native_inspect.run(cycle, self.root/'inspection')
        west = next(r for r in report['objects'] if r['id'] == 'west')
        self.assertEqual(west['mapping'], 'located')
        self.assertEqual(west['ink_attribution']['state'], 'unknown')
        self.assertNotIn('frame_clearance_px', west)
        self.assertIn('INK_ATTRIBUTION_UNRESOLVED', west['findings'])

    def test_connected_ink_overhang_and_zero_advance_ligature_cell(self):
        mask = Image.new('L', (100, 100), 0)
        ImageDraw.Draw(mask).rectangle((20, 30, 50, 49), fill=255)
        mappings = {'one': {'frame': [.1, .2, .6, .6], 'mapped': {
            'characters': [{'rect': [.2, .25, .4, .55]}, {'rect': [.4, .25, .4, .55]}]}}}
        inks, summary = native_inspect.attribute_ink(mask, mappings)
        self.assertEqual(inks['one']['state'], 'attributed')
        self.assertEqual(inks['one']['shared_or_zero_advance_cells'], 1)
        self.assertEqual(inks['one']['rect'], [.2, .3, .51, .5])
        self.assertEqual(summary['unassigned_pixels'], 0)

    def test_revision_automatically_keeps_unchanged_warning_in_feedback(self):
        import native_review
        self.doc['pages'][0]['texts'][1]['box'][3] = 5
        before = self.cycle('before')
        native_inspect.run(before, before.parent/'inspection')
        # Repair another object; the first object's warning must remain visible.
        self.doc['pages'][0]['texts'][2]['box'][0] += 15
        after = self.cycle('after')
        report = native_inspect.run(after, after.parent/'inspection', previous_cycle=before)
        self.assertIn(['sample', 'west', 'INK_OUTSIDE_FRAME_CANDIDATE'], report['comparison']['persisting'])
        review = native_review.summarize(report)
        west = next(r for r in review['object_findings'] if r['object'] == 'west')
        self.assertTrue(any(f['change'] == 'persisting' for f in west['findings']))
        self.assertTrue(west['evidence'])
        self.assertLess(west['frame_clearance_px']['bottom'], 0)

    def test_revision_distinguishes_repaired_and_new_candidates(self):
        self.doc['pages'][0]['texts'][1]['box'][3] = 5
        before = self.cycle('before'); native_inspect.run(before, before.parent/'inspection')
        self.doc['pages'][0]['texts'][1]['box'][3] = 90
        self.doc['pages'][0]['texts'][2]['box'][3] = 5
        after = self.cycle('after')
        report = native_inspect.run(after, after.parent/'inspection', previous_cycle=before)
        self.assertIn(['sample','west','INK_OUTSIDE_FRAME_CANDIDATE'], report['comparison']['no_longer_detected'])
        self.assertIn(['sample','east','INK_OUTSIDE_FRAME_CANDIDATE'], report['comparison']['new'])
        self.assertFalse(report['visual_quality_verified'])

    def test_expanding_owner_is_not_reported_as_a_text_repair(self):
        page = self.doc['pages'][0]
        page['texts'][1]['box'][3] = 5
        page['texts'][1]['region'] = 'small'
        page['regions'] = {'small': [75,225,340,10]}
        before = self.cycle('before'); native_inspect.run(before, before.parent/'inspection')
        page['regions']['small'][3] = 200
        after = self.cycle('after')
        report = native_inspect.run(after, after.parent/'inspection', previous_cycle=before)
        key = ['sample','west','INK_OUTSIDE_OWNER_CANDIDATE']
        self.assertIn(key, report['comparison']['owner_basis_changed'])
        self.assertNotIn(key, report['comparison']['no_longer_detected'])

    def test_wrong_previous_cycle_and_changed_policy_do_not_claim_progress(self):
        before = self.cycle('before'); old = before.parent/'inspection'
        native_inspect.run(before, old)
        after = self.cycle('after')
        with self.assertRaisesRegex(ValueError, 'does not belong'):
            native_inspect.run(after, self.root/'wrong', previous=old/'report.json', previous_cycle=after)
        policy = self.root/'policy.json'
        native_deck.atomic_json(policy, {'schema_version':1,'pages':[
            {'id':'sample','basis':'Explicit test policy','policy':{'min_size_pt':20}}]})
        report = native_inspect.run(after, self.root/'changed-policy', regions=policy, previous_cycle=before)
        self.assertTrue(report['comparison']['measurement_criteria_changed'])
        self.assertEqual(report['comparison']['no_longer_detected'], [])

    def test_broad_owner_visible_without_claiming_semantic_correctness(self):
        page = self.doc['pages'][0]
        page['regions'] = {'panel':[40,180,920,440]}
        for text in page['texts'][1:]: text['region'] = 'panel'
        cycle = self.cycle()
        report = native_inspect.run(cycle, self.root/'inspection')
        west = next(r for r in report['objects'] if r['id'] == 'west')
        self.assertEqual(west['owner']['assigned_objects'], ['west','east'])
        self.assertGreater(west['owner']['area_to_frame_ratio'], 10)
        overlay = next(a for a in report['artifacts'] if a['kind'] == 'ownership')
        with Image.open(overlay['path']) as im: self.assertEqual(im.size, (1920,620))
        self.assertFalse(report['ownership_summary']['semantic_ownership_verified'])


if __name__ == '__main__':
    unittest.main()
