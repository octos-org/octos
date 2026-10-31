"""Independent synthetic deck checks, never production page coordinates."""
import json
from pathlib import Path
import sys
import unittest
import tempfile
import subprocess
sys.path.insert(0, str(Path(__file__).parents[1]))
import continuation_evidence as e
import native_deck
from test_native_inspect import InspectionFixture


class ContinuationStatusTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.owner = 'session:synthetic-maker'

    def configure(self):
        e.init(self.root, 'synthetic-task', self.owner, ['exports/editable.pptx'])

    def observe_fixture(self):
        # A fixture of the Rust observer's persisted wire format, not a live host.
        evidence = self.root/'.octos/iteration-evidence.json'
        native_deck.atomic_json(evidence, {'schema_version': 1, 'task_id': 'synthetic-task',
                                         'attempt': 1, 'phase': 'checked'})
        state = self.root/'.octos/continuation-state.json'
        native_deck.atomic_json(state, {'policy_digest': e.sha(self.root/'.octos/continuation.json'),
                                       'snapshot_digest': e.sha(evidence), 'attempt': 1,
                                       'flat_attempts': 0, 'warned': False})
        return evidence, state

    def test_missing_policy_and_wrong_owner_do_not_look_enabled(self):
        self.assertFalse(e.status(self.root, self.owner)['configured'])
        self.configure()
        result = e.status(self.root, 'session:coordinator-not-maker')
        self.assertFalse(result['configured'])
        self.assertFalse(result['runtime_consumed_current_evidence'])
        self.assertIn('owner', result['errors'][0])

    def test_installed_policy_is_not_consumed_evidence(self):
        self.configure()
        result = e.status(self.root, self.owner)
        self.assertTrue(result['configured'])
        self.assertFalse(result['runtime_consumed_current_evidence'])

    def test_consumption_matches_current_policy_snapshot_and_attempt(self):
        self.configure(); evidence, state = self.observe_fixture()
        result = e.status(self.root, self.owner)
        self.assertTrue(result['runtime_consumed_current_evidence'])
        self.assertFalse(result['quality_approved'])
        self.assertFalse(result['process_liveness_verified'])
        old = evidence.read_bytes()
        value = e.read_json(evidence); value['attempt'] = 2
        native_deck.atomic_json(evidence, value)
        self.assertFalse(e.status(self.root, self.owner)['runtime_consumed_current_evidence'])
        evidence.write_bytes(old)
        value = e.read_json(self.root/'.octos/continuation.json'); value['stagnant_attempts'] = 4
        native_deck.atomic_json(self.root/'.octos/continuation.json', value)
        self.assertFalse(e.status(self.root, self.owner)['runtime_consumed_current_evidence'])

    def test_wrong_task_or_malformed_state_is_not_accepted(self):
        self.configure(); evidence, state = self.observe_fixture()
        value = e.read_json(evidence); value['task_id'] = 'another-task'
        native_deck.atomic_json(evidence, value)
        self.assertTrue(e.status(self.root, self.owner)['errors'])
        self.observe_fixture(); native_deck.atomic_json(state, [])
        self.assertTrue(e.status(self.root, self.owner)['errors'])

    def test_cli_requires_consumption_only_when_requested(self):
        self.configure()
        cmd = [sys.executable, str(Path(e.__file__)), 'status', '--workspace', str(self.root),
               '--owner-agent', self.owner]
        plain = subprocess.run(cmd, capture_output=True, text=True)
        strict = subprocess.run(cmd + ['--require-consumed'], capture_output=True, text=True)
        self.assertEqual(plain.returncode, 0)
        self.assertEqual(strict.returncode, 2)
        self.observe_fixture()
        observed = subprocess.run(cmd + ['--require-consumed'], capture_output=True, text=True)
        self.assertEqual(observed.returncode, 0)


