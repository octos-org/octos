"""Bound detail output without hiding which parts of a deliverable failed."""
def summarize(result, limit=12):
    findings=result.get('findings', [])
    groups={}
    for finding in findings:
        page=str(finding.get('page', 'artifact'))
        group=groups.setdefault(page, {'page':page, 'count':0, 'checks':[]})
        group['count']+=1
        check=finding.get('check', 'unspecified')
        if check not in group['checks']:group['checks'].append(check)
    brief={key:result.get(key) for key in ['status','summary','artifact','sha256','checker_revision','report','reused','error_scope','retryable','instruction']}
    brief.update(finding_count=len(findings), affected_pages=list(groups.values()),
                 findings_truncated=len(findings)>limit, omitted_findings=max(0,len(findings)-limit),
                 checker_errors=result.get('checker_errors',[]))
    if len(findings)>limit:
        brief['required_action']='Read the full machine report before assigning repairs; the sample below omits findings. The affected_pages list covers every page with findings.'
    brief['findings']=findings[:limit]
    return brief
