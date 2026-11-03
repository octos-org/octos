//! Batching for one read-only capability, not a cross-tool dispatcher.
//! Each item uses the original filesystem/network validation.

use eyre::{Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolResult};

pub(super) fn schema(mut single: Value, field: &str) -> Value {
    let mut item = single.clone();
    // Parameter documentation already appears in the single-request form.
    // Repeating it inside every batch schema cancels out the prompt savings.
    // Keep types and validation constraints; do not use provider-sensitive refs.
    strip_schema_descriptions(&mut item);
    single["properties"][field] = json!({
        "type": "array", "minItems": 1, "maxItems": 32,
        "description": "Independent requests in one call. Use this field alone; each item uses the single-request parameters. Partial failures are reported separately.",
        "items": item
    });
    // Avoid oneOf: some supported providers reject it. Runtime checks enforce
    // exclusive single/batch arguments; the item schema remains fully typed.
    if let Some(object) = single.as_object_mut() {
        object.remove("required");
    }
    single
}

fn strip_schema_descriptions(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    object.remove("description");
    // Walk schema positions only: a property *named* description is an
    // argument and must never be removed.
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            children.values_mut().for_each(strip_schema_descriptions);
        }
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(child) = object.get_mut(key) {
            strip_schema_descriptions(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            children.iter_mut().for_each(strip_schema_descriptions);
        }
    }
}

pub(super) async fn execute(
    tool: &dyn Tool,
    ctx: &ToolContext,
    args: &Value,
    field: &str,
) -> Result<ToolResult> {
    let items = args[field]
        .as_array()
        .ok_or_else(|| eyre::eyre!("{field} must be an array"))?;
    if args.as_object().is_none_or(|object| object.len() != 1) {
        bail!("Use either single-request arguments or {field}, not both");
    }
    if items.is_empty() || items.len() > 32 {
        bail!("{field} must contain 1 to 32 requests; split larger batches");
    }
    if items
        .iter()
        .any(|item| !item.is_object() || item.get(field).is_some())
    {
        bail!(
            "Each {field} item must be a single-request object; nested batches are not supported"
        );
    }

    // A batch output cap can hide part of a read. Do not teach the cache that
    // the model saw complete files; later focused reads must return the text.
    let mut child_ctx = ctx.clone();
    child_ctx.file_state_cache = None;
    let cap = octos_core::tool_output_limit(tool.name());
    let item_budget = (cap / items.len()).saturating_sub(256);
    let mut output = String::new();
    let mut media = Vec::new();
    let mut records = Vec::new();
    let mut success = true;
    // Bounded I/O concurrency is independent of the number of agent workers.
    // `buffered` preserves request order and drops outstanding reads on cancel.
    let mut pending: Vec<futures::future::BoxFuture<'_, _>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let child_ctx = &child_ctx;
        pending.push(Box::pin(async move {
            (
                index,
                item,
                tool.execute_with_context(child_ctx, item).await,
            )
        }));
    }
    let mut reads = futures::stream::iter(pending).buffered(4);
    while let Some((index, item, result)) = reads.next().await {
        let mut result = match result {
            Ok(result) => result,
            Err(error) => ToolResult {
                output: error.to_string(),
                success: false,
                ..Default::default()
            },
        };
        success &= result.success;
        let truncated = result.output.len() > item_budget;
        if truncated {
            octos_core::truncate_utf8(&mut result.output, item_budget.saturating_sub(256), "");
            result.output.push_str("\n[Batch item truncated. Repeat this item alone for its single-request result; use offset/limit to page files.]");
        }
        let mut label = item
            .get("path")
            .or_else(|| item.get("filePath"))
            .or_else(|| item.get("url"))
            .or_else(|| item.get("query"))
            .map(Value::to_string)
            .unwrap_or_default();
        octos_core::truncate_utf8(&mut label, 160, "...");
        output.push_str(&format!(
            "\nRequest {} {label}: {}\n{}\n",
            index + 1,
            if result.success { "ok" } else { "failed" },
            result.output
        ));
        media.extend(result.model_media);
        records.push(json!({"index": index, "request": item, "success": result.success, "truncated": truncated, "metadata": result.structured_metadata}));
    }
    Ok(ToolResult {
        output,
        success,
        model_media: media,
        structured_metadata: Some(json!({"reads": records})),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn batch_schema_keeps_validation_and_description_named_arguments() {
        let original = json!({"type":"object", "required":["description"],
            "properties":{"description":{"type":"string", "description":"Parameter help", "minLength":2}}
        });
        let batched = schema(original, "requests");
        let item = &batched["properties"]["requests"]["items"];
        assert_eq!(item["required"], json!(["description"]));
        assert_eq!(item["properties"]["description"]["type"], "string");
        assert_eq!(item["properties"]["description"]["minLength"], 2);
        assert!(
            item["properties"]["description"]
                .get("description")
                .is_none()
        );
    }

    struct PendingRead(AtomicUsize);
    struct Active<'a>(&'a AtomicUsize);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl Tool for PendingRead {
        fn name(&self) -> &str {
            "pending_read"
        }
        fn description(&self) -> &str {
            "Test cancellable read"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, _: &Value) -> Result<ToolResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let _active = Active(&self.0);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancelling_batch_drops_every_in_flight_read() {
        let tool = PendingRead(AtomicUsize::new(0));
        let ctx = ToolContext::zero();
        let args = json!({"requests":[{}, {}, {}, {}, {}]});
        let mut batch = Box::pin(execute(&tool, &ctx, &args, "requests"));
        assert!(futures::poll!(&mut batch).is_pending());
        assert_eq!(tool.0.load(Ordering::SeqCst), 4);
        drop(batch);
        assert_eq!(tool.0.load(Ordering::SeqCst), 0);
    }
}
