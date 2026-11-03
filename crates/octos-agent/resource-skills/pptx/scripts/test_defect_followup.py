import json,tempfile,unittest
from pathlib import Path
import defect_followup as d

class DefectTests(unittest.TestCase):
    def proposals(self):
        return [{'id':'arbitrary-1','object':'shape-x','crop':[.1,.1,.3,.3],
                 'finding':{'detail':'Text crosses card border'},'receipt':'protected/receipt.json'}]
    def test_unknown_and_open_are_not_resolved(self):
        for status in ['unknown','open','resolved']:
            result=d.parse(json.dumps({'defects':[{'id':'arbitrary-1','status':status,'evidence':'Observed current pixels'}]}),self.proposals())
            self.assertEqual(result['status']=='pass',status=='resolved')
    def test_missing_coverage_cannot_pass(self):
        with self.assertRaises(ValueError):d.parse('{"defects":[]}',self.proposals())
    def test_crop_includes_original_and_moved_object(self):
        b=d.crop_box([.2,.2,.3,.3],[.4,.4,.5,.5]);self.assertLess(b[0],.2);self.assertGreater(b[2],.5)
    def test_later_general_pass_does_not_close_known_defect(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);a=root/'old';b=root/'new';(a/'render').mkdir(parents=True);b.mkdir()
            (a/'render/page.png').write_bytes(b'fixture')
            (a/'render/render-report.json').write_text(json.dumps({'page_images':[{'path':'page.png'}]}))
            (a/'receipt.json').write_text(json.dumps({'scope':['topic-z'],'findings':[{'page':'topic-z','object':'box','bbox':[.1,.1,.2,.2],'check':'overflow','detail':'Visible overflow'}]}))
            (b/'receipt.json').write_text(json.dumps({'scope':['topic-z'],'status':'pass','findings':[]}))
            p=d.collect(root,'topic-z',[{'id':'box','bbox':[.1,.1,.2,.2]}]);self.assertEqual(len(p),1)
            self.assertEqual(d.collect(root,'other-topic',[]),[])

    def test_native_inventory_supplies_region_when_finding_has_only_object_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'render').mkdir()
            (root/'render/page.png').write_bytes(b'fixture')
            (root/'render/render-report.json').write_text(json.dumps({'page_images':[{'path':'page.png'}]}))
            (root/'native-inventory.json').write_text(json.dumps({'pages':[[{'id':'native-z','bbox':[.2,.3,.4,.5]}]]}))
            (root/'receipt.json').write_text(json.dumps({'scope':['arbitrary-topic'],'findings':[{'page':'arbitrary-topic','object':'native-z','detail':'Label remains clipped'}]}))
            result=d.collect(root,'arbitrary-topic',[])
            self.assertEqual(result[0]['finding']['bbox'],[.2,.3,.4,.5])

if __name__=='__main__':unittest.main()
