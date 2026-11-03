"""Regression tests against real python-pptx output and malformed XML choices."""
from pathlib import Path
import sys
import tempfile
import unittest
from zipfile import ZipFile

from pptx import Presentation
from pptx.enum.text import MSO_AUTO_SIZE
from pptx.util import Inches

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from check_pptx_compat import inspect


class CompatibilityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'test.pptx'
        presentation = Presentation()
        slide = presentation.slides.add_slide(presentation.slide_layouts[6])
        frame = slide.shapes.add_textbox(Inches(1), Inches(1), Inches(4), Inches(1)).text_frame
        frame.text = 'Editable 中文 123'
        # Public setter is idempotent; a second manual XML append is not needed.
        for _ in range(3):
            frame.auto_size = MSO_AUTO_SIZE.NONE
        presentation.save(self.path)

    def mutate(self, transform):
        with ZipFile(self.path) as source:
            parts = {name: source.read(name) for name in source.namelist()}
        name = 'ppt/slides/slide1.xml'
        parts[name] = transform(parts[name])
        with ZipFile(self.path, 'w') as output:
            for name, content in parts.items():
                output.writestr(name, content)

    def test_public_api_repeated_set_remains_valid(self):
        before = self.path.read_bytes()
        self.assertTrue(inspect(self.path)['preflight_passed'])
        self.assertEqual(before, self.path.read_bytes())

    def test_duplicate_no_autofit_is_rejected(self):
        self.mutate(lambda b: b.replace(b'<a:noAutofit/>', b'<a:noAutofit/><a:noAutofit/>'))
        result = inspect(self.path)
        self.assertFalse(result['preflight_passed'])
        self.assertEqual(result['issues'][0]['code'], 'autofit_choice')

    def test_conflicting_autofit_modes_are_rejected(self):
        self.mutate(lambda b: b.replace(b'<a:noAutofit/>', b'<a:noAutofit/><a:spAutoFit/>'))
        self.assertFalse(inspect(self.path)['preflight_passed'])

    def test_no_explicit_autofit_is_allowed(self):
        self.mutate(lambda b: b.replace(b'<a:noAutofit/>', b''))
        self.assertTrue(inspect(self.path)['preflight_passed'])

    def test_invalid_xml_is_not_reported_as_pass(self):
        self.mutate(lambda b: b[:-12])
        self.assertFalse(inspect(self.path)['preflight_passed'])


if __name__ == '__main__':
    unittest.main()
