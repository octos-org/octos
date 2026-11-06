use super::*;

// ===================================================================
// WEB-WORKSPACE-BROWSER-CONTRACT-5000 — `onboarding/workspace_list` and
// `onboarding/workspace_create`, the local-solo folder browser that lets
// the web onboarding form pick a workspace instead of typing a blind
// absolute path. Gated on `onboarding.workspace_browse.v1`; both methods
// refuse tenant/cloud exactly like `onboarding/workspace_probe` (#1057).
// ===================================================================

/// Helper: pull the typed `data.kind` discriminant off an `RpcError`.
fn workspace_browse_error_kind(error: &RpcError) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("kind"))
        .and_then(|kind| kind.as_str())
        .map(ToOwned::to_owned)
}

/// Contract §1 — a listing returns DIRECTORIES ONLY, never files, sorted
/// case-insensitively by name, each with its canonical absolute path.
#[test]
fn should_list_only_sorted_directories_with_canonical_paths_when_listing_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    // Feed the canonical base, the way a real client does: every path it
    // sends back came from a previous `canonical_path` / entry `path`.
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("projects");
    std::fs::create_dir_all(&root).unwrap();
    for name in ["Zebra", "alpha", "Beta"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    std::fs::write(root.join("notes.txt"), "not a directory").unwrap();
    let canonical_root = std::fs::canonicalize(&root).unwrap();

    let result = onboarding_workspace_list_result(&state, Some(root.to_str().unwrap()))
        .expect("list an existing directory");

    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        canonical_root.to_string_lossy()
    );
    let entries = result["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["alpha", "Beta", "Zebra"],
        "entries must be directories only, sorted case-insensitively"
    );
    for entry in entries {
        let name = entry["name"].as_str().unwrap();
        assert_eq!(
            entry["path"].as_str().unwrap(),
            std::fs::canonicalize(canonical_root.join(name))
                .unwrap()
                .to_string_lossy(),
            "each entry path must be the canonical absolute path"
        );
        assert_eq!(entry["writable"], json!(true));
    }
    assert_eq!(result["writable"], json!(true));
    assert_eq!(result["truncated"], json!(false));
    assert_eq!(result["hidden_skipped"], json!(0));
    assert_eq!(
        result["parent_path"].as_str().unwrap(),
        canonical_root.parent().unwrap().to_string_lossy()
    );
}

/// Contract §1 — dot-directories are omitted from `entries` and counted
/// in `hidden_skipped` (files, hidden or not, are never counted: they are
/// not listable entries in the first place).
#[test]
fn should_skip_and_count_hidden_directories_when_listing_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("projects");
    std::fs::create_dir_all(root.join("visible")).unwrap();
    for hidden in [".git", ".cache", ".config"] {
        std::fs::create_dir_all(root.join(hidden)).unwrap();
    }
    std::fs::write(root.join(".dotfile"), "hidden file, not a dir").unwrap();

    let result = onboarding_workspace_list_result(&state, Some(root.to_str().unwrap()))
        .expect("list a directory holding dot-directories");

    let names: Vec<&str> = result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["visible"]);
    assert_eq!(
        result["hidden_skipped"],
        json!(3),
        "the three dot-directories must be counted, the dot-FILE must not"
    );
}

/// Contract §1 — at most 500 entries; `truncated` is true when more
/// existed. The kept 500 are the first 500 of the sorted order.
#[test]
fn should_set_truncated_when_directory_exceeds_the_five_hundred_entry_cap() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("many");
    std::fs::create_dir_all(&root).unwrap();
    for index in 0..ONBOARDING_WORKSPACE_LIST_MAX_ENTRIES + 1 {
        std::fs::create_dir_all(root.join(format!("dir-{index:04}"))).unwrap();
    }

    let result = onboarding_workspace_list_result(&state, Some(root.to_str().unwrap()))
        .expect("list an over-cap directory");

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), ONBOARDING_WORKSPACE_LIST_MAX_ENTRIES);
    assert_eq!(result["truncated"], json!(true));
    assert_eq!(entries[0]["name"], json!("dir-0000"));
}

