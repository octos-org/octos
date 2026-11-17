"""Event handoff tests; synthetic bytes are not a deck-quality evaluation."""
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1]/'crates/octos-agent/resource-skills/pptx/scripts/peer_handoff.py'
spec=importlib.util.spec_from_file_location('pptx_peer_handoff',SOURCE)
module=importlib.util.module_from_spec(spec);sys.modules[spec.name]=module;spec.loader.exec_module(module)

class HandoffTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.peers = {'maker-a':{'session_id':'maker-a'},'reviewer':{'session_id':'reviewer'}}
        self.assignments = {'reviewers':{'reviewer':{'session_id':'reviewer','pages':['a']}}}
        self.put('source.json',{'id':'a'})
        self.put('artifact.pptx',{'synthetic':True})
        self.put('render.png',{'synthetic':True})
        self.put('cycle.json',{'pages':['a']})
        self.put('render.json',{'synthetic':True})
        self.put('background.png',{'synthetic':True})
        self.put('.octos/pptx-page-plan.json',{'schema_version':1,'coordinator_session':'root','min_score':95,
            'pages':[{'id':'a','maker_session':'maker-a','candidate':'candidate.json','review':'review.json'}]})
        self.put('candidate.json',{'page_id':'a','maker_session':'maker-a',
            'source':self.bind('source.json'),'artifact':self.bind('artifact.pptx'),
            'render':self.bind('render.png'),'build_receipt':self.bind('cycle.json'),
            'render_receipt':self.bind('render.json'),'dependencies':[self.bind('background.png')],
            'receipts':[self.bind('cycle.json')]})

    def put(self,name,value):
        p=self.root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(json.dumps(value))

    def bind(self,name):
        return {'path':name,'sha256':hashlib.sha256((self.root/name).read_bytes()).hexdigest()}

    def dispatch(self,seen=None,actor='maker-a'):
        return module.prepare_review_handoff(self.root,actor,self.assignments,self.peers,seen or set())

    def test_queues_current_candidate_once_without_awarding_a_score(self):
        job=self.dispatch()
        self.assertEqual(job['page_id'],'a');self.assertEqual(job['reviewer_slug'],'reviewer')
        self.assertEqual(job['candidate'],self.bind('candidate.json'))
        self.assertNotIn('score',job);self.assertNotIn('accepted',job)
        self.assertIsNone(self.dispatch({job['id']}))

    def test_changed_dependency_does_not_dispatch_a_stale_candidate(self):
        self.put('background.png',{'changed':True})
        self.assertIsNone(self.dispatch())

    def test_native_absolute_bindings_inside_workspace_dispatch(self):
        candidate=json.loads((self.root/'candidate.json').read_text())
        candidate['dependencies'][0]['path']=str(self.root/'background.png')
        self.put('candidate.json',candidate)
        self.assertIsNotNone(self.dispatch())

    def test_absolute_bindings_cannot_escape_or_follow_symlinks(self):
        candidate=json.loads((self.root/'candidate.json').read_text())
        candidate['dependencies'][0]['path']=str(self.root.parent/'outside.png')
        self.put('candidate.json',candidate)
        self.assertIsNone(self.dispatch())
        candidate['dependencies'][0]['path']=str(self.root/'linked.png')
        (self.root/'linked.png').symlink_to('background.png')
        self.put('candidate.json',candidate)
        self.assertIsNone(self.dispatch())

    def test_wrong_actor_or_non_independent_reviewer_cannot_dispatch(self):
        self.assertIsNone(self.dispatch(actor='different-maker'))
        self.assignments['reviewers']['reviewer']['session_id']='maker-a'
        self.assertIsNone(self.dispatch())

    def test_workspace_escape_and_symlink_are_rejected(self):
        p=self.root/'candidate.json';v=json.loads(p.read_text());v['artifact']['path']='../outside'
        self.put('candidate.json',v);self.assertIsNone(self.dispatch())
        v['artifact']=self.bind('source.json');self.put('candidate.json',v)
        (self.root/'source.json').unlink();(self.root/'source.json').symlink_to('background.png')
        self.assertIsNone(self.dispatch())

    def test_idle_incomplete_is_distinct_from_running_and_verified_complete(self):
        self.assertEqual(module.execution_state(False, ['idle'], False, False, 'in_progress', 'complete_turn'),'idle_incomplete')
        self.assertEqual(module.execution_state(False, ['idle'], False, False, 'complete', 'complete_turn'),'idle_incomplete')
        self.assertEqual(module.execution_state(False, ['running'], False, False, 'in_progress', None),'running')
        self.assertEqual(module.execution_state(False, ['idle'], True, False, 'complete', None),'complete')
        self.assertEqual(module.execution_state(False, ['idle'], False, False, 'in_progress', 'replan'),'blocked')
        self.assertEqual(module.execution_state(False, ['idle'], False, False, 'in_progress', 'errored'),'blocked')

if __name__=='__main__':
    unittest.main()
