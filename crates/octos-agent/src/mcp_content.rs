//! Preserve MCP content through the native tool-result boundary.
use crate::tools::ToolResult;
use base64::Engine;
use rmcp::model::{CallToolResult, RawContent, ResourceContents};
use serde_json::json;
use std::path::Path;

pub(super) fn render(result: CallToolResult, workspace: &Path) -> ToolResult {
    let mut converted = ToolResult {
        success: !result.is_error.unwrap_or(false),
        ..Default::default()
    };
    let mut parts = Vec::new();
    let mut files = Vec::new();
    let mut remaining = octos_llm::vision::MAX_MEDIA_BYTES as usize;
    for item in &result.content {
        match &item.raw {
            RawContent::Text(text) => parts.push(text.text.clone()),
            RawContent::Image(image) => attach_binary(
                &image.data,
                &image.mime_type,
                "MCP image",
                workspace,
                &mut remaining,
                &mut converted,
                &mut parts,
                &mut files,
            ),
            RawContent::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    parts.push(format!("[Resource: {uri}]\n{text}"))
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => attach_binary(
                    blob,
                    mime_type.as_deref().unwrap_or("application/octet-stream"),
                    uri,
                    workspace,
                    &mut remaining,
                    &mut converted,
                    &mut parts,
                    &mut files,
                ),
            },
            RawContent::ResourceLink(link) => parts.push(format!(
                "[Resource link: {}] {}{}",
                link.name,
                link.uri,
                link.description
                    .as_ref()
                    .map(|d| format!("\n{d}"))
                    .unwrap_or_default()
            )),
            RawContent::Audio(audio) => attach_binary(
                &audio.data,
                &audio.mime_type,
                "MCP audio",
                workspace,
                &mut remaining,
                &mut converted,
                &mut parts,
                &mut files,
            ),
        }
    }
    if let Some(value) = &result.structured_content {
        parts.push(format!("[Structured result]\n{value}"));
    }
    converted.output = parts.join("\n");
    converted.structured_metadata =
        Some(json!({"structuredContent":result.structured_content,"resource_files":files}));
    converted
}

#[allow(clippy::too_many_arguments)]
fn attach_binary(
    encoded: &str,
    mime: &str,
    label: &str,
    workspace: &Path,
    remaining: &mut usize,
    result: &mut ToolResult,
    parts: &mut Vec<String>,
    files: &mut Vec<serde_json::Value>,
) {
    let decoded = (|| -> eyre::Result<Vec<u8>> {
        eyre::ensure!(
            encoded.len() <= remaining.saturating_add(2) / 3 * 4,
            "binary attachment exceeds remaining size budget"
        );
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        eyre::ensure!(
            bytes.len() <= *remaining,
            "binary attachment exceeds remaining size budget"
        );
        *remaining -= bytes.len();
        Ok(bytes)
    })();
    let outcome = decoded.and_then(|bytes| {
        if mime.starts_with("image/") {
            eyre::ensure!(
                result.model_media.len() < 16,
                "too many images in one tool result"
            );
            let path = octos_llm::vision::snapshot_image_bytes(&bytes)?;
            result.model_media.push(path);
            parts.push(format!("[Image attached: {label} ({mime})]"));
        } else {
            let capture = crate::tools::command_output::CommandOutput::new(workspace)?;
            capture.append(&bytes);
            capture.finish();
            let saved = capture.view(false);
            eyre::ensure!(
                saved.error.is_none(),
                "failed to save binary resource: {:?}",
                saved.error
            );
            files.push(json!({"uri":label,"mime_type":mime,"path":saved.path,"bytes":bytes.len()}));
            parts.push(format!(
                "[Binary resource: {label} ({mime}), {} bytes; saved to {}]",
                bytes.len(),
                saved.path.display()
            ));
        }
        Ok(())
    });
    if let Err(error) = outcome {
        parts.push(format!("[Attachment {label} not shown: {error}]"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn should_preserve_images_resources_and_structured_content() {
        let dir = tempfile::tempdir().unwrap();
        let pixels = b"\x89PNG\r\n\x1a\nMCP pixels";
        let result: CallToolResult = serde_json::from_value(json!({
            "content":[
                {"type":"text","text":"screenshot follows"},
                {"type":"image","mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(pixels)},
                {"type":"resource","resource":{"uri":"doc://readme","mimeType":"text/plain","text":"resource text"}},
                {"type":"resource_link","uri":"doc://other","name":"another document"},
                {"type":"resource","resource":{"uri":"doc://report.bin","mimeType":"application/octet-stream","blob":"YWJj"}}
            ],"structuredContent":{"count":3},"isError":false
        })).unwrap();
        let converted = render(result, dir.path());
        assert!(converted.success);
        assert_eq!(converted.model_media.len(), 1);
        assert_eq!(
            octos_llm::vision::encode_image(&converted.model_media[0].to_string_lossy(), None)
                .unwrap()
                .1,
            base64::engine::general_purpose::STANDARD.encode(pixels)
        );
        assert!(converted.output.contains("resource text"));
        assert!(converted.output.contains("doc://other"));
        assert!(converted.output.contains("count"));
        assert_eq!(
            converted.structured_metadata.as_ref().unwrap()["structuredContent"]["count"],
            3
        );
        let path = converted.structured_metadata.as_ref().unwrap()["resource_files"][0]["path"]
            .as_str()
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abc");
    }

    #[test]
    fn should_report_invalid_image_and_preserve_mcp_failure() {
        let dir = tempfile::tempdir().unwrap();
        let result: CallToolResult = serde_json::from_value(json!({"content":[
            {"type":"image","mimeType":"image/png","data":"not base64"},
            {"type":"text","text":"partial result"}],"isError":true}))
        .unwrap();
        let converted = render(result, dir.path());
        assert!(!converted.success);
        assert!(converted.model_media.is_empty());
        assert!(converted.output.contains("not shown"));
        assert!(converted.output.contains("partial result"));
    }
}
