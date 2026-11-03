#!/usr/bin/env python3
"""Read-only client for a host-owned checker. No model credentials or verdicts."""
import argparse
import json
import sys
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--pptx')
    parser.add_argument('--pages', nargs='+')
    parser.add_argument('--timeout', type=float, default=1800)
    args = parser.parse_args()
    if bool(args.pptx) != bool(args.pages):
        parser.error('Page preflight requires both --pptx and --pages')
    payload = {'mode': 'delivery'} if not args.pptx else {'mode': 'pages', 'pptx': args.pptx, 'pages': args.pages}
    request = urllib.request.Request(args.url.rstrip('/') + '/check', data=json.dumps(payload).encode(), headers={'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=args.timeout) as response:
            result = json.load(response)
        if result.get('status') not in ('pass', 'fail', 'error'):
            raise ValueError('Invalid checker response status')
    except Exception as error:
        print('CHECKER_UNAVAILABLE: ' + str(error), file=sys.stderr)
        return 2
    summary = {k: result.get(k) for k in ('status', 'summary', 'artifact', 'sha256', 'report', 'visual', 'reused', 'error_scope', 'instruction')}
    summary['finding_count'] = len(result.get('findings', []))
    summary['findings'] = result.get('findings', [])[:12]
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    if result['status'] != 'pass':
        print(json.dumps(summary, ensure_ascii=False), file=sys.stderr)
    return {'pass': 0, 'fail': 1, 'error': 2}[result['status']]


if __name__ == '__main__':
    raise SystemExit(main())
