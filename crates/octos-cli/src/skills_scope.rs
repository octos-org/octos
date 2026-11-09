use std::path::{Path, PathBuf};

use eyre::Result;
use octos_agent::{SkillFilter, SkillsLoader};

use crate::profiles::{ProfileStore, SkillSelectionMode, UserProfile};

/// Outcome of resolving a profile's inherited skill selection against a set of
/// discovered candidate skills (skill layering v1).
///
/// Callers pass the output of
/// [`ProfileStore::resolve_runtime_profile`](crate::profiles::ProfileStore::resolve_runtime_profile)
/// (an already parent- and defaults-merged profile) plus the discovered
/// candidate skill ids; the resolver partitions the candidates into the ones
/// that load and the ones an inherited rule excludes, and produces the
/// crate-agnostic [`SkillFilter`] handed to both the plugin loader and the
/// [`SkillsLoader`].
#[derive(Debug, Clone)]
pub struct ResolvedSkillCatalog {
    /// The effective selection mode that was applied.
    pub mode: SkillSelectionMode,
    /// Candidate skill ids that will load (deduped, precedence preserved).
    pub enabled: Vec<String>,
    /// Candidate skill ids excluded by the active selection (debug logging).
    pub disabled: Vec<String>,
    /// Filter to hand to the loaders. `None` ⇒ the profile has no skills layer
    /// ⇒ load everything (byte-identical to pre-skill-layering behavior).
    pub filter: Option<SkillFilter>,
}

impl ResolvedSkillCatalog {
    /// Whether any candidate was excluded (for a one-line debug log).
    pub fn has_disabled(&self) -> bool {
        !self.disabled.is_empty()
    }
}

/// Resolve a profile's inherited skill selection over `discovered` candidate
/// skill ids.
///
/// `effective_profile` MUST be the resolved runtime profile (parent + global
/// `profile-defaults.json` already merged via
/// [`ProfileStore::resolve_runtime_profile`](crate::profiles::ProfileStore::resolve_runtime_profile))
/// so its `config.skills` carries the fully-merged selection layer.
///
/// Candidates are deduped by id keeping first-occurrence precedence (mirrors
/// the loader's "first dir wins" discovery). Rule matching is last-wins per id:
/// in `AllDiscovered` a skill loads unless a rule disables it; in `AllowList`
/// only skills with an enabling rule load.
///
/// When the profile has no skills layer at all the returned `filter` is `None`
/// and every candidate is reported enabled — callers pass `None` to the loaders
/// and nothing is filtered.
pub fn resolve_profile_skills(
    effective_profile: &UserProfile,
    discovered: &[String],
) -> ResolvedSkillCatalog {
    // Dedupe candidates, preserving first-occurrence precedence.
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = discovered
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();

    match effective_profile.config.skills.as_ref() {
        None => ResolvedSkillCatalog {
            mode: SkillSelectionMode::AllDiscovered,
            enabled: unique,
            disabled: Vec::new(),
            filter: None,
        },
        Some(skills) => {
            let (mut enabled, mut disabled) = (Vec::new(), Vec::new());
            for id in unique {
                if skills.allows(&id) {
                    enabled.push(id);
                } else {
                    disabled.push(id);
                }
            }
            ResolvedSkillCatalog {
                mode: skills.effective_mode(),
                enabled,
                disabled,
                filter: Some(skills.to_agent_filter()),
            }
        }
    }
}

/// Resolve the installed skills directory for exactly the requested account.
///
/// This is intentionally strict: sub-accounts do not inherit their parent
/// profile's installed customer skills.
pub fn resolve_account_skills_dir(store: &ProfileStore, profile_id: &str) -> Result<PathBuf> {
    let profile = store
        .get(profile_id)?
        .ok_or_else(|| eyre::eyre!("profile '{profile_id}' not found"))?;
    let data_dir = store.resolve_data_dir(&profile);
    Ok(data_dir.join("skills"))
}

/// Build a skills loader scoped to the current account only.
pub fn build_account_skills_loader(data_dir: &Path) -> SkillsLoader {
    SkillsLoader::new(data_dir)
}

/// Include instruction-only resources in the same read-only scope used for
/// plugin packages. Preparing the loader materializes embedded resources before
/// canonicalization; unavailable roots are omitted instead of granting a raw path.
pub fn account_skill_read_zones(data_dir: &Path, plugin_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let loader = build_account_skills_loader(data_dir);
    let mut dirs = plugin_dirs.to_vec();
    dirs.extend_from_slice(loader.skill_directories());
    octos_core::canonicalize_skill_read_zones(&dirs)
}

