"""General geometry behavior; no presentation-specific copy, coordinates or answers."""
import copy
import json
from pathlib import Path
import random
import sys
import unittest

sys.path.insert(0,str(Path(__file__).parents[1]))
import label_geometry as g
import native_deck
import native_inspect
from pptx import Presentation
from test_native_deck import Fixture
from test_native_inspect import InspectionFixture


class GeometryTests(unittest.TestCase):
    def test_native_clockwise_center_rotation_and_proposal(self):
        for angle in [-71,-12,0,19,89,135]:
            q=g.frame_quad([200,250,240,80],angle)
            measured=g.plane(q)
            self.assertAlmostEqual(g.angle_delta(measured['rotation'],angle),0,places=8)
            self.assertAlmostEqual(measured['rigid_fit_error_px'],0,places=8)
            p=g.propose(q,6)
            self.assertTrue(g.contains(q,g.frame_quad(p['box'],p['rotation']),-5.999))
            self.assertIsNone(p['limitation'])

    def test_taper_is_not_claimed_to_be_reconstructed_by_rotation(self):
        p=g.propose([[100,100],[400,100],[350,220],[150,220]],5)
        self.assertEqual(p['plane']['kind'],'perspective_or_shear')
        self.assertIsNotNone(p['limitation'])
        self.assertTrue(g.contains([[100,100],[400,100],[350,220],[150,220]],g.frame_quad(p['box'],p['rotation'])))

    def test_quad_and_numeric_validation(self):
        for quad in [[[0,0],[10,10],[0,10],[10,0]],[[0,0],[0,0],[1,1],[0,1]],[[0,0],[10,0],[5,2],[0,10]]]:
            with self.assertRaises(ValueError):g.valid_quad(quad)
        for value in [True,float('nan'),float('inf'),360]:
            with self.assertRaises(ValueError):g.rotation(value)

    def test_pixels_in_bounding_box_but_outside_slanted_plane_are_found(self):
        quad=g.frame_quad([30,40,40,10],30)
        component={'runs':[(30,28,72)]}
        self.assertGreater(g.outside_ink([component],quad,(100,100),{'width_px':100,'height_px':100}),0)
        rng=random.Random(7)
        for _ in range(40):
            angle=rng.uniform(-70,70)
            q=g.frame_quad([20,30,60,40],angle)
            runs=[(rng.randrange(100),rng.randrange(25),rng.randrange(75,100)) for _ in range(6)]
            brute=sum(not g.contains(q,[[(x+.5)*2,(y+.5)*2]],1.2) for y,a,b in runs for x in range(a,b))
            self.assertEqual(brute,g.outside_ink([{'runs':runs}],q,(100,100),{'width_px':200,'height_px':200},1.2))


class BuildRotationTests(Fixture):
    def test_rotation_survives_build_patch_and_native_copy(self):
        self.doc['pages'][0]['texts'][1].update(rotation=-14,box=[100,260,240,110])
        self.save()
        out=self.root/'rotated';native_deck.build(self.spec,out)
        shape=Presentation(out/'editable.pptx').slides[0].shapes[2]
        self.assertAlmostEqual(shape.rotation,346)
        self.assertEqual(shape.text,'Repeat\n7')
        edit=self.root/'patch.json';native_deck.atomic_json(edit,[{'page':'alpha','id':'first','set':{'rotation':17}}])
        new=self.root/'patched.json';native_deck.patch_spec(self.spec,edit,new)
        native_deck.build(new,self.root/'patched')
        self.assertAlmostEqual(Presentation(self.root/'patched/editable.pptx').slides[0].shapes[2].rotation,17)

    def test_quad_owner_and_rotated_canvas_bounds_are_enforced(self):
        pg=self.doc['pages'][0];pg['regions']['left']={'quad':g.frame_quad([40,200,400,350],-13),
                                                       'scope':'label','basis':'Measured label outline in the supplied artwork'}
        pg['texts'][1].update(rotation=-13,box=[100,280,200,120]);self.save()
        native_deck.load_spec(self.spec)
        pg['texts'][0].update(box=[0,0,900,80],rotation=-25);self.save()
        with self.assertRaisesRegex(ValueError,'outside canvas'):native_deck.load_spec(self.spec)


class GeometryInspectionTests(InspectionFixture):
    def test_horizontal_text_on_slanted_label_is_flagged_and_quad_overlay_exists(self):
        cycle=self.cycle()
        config={'schema_version':1,'pages':[{'id':'sample','basis':'Measured independently on artwork',
            'regions':{'tag':{'quad':g.frame_quad([65,220,370,140],12),'scope':'label','basis':'Four corners of the local sign'}},
            'objects':{'west':{'region':'tag'}}}]}
        regions=self.root/'regions.json';native_deck.atomic_json(regions,config)
        report=native_inspect.run(cycle,self.root/'inspect',regions=regions,objects=['west'])
        row=report['objects'][0]
        self.assertIn('LABEL_BASELINE_MISMATCH',row['findings'])
        self.assertAlmostEqual(row['baseline_error_deg'],-12)
        self.assertEqual(len(row['owner']['quad_px']),4)
        self.assertTrue(any(a['kind']=='details' for a in report['artifacts']))

    def test_panel_is_not_reported_as_verified_label(self):
        self.doc['pages'][0]['regions']={'panel':[40,180,920,440]}
        self.doc['pages'][0]['texts'][1]['region']='panel'
        report=native_inspect.run(self.cycle(),self.root/'inspect',objects=['west'])
        self.assertIn('LABEL_OWNER_UNVERIFIED',report['objects'][0]['findings'])

    def test_rendered_wrong_angle_is_detected_even_with_correct_pptx_metadata(self):
        self.doc['pages'][0]['texts'][1]['rotation']=12
        # The fixture deliberately creates horizontal PDF text independently.
        report=native_inspect.run(self.cycle(),self.root/'inspect',objects=['west'])
        self.assertIn('RENDERED_BASELINE_MISMATCH',report['objects'][0]['findings'])


if __name__=='__main__':unittest.main()
