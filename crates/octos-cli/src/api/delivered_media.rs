use std::path::Path;

use octos_core::SessionKey;

/// Store a browser-downloadable copy of each file the agent delivers, under
/// `download_root` (the profile data dir `/api/files` resolves). The
/// transcript keeps the original paths — what a local client shows — and
/// `/api/files` serves the copy for a `(session, path)` request; without it a
/// delivery from an approved external project folder was `403` forever.
/// Runs the copies off the runtime.
pub(super) async fn store_delivered_media_copies(
    download_root: &Path,
    session_id: &SessionKey,
    media: &[String],
    retention: octos_bus::session_artifacts::ArtifactRetentionPolicy,
) {
    if media.is_empty() {
        return;
    }
    let root = download_root.to_path_buf();
    let key = session_id.clone();
    let media = media.to_vec();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        octos_bus::session_artifacts::store_delivered_copies_with_policy(
            &root, &key, &media, retention,
        );
    })
    .await
    {
        tracing::warn!(%error, "api/serve: delivered-media copy task failed");
    }
}
