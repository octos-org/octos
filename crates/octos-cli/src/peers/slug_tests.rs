use super::*;

#[test]
fn stage_peer_unicode_titles_use_safe_handles_and_keep_the_brief() {
    let tmp = tempfile::tempdir().unwrap();
    let brief = "只回复一句话：协作者启动成功。";
    for (index, title) in [
        brief.to_owned(),
        "修复 CI 并 Review 代码".to_owned(),
        "🔬🧑‍💻🚀".to_owned(),
        "请协作者检查中文任务。".repeat(100),
    ]
    .iter()
    .enumerate()
    {
        let peers = tmp.path().join(index.to_string());
        let expected = name_to_slug(title).unwrap();
        for attempt in 1..=2 {
            let staged = stage_peer(
                &peers,
                tmp.path(),
                title,
                None,
                None,
                brief,
                false,
                None,
                None,
            )
            .unwrap();
            assert_eq!(
                staged.slug,
                if attempt == 1 {
                    expected.clone()
                } else {
                    format!("{expected}-2")
                }
            );
            assert!(peer_slug_is_safe(&staged.slug));
            assert!(
                staged
                    .slug
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            );
            assert_eq!(staged.topic, format!("peer-{}", staged.slug));
            assert_eq!(std::fs::read_to_string(staged.brief_path).unwrap(), brief);
            assert!(staged_peer_dir(&peers, &staged.slug).is_some());
        }
        if index == 1 {
            assert_eq!(expected, "ci-review");
        } else {
            assert!(expected.starts_with("peer-"));
            assert_eq!(expected.len(), 21);
        }
    }
}

#[test]
fn reserve_peer_dir_bounds_collision_suffixes_at_the_slug_limit() {
    let tmp = tempfile::tempdir().unwrap();
    // Put a dash at a truncation boundary as well as exceeding the cap.
    let title = format!("{}-{}", "a".repeat(60), "b".repeat(20));
    let mut claimed = std::collections::HashSet::new();
    for attempt in 1..=100 {
        let (slug, dir) = reserve_peer_dir(tmp.path(), &title).unwrap();
        assert!(peer_slug_is_safe(&slug), "attempt {attempt}: {slug}");
        assert!(!slug.contains("--"));
        assert!(dir.is_dir());
        assert!(claimed.insert(slug.clone()));
        if attempt > 1 {
            assert!(slug.ends_with(&format!("-{attempt}")));
        }
    }
    assert!(reserve_peer_dir(tmp.path(), &title).is_err());
    let (slug, _) = reserve_peer_dir(tmp.path(), "   ").unwrap();
    assert_eq!(slug, "peer", "blank seeds still get a usable handle");
}

#[test]
fn stage_peer_unicode_names_keep_display_names_and_reject_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let peers = tmp.path().join("peers");
    for name in ["协作者", "中文 Review", "🔬"] {
        let staged = stage_peer(
            &peers,
            tmp.path(),
            "unused",
            Some(name),
            None,
            "简报",
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(peers.join(&staged.slug).join("name")).unwrap(),
            name
        );
        assert_eq!(resolve_peer_name_to_slug(&peers, name), Some(staged.slug));
        let error = stage_peer(
            &peers,
            tmp.path(),
            "unused",
            Some(name),
            None,
            "简报",
            false,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.message.contains("already exists"));
    }
}
