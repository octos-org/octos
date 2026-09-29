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
    "publisher_feeds",
    "duckduckgo",
    "bing",
    "google",
    "google_cse",
    "brave_web",
    "bing_news",
    "brave",
];

/// Read a pins file (`{"<engine id>": "sha256:<hex>"}`) for [`Registry::load_dir`].
/// Refused when the file lies inside `engines_dir`: pins must be kept where
/// the engine files' writer cannot also change them.
pub fn read_pins(pins_file: &Path, engines_dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(pins_file).starts_with(canon(engines_dir)) {
        return Err(format!(
            "{} is inside the engines directory; keep pins outside it",
            pins_file.display()
        ));
    }
    let text =
        std::fs::read_to_string(pins_file).map_err(|e| format!("{}: {e}", pins_file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", pins_file.display()))
}

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
    /// pinned: its digest must equal `pins[id]`. Pins come from the host
    /// (see [`read_pins`]), never from the engine directory itself, so write
    /// access to the directory is not enough to add or change an engine.
    /// An engine whose id matches a built-in is rejected unless
    /// `allow_override` is set.
    pub fn load_dir(&mut self, dir: &Path, pins: &BTreeMap<String, String>, allow_override: bool) {
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
            let shadows_builtin = BUILTIN.iter().any(|(id, _, _)| *id == engine.id());
            if shadows_builtin && !allow_override {
                self.rejected.push(Rejected {
                    path: shown,
                    reason: format!(
                        "id {:?} is a built-in engine; replacing it needs the host's override setting",
                        engine.id()
                    ),
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
    fn should_load_only_engines_the_host_pinned() {
        let root = std::env::temp_dir().join(format!("octos-engines-{}", std::process::id()));
        let dir = root.join("engines");
        let _ = std::fs::remove_dir_all(&root);
        let (_, hn_manifest, source) = BUILTIN
            .iter()
            .find(|(id, _, _)| *id == "hackernews")
            .unwrap();
        // A third-party engine: the Hacker News engine under another id.
        let manifest = hn_manifest.replace("\"id\": \"hackernews\"", "\"id\": \"hn_mirror\"");
        for name in ["hn_mirror", "wrongname"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("manifest.json"), &manifest).unwrap();
            std::fs::write(dir.join(name).join("engine.octoscript"), source).unwrap();
        }
        let good = digest(&manifest, source);

        // Unpinned: not loaded.
        let mut r = Registry::default();
        r.load_dir(&dir, &BTreeMap::new(), false);
        assert!(r.get("hn_mirror").is_none());
        assert!(
            r.rejected
                .iter()
                .any(|x| x.reason.starts_with("not pinned"))
        );

        // A pins.json inside the engine directory is not trusted.
        std::fs::write(
            dir.join("pins.json"),
            serde_json::json!({ "hn_mirror": good }).to_string(),
        )
        .unwrap();
        let mut r = Registry::default();
        r.load_dir(&dir, &BTreeMap::new(), false);
        assert!(r.get("hn_mirror").is_none(), "directory writers cannot pin");
        assert!(read_pins(&dir.join("pins.json"), &dir).is_err());

        // Wrong digest.
        let mut r = Registry::default();
        let pins = BTreeMap::from([("hn_mirror".to_string(), "sha256:00".to_string())]);
        r.load_dir(&dir, &pins, false);
        assert!(r.get("hn_mirror").is_none());
        assert!(
            r.rejected
                .iter()
                .any(|x| x.reason.contains("does not match"))
        );

        // Host pins kept outside the directory.
        let pins_file = root.join("pins.json");
        std::fs::write(
            &pins_file,
            serde_json::json!({ "hn_mirror": good }).to_string(),
        )
        .unwrap();
        let pins = read_pins(&pins_file, &dir).unwrap();
        let mut r = Registry::default();
        r.load_dir(&dir, &pins, false);
        let e = r.get("hn_mirror").expect("pinned engine loads");
        assert_eq!(e.origin, EngineOrigin::Dir(dir.join("hn_mirror")));
        assert!(
            r.rejected
                .iter()
                .any(|x| x.reason.contains("directory name"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn should_not_let_a_directory_engine_shadow_a_builtin_by_default() {
        let dir = std::env::temp_dir().join(format!("octos-shadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (_, manifest, source) = BUILTIN.iter().find(|(id, _, _)| *id == "gdelt").unwrap();
        std::fs::create_dir_all(dir.join("gdelt")).unwrap();
        std::fs::write(dir.join("gdelt").join("manifest.json"), manifest).unwrap();
        std::fs::write(dir.join("gdelt").join("engine.octoscript"), source).unwrap();
        let pins = BTreeMap::from([("gdelt".to_string(), digest(manifest, source))]);

        let mut r = Registry::builtin();
        r.load_dir(&dir, &pins, false);
        assert_eq!(r.get("gdelt").unwrap().origin, EngineOrigin::Builtin);
        assert!(r.rejected.iter().any(|x| x.reason.contains("built-in")));

        let mut r = Registry::builtin();
        r.load_dir(&dir, &pins, true);
        assert_eq!(
            r.get("gdelt").unwrap().origin,
            EngineOrigin::Dir(dir.join("gdelt"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
