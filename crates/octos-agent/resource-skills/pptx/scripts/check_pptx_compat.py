#!/usr/bin/env python3
"""Read-only package/XML preflight; NOT a full schema or PowerPoint-open test."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import xml.etree.ElementTree as ET
from zipfile import BadZipFile, ZipFile

A = '{http://schemas.openxmlformats.org/drawingml/2006/main}'
AUTOFIT = {A + name for name in ('noAutofit', 'normAutofit', 'spAutoFit')}


def inspect(path):
    path = Path(path)
    issues = []
    counts = Counter()
    try:
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        with ZipFile(path) as archive:
            bad = archive.testzip()
            if bad:
                issues.append({'code': 'zip_crc', 'part': bad})
            for name, count in Counter(archive.namelist()).items():
                if count > 1:
                    issues.append({'code': 'duplicate_part', 'part': name, 'count': count})
            for name in archive.namelist():
                if not name.endswith(('.xml', '.rels')):
                    continue
                try:
                    root = ET.fromstring(archive.read(name))
                except ET.ParseError as error:
                    issues.append({'code': 'xml_parse', 'part': name, 'detail': str(error)})
                    continue
                counts['xml_parts'] += 1
                if re.fullmatch(r'ppt/slides/slide[0-9]+\.xml', name):
                    counts['slides'] += 1
                for index, body in enumerate(root.iter(A + 'bodyPr')):
                    counts['text_bodies'] += 1
                    modes = [child.tag.split('}')[-1] for child in body if child.tag in AUTOFIT]
                    # DrawingML EG_TextAutofit is an optional, mutually exclusive choice.
                    if len(modes) > 1:
                        issues.append({'code': 'autofit_choice', 'part': name,
                                       'body_index': index, 'children': modes})
    except (OSError, BadZipFile, RuntimeError) as error:
        digest = locals().get('digest')
        issues.append({'code': 'package_read', 'detail': str(error)})
    return {'path': str(path), 'sha256': digest, 'preflight_passed': not issues,
            'scope': 'ZIP CRC, duplicate parts, XML parsing, DrawingML autofit exclusivity only; '
                     'not full OOXML schema validation or a Microsoft PowerPoint open test',
            'counts': dict(counts), 'issue_count': len(issues), 'issues': issues}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('pptx')
    parser.add_argument('--out', help='Save full JSON report; never changes the PPTX')
    args = parser.parse_args()
    result = inspect(args.pptx)
    if args.out:
        Path(args.out).write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    summary = {k: v for k, v in result.items() if k != 'issues'}
    summary['first_issues'] = result['issues'][:10]
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    raise SystemExit(0 if result['preflight_passed'] else 1)


if __name__ == '__main__':
    main()
