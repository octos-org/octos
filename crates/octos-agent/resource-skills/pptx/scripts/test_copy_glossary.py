import unittest
from copy_glossary import findings
class GlossaryTests(unittest.TestCase):
    def test_supplied_terms_apply_without_page_coordinates(self):
        terms={'ABC':{'expansions':['Alpha Beta Count']}}
        self.assertEqual(findings([{'id':'x','text':'ABC（Alpha Beta Count）'}],terms),[])
        result=findings([{'id':'other','text':'ABC (Apple Beta Count)'}],terms)
        self.assertEqual(result[0]['object'],'other')
        self.assertEqual(result[0]['detail']['approved_expansions'],['Alpha Beta Count'])
    def test_no_glossary_or_no_expansion_adds_no_policy(self):
        self.assertEqual(findings([{'id':'x','text':'ABC (anything)'}],{}),[])
        self.assertEqual(findings([{'id':'x','text':'ABC P95 < 5 ms'}],{'ABC':{'expansions':['Alpha Beta Count']}}),[])
    def test_invalid_contract_fails_closed(self):
        with self.assertRaises(ValueError):findings([],{'ABC':{'expansions':[]}})
if __name__=='__main__':unittest.main()
