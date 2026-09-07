#!/usr/bin/env python3
"""Regression checks for the documented build-cache entry points."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class BuildCacheDocs(unittest.TestCase):
    def test_solo_pool_is_documented_as_disabled(self):
        doc = (ROOT / "docs/build-cache-pool.md").read_text()
        section = doc.split("### 4.1", 1)[1].split("### 4.2", 1)[0]
        self.assertIn("solo 路径尚未接入构建缓存池", section)
        self.assertNotIn("solo 侧 `run_chat_peer`", section)

    def test_builder_comment_only_claims_implemented_entry_points(self):
        agent = (ROOT / "crates/octos-agent/src/agent/mod.rs").read_text()
        comment = agent.split("pub fn with_build_cache_slot(", 1)[0].rsplit("/// Builder:", 1)[1]
        self.assertNotIn("solo acquires", comment)
        self.assertIn("serve", comment)


if __name__ == "__main__":
    unittest.main()
