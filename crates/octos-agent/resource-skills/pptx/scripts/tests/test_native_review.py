"""Useful feedback survives uncertain geometry and compact output limits."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
from native_review import summarize


class ReviewTests(unittest.TestCase):
    def test_uncertainty_is_not_invented_as_measured_overflow(self):
        report = {'objects':[{'page':'diagram','id':'caption','text':'A',
                              'ink_attribution':{'state':'unknown'}}],
                  'findings':[{'page':'diagram','object':'caption','code':'INK_ATTRIBUTION_UNRESOLVED',
                               'severity':'unknown','details':['Overlapping ink']}],
                  'ownership_summary':{'without_declared_owner':[{'page':'diagram','id':'caption'}]},
                  'artifacts':[]}
        summary = summarize(report)
        row = summary['object_findings'][0]
        self.assertEqual(row['ink_attribution']['state'], 'unknown')
        self.assertIsNone(row['frame_clearance_px'])
        self.assertFalse(summary['visual_quality_verified'])
        self.assertEqual(summary['finding_comparison']['state'], 'no_previous_inspection')

    def test_truncation_keeps_complete_page_coverage_and_signals_full_report(self):
        report = {'objects':[], 'findings':[
            {'page':page,'object':'caption','code':'COPY_MISMATCH','severity':'error','details':{}}
            for page in ['one','two','three']], 'artifacts':[],
            'ownership_summary':{'without_declared_owner':[]}}
        summary = summarize(report,limit=1)
        self.assertEqual(summary['affected_objects'],3)
        self.assertEqual(set(summary['affected_pages']),{'one','two','three'})
        self.assertEqual(summary['omitted_objects'],2)
        self.assertTrue(summary['more_findings_in_report'])


if __name__ == '__main__': unittest.main()
