//! Discovery for local OpenAI-compatible model servers.
//!
//! Every supported local engine — llama.cpp's `llama-server`, Ollama, vLLM,
//! LM Studio — answers `GET {base_url}/models` with the OpenAI list-models
//! shape (`{"data": [{"id": "..."}]}`). That one endpoint is enough to (a)
//! verify a server is reachable and (b) learn the real model id(s) so the
//! user never has to type one. The HTTP call itself stays with the caller
//! (doctor uses its own credential-stripping blocking probe); this module owns
//! the engine-agnostic facts: where local servers usually listen and how to
//! read their answer.

/// Default localhost base URLs of the common engines, in probe order:
/// Ollama (11434), llama.cpp `llama-server` (8080), vLLM (8000),
/// LM Studio (1234).
pub const CANDIDATE_BASE_URLS: &[&str] = &[
    "http://127.0.0.1:11434/v1",
    "http://127.0.0.1:8080/v1",
    "http://127.0.0.1:8000/v1",
    "http://127.0.0.1:1234/v1",
];

/// Model ids from an OpenAI-compatible `GET /v1/models` response body.
///
/// Returns the ids in server order; an empty vec means the body did not carry
/// the expected shape (not an error — callers treat it as "answered, but not
/// a model list", which already distinguishes a live server from a dead port).
pub fn parse_models_response(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(data) = value.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
        .map(str::to_owned)
        .collect()
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
            parse_models_response(body),
            vec!["llama3.2".to_string(), "qwen2.5-coder".to_string()]
        );
    }

    /// llama.cpp reports the loaded GGUF path as the id — pass it through
    /// verbatim, it is what the server will accept back.
    #[test]
    fn should_pass_through_gguf_path_ids() {
        let body = r#"{"data":[{"id":"/models/Qwen3-8B-Q4_K_M.gguf"}]}"#;
        assert_eq!(
            parse_models_response(body),
            vec!["/models/Qwen3-8B-Q4_K_M.gguf".to_string()]
        );
    }

    /// Non-JSON, wrong-shape, and empty-list bodies all degrade to "no models"
    /// rather than erroring — reachability and shape are separate signals.
    #[test]
    fn should_return_empty_when_body_is_not_a_model_list() {
        assert!(parse_models_response("<html>404</html>").is_empty());
        assert!(parse_models_response(r#"{"models":["x"]}"#).is_empty());
        assert!(parse_models_response(r#"{"data":[]}"#).is_empty());
        assert!(parse_models_response(r#"{"data":[{"name":"no-id"}]}"#).is_empty());
    }
}
