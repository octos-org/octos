//! Discovery for local OpenAI-compatible model servers.
//!
//! Every supported local engine — llama.cpp's `llama-server`, Ollama, vLLM,
//! LM Studio — answers `GET {base_url}/models` with the OpenAI list-models
//! shape (`{"data": [{"id": "..."}]}`). That one endpoint is enough to (a)
//! verify a server is reachable and (b) learn the real model id(s) so the
//! user never has to type one. The HTTP call itself stays with the caller
//! (doctor uses its own credential-stripping blocking probe); this module owns
//! the engine-agnostic facts: where local servers usually listen, the shared
//! placeholder model id, and how to read a `/models` answer.

/// The `local` family's default base URL: llama.cpp's `llama-server` default.
/// Also the second entry of [`CANDIDATE_BASE_URLS`].
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";

/// Placeholder model id used when neither config nor catalog names one.
/// Single-model local servers (llama.cpp, LM Studio, vLLM) ignore the
/// request's `model` field, so any placeholder works against them.
///
/// Deliberately namespaced (`local-default`, not `default`): the catalog row
/// `local/<this>` registers the id as a context/pricing alias in every
/// deployment, and those lookups fall back to substring matching — a generic
/// id like `default` would shadow any real model id containing it.
/// Must stay in sync with the `local/<this>` row in `model_catalog.json`.
pub const PLACEHOLDER_MODEL: &str = "local-default";

/// Default localhost base URLs of the common engines: Ollama (11434),
/// llama.cpp `llama-server` (8080), vLLM (8000), LM Studio (1234). Nothing
/// probes these automatically yet — doctor lists them in its "server not
/// answering" hint, and they are the natural order for a future auto-probe.
pub const CANDIDATE_BASE_URLS: &[&str] = &[
    "http://127.0.0.1:11434/v1",
    DEFAULT_BASE_URL,
    "http://127.0.0.1:8000/v1",
    "http://127.0.0.1:1234/v1",
];

/// Bounds for a believable context window read off a server response.
/// Anything below 1K is not a window a session could run in (and is more
/// likely a mis-keyed field); the upper bound guards against reading a
/// byte count or a typo'd value into the compaction budget, where an
/// absurd window would disable compaction entirely.
fn sane_context_window(value: u64) -> Option<u32> {
    (1_024..=16_777_216).contains(&value).then_some(value as u32)
}

/// Context window from a llama.cpp `GET /props` response body.
///
/// `/props` reports `default_generation_settings.n_ctx` — the context the
/// server was actually LAUNCHED with (`llama-server -c N`), which is the
/// number a session budget must respect. This is more authoritative than
/// any per-model metadata: a model trained for 256K but served with
/// `-c 32768` really does have 32K. `None` means the body is not the
/// `/props` shape or carries no plausible window.
pub fn parse_props_context_window(body: &str) -> Option<u32> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let n_ctx = value
        .get("default_generation_settings")?
        .get("n_ctx")?
        .as_u64()?;
    sane_context_window(n_ctx)
}

/// Context window from an OpenAI-compatible `GET /v1/models` response body.
///
/// The engines disagree on where they put it, so this checks every known
/// spelling on each model entry and takes the first plausible value:
/// llama.cpp exposes `meta.n_ctx` (runtime) and `meta.n_ctx_train`
/// (model's trained maximum), vLLM exposes `max_model_len`, LM Studio
/// exposes `max_context_length` / `loaded_context_length`. Runtime values
/// are preferred over trained maxima within an entry. `None` means no
/// entry carried a plausible window — callers should fall back to the
/// catalog, not error.
pub fn parse_models_context_window(body: &str) -> Option<u32> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let data = value.get("data")?.as_array()?;
    for model in data {
        let meta = model.get("meta");
        let candidates = [
            meta.and_then(|m| m.get("n_ctx")),
            model.get("loaded_context_length"),
            model.get("max_model_len"),
            model.get("context_length"),
            model.get("max_context_length"),
            meta.and_then(|m| m.get("n_ctx_train")),
        ];
        for candidate in candidates.into_iter().flatten() {
            if let Some(window) = candidate.as_u64().and_then(sane_context_window) {
                return Some(window);
            }
        }
    }
    None
}

