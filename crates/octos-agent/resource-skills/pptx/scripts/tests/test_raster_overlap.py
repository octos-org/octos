import json
from pathlib import Path
import tempfile
import unittest
import zipfile
from PIL import Image
from pptx import Presentation
from pptx.util import Inches
from raster_overlap import proposals, parse_review, textless_snapshot

class RasterOverlapTests(unittest.TestCase):
    def test_proposal_does_not_require_matching_language_or_confidence(self):
        detections=[{'text':'otherwise','confidence':.2,'x':40,'y':40,'w':15,'h':10}]
        objects=[{'id':'label','text':'或者','bbox':[.4,.4,.6,.6]}]
        found=proposals(detections,(100,100),objects)
        self.assertEqual(len(found),1)
        self.assertEqual(found[0]['native'][0]['id'],'label')

    def test_nearby_graphic_label_is_not_a_collision(self):
        self.assertEqual(proposals([{'text':'A','x':10,'y':10,'w':5,'h':5}],(100,100),[{'id':'b','text':'B','bbox':[.3,.3,.5,.5]}]),[])

    def test_ocr_proposal_can_be_rejected_by_visual_inspection(self):
        result=parse_review(json.dumps({'candidates':[{'id':'1','status':'pass','detail':'OCR interpreted a decorative gear as a letter; native text is separately legible.'}]}),[{'id':'1'}])
        self.assertEqual(result['status'],'pass')

    def test_unknown_or_incomplete_review_never_passes(self):
        self.assertEqual(parse_review('{"candidates":[{"id":"1","status":"unknown","detail":"Pixels unresolved"}]}',[{'id':'1'}])['status'],'fail')
        with self.assertRaises(ValueError):parse_review('{"candidates":[]}',[{'id':'1'}])

    def test_textless_derivative_preserves_picture_bytes_and_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);image=root/'image.png';Image.new('RGB',(160,90),'white').save(image)
            prs=Presentation();slide=prs.slides.add_slide(prs.slide_layouts[6]);slide.shapes.add_picture(str(image),0,0,width=prs.slide_width,height=prs.slide_height)
            box=slide.shapes.add_textbox(Inches(1),Inches(1),Inches(2),Inches(1));box.text='Native text'
            source=root/'source.pptx';target=root/'derivative.pptx';prs.save(source);before=source.read_bytes()
            textless_snapshot(source,target)
            self.assertEqual(source.read_bytes(),before)
            derivative=Presentation(target)
            self.assertFalse(any(s.text for s in derivative.slides[0].shapes if s.has_text_frame))
            with zipfile.ZipFile(source) as original,zipfile.ZipFile(target) as blank:
                for name in original.namelist():
                    if name.startswith('ppt/media/'):self.assertEqual(original.read(name),blank.read(name))

if __name__=='__main__':unittest.main()
