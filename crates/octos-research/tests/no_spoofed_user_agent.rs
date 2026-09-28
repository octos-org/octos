//! Policy check (OctoSense ADR 0002 §6, review of octos#2568): the research
//! fetch paths must not pose as a desktop browser. No hard-coded browser
//! User-Agent (`Mozilla/5.0 (...) ... Chrome/...`, `AppleWebKit`, `Safari/`)
//! may appear in the research crates or the built-in search tools — including
//! behind the opt-in search-results scrapers — nor in the metasearch engine
//! scripts and manifests.

use std::path::{Path, PathBuf};

fn sources() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = Vec::new();
    for dir in [
        "octos-research/src",
        "octos-research/engines",
        "octos-research/examples",
        "app-skills/deep-search/src",
        "app-skills/deep-crawl/src",
    ] {
        collect(&crates.join(dir), &mut out);
    }
    for file in [
        "octos-agent/src/tools/web_search.rs",
        "octos-agent/src/tools/deep_search.rs",
        "octos-agent/src/tools/site_crawl.rs",
        "app-skills/news/src/main.rs",
        "octos-cli/src/api/bilibili.rs",
    ] {
        out.push(crates.join(file));
    }
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            // Recorded provider responses are data, not requests we send.
            if path.file_name().is_some_and(|n| n != "fixtures") {
                collect(&path, out);
            }
        } else if path
            .extension()
            .is_some_and(|e| e == "rs" || e == "octoscript" || e == "json")
        {
            out.push(path);
        }
    }
}

#[test]
fn should_not_contain_spoofed_browser_user_agents() {
    // Built from pieces so this file does not match itself.
    let spoof = regex::Regex::new(&format!(
        r"{}[^\n]*({}|{}|{})",
        regex::escape(&["Mozilla", "/5.0 ("].concat()),
        ["Chrome", "/"].concat(),
        ["Apple", "WebKit/"].concat(),
        ["Safari", "/"].concat(),
    ))
    .unwrap();
    let files = sources();
    assert!(files.len() >= 6, "scanned too few files: {files:?}");
    let mut hits = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, line) in text.lines().enumerate() {
            if spoof.is_match(line) {
                hits.push(format!("{}:{}: {}", f.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "spoofed browser User-Agent strings found:\n{}",
        hits.join("\n")
    );
}
