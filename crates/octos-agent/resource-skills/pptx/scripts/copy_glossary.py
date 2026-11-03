"""Check explicitly supplied acronym expansions; terms are host contract data."""
import re

def validate(glossary):
    if not isinstance(glossary, dict):raise ValueError('glossary must be an object')
    for term, item in glossary.items():
        if not isinstance(term,str) or not term or not isinstance(item,dict):raise ValueError('Invalid glossary entry')
        values=item.get('expansions')
        if not isinstance(values,list) or not values or not all(isinstance(v,str) and v.strip() for v in values):raise ValueError('Approved glossary expansions required')

def findings(objects, glossary):
    validate(glossary)
    issues=[]
    norm=lambda s:re.sub(r'[\s-]+','',s).casefold()
    for term, item in glossary.items():
        pattern=re.compile(r'(?<!\w)'+re.escape(term)+r'\s*[（(]\s*([A-Za-z][A-Za-z\s-]+)\s*[）)]',re.I)
        allowed={norm(s) for s in item['expansions']}
        for obj in objects:
            for match in pattern.finditer(obj['text']):
                if norm(match.group(1)) not in allowed:
                    issues.append({'object':obj['id'],'check':'glossary-expansion','status':'fail','detail':{'term':term,'actual':match.group(1),'approved_expansions':item['expansions'],'authority':item.get('source')}})
    return issues
