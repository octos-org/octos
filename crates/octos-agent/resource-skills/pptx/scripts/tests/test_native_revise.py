"""Revision identity and publication tests; fake render pixels do not prove quality."""
from pathlib import Path
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1]))
from test_native_deck import CycleTests, engine, prepare
import native_cycle
import native_revise
from validate_page_handoff import validate_pair


class RevisionTests(CycleTests):
    def revision_fixture(self):
        self.service.complete = True
        engine.atomic_json(self.root/'contract.json', {'page_order': ['alpha']})
        (self.root/'copy.txt').write_text('Synthetic authoritative text')
        (self.root/'TOOLS.md').write_text('Test tools')
        engine.atomic_json(self.root/'plan.json', {'shared_contract': 'contract.json', 'pages': [
            {'page_id': 'alpha', 'inputs': [{'role': 'canonical-copy', 'path': 'copy.txt'},
                {'role': 'design', 'path': 'art.png'}, {'role': 'blank-art', 'path': 'art.png'}]}]})
        jobs = prepare(self.root/'plan.json', self.root/'jobs', Path(__file__).parents[2]/'SKILL.md', self.root/'TOOLS.md')
        entry = jobs['jobs'][0]; out = Path(entry['result']).parent/'cycle'
        native_cycle.run(self.spec, out, self.root, 'alpha', seconds=0)
        review = self.root/'review.json'
        engine.atomic_json(review, {'inspection': {'source': True, 'editable_render': True, 'blank_render': True},
                                    'open_findings': [], 'note': 'Synthetic lifecycle declaration, not real visual QA.'})
        native_cycle.handoff(entry['job'], out/'cycle.json', review)
        edits = self.root/'edits.json'
        engine.atomic_json(edits, [{'page':'alpha','id':'first','set':{'box':[65,235,300,150]}}])
        return entry, out, edits, review

    def test_revision_preserves_old_handoff_and_assembles_new_receipt(self):
        entry, out, edits, review = self.revision_fixture()
        old_paths = [Path(entry['job']), Path(entry['result']), out/'build/editable.pptx', self.spec]
        hashes = {p: engine.sha(p) for p in old_paths}
        rev = native_revise.prepare(out/'cycle.json', edits, self.root, entry['job'])
        prepared = engine.read_json(rev)
        self.assertNotEqual(prepared['job'], entry['job'])
        result = native_revise.run(rev, seconds=0, inspect=False)
        self.assertEqual(result['state'], 'ready_for_visual_review')
        first_count = len(self.service.jobs)
        native_revise.run(rev, seconds=0, inspect=False)
        self.assertEqual(first_count, len(self.service.jobs))
        receipt = self.root/'jobs/prepared-jobs.json'
        original_receipt = receipt.read_bytes()
        published = native_revise.publish(rev, review, receipt)
        checked = validate_pair(prepared['job'], published['result'])
        self.assertEqual(checked['page_id'], 'alpha')
        assembled = engine.assemble(published['updated_jobs'], self.root/'assembled')
        self.assertEqual(assembled['page_order'], ['alpha'])
        self.assertEqual(receipt.read_bytes(), original_receipt)
        self.assertEqual(hashes, {p: engine.sha(p) for p in old_paths})

    def test_pending_resume_and_no_auto_approval(self):
        entry, out, edits, review = self.revision_fixture()
        rev = native_revise.prepare(out/'cycle.json', edits, self.root, entry['job'])
        self.service.complete = False
        result = native_revise.run(rev, seconds=0, inspect=False)
        self.assertEqual(result['state'], 'pending')
        self.assertFalse(Path(result['result']).exists())
        self.service.complete = True
        self.assertEqual(native_revise.run(rev, seconds=0, inspect=False)['state'], 'ready_for_visual_review')
        value = engine.read_json(review); value['open_findings'] = ['Unreadable label']
        engine.atomic_json(review, value)
        with self.assertRaisesRegex(ValueError, 'incomplete'):
            native_revise.publish(rev, review)
        self.assertFalse(Path(result['result']).exists())

    def test_tampering_and_wrong_page_do_not_produce_a_handoff(self):
        entry, out, edits, review = self.revision_fixture()
        engine.atomic_json(edits, [{'page':'beta','id':'first','set':{'box':[65,235,300,150]}}])
        with self.assertRaisesRegex(ValueError, 'assigned page'):
            native_revise.prepare(out/'cycle.json', edits, self.root, entry['job'])
        engine.atomic_json(edits, [{'page':'alpha','id':'first','set':{'box':[65,235,300,150]}}])
        rev = native_revise.prepare(out/'cycle.json', edits, self.root, entry['job'])
        source = Path(engine.read_json(rev)['source_spec']); source.write_text('{}')
        with self.assertRaisesRegex(ValueError, 'Revision input changed'):
            native_revise.run(rev, seconds=0, inspect=False)
