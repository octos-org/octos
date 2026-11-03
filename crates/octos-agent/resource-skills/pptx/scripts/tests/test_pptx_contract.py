"""Project-independent acceptance: no fixed fonts, pages, model or image pairs."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import pptx_contract as q
from pptx import Presentation
from pptx.enum.text import PP_ALIGN
from pptx.util import Inches, Pt
from pptx.dml.color import RGBColor


class Contracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / 'deck.pptx'

    def deck(self, titles=('Quarterly review', 'Results', 'Next steps')):
        prs = Presentation()
        for i, title in enumerate(titles):
            slide = prs.slides.add_slide(prs.slide_layouts[6])
            box = slide.shapes.add_textbox(Inches(.5), Inches(.3), Inches(8), Inches(.7))
            box.name = 'heading'
            p = box.text_frame.paragraphs[0]
            p.alignment = PP_ALIGN.CENTER if i == 0 else PP_ALIGN.LEFT
            run = p.add_run(); run.text = title
            run.font.name = 'Arial'; run.font.size = Pt(28 if i == 0 else 24)
            run.font.bold = True; run.font.color.rgb = RGBColor.from_string('153E75')
        prs.save(self.path)
        return prs

    def contract(self):
        return {'version': 1, 'artifact': 'deck.pptx', 'pages': [
            {'id': 'cover', 'title': 'Quarterly review'},
            {'id': 'results', 'title': 'Results'},
            {'id': 'actions', 'title': 'Next steps'}], 'rules': [
                {'id': 'cover-style', 'page_ids': ['cover'], 'select': {'role': 'title'},
                 'count': 1, 'style': {'font_names': ['Arial'], 'font_size_pt': 28,
                                     'alignment': 'center', 'color_rgb': '153E75'}},
                {'id': 'body-style', 'page_ids': ['results', 'actions'],
                 'select': {'role': 'title'}, 'count': 1,
                 'style': {'font_names': ['Arial'], 'font_size_pt': 24, 'alignment': 'left'},
                 'consistent': ['left_origin', 'top_origin']} ]}

    def findings(self, c=None):
        return q.inspect(self.path, c or self.contract())['findings']

    def test_non_kdn_native_deck_accepts_own_style_without_images(self):
        self.deck()
        self.assertEqual(self.findings(), [])

    def test_unconfigured_deck_does_not_invent_style_or_count(self):
        self.deck(('Different subject', 'Diagram'))
        self.assertEqual(self.findings({'version': 1, 'artifact': 'deck.pptx'}), [])

    def test_glossary_checks_exported_text_across_arbitrary_pages(self):
        for index in (0, 2):
            prs=self.deck()
            box=prs.slides[index].shapes.add_textbox(Inches(1), Inches(2), Inches(5), Inches(1))
            box.text='ABC (Apple Beta Count)'; prs.save(self.path)
            c=self.contract();c['glossary']={'ABC':{'expansions':['Alpha Beta Count']}}
            issues=self.findings(c)
            self.assertEqual(len(issues),1)
            self.assertEqual(issues[0]['check'],'glossary-expansion')
            self.assertEqual(issues[0]['page'],c['pages'][index]['id'])
            box.text='ABC (Alpha Beta Count)';prs.save(self.path)
            self.assertEqual(self.findings(c),[])

    def test_actual_style_and_color_violation(self):
        prs = self.deck()
        run = prs.slides[1].shapes[0].text_frame.paragraphs[0].runs[0]
        run.font.size = Pt(31); prs.save(self.path)
        self.assertTrue(any(x['check'] == 'body-style' for x in self.findings()))
        prs = self.deck()
        prs.slides[0].shapes[0].text_frame.paragraphs[0].runs[0].font.color.rgb = RGBColor(0, 0, 0)
        prs.save(self.path)
        self.assertTrue(any('color_rgb' in x['detail'] for x in self.findings()))

    def test_missing_reordered_duplicate_titles_are_rejected(self):
        for titles in [('Quarterly review', 'Results'), ('Results', 'Quarterly review', 'Next steps')]:
            self.deck(titles)
            self.assertTrue(self.findings())
        prs = self.deck()
        prs.slides[1].shapes.add_textbox(0, 0, Inches(1), Inches(1)).text = 'Results'
        prs.save(self.path)
        self.assertTrue(any(x['check'] == 'title' for x in self.findings()))

    def test_native_table_text_participates_in_copy_checks(self):
        prs = self.deck()
        table = prs.slides[1].shapes.add_table(2, 2, Inches(1), Inches(2), Inches(5), Inches(2)).table
        table.cell(0, 0).text = 'Revenue'; table.cell(1, 0).text = '$12M'
        prs.save(self.path)
        c = self.contract(); c['pages'][1]['required_text'] = [{'text': '$12M', 'count': 1}]
        self.assertEqual(self.findings(c), [])
        c['pages'][1]['required_text'][0]['text'] = '$21M'
        self.assertTrue(any(x['check'] == 'required-text' for x in self.findings(c)))

    def test_subsets_preserve_authoritative_page_mapping(self):
        self.deck(('Results',))
        # Page candidate must retain the configured body style, not act as a cover.
        prs = Presentation(self.path); p = prs.slides[0].shapes[0].text_frame.paragraphs[0]
        p.alignment = PP_ALIGN.LEFT; p.runs[0].font.size = Pt(24); prs.save(self.path)
        c = q.select_pages(self.contract(), ['results'])
        self.assertEqual(self.findings(c), [])
        with self.assertRaises(ValueError): q.select_pages(self.contract(), ['producer-invented-id'])

    def test_inherited_style_is_unknown_not_assumed_wrong_or_verified(self):
        prs = self.deck()
        prs.slides[1].shapes[0].text_frame.paragraphs[0].runs[0].font.name = None
        prs.save(self.path)
        self.assertTrue(any(x.get('status') == 'unknown' for x in self.findings()))

    def test_invalid_policy_fails_loudly(self):
        for bad in ({'version': 1, 'artifact': 'a', 'rules': [{'id': 'oops', 'select': {'rol': 'title'}}]},
                    {'version': 1, 'artifact': 'a', 'checks': {'typo': True}},
                    {'version': 1, 'artifact': 'a', 'pages': [{'id': 'a'}, {'id': 'a'}]}):
            with self.assertRaises(ValueError): q.validate_contract(bad)

    def test_content_spaces_not_silently_removed(self):
        self.deck(('Net work',))
        self.assertTrue(self.findings({'version': 1, 'artifact': 'a', 'pages': [{'id': 'x', 'title': 'Network'}]}))

    def test_group_scaling_is_measured_in_slide_coordinates(self):
        prs = self.deck()
        group = prs.slides[1].shapes.add_group_shape()
        group.shapes.add_textbox(Inches(1), Inches(2), Inches(2), Inches(1)).text = 'Grouped label'
        group.left = Inches(3); group.top = Inches(3); group.width = Inches(4)
        prs.save(self.path)
        records = q.inspect(self.path, self.contract())['pages'][1]
        obj = next(o for o in records if o['text'] == 'Grouped label')
        self.assertAlmostEqual(obj['bbox'][0], Inches(3) / prs.slide_width)
        self.assertAlmostEqual(obj['bbox'][2], Inches(7) / prs.slide_width)

    def test_rotation_does_not_claim_precise_geometry(self):
        prs = self.deck(); prs.slides[0].shapes[0].rotation = 30; prs.save(self.path)
        c = self.contract(); c['checks'] = {'text_in_canvas': True}
        self.assertTrue(any(f.get('status') == 'unknown' and f['check'] == 'text-in-canvas' for f in self.findings(c)))


if __name__ == '__main__':
    unittest.main()