/// Model ids from an OpenAI-compatible `GET /v1/models` response body.
///
/// `None` means the body is not the list-models shape at all (HTML from an
/// unrelated web app on the port, arbitrary JSON, garbage) — callers should
/// diagnose "this is not an OpenAI-compatible model server", not "no models
/// loaded". `Some(vec![])` means the server really answered the list shape
/// with zero models. Ids come back in server order.
pub fn parse_models_response(body: &str) -> Option<Vec<String>> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let data = value.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical OpenAI shape, as served by llama.cpp / Ollama / vLLM /
    /// LM Studio alike.
    #[test]
    fn should_extract_ids_when_body_is_openai_list_models_shape() {
        let body = r#"{"object":"list","data":[
            {"id":"llama3.2","object":"model","created":0,"owned_by":"library"},
            {"id":"qwen2.5-coder","object":"model","created":0,"owned_by":"library"}
        ]}"#;
        assert_eq!(
            parse_models_response(body).as_deref(),
            Some(&["llama3.2".to_string(), "qwen2.5-coder".to_string()][..])
        );
    }

    /// llama.cpp reports the loaded GGUF path as the id — pass it through
    /// verbatim, it is what the server will accept back.
    #[test]
    fn should_pass_through_gguf_path_ids() {
        let body = r#"{"data":[{"id":"/models/Qwen3-8B-Q4_K_M.gguf"}]}"#;
        assert_eq!(
            parse_models_response(body).as_deref(),
            Some(&["/models/Qwen3-8B-Q4_K_M.gguf".to_string()][..])
        );
    }

    /// A list-shaped body with zero models is Some(empty) — a real "nothing
    /// loaded" signal, distinct from not-a-model-server.
    #[test]
    fn should_return_some_empty_when_list_has_no_models() {
        assert_eq!(
            parse_models_response(r#"{"data":[]}"#).as_deref(),
            Some(&[][..])
        );
        // Entries without ids contribute nothing but the shape still counts.
        assert_eq!(
            parse_models_response(r#"{"data":[{"name":"no-id"}]}"#).as_deref(),
            Some(&[][..])
        );
    }

    /// Non-JSON and wrong-shape bodies are None — "not a model server", so
    /// callers don't tell the user to load a model into their web app.
    #[test]
    fn should_return_none_when_body_is_not_a_model_list() {
        assert_eq!(parse_models_response("<html>404</html>"), None);
        assert_eq!(parse_models_response(r#"{"models":["x"]}"#), None);
        assert_eq!(parse_models_response(r#"{"data":"nope"}"#), None);
    }

    /// llama.cpp `/props`: the launched `-c` value is the authoritative
    /// window.
    #[test]
    fn should_read_n_ctx_from_props_shape() {
        let body = r#"{"default_generation_settings":{"n_ctx":262144,"n_predict":-1},"total_slots":1}"#;
        assert_eq!(parse_props_context_window(body), Some(262_144));
    }

    /// Wrong shapes and implausible values fall through to None so callers
    /// keep the catalog fallback.
    #[test]
    fn should_reject_props_without_plausible_window() {
        assert_eq!(parse_props_context_window("<html>404</html>"), None);
        assert_eq!(parse_props_context_window(r#"{"n_ctx":262144}"#), None);
        assert_eq!(
            parse_props_context_window(r#"{"default_generation_settings":{"n_ctx":16}}"#),
            None
        );
    }

    /// llama.cpp `/v1/models` puts the runtime window in `meta.n_ctx`;
    /// prefer it over the trained maximum.
    #[test]
    fn should_prefer_runtime_n_ctx_over_trained_maximum() {
        let body = r#"{"data":[{"id":"qwen","meta":{"n_ctx":65536,"n_ctx_train":262144}}]}"#;
        assert_eq!(parse_models_context_window(body), Some(65_536));
        // Only the trained maximum present: better than nothing.
        let body = r#"{"data":[{"id":"qwen","meta":{"n_ctx_train":262144}}]}"#;
        assert_eq!(parse_models_context_window(body), Some(262_144));
    }

    /// vLLM (`max_model_len`) and LM Studio (`max_context_length`) spellings.
    #[test]
    fn should_read_vllm_and_lmstudio_spellings() {
        let body = r#"{"data":[{"id":"m","max_model_len":131072}]}"#;
        assert_eq!(parse_models_context_window(body), Some(131_072));
        let body = r#"{"data":[{"id":"m","max_context_length":32768,"loaded_context_length":8192}]}"#;
        assert_eq!(parse_models_context_window(body), Some(8_192));
    }

    /// No known field → None (catalog fallback), not a guess.
    #[test]
    fn should_return_none_when_models_carry_no_window() {
        assert_eq!(parse_models_context_window(r#"{"data":[{"id":"m"}]}"#), None);
        assert_eq!(parse_models_context_window(r#"{"data":[]}"#), None);
        assert_eq!(parse_models_context_window("<html></html>"), None);
    }

    /// The placeholder stays namespaced — a generic id would become a broad
    /// substring-collision key in the context/pricing catalogs.
    #[test]
    fn should_keep_placeholder_namespaced() {
        assert_ne!(PLACEHOLDER_MODEL, "default");
        assert!(PLACEHOLDER_MODEL.contains('-'));
    }
}
