//! Engine discovery and pinning.
//!
//! Built-in engines are compiled into the crate from `engines/<id>/`. More
//! can be loaded from a directory with the same layout; each one is pinned by
//! the SHA-256 digest of its manifest and script, so an engine is added or
//! fixed by dropping in reviewed files and their digest, without a rebuild.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::manifest::EngineManifest;
use super::sandbox::check_engine_source;

/// Where an engine came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineOrigin {
    Builtin,
    Dir(PathBuf),
}

/// One loaded engine.
#[derive(Debug, Clone)]
pub struct Engine {
    pub manifest: EngineManifest,
    pub source: String,
    /// `sha256:<hex>` over the manifest bytes, a NUL, and the script bytes.
    pub digest: String,
    pub origin: EngineOrigin,
}

impl Engine {
    pub fn load(manifest_json: &str, source: &str, origin: EngineOrigin) -> Result<Self, String> {
        let manifest = EngineManifest::parse(manifest_json)?;
        check_engine_source(source).map_err(|e| format!("{}: {e}", manifest.id))?;
        Ok(Self {
            digest: digest(manifest_json, source),
            manifest,
            source: source.to_string(),
            origin,
        })
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }
}

/// Digest that pins an engine.
pub fn digest(manifest_json: &str, source: &str) -> String {
    let mut h = Sha256::new();
    h.update(manifest_json.as_bytes());
    h.update([0u8]);
    h.update(source.as_bytes());
    let bytes = h.finalize();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

macro_rules! builtin {
    ($($id:literal),* $(,)?) => {
        &[$((
            $id,
            include_str!(concat!("../../engines/", $id, "/manifest.json")),
            include_str!(concat!("../../engines/", $id, "/engine.octoscript")),
        )),*]
    };
}

/// `(id, manifest.json, engine.octoscript)` of every built-in engine.
pub const BUILTIN: &[(&str, &str, &str)] = builtin![
    "gdelt",
    "google_news",
    "wikipedia",
    "wikidata",
    "arxiv",
    "openalex",
    "hackernews",
    "github",
    "stackexchange",
    "mastodon",
    "brave",
];

/// A skipped engine and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub path: String,
    pub reason: String,
}

/// The engines a metasearch can use, by id.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    engines: BTreeMap<String, Engine>,
    pub rejected: Vec<Rejected>,
}

impl Registry {
    /// Just the built-in engines.
    pub fn builtin() -> Self {
        let mut r = Self::default();
        for (id, manifest, source) in BUILTIN {
            match Engine::load(manifest, source, EngineOrigin::Builtin) {
                Ok(e) => {
                    debug_assert_eq!(e.id(), *id);
                    r.engines.insert(e.id().to_string(), e);
                }
                // A broken built-in is a build bug; tests catch it. Keep the
                // rest usable.
                Err(reason) => r.rejected.push(Rejected {
                    path: format!("builtin:{id}"),
                    reason,
                }),
            }
        }
        r
    }

    /// Load `dir/<id>/{manifest.json, engine.octoscript}`. Each engine must be
    /// pinned: its digest must appear in `pins` (id → digest) or in
    /// `dir/pins.json`. A pinned engine replaces a built-in with the same id.
    pub fn load_dir(&mut self, dir: &Path, pins: &BTreeMap<String, String>) {
        let mut pins = pins.clone();
        if let Ok(text) = std::fs::read_to_string(dir.join("pins.json")) {
            match serde_json::from_str::<BTreeMap<String, String>>(&text) {
                Ok(file_pins) => {
                    for (k, v) in file_pins {
                        pins.entry(k).or_insert(v);
                    }
                }
                Err(e) => self.rejected.push(Rejected {
                    path: dir.join("pins.json").display().to_string(),
                    reason: format!("unreadable pins.json: {e}"),
                }),
            }
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        paths.sort();
        for path in paths {
            let shown = path.display().to_string();
            let read = |name: &str| std::fs::read_to_string(path.join(name));
            let (Ok(manifest), Ok(source)) = (read("manifest.json"), read("engine.octoscript"))
            else {
                self.rejected.push(Rejected {
                    path: shown,
                    reason: "missing manifest.json or engine.octoscript".into(),
                });
                continue;
            };
            let engine = match Engine::load(&manifest, &source, EngineOrigin::Dir(path.clone())) {
                Ok(e) => e,
                Err(reason) => {
                    self.rejected.push(Rejected {
                        path: shown,
                        reason,
                    });
                    continue;
                }
            };
            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if dir_name != engine.id() {
                self.rejected.push(Rejected {
                    path: shown,
                    reason: format!("directory name must equal the engine id {:?}", engine.id()),
                });
                continue;
            }
            match pins.get(engine.id()) {
                Some(p) if *p == engine.digest => {
                    self.engines.insert(engine.id().to_string(), engine);
                }
                Some(p) => self.rejected.push(Rejected {
                    path: shown,
                    reason: format!("digest {} does not match pin {p}", engine.digest),
                }),
                None => self.rejected.push(Rejected {
                    path: shown,
                    reason: format!("not pinned (digest {})", engine.digest),
                }),
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Engine> {
        self.engines.get(id)
    }

    pub fn engines(&self) -> impl Iterator<Item = &Engine> {
        self.engines.values()
    }

    pub fn insert(&mut self, engine: Engine) {
        self.engines.insert(engine.id().to_string(), engine);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_load_every_builtin_engine() {
        let r = Registry::builtin();
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        assert_eq!(r.engines().count(), BUILTIN.len());
        for e in r.engines() {
            assert!(e.digest.starts_with("sha256:") && e.digest.len() == 71);
        }
    }

    #[test]
    fn should_load_only_pinned_engines_from_a_directory() {
        let dir = std::env::temp_dir().join(format!("octos-engines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (_, manifest, source) = BUILTIN
            .iter()
            .find(|(id, _, _)| *id == "hackernews")
            .unwrap();
        for name in ["hackernews", "wrongname"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("manifest.json"), manifest).unwrap();
            std::fs::write(dir.join(name).join("engine.octoscript"), source).unwrap();
        }
        let good = digest(manifest, source);

        let mut r = Registry::default();
        r.load_dir(&dir, &BTreeMap::new());
        assert!(
            r.get("hackernews").is_none(),
            "unpinned engines are not loaded"
        );
        assert!(
            r.rejected
                .iter()
                .any(|x| x.reason.starts_with("not pinned"))
        );

        let mut r = Registry::default();
        let pins = BTreeMap::from([("hackernews".to_string(), "sha256:00".to_string())]);
        r.load_dir(&dir, &pins);
        assert!(r.get("hackernews").is_none());
        assert!(
            r.rejected
                .iter()
                .any(|x| x.reason.contains("does not match"))
        );

        let mut r = Registry::default();
        std::fs::write(
            dir.join("pins.json"),
            serde_json::json!({ "hackernews": good }).to_string(),
        )
        .unwrap();
        r.load_dir(&dir, &BTreeMap::new());
        let e = r.get("hackernews").expect("pinned engine loads");
        assert_eq!(e.origin, EngineOrigin::Dir(dir.join("hackernews")));
        assert!(
            r.rejected
                .iter()
                .any(|x| x.reason.contains("directory name"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
