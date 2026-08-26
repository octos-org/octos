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
    (1_024..=16_777_216)
        .contains(&value)
        .then_some(value as u32)
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

/// Context window from an OpenAI-compatible `GET /v1/models` response body,
/// for the entry that IS `model_id` — never another model's.
///
/// A multi-model server (LM Studio, llama.cpp router mode, a LiteLLM
/// proxy) lists every loaded model; taking "the first entry with a window"
/// would budget the session against whatever happens to be listed first —
/// an embedding model's 8K, say. Entry selection therefore mirrors how
/// these servers themselves resolve the request's `model` field:
///
/// 1. the entry whose id equals `model_id`;
/// 2. else a case-insensitive substring match in either direction (llama.cpp
///    reports GGUF paths as ids, so the configured short name is usually a
///    substring of the served id);
/// 3. else, when the list has exactly ONE entry or `model_id` is the
///    [`PLACEHOLDER_MODEL`], that sole/first entry — single-model servers
///    ignore the `model` field entirely;
/// 4. else `None`: guessing a window across models is worse than the
///    catalog fallback.
///
/// Within the chosen entry, only RUNTIME/allocated spellings are accepted:
/// llama.cpp `meta.n_ctx` (the launched `-c`), LM Studio
/// `loaded_context_length`, vLLM `max_model_len` (the serving limit).
/// Trained maxima (`meta.n_ctx_train`, `max_context_length`,
/// `context_length`) are deliberately NOT candidates: a model trained for
/// 256K but launched at 32K would be pinned as 256K — an over-estimate
/// that lets the transcript grow past the server's real window until
/// requests fail (#2135 review, P2). `None` means the chosen entry carried
/// no plausible runtime window — callers should fall back to the catalog,
/// not error.
pub fn parse_models_context_window(body: &str, model_id: &str) -> Option<u32> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let data = value.get("data")?.as_array()?;
    let window_of = |model: &serde_json::Value| -> Option<u32> {
        let meta = model.get("meta");
        let candidates = [
            meta.and_then(|m| m.get("n_ctx")),
            model.get("loaded_context_length"),
            model.get("max_model_len"),
        ];
        candidates
            .into_iter()
            .flatten()
            .find_map(|candidate| candidate.as_u64().and_then(sane_context_window))
    };
    let id_of = |model: &serde_json::Value| -> Option<String> {
        model
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::to_owned)
    };
    // Pass 1: exact id.
    if let Some(entry) = data.iter().find(|m| id_of(m).as_deref() == Some(model_id)) {
        return window_of(entry);
    }
    // Pass 2: substring either direction, case-insensitive (skipped for the
    // placeholder — it is not a real id and "default" would collide).
    if model_id != PLACEHOLDER_MODEL {
        let needle = model_id.to_lowercase();
        if let Some(entry) = data.iter().find(|m| {
            id_of(m).is_some_and(|id| {
                let id = id.to_lowercase();
                id.contains(&needle) || needle.contains(&id)
            })
        }) {
            return window_of(entry);
        }
    }
    // Pass 3: a single-model server ignores the `model` field, so its sole
    // entry is the one serving this session regardless of configured id;
    // the placeholder id means the config never named a model at all.
    if data.len() == 1 || model_id == PLACEHOLDER_MODEL {
        return data.first().and_then(window_of);
    }
    None
}

