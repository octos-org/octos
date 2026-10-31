"""Real-font proposal behavior on synthetic copy, without page-specific solutions."""
from pathlib import Path
import copy
import os
import sys
import tempfile
import unittest

from PIL import Image

sys.path.insert(0, str(Path(__file__).parents[1]))
import native_deck
import native_fit


class FitTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        candidates = [os.environ.get('PPTX_TEST_FONT', ''), '/System/Library/Fonts/Supplemental/Arial.ttf',
                      '/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf']
        self.font = next((Path(p) for p in candidates if p and Path(p).is_file()), None)
        if self.font is None: self.skipTest('Provide PPTX_TEST_FONT for real-font fitting tests')
        Image.new('RGB', (1000, 600), 'white').save(self.root/'art.png')
        self.doc = {'schema_version': 1, 'canvas': {'width_px': 1000, 'height_px': 600, 'width_in': 10, 'height_in': 6},
                    'styles': {'label': {'font': 'TestFace', 'size_pt': 24, 'wrap': True}},
                    'pages': [{'id': 'network', 'background': 'art.png', 'copy': {'caption': '400GbE'},
                               'texts': [{'id': 'caption', 'text': '400GbE', 'box': [30, 40, 95, 70], 'style': 'label'}]}]}
        self.spec, self.fonts = self.root/'doc.json', self.root/'fonts.json'
        native_deck.atomic_json(self.spec, self.doc)
        native_deck.atomic_json(self.fonts, {'schema_version': 1, 'fonts': [{'family': 'TestFace', 'path': str(self.font)}]})

    def test_default_does_not_silently_shrink_or_split_identifier(self):
        before = self.spec.read_bytes()
        report = native_fit.propose(self.spec, self.fonts)
        self.assertEqual(report['objects'][0]['state'], 'no_fit_within_bounds')
        self.assertEqual(report['proposed_edits'], [])
        self.assertEqual(self.spec.read_bytes(), before)
        self.assertFalse(report['visual_quality_verified'])

    def test_explicit_bounds_produce_measured_copy_preserving_patch(self):
        report = native_fit.propose(self.spec, self.fonts, min_size=8, max_lines=1)
        row = report['objects'][0]
        self.assertEqual(row['state'], 'proposal')
        self.assertGreaterEqual(row['size_pt'], 8)
        self.assertLess(row['size_pt'], 24)
        self.assertEqual(row['lines'], ['400GbE'])
        self.assertLessEqual(row['measured_pt'][0], row['available_pt'][0]+.001)
        native_deck.atomic_json(self.root/'edits.json', report['proposed_edits'])
        native_deck.patch_spec(self.spec, self.root/'edits.json', self.root/'revised.json')
        revised, _ = native_deck.load_spec(self.root/'revised.json')
        self.assertEqual(revised['pages'][0]['copy']['caption'], '400GbE')

    def test_locked_style_and_unknown_fonts_are_not_overridden(self):
        self.doc['styles']['label']['locked'] = True
        native_deck.atomic_json(self.spec, self.doc)
        report = native_fit.propose(self.spec, self.fonts, min_size=8)
        self.assertEqual(report['proposed_edits'], [])
        self.assertTrue(report['objects'][0]['locked_style'])
        self.doc['styles']['label']['font'] = 'NotInstalled'
        native_deck.atomic_json(self.spec, self.doc)
        row = native_fit.propose(self.spec, self.fonts)['objects'][0]
        self.assertEqual(row['state'], 'unresolved')

    def test_missing_glyphs_refuse_a_false_fit(self):
        self.doc['pages'][0]['texts'][0]['text'] = '\U0010ffff'
        self.doc['pages'][0]['copy']['caption'] = '\U0010ffff'
        native_deck.atomic_json(self.spec, self.doc)
        report = native_fit.propose(self.spec, self.fonts, min_size=8)
        self.assertEqual(report['objects'][0]['state'], 'unresolved')
        self.assertIn('lacks glyphs', report['objects'][0]['reason'])
        self.assertFalse(report['proposed_edits'])

    def test_explicit_bounds_cannot_override_locked_style(self):
        self.doc['styles']['label']['locked'] = True
        native_deck.atomic_json(self.spec, self.doc)
        row = native_fit.propose(self.spec, self.fonts, min_size=30, max_size=35)['objects'][0]
        self.assertEqual(row['state'], 'unresolved')
        self.assertIn('Locked style conflicts', row['reason'])

    def test_invalid_font_file_is_reported_without_a_proposal(self):
        invalid = self.root/'invalid.ttf'; invalid.write_text('not a font')
        native_deck.atomic_json(self.fonts, {'schema_version':1,'fonts':[{'family':'TestFace','path':str(invalid)}]})
        report = native_fit.propose(self.spec, self.fonts, min_size=8)
        self.assertEqual(report['objects'][0]['state'], 'unresolved')
        self.assertIn('font face', report['objects'][0]['reason'])
        self.assertFalse(report['proposed_edits'])

    def test_explicit_line_limit_and_paragraphs_are_preserved(self):
        self.doc['pages'][0]['texts'][0].update(text='Alpha beta\nGamma delta', box=[30,40,220,160])
        self.doc['pages'][0]['copy']['caption'] = 'Alpha beta\nGamma delta'
        native_deck.atomic_json(self.spec, self.doc)
        report = native_fit.propose(self.spec, self.fonts, min_size=12, max_lines=1)
        self.assertEqual(report['objects'][0]['state'], 'no_fit_within_bounds')
        report = native_fit.propose(self.spec, self.fonts, min_size=12, max_lines=4)
        self.assertEqual(report['objects'][0]['state'], 'proposal')
        self.assertEqual(native_deck.normalized('\n'.join(report['objects'][0]['lines'])),
                         native_deck.normalized('Alpha beta\nGamma delta'))

    def test_already_fitting_text_is_not_changed(self):
        self.doc['pages'][0]['texts'][0]['box'] = [30,40,500,100]
        native_deck.atomic_json(self.spec, self.doc)
        report = native_fit.propose(self.spec, self.fonts, min_size=8)
        self.assertEqual(report['objects'][0]['size_pt'], 24)
        self.assertEqual(report['proposed_edits'], [])

    def test_cjk_labels_use_actual_glyphs_and_preserve_copy(self):
        candidates = [os.environ.get('PPTX_TEST_CJK_FONT', ''), '/System/Library/Fonts/STHeiti Medium.ttc']
        path = next((Path(p) for p in candidates if p and Path(p).is_file()), None)
        if path is None: self.skipTest('Provide PPTX_TEST_CJK_FONT to exercise CJK metrics')
        native_deck.atomic_json(self.fonts, {'schema_version':1,'fonts':[{'family':'TestFace','path':str(path),'index':0}]})
        self.doc['pages'][0]['copy']['caption'] = '温度分布'
        self.doc['pages'][0]['texts'][0]['text'] = '温度分布'
        native_deck.atomic_json(self.spec, self.doc)
        report = native_fit.propose(self.spec, self.fonts, min_size=12, max_lines=1)
        self.assertEqual(report['objects'][0]['state'], 'proposal')
        self.assertEqual(report['objects'][0]['lines'], ['温度分布'])
        self.assertGreaterEqual(report['objects'][0]['size_pt'], 12)

    def test_different_page_ids_and_order_do_not_change_proposals(self):
        before = native_fit.propose(self.spec, self.fonts, min_size=8)['objects'][0]
        self.doc['pages'][0]['id'] = 'arbitrary-copy'
        self.doc['pages'].insert(0, dict(id='unrelated', background='art.png', copy={'x':'Other'},
                                        texts=[dict(id='x', text='Other', style='label', box=[30,200,300,100])]))
        native_deck.atomic_json(self.spec, self.doc)
        after = native_fit.propose(self.spec, self.fonts, ['arbitrary-copy/caption'], min_size=8)['objects'][0]
        self.assertEqual(before['size_pt'], after['size_pt'])

    def test_ink_alignment_proposes_geometry_and_keeps_size_copy_and_source(self):
        self.doc['pages'][0]['texts'][0]['box'] = [80,150,400,100]
        native_deck.atomic_json(self.spec, self.doc)
        before = self.spec.read_bytes()
        report = native_fit.propose(self.spec, self.fonts, ink_align='middle')
        row = report['objects'][0]
        self.assertEqual(row['state'], 'proposal')
        self.assertEqual(row['size_pt'], 24)
        placement = row['ink_placement']; x,y,w,h = placement['predicted_ink_box_px']
        self.assertAlmostEqual(y+h/2, 200, places=4)
        self.assertAlmostEqual(x, 80, places=4)
        self.assertNotEqual(placement['proposed_frame_px'], row['box'])
        native_deck.atomic_json(self.root/'edits.json', report['proposed_edits'])
        native_deck.patch_spec(self.spec, self.root/'edits.json', self.root/'positioned.json')
        revised, _ = native_deck.load_spec(self.root/'positioned.json')
        self.assertEqual(revised['pages'][0]['copy']['caption'], '400GbE')
        self.assertEqual(self.spec.read_bytes(), before)
        self.assertFalse(native_deck.effective_style(revised, revised['pages'][0]['texts'][0])['wrap'])

    def test_explicit_target_and_multiline_center_preserve_locked_typography(self):
        self.doc['styles']['label'].update(locked=True, wrap=False, align='center')
        item = self.doc['pages'][0]['texts'][0]
        item.update(box=[80,100,600,200], text='Alpha\nGamma')
        self.doc['pages'][0]['copy']['caption'] = item['text']
        native_deck.atomic_json(self.spec, self.doc)
        target = self.root/'targets.json'
        native_deck.atomic_json(target, {'schema_version':1,'objects':[
            {'page':'network','id':'caption','box':[100,200,500,160]}]})
        report = native_fit.propose(self.spec,self.fonts,ink_align='middle',targets=target)
        row = report['objects'][0]; self.assertEqual(row['state'], 'proposal')
        x,y,w,h = row['ink_placement']['predicted_ink_box_px']
        self.assertAlmostEqual(x+w/2, 350, places=4)
        self.assertAlmostEqual(y+h/2, 280, places=4)
        self.assertNotIn('format', report['proposed_edits'][0]['set'])
        self.assertIn(str(target.resolve()), report['dependencies'])

    def test_locked_wrap_requires_model_style_decision_without_unsafe_patch(self):
        self.doc['styles']['label']['locked'] = True
        self.doc['pages'][0]['texts'][0]['box'] = [80,100,600,200]
        native_deck.atomic_json(self.spec,self.doc)
        report = native_fit.propose(self.spec,self.fonts,ink_align='middle')
        self.assertEqual(report['objects'][0]['state'], 'needs_style_decision')
        self.assertEqual(report['proposed_edits'], [])

    def test_ink_proposal_does_not_move_owner_borders_or_leave_canvas(self):
        item = self.doc['pages'][0]['texts'][0]
        item['box'] = [80,0,400,30]
        native_deck.atomic_json(self.spec,self.doc)
        report = native_fit.propose(self.spec,self.fonts,ink_align='top')
        self.assertEqual(report['objects'][0]['state'], 'unresolved')
        self.assertEqual(report['proposed_edits'], [])
        item['box'] = [80,100,400,30]; item['region']='tight'
        self.doc['pages'][0]['regions']={'tight':list(item['box'])}
        native_deck.atomic_json(self.spec,self.doc)
        report = native_fit.propose(self.spec,self.fonts,ink_align='top')
        self.assertEqual(report['objects'][0]['state'], 'unresolved')
        self.assertEqual(report['proposed_edits'], [])

    def test_ink_target_preserves_vertical_anchor_and_margins(self):
        for anchor in ['top','middle','bottom']:
            with self.subTest(anchor=anchor):
                self.doc['styles']['label'].update(anchor=anchor,align='right',margins_px=[4,8,6,10])
                self.doc['pages'][0]['texts'][0]['box']=[100,150,500,180]
                native_deck.atomic_json(self.spec,self.doc)
                report=native_fit.propose(self.spec,self.fonts,ink_align='middle')
                row=report['objects'][0]
                self.assertEqual(row['state'],'proposal')
                x,y,w,h=row['ink_placement']['predicted_ink_box_px']
                self.assertAlmostEqual(y+h/2,240,places=4)
                self.assertAlmostEqual(x+w,600,places=4)
                self.assertNotIn('anchor',report['proposed_edits'][0]['set'].get('format',{}))


if __name__ == '__main__': unittest.main()
