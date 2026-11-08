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
