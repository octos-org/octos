#!/usr/bin/env python3
"""Offline OUP release smoke. Optional --render installs dependencies and runs LO.

Usage: python3.12 scripts/tests/test-bundled-pptx-runtime.py --binary <octos> --render
Evidence directory can be selected with OUP_TEST_OUTPUT_DIR. No image API is used.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
import zipfile
import xml.etree.ElementTree as ET

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--render', action='store_true')
args = parser.parse_args()
sys.dont_write_bytecode = True
os.environ['OCTOS_BIN'] = str(args.binary.resolve())
spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('test-oup-runtime.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Smoke(fixture.RuntimeContract):
    def test_packaged_pptx(self):
        read = fixture.reply('Read packaged instructions.', finish='tool_calls', tool=True)
        with self.fixture('bundled-pptx', [read, fixture.reply('Ready.')]) as (directory, provider, env, launch):
            expected = directory/'data/profiles/oup-ci/data/bundled-resource-skills/pptx/SKILL.md'
            read['message']['tool_calls'][0]['function']['arguments'] = json.dumps({'path':str(expected)})
            session = 'oup-ci:local:bundled-pptx'
            client = self.oup_connect(directory, env, launch, 'release')
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, session)
                outcome = self.oup_turn(client, session, 'Read the packaged PPTX skill and say ready.')
                self.assertEqual(outcome['payload']['data']['outcome'], 'completed')
                systems = '\n'.join(str(m.get('content', '')) for m in provider.requests[0]['messages']
                                    if m['role'] in ('system', 'developer'))
                self.assertIn('pptx/SKILL.md', systems)
                self.assertIn('parallel-work/SKILL.md', systems)
                entries = list((directory/'data').rglob('bundled-resource-skills/pptx/SKILL.md'))
                self.assertTrue(entries, 'the binary must extract real resources without the source checkout')
                skill = entries[0].parent
                self.assertIn(str(skill/'SKILL.md'), systems)
                self.assertTrue((skill/'scripts/setup_runtime.py').is_file())
                self.assertEqual(len(provider.requests), 2)
                tool_rows = [m for m in provider.requests[1]['messages'] if m['role'] == 'tool']
                self.assertTrue(any('新环境首次制作' in str(m['content']) for m in tool_rows),
                                'the actual read tool must return the packaged instructions to the model')
            finally:
                client.close()
            report = {'model_received_skill_catalog': True, 'model_read_packaged_skill': True,
                      'extracted_skill': str(skill), 'render_tested': False}
            if args.render:
                workspace = directory/'workspace'
                self.render_fixture(skill, workspace, env, report)
            (directory/'smoke.json').write_text(json.dumps(report, indent=2)+'\n')
            print(json.dumps(report, indent=2))

    def render_fixture(self, skill, workspace, env, report):
        def run(command, name, timeout=240):
            result = subprocess.run(command, cwd=workspace, env=env, text=True, capture_output=True, timeout=timeout)
            (workspace/(name+'.log')).write_text(result.stdout+result.stderr)
            self.assertEqual(result.returncode, 0, f'{name}: {result.stderr[-2500:]} {result.stdout[-1500:]}')
            return result.stdout
        setup = json.loads(run([sys.executable, str(skill/'scripts/setup_runtime.py'),
                                '--workspace', str(workspace), '--install'], 'setup', 300))
        python = setup['python']
        self.assertTrue(setup['render_ready'])
        run([python, '-c', "from PIL import Image; Image.new('RGB',(1600,900),'white').save('blank.png')"], 'fixture-image')
        doc = {'schema_version':1, 'canvas':{'width_px':1600,'height_px':900,'width_in':12,'height_in':6.75},
               'styles':{'heading':{'font':'Arial','size_pt':30,'color':'000000'}},
               'pages':[{'id':'fixture','background':'blank.png','design':'blank.png',
                         'copy':{'title':'Portable PPTX fixture'},
                         'texts':[{'id':'title','text':'Portable PPTX fixture','style':'heading','box':[100,100,1400,140]}]}]}
        source = workspace/'deck.json'
        source.write_text(json.dumps(doc))
        run([python,str(skill/'scripts/native_deck.py'),'build','--spec',str(source),'--out','candidate'], 'build')
        pptx = workspace/'candidate/editable.pptx'
        run([python,str(skill/'scripts/check_pptx_compat.py'),str(pptx)], 'structure')
        with zipfile.ZipFile(pptx) as archive:
            slide = ET.fromstring(archive.read('ppt/slides/slide1.xml'))
        self.assertIn('Portable PPTX fixture', [n.text for n in slide.iter('{http://schemas.openxmlformats.org/drawingml/2006/main}t')])
        run([python,str(skill/'scripts/render_deck.py'),str(pptx),'--out','render',
             '--soffice',setup['libreoffice'],'--size','1600x900'], 'render')
        check = json.loads(run([python,'-c',
            "import pymupdf,json; p=pymupdf.open('render/editable.pdf'); print(json.dumps({'pages':len(p),'text':p[0].get_text()}))"], 'pdf-text'))
        self.assertEqual(check['pages'], 1)
        self.assertIn('Portable PPTX fixture', check['text'])
        report.update(render_tested=True, native_text_verified=True, pptx=str(pptx),
                      renderer=setup['libreoffice'], image_api_called=False)


result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite([Smoke('test_packaged_pptx')]))
raise SystemExit(0 if result.wasSuccessful() else 1)
