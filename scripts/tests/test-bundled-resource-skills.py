#!/usr/bin/env python3
"""Release/package checks requiring only Python's standard library."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
RESOURCES = ROOT / 'crates/octos-agent/resource-skills'


class BundledResources(unittest.TestCase):
    def test_resource_entrypoints_and_local_references_exist(self):
        for package in ('pptx', 'parallel-work'):
            self.assertTrue((RESOURCES / package / 'SKILL.md').is_file())
        for doc in RESOURCES.rglob('*.md'):
            for target in re.findall(r'\[[^\]]*\]\(([^)]+)\)', doc.read_text()):
                if target.startswith(('https:', 'http:', '#')):
                    continue
                path = (doc.parent / target.split('#', 1)[0]).resolve()
                self.assertTrue(path.is_relative_to(RESOURCES.resolve()), (doc, target))
                self.assertTrue(path.exists(), (doc, target))

    def test_no_machine_state_or_host_paths_in_resources(self):
        for path in RESOURCES.rglob('*'):
            if '__pycache__' in path.parts or path.suffix == '.pyc':
                continue
            self.assertFalse(path.is_symlink(), path)
            if not path.is_file():
                continue
            self.assertIn(path.suffix, {'.md', '.py', '.txt', '.yaml'}, path)
            self.assertNotRegex(path.read_text(), r'/Users/[^\s/]+/|/home/(?:cloud|ychen)/|sk-proj-[A-Za-z0-9_-]{20,}')

    def test_setup_check_is_read_only_and_does_not_expose_keys(self):
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp) / 'fresh-workspace'
            env = dict(os.environ, OPENAI_API_KEY='fixture-secret-never-print')
            result = subprocess.run([sys.executable, str(RESOURCES/'pptx/scripts/setup_runtime.py'),
                                     '--workspace', str(workspace), '--check'],
                                    env=env, capture_output=True, text=True, timeout=35)
            self.assertEqual(result.returncode, 1, result.stderr)
            report = json.loads(result.stdout)
            self.assertFalse(report['python_ready'])
            self.assertFalse(report['image_api_tested'])
            self.assertTrue(report['image_api_configured'])
            self.assertNotIn(env['OPENAI_API_KEY'], result.stdout + result.stderr)
            self.assertFalse(workspace.exists())

    def test_declared_binaries_have_release_builds_and_packaging(self):
        declared = (ROOT/'crates/octos-agent/src/bundled_app_skills.rs').read_text()
        binaries = re.findall(r'\(\s*"[^\"]+",\s*"([^\"]+)",\s*include_str!', declared)
        bundle = (ROOT/'scripts/bundle-release.sh').read_text().split('BINARIES=(', 1)[1].split(')', 1)[0].split()
        self.assertIn('skill-evolve', binaries)
        self.assertTrue(set(binaries).issubset(bundle), set(binaries)-set(bundle))
        crates = {}
        for area in ('app-skills', 'platform-skills'):
            for path in (ROOT/'crates'/area).glob('*/Cargo.toml'):
                names = re.findall(r'^name\s*=\s*"([^\"]+)"', path.read_text(), re.M)
                for binary in names[1:]:
                    crates[binary] = names[0]
        for workflow in ('ci.yml', 'release.yml'):
            text = (ROOT/'.github/workflows'/workflow).read_text()
            line = re.search(r'^\s*SKILL_CRATES:\s*"([^\"]+)"', text, re.M).group(1)
            built = re.findall(r'-p\s+(\S+)', line)
            for binary in binaries:
                self.assertIn(crates[binary], built, (workflow, binary))
        self.assertIn('./scripts/bundle-release.sh "scripts/${TARBALL}"',
                      (ROOT/'scripts/build-local-bundle.sh').read_text())


if __name__ == '__main__':
    unittest.main()