/// Context window from an Ollama-native `GET /api/ps` response body, for
/// the running model matching `model_id`.
///
/// Ollama serves no `/props`, and its OpenAI-compatible `/v1/models` list
/// carries no context length — the generic probe resolves to "no window"
/// on every Ollama deployment (#2135 review, P2). `/api/ps` lists the
/// RUNNING models with their allocated `context_length` (the num_ctx the
/// model is actually loaded with), which is exactly the number a session
/// budget must respect. Matching mirrors [`parse_models_context_window`]:
/// exact id (`name` or `model`, with and without the `:latest` suffix),
/// then substring, then the sole running model. `None` (older Ollama
/// without the field, model not loaded yet) falls back to the catalog.
pub fn parse_ollama_ps_context_window(body: &str, model_id: &str) -> Option<u32> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let models = value.get("models")?.as_array()?;
    let window_of = |entry: &serde_json::Value| -> Option<u32> {
        entry
            .get("context_length")
            .and_then(|v| v.as_u64())
            .and_then(sane_context_window)
    };
    let ids_of = |entry: &serde_json::Value| -> Vec<String> {
        ["name", "model"]
            .iter()
            .filter_map(|k| entry.get(*k).and_then(|v| v.as_str()))
            .map(str::to_owned)
            .collect()
    };
    let normalized = model_id.trim_end_matches(":latest").to_lowercase();
    // Pass 1: exact (suffix-insensitive) id.
    if let Some(entry) = models.iter().find(|m| {
        ids_of(m)
            .iter()
            .any(|id| id.trim_end_matches(":latest").to_lowercase() == normalized)
    }) {
        return window_of(entry);
    }
    // Pass 2: substring either direction.
    if model_id != PLACEHOLDER_MODEL {
        if let Some(entry) = models.iter().find(|m| {
            ids_of(m).iter().any(|id| {
                let id = id.to_lowercase();
                id.contains(&normalized) || normalized.contains(&id)
            })
        }) {
            return window_of(entry);
        }
    }
    // Pass 3: exactly one running model serves whatever was configured.
    if models.len() == 1 || model_id == PLACEHOLDER_MODEL {
        return models.first().and_then(window_of);
    }
    None
}

