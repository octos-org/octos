//! Compare redacted provider cache-input manifests captured from a soak run.
//!
//! Input may be legacy JSONL with one `PromptCacheInputManifest` per line or
//! the runtime observer JSONL selected by `OCTOS_PROMPT_CACHE_MANIFEST_JSONL`.
//! Runtime comparisons come from the observer's per-stream predecessor, even
//! across interleaved requests and daemon restarts. Usage rows are skipped.
//! Only legacy raw manifests use adjacent-row comparison. The output contains
//! hashes, epoch identifiers, counts, and byte estimates; prompt bodies are neither
//! accepted by the schema nor emitted.

use std::{env, fs, process};

use octos_llm::{PromptCacheInputManifest, PromptCacheObservation};
use serde_json::{Value, json};

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: prompt_cache_manifest_diff <manifests.jsonl>");
        process::exit(2);
    };
    let input = fs::read_to_string(&path).unwrap_or_else(|error| {
        eprintln!("failed to read {path}: {error}");
        process::exit(2);
    });
    let comparisons = compare_jsonl(&input).unwrap_or_else(|error| {
        eprintln!("{error}");
        process::exit(2);
    });
    for comparison in comparisons {
        println!("{comparison}");
    }
}

fn compare_jsonl(input: &str) -> Result<Vec<Value>, String> {
    let mut comparisons = Vec::new();
    let mut legacy_previous: Option<(usize, PromptCacheInputManifest)> = None;
    for (index, line) in input
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        // Serde type errors can quote malformed input values. Report the row
        // only so a corrupted/raw prompt field cannot leak through stderr.
        let invalid = |_| format!("invalid manifest at line {}", index + 1);
        let value: Value = serde_json::from_str(line).map_err(invalid)?;
        if let Some(kind) = value.get("event_kind") {
            legacy_previous = None;
            match kind.as_str() {
                Some("usage" | "usage_unmatched") => continue,
                Some("manifest") => {}
                _ => return Err(format!("invalid event_kind at line {}", index + 1)),
            }
            let observation: PromptCacheObservation =
                serde_json::from_value(value).map_err(invalid)?;
            // The observer already computed this against its own lane's
            // previous_sequence. Sequence numbers restart with the daemon:
            // never resolve them globally or infer a predecessor from line order.
            if let (Some(previous), Some(comparison)) =
                (observation.previous_sequence, observation.comparison)
            {
                comparisons.push(json!({
                    "from_sequence": previous,
                    "to_sequence": observation.sequence,
                    "to_index": index,
                    "to_input_hash": observation.manifest.input_hash,
                    "to_epoch_id": observation.manifest.epoch_id,
                    "session_hash": observation.session_hash,
                    "affinity_hash": observation.affinity_hash,
                    "request_key_hash": observation.request_key_hash,
                    "provider": observation.manifest.provider,
                    "model": observation.manifest.model,
                    "comparison": comparison,
                }));
            }
        } else {
            let next: PromptCacheInputManifest = serde_json::from_value(value).map_err(invalid)?;
            if let Some((previous_index, previous)) = legacy_previous.as_ref() {
                comparisons.push(json!({
                    "from_index": previous_index, "to_index": index,
                    "from_input_hash": previous.input_hash, "to_input_hash": next.input_hash,
                    "from_epoch_id": previous.epoch_id, "to_epoch_id": next.epoch_id,
                    "comparison": previous.compare_prefix(&next),
                }));
            }
            legacy_previous = Some((index, next));
        }
    }
    Ok(comparisons)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(hash: &str) -> Value {
        json!({"schema": "octos.provider-cache-input-manifest.v1", "provider": "test", "model": "test",
            "epoch_id": hash, "stable_prefix_hash": hash, "conversation_hash": hash, "input_hash": hash,
            "stable_segments": [], "conversation_segments": [], "normalized_bytes": 0})
    }

    fn observation(hash: &str, seq: u64, previous: Option<u64>) -> Value {
        let mut value = manifest(hash);
        value.as_object_mut().unwrap().extend(json!({
            "observation_schema": "octos.provider-cache-observation.v1", "event_kind": "manifest",
            "sequence": seq, "previous_sequence": previous, "request_key_hash": hash,
            "observed_at_unix_ms": seq, "session_hash": hash, "affinity_hash": hash,
            "relation": if previous.is_some() {"append_only"} else {"initialized"},
            "comparison": previous.map(|_| json!({"compatible_route": true, "stable_prefix_matches": true,
                "conversation_prefix_segments": 2, "reusable_normalized_bytes": 42, "invalidation_reason": null}))
        }).as_object().unwrap().clone());
        value
    }

    #[test]
    fn should_use_observer_predecessors_across_interleaved_streams_and_restarts() {
        let rows = [
            observation("a", 1, None),
            observation("b", 2, None),
            observation("a", 3, Some(1)),
            observation("b", 4, Some(2)),
            observation("new-process", 1, None),
            observation("new-process", 2, Some(1)),
        ];
        let input = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let comparisons = compare_jsonl(&input).unwrap();
        assert_eq!(comparisons.len(), 3);
        assert_eq!(comparisons[0]["from_sequence"], 1);
        assert_eq!(comparisons[1]["from_sequence"], 2);
        assert_eq!(comparisons[2]["to_sequence"], 2);
        for row in comparisons {
            assert_eq!(row["comparison"]["reusable_normalized_bytes"], 42);
        }
    }

    #[test]
    fn should_skip_all_usage_rows_and_accept_legacy_manifests() {
        let mut usage = observation("usage", 5, None);
        usage["event_kind"] = json!("usage_unmatched");
        let input = [
            manifest("a"),
            manifest("b"),
            usage,
            json!({"event_kind": "usage"}),
            observation("a", 3, Some(1)),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        let comparisons = compare_jsonl(&input).unwrap();
        assert_eq!(comparisons.len(), 2);
        assert_eq!(comparisons[0]["from_input_hash"], "a");
        assert_eq!(comparisons[0]["to_input_hash"], "b");
        assert_eq!(comparisons[1]["comparison"]["stable_prefix_matches"], true);
    }

    #[test]
    fn should_report_bad_input_line_without_emitting_prompt_data() {
        assert!(compare_jsonl("\nnot-json").unwrap_err().contains("line 2"));
        assert!(compare_jsonl("{\"event_kind\":\"unexpected\"}").is_err());
        let mut malformed = observation("a", 1, None);
        malformed["sequence"] = json!("private-prompt-marker");
        assert_eq!(
            compare_jsonl(&malformed.to_string()).unwrap_err(),
            "invalid manifest at line 1"
        );
    }
}