/// Contract §1 — `parent_path` is null at the filesystem root.
#[test]
fn should_report_null_parent_path_when_listing_the_filesystem_root() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    // The root is `/` on Unix and `<drive>:\` on Windows; derive it from
    // the canonical working directory so the contract runs on both.
    let root = std::fs::canonicalize(std::env::current_dir().unwrap())
        .unwrap()
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();

    let result = onboarding_workspace_list_result(&state, Some(root.to_str().unwrap()))
        .expect("list the filesystem root");

    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        root.to_string_lossy()
    );
    assert_eq!(
        result["parent_path"],
        Value::Null,
        "the filesystem root has no parent"
    );
}

/// Contract §1 — a null/empty `path` means "the server's own working
/// directory".
#[test]
fn should_list_the_server_working_directory_when_path_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();

    let result =
        onboarding_workspace_list_result(&state, None).expect("list the server working directory");

    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        cwd.to_string_lossy()
    );
}

/// Contract §1 — a missing path, a file path and a banned system root
/// each get their own typed `data.kind`.
#[test]
fn should_return_typed_not_found_when_listing_a_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let missing = base.join("never-created");

    let error = onboarding_workspace_list_result(&state, Some(missing.to_str().unwrap()))
        .expect_err("a missing path must be rejected");

    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_list_not_found")
    );
}

#[test]
fn should_return_typed_not_a_directory_when_listing_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let file = base.join("README.md");
    std::fs::write(&file, "a regular file").unwrap();

    let error = onboarding_workspace_list_result(&state, Some(file.to_str().unwrap()))
        .expect_err("a file path must be rejected");

    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_list_not_a_directory")
    );
}

/// Windows counterpart of the unix banned-root test: a unix-style path
/// is not absolute on Windows (no drive prefix), so the resolver refuses
/// it as an invalid path long before the unix-only banned-root rule runs.
#[cfg(windows)]
#[test]
fn should_return_typed_invalid_path_when_listing_a_unix_style_path_on_windows() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let error = onboarding_workspace_list_result(&state, Some("/etc"))
        .expect_err("a unix-style path is not absolute on Windows");

    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_list_invalid_path")
    );
}

// The banned-system-root list is Unix-only (`/etc`, `/usr`, `/proc`, …):
// on Windows `/etc` is not absolute and is refused as an invalid path
// before the banned-root rule runs, and no Windows roots are banned.
#[cfg(unix)]
#[test]
fn should_return_typed_root_escape_when_listing_a_banned_system_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let error = onboarding_workspace_list_result(&state, Some("/etc"))
        .expect_err("a banned system root must be rejected");

    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_list_root_escape")
    );
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("banned_root")),
        Some(&json!("etc")),
        "the root-escape error must name the banned system component"
    );
}

#[test]
fn should_return_typed_invalid_path_when_listing_an_empty_or_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    // An explicitly blank string is not "null means cwd": it is unusable.
    let error = onboarding_workspace_list_result(&state, Some("   "))
        .expect_err("a blank path must be rejected");
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_list_invalid_path")
    );

    let relative = onboarding_workspace_list_result(&state, Some("relative/path"))
        .expect_err("a relative path must be rejected");
    assert_eq!(
        workspace_browse_error_kind(&relative).as_deref(),
        Some("workspace_list_invalid_path")
    );
}

/// Contract §1 — `~` is expanded exactly the way `onboarding/workspace_probe`
/// expands it.
#[test]
fn should_expand_home_prefix_when_listing_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let Ok(canonical_home) = std::fs::canonicalize(&home) else {
        return;
    };

    let result = onboarding_workspace_list_result(&state, Some("~")).expect("list the home dir");

    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        canonical_home.to_string_lossy()
    );
}

/// Contract §1 — tenant / cloud deployments are refused exactly like the
/// probe refuses them.
#[test]
fn should_refuse_workspace_list_when_local_solo_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let local = local_profile_state(dir.path());
    let tenant = AppState {
        deployment_mode: crate::config::DeploymentMode::Tenant,
        profile_store: local.profile_store.clone(),
        user_store: local.user_store.clone(),
        ..AppState::empty_for_tests()
    };

    let error = onboarding_workspace_list_result(&tenant, Some(dir.path().to_str().unwrap()))
        .expect_err("tenant rejection");

    assert_eq!(error.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("profile_local_unsupported")
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("runtime_mode")),
        Some(&json!("multi_tenant"))
    );
}