/// Return plugin/skill package directories for the current account only.
pub fn build_account_plugin_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let skills_dir = data_dir.join("skills");
    if skills_dir.exists() {
        vec![skills_dir]
    } else {
        Vec::new()
    }
}

/// Resolve the ominix-api URL the runtime should hand to skills as
/// `OMINIX_API_URL`. Prefers the explicit env override, falls back to
/// the `<ominix home>/.ominix/api_url` discovery file dropped by the
/// installer.
///
/// The OMiniX home resolution mirrors the runtime installer
/// (`api::ominix_runtime`): `OCTOS_OMINIX_HOME` relocates the whole
/// OMiniX home (including the discovery file), falling back to `HOME`
/// and then the OS home directory.
///
/// Used by both `gateway` and `serve` plugin loaders so dashboard-
/// installed skills (`mofa-fm`, etc.) can reach the local inference
/// server.
pub(crate) fn discover_ominix_url() -> Option<String> {
    std::env::var("OMINIX_API_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let home = std::env::var_os("OCTOS_OMINIX_HOME")
                .or_else(|| std::env::var_os("HOME"))
                .map(std::path::PathBuf::from)
                .or_else(dirs::home_dir)?;
            discover_ominix_url_under(&home)
        })
}

fn discover_ominix_url_under(home: &std::path::Path) -> Option<String> {
    for dir in [".ominix", ".OminiX"] {
        let discovery = home.join(dir).join("api_url");
        if let Some(url) = std::fs::read_to_string(discovery)
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(url);
        }
    }
    None
}

/// Resolve the ASR endpoint independently from OminiX. `ASR_API_URL` is an
/// explicit Octos/OMiniX JSON batch-transcription override at
/// `/v1/audio/transcriptions`; without it, ASR continues to use OminiX for
/// backward compatibility.
pub(crate) fn discover_asr_url() -> Option<String> {
    discover_asr_route().into_url()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AsrRoute {
    External(String),
    Ominix(Option<String>),
}

impl AsrRoute {
    fn into_url(self) -> Option<String> {
        match self {
            Self::External(url) => Some(url),
            Self::Ominix(url) => url,
        }
    }
}

pub(crate) fn discover_asr_route() -> AsrRoute {
    resolve_asr_route(std::env::var("ASR_API_URL").ok(), discover_ominix_url)
}

fn resolve_asr_route<F>(explicit: Option<String>, discover_ominix: F) -> AsrRoute
where
    F: FnOnce() -> Option<String>,
{
    let explicit = explicit
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    match explicit {
        Some(url) => AsrRoute::External(url),
        None => AsrRoute::Ominix(discover_ominix()),
    }
}

/// Append the standard per-profile runtime env vars onto a plugin-env
/// vector. Mirrors the gateway path's call site at
/// `gateway_runtime.rs:435` so the `serve` plugin loader can spawn
/// dashboard-installed skills with the same environment they expect.
///
/// The set is intentionally narrow: every entry is something a
/// dashboard-installed skill (e.g. `mofa-fm`) needs to locate
/// per-profile state (voice profiles, data dir) or to reach the
/// local inference server (`ominix-api`).
pub(crate) fn push_runtime_plugin_env(
    plugin_env: &mut Vec<(String, String)>,
    data_dir: &Path,
    octos_home: &Path,
    profile_id: Option<&str>,
    ominix_url: Option<&str>,
) {
    plugin_env.push((
        "OCTOS_DATA_DIR".to_string(),
        data_dir.to_string_lossy().to_string(),
    ));
    plugin_env.push((
        "OCTOS_HOME".to_string(),
        octos_home.to_string_lossy().to_string(),
    ));
    if let Some(profile_id) = profile_id {
        plugin_env.push(("OCTOS_PROFILE_ID".to_string(), profile_id.to_string()));
    }
    plugin_env.push((
        "OCTOS_VOICE_DIR".to_string(),
        data_dir
            .join("voice_profiles")
            .to_string_lossy()
            .to_string(),
    ));
    if let Some(ominix_url) = ominix_url {
        plugin_env.push(("OMINIX_API_URL".to_string(), ominix_url.to_string()));
    }
}
