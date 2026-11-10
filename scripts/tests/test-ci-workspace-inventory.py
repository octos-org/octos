#!/usr/bin/env python3
"""Workspace growth must retain every member in CI rather than aborting selection."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("inventory", ROOT / "scripts/ci-workspace-inventory.py")
INVENTORY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INVENTORY)


def metadata(count):
    packages = [
        {"id": f"package-{i}", "name": f"package-{i}", "dependencies": [], "targets": [],
         "manifest_path": str(ROOT / f"crates/package-{i}/Cargo.toml")}
        for i in range(count)
    ]
    return {"workspace_members": [p["id"] for p in packages], "packages": packages,
            "resolve": {"nodes": []}}


class InventoryTests(unittest.TestCase):
    def test_growth_preserves_all_members(self):
        for count in (39, 40, 41):
            with self.subTest(count=count), patch.object(INVENTORY, "run", return_value="head"):
                result = INVENTORY.build(ROOT, metadata(count), "fixture")
                self.assertEqual(result["counts"]["workspace"], count)
                self.assertEqual(len(result["packages"]), count)
                self.assertEqual(len(result["declared_direct_dependents"]), count)

    def test_missing_metadata_for_member_is_rejected(self):
        data = metadata(40)
        data["packages"].pop()
        with self.assertRaisesRegex(SystemExit, "every workspace member"):
            INVENTORY.build(ROOT, data, "fixture")

    def test_empty_workspace_is_rejected(self):
        with self.assertRaisesRegex(SystemExit, "every workspace member"):
            INVENTORY.build(ROOT, metadata(0), "fixture")


if __name__ == "__main__":
    unittest.main()