/// Contract §2 — `name` is exactly one path component: no `/`, no `\`,
/// not `.`, not `..`, no control characters, 1..=255 bytes, and it must
/// not start or end with whitespace.
#[test]
fn should_reject_invalid_names_when_creating_workspace_folder() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let parent_dir = dir.path().join("parent");
    std::fs::create_dir_all(&parent_dir).unwrap();
    let parent = parent_dir.to_str().unwrap();
    let over_long = "a".repeat(ONBOARDING_WORKSPACE_CREATE_MAX_NAME_BYTES + 1);

    for name in [
        "..",
        ".",
        "a/b",
        "a\\b",
        "",
        " leading",
        "trailing ",
        "ctrl\u{0007}char",
        "nul\0byte",
        over_long.as_str(),
    ] {
        let error = onboarding_workspace_create_result(&state, parent, name)
            .expect_err(&format!("{name:?} must be rejected"));
        assert_eq!(
            error.code,
            rpc_error_codes::INVALID_PARAMS,
            "{name:?} must be an invalid-params rejection"
        );
        assert_eq!(
            workspace_browse_error_kind(&error).as_deref(),
            Some("workspace_create_invalid_name"),
            "{name:?} must be rejected as an invalid name"
        );
    }

    let leftovers = std::fs::read_dir(&parent_dir)
        .unwrap()
        .filter_map(Result::ok)
        .count();
    assert_eq!(leftovers, 0, "a rejected name must never create anything");
}

#[test]
fn should_create_directory_and_report_created_when_creating_workspace_folder() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let parent = std::fs::canonicalize(dir.path()).unwrap();

    let result = onboarding_workspace_create_result(&state, parent.to_str().unwrap(), "new-app")
        .expect("create a folder");

    assert_eq!(result["created"], json!(true));
    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        std::fs::canonicalize(parent.join("new-app"))
            .unwrap()
            .to_string_lossy()
    );
    assert!(parent.join("new-app").is_dir());
    assert_eq!(
        result.as_object().unwrap().len(),
        2,
        "the create result carries exactly canonical_path + created"
    );
}

/// Contract §2 — `created` is false when a directory of that name already
/// existed: idempotent success, not an error.
#[test]
fn should_report_created_false_when_workspace_folder_already_exists() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(parent.join("existing")).unwrap();

    let result = onboarding_workspace_create_result(&state, parent.to_str().unwrap(), "existing")
        .expect("an existing directory is an idempotent success");

    assert_eq!(result["created"], json!(false));
    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        std::fs::canonicalize(parent.join("existing"))
            .unwrap()
            .to_string_lossy()
    );
}

/// Contract §2 — a NON-directory already at that path is an error.
#[test]
fn should_return_typed_exists_not_directory_when_a_file_occupies_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(parent.join("occupied"), "a file, not a folder").unwrap();

    let error = onboarding_workspace_create_result(&state, parent.to_str().unwrap(), "occupied")
        .expect_err("a file of that name must be an error");

    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_create_exists_not_directory")
    );
}

#[test]
fn should_return_typed_parent_errors_when_creating_workspace_folder() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let missing = base.join("no-such-parent");
    let file_parent = base.join("parent.txt");
    std::fs::write(&file_parent, "a file used as a parent").unwrap();

    let not_found = onboarding_workspace_create_result(&state, missing.to_str().unwrap(), "child")
        .expect_err("a missing parent must be rejected");
    assert_eq!(
        workspace_browse_error_kind(&not_found).as_deref(),
        Some("workspace_create_parent_not_found")
    );

    let not_a_directory =
        onboarding_workspace_create_result(&state, file_parent.to_str().unwrap(), "child")
            .expect_err("a file parent must be rejected");
    assert_eq!(
        workspace_browse_error_kind(&not_a_directory).as_deref(),
        Some("workspace_create_parent_not_a_directory")
    );
}