class ContinuationEvidenceTests(InspectionFixture):
    def review(self, cycle, findings=None):
        path = self.root/'review.json'
        value = {'cycle_sha256': e.sha(cycle), 'inspected': {k: ['sample'] for k in e.DOMAINS},
                 'findings': findings or []}
        native_deck.atomic_json(path, value)
        return path

    def test_preflight_success_is_not_visual_success(self):
        cycle=self.cycle(); e.init(self.root, 'fixture', 'fixture-agent', ['exports/editable.pptx'])
        result=e.record(self.root, cycle)
        checks={c['id']:c['status'] for c in result['checks']}
        self.assertEqual(checks['package'],'pass')
        self.assertEqual(checks['readability'],'unknown')
        self.assertEqual(checks['copy'],'unknown')
        self.assertFalse(result['visual_quality_verified_by_tool'])

    def test_identical_check_does_not_consume_new_round(self):
        cycle=self.cycle(); e.init(self.root,'fixture','fixture-agent', ['exports/editable.pptx'])
        review=self.review(cycle)
        first=e.record(self.root,cycle,review); again=e.record(self.root,cycle,review)
        self.assertEqual(first['attempt'],again['attempt'])
        self.assertTrue(all(c['status']=='pass' for c in first['checks']))
        self.assertIn('producer self-review',first['review_provenance'])

    def test_omitting_a_known_defect_does_not_close_it(self):
        cycle=self.cycle();e.init(self.root,'fixture','fixture-agent', ['exports/editable.pptx'])
        findings=[{'id':'local-label','check':'readability','state':'open','note':'wrong owner',
                   'evidence':str(cycle.parent/'render-editable/page-1.png')}]
        first=e.record(self.root,cycle,self.review(cycle,findings))
        again=e.record(self.root,cycle,self.review(cycle))
        self.assertEqual(next(c for c in again['checks'] if c['id']=='readability')['status'],'fail')
        self.assertGreater(again['attempt'],first['attempt'])
        findings[0]['state']='resolved';findings[0]['note']='checked correction in actual render'
        repaired=e.record(self.root,cycle,self.review(cycle,findings))
        self.assertEqual(next(c for c in repaired['checks'] if c['id']=='readability')['status'],'pass')

    def test_stale_review_and_changed_render_are_rejected(self):
        cycle=self.cycle();e.init(self.root,'fixture','fixture-agent', ['exports/editable.pptx'])
        review=self.review(cycle);value=e.read_json(review);value['cycle_sha256']='0'*64
        native_deck.atomic_json(review,value)
        with self.assertRaisesRegex(ValueError,'another candidate'):e.record(self.root,cycle,review)
        review=self.review(cycle)
        image=cycle.parent/'render-editable/page-1.png';image.write_bytes(image.read_bytes()+b'changed')
        with self.assertRaisesRegex(ValueError,'stale hash'):e.record(self.root,cycle,review)

    def test_scope_cannot_be_silently_shrunk(self):
        cycle=self.cycle();e.init(self.root,'fixture','fixture-agent', ['exports/editable.pptx']);e.record(self.root,cycle)
        history=self.root/'.octos/pptx-evidence-history.json'
        value=e.read_json(history);value['pages']=['sample','another-required-page'];native_deck.atomic_json(history,value)
        with self.assertRaisesRegex(ValueError,'inventory changed'):e.record(self.root,cycle)
        with self.assertRaisesRegex(ValueError,'Preserve'):e.init(self.root,'replacement','fixture-agent', ['exports/editable.pptx'])

    def test_source_json_is_not_visual_evidence(self):
        cycle=self.cycle();e.init(self.root,'fixture','fixture-agent',['exports/editable.pptx'])
        findings=[{'id':'x','check':'readability','state':'resolved','note':'claimed good',
                   'evidence':str(cycle)}]
        with self.assertRaisesRegex(ValueError,'rendered image'):
            e.record(self.root,cycle,self.review(cycle,findings))

    def test_unknowns_stay_unknown_and_finding_closure_needs_evidence(self):
        checks,_=e.review_checks(['one','two'],{'inspected':{'readability':['one']}},{})
        self.assertEqual(next(c for c in checks if c['id']=='readability'),
                         {'id':'readability','status':'unknown','remaining':1})
        with self.assertRaisesRegex(ValueError,'evidence'):
            e.review_checks(['one'],{'findings':[{'id':'x','check':'copy','state':'resolved','note':'done'}]}, {})


if __name__ == '__main__':unittest.main()
