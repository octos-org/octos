import unittest
from review_summary import summarize

class ReviewSummaryTests(unittest.TestCase):
    def test_late_failed_pages_remain_visible_when_details_are_truncated(self):
        result={'status':'fail','findings':[{'page':'cover','check':'overlap'}]*15+[{'page':'appendix','check':'missing'}], 'report':'report.json'}
        summary=summarize(result)
        self.assertEqual(summary['affected_pages'],[{'page':'cover','count':15,'checks':['overlap']},{'page':'appendix','count':1,'checks':['missing']}])
        self.assertEqual(len(summary['findings']),12)
        self.assertTrue(summary['findings_truncated'])
        self.assertEqual(summary['omitted_findings'],4)

    def test_global_and_page_errors_remain_distinct(self):
        summary=summarize({'status':'error','findings':[{'check':'coverage'}],'checker_errors':[{'page':'detail','summary':'backend unavailable'}]})
        self.assertEqual(summary['affected_pages'][0]['page'],'artifact')
        self.assertEqual(summary['checker_errors'][0]['page'],'detail')
        self.assertFalse(summary['findings_truncated'])