/// Contract §2 — a `parent` rooted under a banned system path is a typed
/// root escape naming the banned component. Unix-only for the same reason
/// as the list-side banned-root test above.
#[cfg(unix)]
#[test]
fn should_return_typed_root_escape_when_creating_under_a_banned_system_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let root_escape = onboarding_workspace_create_result(&state, "/etc", "child")
        .expect_err("a banned system parent must be rejected");
    assert_eq!(
        workspace_browse_error_kind(&root_escape).as_deref(),
        Some("workspace_create_root_escape")
    );
    assert_eq!(
        root_escape
            .data
            .as_ref()
            .and_then(|data| data.get("banned_root")),
        Some(&json!("etc"))
    );
}

/// Windows counterpart: the contract maps an unusable `parent` to
/// `workspace_create_parent_not_found`, and a unix-style parent is
/// unusable on Windows because it is not absolute.
#[cfg(windows)]
#[test]
fn should_report_unix_style_parent_as_not_found_when_creating_on_windows() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let error = onboarding_workspace_create_result(&state, "/etc", "child")
        .expect_err("a unix-style parent is not absolute on Windows");

    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_create_parent_not_found")
    );
}

/// Contract §2 — the created path, canonicalized, must still live under
/// `parent`: a pre-existing symlink that points outside is a root escape,
/// never a silent success on someone else's directory.
#[cfg(unix)]
#[test]
fn should_return_typed_root_escape_when_the_name_symlinks_outside_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    let outside = parent.join("outside");
    let inside = parent.join("inside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&inside).unwrap();
    std::os::unix::fs::symlink(&outside, inside.join("escape")).unwrap();

    let error = onboarding_workspace_create_result(&state, inside.to_str().unwrap(), "escape")
        .expect_err("a symlink escape must be rejected");

    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("workspace_create_root_escape")
    );
}

#[test]
fn should_refuse_workspace_create_when_local_solo_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let local = local_profile_state(dir.path());
    let tenant = AppState {
        deployment_mode: crate::config::DeploymentMode::Tenant,
        profile_store: local.profile_store.clone(),
        user_store: local.user_store.clone(),
        ..AppState::empty_for_tests()
    };

    let error =
        onboarding_workspace_create_result(&tenant, dir.path().to_str().unwrap(), "new-app")
            .expect_err("tenant rejection");

    assert_eq!(error.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        workspace_browse_error_kind(&error).as_deref(),
        Some("profile_local_unsupported")
    );
    assert!(
        !dir.path().join("new-app").exists(),
        "a refused create must not touch the filesystem"
    );
}

/// Contract gate — both methods and `onboarding.workspace_browse.v1` are
/// advertised for local-solo deployments and withheld from tenant ones, so
/// a client that cannot see the feature fails closed to the typed-path form.
#[test]
fn should_advertise_workspace_browse_only_for_local_solo_deployments() {
    let dir = tempfile::tempdir().unwrap();
    let local = local_profile_state(dir.path());
    let local_capabilities = ConnectionUiFeatures::default().advertised_capabilities(&local);
    for method in [
        APPUI_METHOD_ONBOARDING_WORKSPACE_LIST,
        APPUI_METHOD_ONBOARDING_WORKSPACE_CREATE,
    ] {
        assert!(
            local_capabilities
                .supported_methods
                .iter()
                .any(|advertised| advertised == method),
            "local solo deployment must advertise {method}",
        );
    }
    assert!(
        local_capabilities.supports_feature(APPUI_FEATURE_ONBOARDING_WORKSPACE_BROWSE_V1),
        "local solo deployment must advertise the workspace browse feature",
    );

    let tenant = AppState {
        deployment_mode: crate::config::DeploymentMode::Tenant,
        profile_store: local.profile_store.clone(),
        user_store: local.user_store.clone(),
        ..AppState::empty_for_tests()
    };
    let tenant_capabilities = ConnectionUiFeatures::default().advertised_capabilities(&tenant);
    for method in [
        APPUI_METHOD_ONBOARDING_WORKSPACE_LIST,
        APPUI_METHOD_ONBOARDING_WORKSPACE_CREATE,
    ] {
        assert!(
            !tenant_capabilities
                .supported_methods
                .iter()
                .any(|advertised| advertised == method),
            "tenant deployment must NOT advertise {method}",
        );
    }
    assert!(
        !tenant_capabilities.supports_feature(APPUI_FEATURE_ONBOARDING_WORKSPACE_BROWSE_V1),
        "tenant deployment must NOT advertise the workspace browse feature",
    );
}