/// Runtime-configured context from an Ollama-native `POST /api/show`
/// response body: the Modelfile `parameters` text block, line
/// `num_ctx <n>`.
///
/// This is the COLD-model fallback (#2135 re-review, P2): before a model
/// is loaded, `/api/ps` lists nothing, but `/api/show` answers from the
/// registry — and `num_ctx` there is a runtime CONFIGURATION (what the
/// model will be loaded with), not a trained maximum, so it is safe to
/// pin. The trained `context_length` under `model_info` is deliberately
/// ignored. Models whose Modelfile sets no num_ctx yield `None` — the
/// catalog stands for the first turn and `/api/ps` corrects on the next
/// probe once the model is running.
pub fn parse_ollama_show_num_ctx(body: &str) -> Option<u32> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let parameters = value.get("parameters")?.as_str()?;
    for line in parameters.lines() {
        let mut tokens = line.split_whitespace();
        if tokens.next() == Some("num_ctx") {
            if let Some(window) = tokens
                .next()
                .and_then(|raw| raw.parse::<u64>().ok())
                .and_then(sane_context_window)
            {
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
        let body =
            r#"{"default_generation_settings":{"n_ctx":262144,"n_predict":-1},"total_slots":1}"#;
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
        assert_eq!(parse_models_context_window(body, "qwen"), Some(65_536));
        // Trained maxima are NOT runtime capacity: a model trained for 256K
        // but launched smaller must not be pinned at 256K (#2135 review P2).
        let body = r#"{"data":[{"id":"qwen","meta":{"n_ctx_train":262144}}]}"#;
        assert_eq!(parse_models_context_window(body, "qwen"), None);
    }

    /// vLLM (`max_model_len`) and LM Studio (`max_context_length`) spellings.
    #[test]
    fn should_read_vllm_and_lmstudio_spellings() {
        let body = r#"{"data":[{"id":"m","max_model_len":131072}]}"#;
        assert_eq!(parse_models_context_window(body, "m"), Some(131_072));
        let body =
            r#"{"data":[{"id":"m","max_context_length":32768,"loaded_context_length":8192}]}"#;
        assert_eq!(parse_models_context_window(body, "m"), Some(8_192));
        // The trained-maximum spelling alone is not accepted (#2135 P2).
        let body = r#"{"data":[{"id":"m","max_context_length":32768}]}"#;
        assert_eq!(parse_models_context_window(body, "m"), None);
    }

    /// No known field → None (catalog fallback), not a guess.
    #[test]
    fn should_return_none_when_models_carry_no_window() {
        assert_eq!(
            parse_models_context_window(r#"{"data":[{"id":"m"}]}"#, "m"),
            None
        );
        assert_eq!(parse_models_context_window(r#"{"data":[]}"#, "m"), None);
        assert_eq!(parse_models_context_window("<html></html>", "m"), None);
    }

    /// Multi-model servers: the window must come from the CONFIGURED
    /// model's entry, never whichever model is listed first. Exact id wins;
    /// substring matches llama.cpp's GGUF-path ids; the placeholder (or a
    /// single-entry list) falls back to the sole/first entry; an unmatched
    /// id on a multi-model list yields None rather than a guess.
    #[test]
    fn should_match_configured_model_in_multi_model_list() {
        let body = r#"{"data":[
            {"id":"small-embed","meta":{"n_ctx":8192}},
            {"id":"/models/Qwen3-Coder-Q8.gguf","meta":{"n_ctx":131072}}
        ]}"#;
        assert_eq!(
            parse_models_context_window(body, "small-embed"),
            Some(8_192)
        );
        // Substring, either direction, case-insensitive.
        assert_eq!(
            parse_models_context_window(body, "qwen3-coder-q8"),
            Some(131_072)
        );
        // Placeholder: first entry (single-model-server assumption).
        assert_eq!(
            parse_models_context_window(body, PLACEHOLDER_MODEL),
            Some(8_192)
        );
        // Unmatched real id against a multi-model list: no guess.
        assert_eq!(parse_models_context_window(body, "unrelated-model"), None);
        // Single-entry list serves whatever was configured.
        let single = r#"{"data":[{"id":"whatever","meta":{"n_ctx":65536}}]}"#;
        assert_eq!(
            parse_models_context_window(single, "my-alias"),
            Some(65_536)
        );
    }

    /// The placeholder stays namespaced — a generic id would become a broad
    /// substring-collision key in the context/pricing catalogs.
    #[test]
    fn should_keep_placeholder_namespaced() {
        assert_ne!(PLACEHOLDER_MODEL, "default");
        assert!(PLACEHOLDER_MODEL.contains('-'));
    }

    /// Ollama `/api/ps`: allocated context of the RUNNING model, matched
    /// suffix-insensitively (`qwen3:latest` vs `qwen3`); a missing
    /// `context_length` (older Ollama) or an empty list yields None so the
    /// catalog stands and the probe retries once the model loads.
    #[test]
    fn should_read_allocated_context_from_ollama_ps() {
        let body = r#"{"models":[
            {"name":"embed:latest","model":"embed:latest","context_length":8192},
            {"name":"qwen3:latest","model":"qwen3:latest","context_length":131072}
        ]}"#;
        assert_eq!(parse_ollama_ps_context_window(body, "qwen3"), Some(131_072));
        assert_eq!(parse_ollama_ps_context_window(body, "embed"), Some(8_192));
        // Sole running model serves whatever id was configured.
        let sole = r#"{"models":[{"name":"anything","context_length":65536}]}"#;
        assert_eq!(
            parse_ollama_ps_context_window(sole, "my-alias"),
            Some(65_536)
        );
        // Older Ollama without the field / nothing running: catalog stands.
        let old = r#"{"models":[{"name":"qwen3:latest","size":123}]}"#;
        assert_eq!(parse_ollama_ps_context_window(old, "qwen3"), None);
        assert_eq!(
            parse_ollama_ps_context_window(r#"{"models":[]}"#, "qwen3"),
            None
        );
        assert_eq!(
            parse_ollama_ps_context_window("<html></html>", "qwen3"),
            None
        );
    }

    /// Ollama `/api/show`: the Modelfile-configured `num_ctx` is a runtime
    /// setting and may be pinned; trained `context_length` under model_info
    /// is ignored; no num_ctx line yields None.
    #[test]
    fn should_read_configured_num_ctx_from_ollama_show() {
        let body = r#"{"parameters":"num_ctx                        32768\nstop                           \"<|im_end|>\"","model_info":{"qwen3.context_length":262144}}"#;
        assert_eq!(parse_ollama_show_num_ctx(body), Some(32_768));
        let no_ctx =
            r#"{"parameters":"stop \"<|im_end|>\"","model_info":{"qwen3.context_length":262144}}"#;
        assert_eq!(parse_ollama_show_num_ctx(no_ctx), None);
        assert_eq!(parse_ollama_show_num_ctx("{}"), None);
    }
}
