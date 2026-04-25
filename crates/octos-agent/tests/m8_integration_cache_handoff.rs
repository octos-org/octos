//! M8.4 / M8.5 / M8.6 hand-off tests (item 7 of fix-first checklist).
//!
//! The three milestones left explicit TODO seams where state flowed
//! between cache / compaction / resume but never actually connected:
//!
//! - Resume sanitise produced `ReplacementStateRef` entries but
//!   dropped them on the floor.
//! - Tier-3 compaction promised to clear the `FileStateCache` but
//!   never did.
//! - The [FILE_UNCHANGED] short-circuit could survive a tier-3 prune.
//!
//! Item 7 pins the hand-offs with four tests.

use std::path::PathBuf;
use std::sync::Arc;

use octos_agent::{
    ApiMicroCompactionConfig, FileStateCache, MicroCompactionPolicy, TieredCompactionRunner,
    compaction::{CompactionOutcome, CompactionPhase},
    compaction_tiered::FullCompactor,
};
use octos_bus::ReplacementStateRef;
use octos_core::Message;

#[test]
fn resume_sanitize_recovered_refs_seed_file_state_cache() {
    // Recovered refs with a content_hash must seed the cache; refs
    // without a hash must be skipped (a zero-hash entry would turn every
    // subsequent read into a false [FILE_UNCHANGED]).
    let cache = FileStateCache::new();
    let refs = vec![
        ReplacementStateRef {
            path: PathBuf::from("/repo/README.md"),
            content_hash: Some("123456".into()),
        },
        ReplacementStateRef {
            path: PathBuf::from("/repo/pending.rs"),
            content_hash: None, // MUST be skipped
        },
        ReplacementStateRef {
            path: PathBuf::from("/repo/lib.rs"),
            content_hash: Some("789012".into()),
        },
    ];

    let seeded = cache.seed_from_replacement_refs(&refs);
    assert_eq!(
        seeded, 2,
        "only refs with a populated content_hash should seed entries"
    );
    assert_eq!(cache.len(), 2, "cache must hold exactly the seeded entries");
}

#[test]
fn tier3_compaction_clears_file_state_cache() {
    // After a tier-3 compaction fires, the shared cache must be empty.
    // Tier-3 prunes / summarises the old tool-result messages that carry
    // the [FILE_UNCHANGED] identity claims — leaving the cache intact
    // would let a subsequent read_file short-circuit against stale data.
    let cache = Arc::new(FileStateCache::new());
    // Seed a value so we can assert the clear actually removed it.
    cache.seed_from_replacement_refs(&[ReplacementStateRef {
        path: PathBuf::from("/repo/stale.rs"),
        content_hash: Some("1".into()),
    }]);
    assert_eq!(cache.len(), 1, "seed succeeded");

    // Build a tiered runner with a mock tier-3 compactor that always
    // fires. The helper must clear the cache after tier-3 runs.
    struct AlwaysCompact;
    impl FullCompactor for AlwaysCompact {
        fn needs_compaction(&self, _messages: &[Message]) -> Option<u32> {
            Some(0)
        }
        fn compact(
            &self,
            _messages: &mut Vec<Message>,
            _phase: CompactionPhase,
        ) -> CompactionOutcome {
            CompactionOutcome::default()
        }
    }
    let runner = TieredCompactionRunner::new(
        MicroCompactionPolicy::default(),
        ApiMicroCompactionConfig::default(),
        Box::new(AlwaysCompact),
    );

    let mut msgs: Vec<Message> = vec![];
    let report =
        runner.run_tier3_and_invalidate_cache(&mut msgs, CompactionPhase::OnDemand, Some(&cache));
    assert!(report.is_some(), "tier-3 must fire in this fixture");
    assert_eq!(
        cache.len(),
        0,
        "tier-3 compaction boundary must clear the file-state cache"
    );
}

#[tokio::test]
async fn read_file_does_not_return_file_unchanged_across_compaction_boundary() {
    // Populate the cache, run tier-3 clear, then a read_file on the
    // same file must NOT hit [FILE_UNCHANGED] — the clear at the
    // boundary guarantees the stale identity does not survive.
    use octos_agent::tools::{ReadFileTool, Tool, ToolContext};
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let file = dir.path().join("boundary.txt");
    std::fs::write(&file, "alpha\nbeta\n").unwrap();

    let cache = Arc::new(FileStateCache::new());
    let tool = ReadFileTool::new(dir.path());

    let mut ctx = ToolContext::zero();
    ctx.tool_id = "test".into();
    ctx.file_state_cache = Some(cache.clone());

    let first = tool
        .execute_with_context(&ctx, &serde_json::json!({"path": "boundary.txt"}))
        .await
        .unwrap();
    assert!(first.output.contains("alpha"));

    // Tier-3 clears the cache.
    cache.clear();

    // Post-clear read must NOT return [FILE_UNCHANGED].
    let second = tool
        .execute_with_context(&ctx, &serde_json::json!({"path": "boundary.txt"}))
        .await
        .unwrap();
    assert!(
        !second.output.contains("[FILE_UNCHANGED]"),
        "post-compaction read must not short-circuit: {}",
        second.output
    );
    assert!(
        second.output.contains("alpha"),
        "post-compaction read must return the file body: {}",
        second.output
    );
}

#[test]
fn resume_then_read_file_uses_restored_cache_only_when_safe() {
    // The recovered refs' hash may not match the live file mtime —
    // the seeding policy uses UNIX_EPOCH for mtime so the first real
    // read MUST miss and repopulate. Here we assert that property: a
    // seed-then-peek sees the seeded entry; a get() with a modern
    // mtime sees None (mtime mismatch).
    let cache = FileStateCache::new();
    let path = PathBuf::from("/tmp/safe-resume.rs");
    cache.seed_from_replacement_refs(&[ReplacementStateRef {
        path: path.clone(),
        content_hash: Some("42".into()),
    }]);

    // `peek` ignores mtime — confirms the seed landed.
    assert!(
        cache.peek(&path).is_some(),
        "seeded entry must be present via peek"
    );

    // `get` checks mtime — with the current wall-clock time, the
    // UNIX_EPOCH seed mtime cannot match, so get() returns None.
    let now = std::time::SystemTime::now();
    assert!(
        cache.get(&path, now).is_none(),
        "seeded entry must NOT satisfy a fresh read — mtime mismatch is the safety floor"
    );
}
