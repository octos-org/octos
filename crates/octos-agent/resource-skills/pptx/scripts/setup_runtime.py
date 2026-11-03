#!/usr/bin/env python3
"""Inspect or install the workspace PPTX Python environment; no API calls."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import venv

SKILL = Path(__file__).resolve().parents[1]
REQUIREMENTS = SKILL / 'requirements.txt'


def renderer():
    found = shutil.which('soffice') or shutil.which('libreoffice')
    if found:
        return found
    candidates = [Path('/Applications/LibreOffice.app/Contents/MacOS/soffice')]
    for var in ('PROGRAMFILES', 'PROGRAMFILES(X86)'):
        if os.environ.get(var):
            candidates.append(Path(os.environ[var]) / 'LibreOffice/program/soffice.exe')
    return next((str(path) for path in candidates if path.is_file()), None)


def python_path(workspace):
    return workspace / '.venv' / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')


def check(workspace):
    python = python_path(workspace)
    expected = dict(line.split('==', 1) for line in REQUIREMENTS.read_text().splitlines()
                    if line and not line.startswith('#'))
    versions = {}
    if python.is_file():
        probe = '''import importlib.metadata as m, json, sys
out = {}
for package in sys.argv[1:]:
    try: out[package] = m.version(package)
    except m.PackageNotFoundError: out[package] = None
print(json.dumps(out))
'''
        result = subprocess.run([str(python), '-c', probe, *expected],
                                capture_output=True, text=True, timeout=30)
        if result.returncode == 0:
            versions = json.loads(result.stdout)
    missing = {name: {'expected': version, 'installed': versions.get(name)}
               for name, version in expected.items() if versions.get(name) != version}
    soffice = renderer()
    return {'workspace': str(workspace), 'python': str(python),
            'python_ready': python.is_file() and not missing,
            'dependencies': versions, 'missing_or_mismatched': missing,
            'libreoffice': soffice, 'render_ready': python.is_file() and not missing and bool(soffice),
            'image_api_configured': bool(os.environ.get('OPENAI_API_KEY')),
            'image_api_tested': False,
            'notes': ['Install LibreOffice separately if missing; install the fonts selected for the deck.',
                      'Image generation needs a configured service or an available authenticated image tool.',
                      'No image request, model request, or visual review is performed by this check.']}


def install(workspace):
    if sys.version_info < (3, 10):
        raise RuntimeError('Python 3.10 or newer is required to set up PPTX tools')
    workspace.mkdir(parents=True, exist_ok=True)
    python = python_path(workspace)
    if not python.is_file():
        # Match `python -m venv`: relocatable/uv Python builds on POSIX need
        # symlinks so the interpreter can still locate its standard library.
        venv.EnvBuilder(with_pip=True, symlinks=os.name != 'nt').create(workspace / '.venv')
    if not check(workspace)['python_ready']:
        subprocess.run([str(python), '-m', 'pip', 'install', '-r', str(REQUIREMENTS)],
                       check=True, stdout=sys.stderr)
    stamp = workspace / '.venv/.pptx-requirements-sha256'
    stamp.write_text(hashlib.sha256(REQUIREMENTS.read_bytes()).hexdigest())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path, required=True)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument('--check', action='store_true', help='Read-only prerequisite report')
    action.add_argument('--install', action='store_true', help='Install pinned Python dependencies in .venv')
    args = parser.parse_args()
    workspace = args.workspace.expanduser().resolve()
    if args.install:
        install(workspace)
    report = check(workspace)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report['render_ready'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
