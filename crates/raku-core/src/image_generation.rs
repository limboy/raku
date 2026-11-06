//! Native Codex image-generation items. Called only by provider/history workers;
//! filesystem access and base64 never run on a desktop frame.

use std::io::Read as _;

use base64::Engine as _;
use serde_json::Value;

use crate::model::{ActivityItem, ActivityKind};

const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Publish small durable references instead of megabytes of base64 to clients.
/// Runs on the daemon's event/history worker before serialization and replay.
pub(crate) fn externalize(activity: &mut ActivityItem, blobs: &crate::blob_store::BlobStore) {
    if activity.kind == ActivityKind::ImageGeneration {
        for url in &mut activity.image_urls {
            *url = blobs.store_data_url(url);
        }
    }
}

pub(crate) fn codex_activity(item: &Value, complete: bool) -> ActivityItem {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let failed = matches!(status, "failed" | "cancelled" | "canceled" | "interrupted")
        || item
            .get("failure")
            .is_some_and(|failure| !failure.is_null());
    let title = tr!(if failed {
        "activity.image_generation_failed"
    } else if complete {
        "activity.generated_image"
    } else {
        "activity.generating_image"
    });
    let image = (complete && !failed).then(|| image_url(item)).flatten();
    let detail =
        if item.pointer("/failure/type").and_then(Value::as_str) == Some("usageLimitExceeded") {
            Some(tr!("activity.image_generation_limit"))
        } else if failed {
            Some(tr!("activity.image_generation_failed"))
        } else if complete && image.is_none() {
            Some(tr!("attachments.preview_unavailable"))
        } else {
            None
        };
    ActivityItem::new(
        item.get("id").and_then(Value::as_str).map(str::to_owned),
        ActivityKind::ImageGeneration,
        title,
        detail,
        complete,
    )
    .with_tool_name(Some("image_generation"))
    .with_arguments(
        item.get("revisedPrompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
    .with_output(
        item.get("savedPath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(str::to_owned),
    )
    .with_image_urls(image.into_iter().collect())
    .with_failed(failed)
}

fn image_url(item: &Value) -> Option<String> {
    // Current app-server versions may replace `result` with `fileId`. Its
    // savedPath is the public local output; copy it before a provider cleanup
    // so Raku's existing blob persistence owns the displayed image.
    if let Some(path) = item
        .get("savedPath")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        && let Ok(file) = std::fs::File::open(path)
    {
        let mut bytes = Vec::new();
        if file
            .take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_ok()
            && !bytes.is_empty()
            && bytes.len() as u64 <= MAX_IMAGE_BYTES
        {
            if let Some(url) = data_url(&bytes) {
                return Some(url);
            }
        }
    }
    let result = item
        .get("result")
        .and_then(Value::as_str)
        .filter(|data| !data.is_empty())?;
    if result.len() as u64 > MAX_IMAGE_BYTES * 4 / 3 + 128 {
        return None;
    }
    let encoded = if result.starts_with("data:image/") {
        let (header, data) = result.split_once(',')?;
        header.ends_with(";base64").then_some(data)?
    } else {
        result
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    (bytes.len() as u64 <= MAX_IMAGE_BYTES)
        .then(|| data_url(&bytes))
        .flatten()
}

fn data_url(bytes: &[u8]) -> Option<String> {
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else {
        return None;
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_generation_is_externalized_before_transport_and_remains_readable() {
        let root =
            std::env::temp_dir().join(format!("raku-generated-blob-{}", uuid::Uuid::new_v4()));
        let blobs = crate::blob_store::BlobStore::new(root.clone());
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(16 * 1024, 1);
        let mut item = ActivityItem::new(None, ActivityKind::ImageGeneration, "Image", None, true)
            .with_image_urls(vec![data_url(&bytes).unwrap()]);
        externalize(&mut item, &blobs);
        let reference = &item.image_urls[0];
        assert!(crate::blob_store::is_blob_reference(reference));
        assert_eq!(
            std::fs::read(blobs.path_for(reference).unwrap()).unwrap(),
            bytes
        );
        assert!(serde_json::to_string(&item).unwrap().len() < 1024);
        externalize(&mut item, &blobs);
        assert!(crate::blob_store::is_blob_reference(&item.image_urls[0]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_result_is_an_image_and_never_activity_text() {
        let item = codex_activity(
            &json!({
                "id":"image-1", "type":"imageGeneration", "status":"completed",
                "result":"iVBORw0KGgo=", "revisedPrompt":"A red circle"
            }),
            true,
        );
        assert_eq!(item.kind, ActivityKind::ImageGeneration);
        assert!(item.is_standalone());
        assert_eq!(item.image_urls, ["data:image/png;base64,iVBORw0KGgo="]);
        assert!(item.output.is_none());
        assert!(!item.failed);
    }

    #[test]
    fn durable_file_identity_reads_saved_output_and_survives_file_removal() {
        let path = std::env::temp_dir().join(format!("raku-image-{}.webp", uuid::Uuid::new_v4()));
        let bytes = b"RIFF0000WEBPtest";
        std::fs::write(&path, bytes).unwrap();
        let item = codex_activity(
            &json!({
                "id":"image-file", "type":"imageGeneration", "status":"completed",
                "fileId":"file-1", "savedPath":path, "result":""
            }),
            true,
        );
        std::fs::remove_file(path).unwrap();
        assert_eq!(item.image_urls, [data_url(bytes).unwrap()]);
        assert!(item.image_urls[0].starts_with("data:image/webp;base64,"));
    }

    #[test]
    fn progress_failure_and_missing_output_do_not_produce_broken_images() {
        let progress = codex_activity(
            &json!({"id":"image-1", "status":"in_progress", "result":"iVBORw0KGgo="}),
            false,
        );
        assert!(!progress.complete);
        assert!(progress.image_urls.is_empty());
        let failed = codex_activity(
            &json!({
                "id":"image-1", "status":"completed", "result":"iVBORw0KGgo=",
                "failure":{"type":"usageLimitExceeded", "limitId":"images", "resetsAt":null}
            }),
            true,
        );
        assert!(failed.failed);
        assert!(failed.image_urls.is_empty());
        assert!(failed.detail.is_some());
        let missing = codex_activity(
            &json!({"id":"image-1", "status":"completed", "result":""}),
            true,
        );
        assert!(!missing.failed);
        assert!(missing.detail.is_some());
        let invalid = codex_activity(
            &json!({"id":"image-1", "status":"completed", "result":"aGVsbG8="}),
            true,
        );
        assert!(invalid.image_urls.is_empty());
        assert!(invalid.detail.is_some());
    }
}
