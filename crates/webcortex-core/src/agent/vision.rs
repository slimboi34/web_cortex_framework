//! Images in and out of a run.
//!
//! WebCortex does not run vision models — that would be embedded inference.
//! It carries pixels to the model that does: an image given with the request,
//! or a frame a tool returned, becomes an image block in the conversation.
//!
//! A tool marks an image in its JSON result with a one-key object,
//! `{"$image": {"media_type": "image/png", "data": "<base64>"}}` or
//! `{"$image": {"url": "https://…"}}` — which is what `webcortex.Image`
//! serialises to. The runtime lifts each one out of the result, leaves a short
//! placeholder in its place, and attaches the image beside the text, so the
//! model sees the picture and the step record keeps only its size.
//!
//! Images are expensive context: every step replays the conversation, and a
//! camera tool called in a loop would resend every frame it ever took.
//! [`prune`] keeps the most recent few and turns the rest into a line of text.

use base64::Engine;
use serde_json::{Value, json};

/// The key that marks an image inside a tool result or a request.
pub const IMAGE_KEY: &str = "$image";

/// Media types every supported provider accepts.
pub const MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Decoded size ceiling for one image. Providers reject larger ones anyway;
/// refusing here gives a clearer error and keeps them out of session memory.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Images accepted with one request, and lifted out of one tool result.
pub const MAX_IMAGES: usize = 16;

/// Turn one image spec into a canonical (Anthropic-shaped) image block.
///
/// Accepts `{"media_type", "data"}`, `{"url"}`, or either wrapped in
/// `{"$image": …}`.
pub fn image_block(spec: &Value) -> Result<Value, String> {
    let spec = spec.get(IMAGE_KEY).unwrap_or(spec);
    if let Some(url) = spec.get("url").and_then(|u| u.as_str()) {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("image url must be http(s): {url:?}"));
        }
        return Ok(json!({"type": "image", "source": {"type": "url", "url": url}}));
    }
    let data = spec
        .get("data")
        .and_then(|d| d.as_str())
        .ok_or("an image needs base64 'data' with a 'media_type', or a 'url'")?;
    let media_type = spec
        .get("media_type")
        .and_then(|m| m.as_str())
        .ok_or("an image with 'data' needs a 'media_type'")?;
    if !MEDIA_TYPES.contains(&media_type) {
        return Err(format!(
            "unsupported image media_type {media_type:?}; use one of {}",
            MEDIA_TYPES.join(", ")
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("image data is not valid base64: {e}"))?;
    if bytes.is_empty() {
        return Err("image data is empty".into());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes; the limit is {MAX_IMAGE_BYTES}",
            bytes.len()
        ));
    }
    Ok(json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type, "data": data},
    }))
}

/// The `images` field of an agent request, as canonical blocks.
pub fn input_images(value: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(value) = value else { return Ok(Vec::new()) };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let items = value.as_array().ok_or("'images' must be a list")?;
    if items.len() > MAX_IMAGES {
        return Err(format!("at most {MAX_IMAGES} images per request ({} given)", items.len()));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, v)| image_block(v).map_err(|e| format!("images[{i}]: {e}")))
        .collect()
}

/// True for a `{"$image": …}` marker object.
fn is_marker(value: &Value) -> bool {
    value.as_object().is_some_and(|o| o.len() == 1 && o.contains_key(IMAGE_KEY))
}

/// Decoded size of a base64 string, without decoding it.
fn decoded_len(data: &str) -> usize {
    let pad = data.bytes().rev().take_while(|&b| b == b'=').count();
    (data.len() / 4 * 3).saturating_sub(pad)
}

/// A one-line description of an image block, for placeholders and transcripts.
pub fn describe(block: &Value) -> String {
    let source = block.get("source").cloned().unwrap_or(Value::Null);
    match source.get("type").and_then(|t| t.as_str()) {
        Some("url") => format!("image at {}", source.get("url").and_then(|u| u.as_str()).unwrap_or("?")),
        _ => format!(
            "{}, {} bytes",
            source.get("media_type").and_then(|m| m.as_str()).unwrap_or("image"),
            decoded_len(source.get("data").and_then(|d| d.as_str()).unwrap_or(""))
        ),
    }
}

/// Lift every image marker out of a tool result.
///
/// Returns the result with each marker replaced by a placeholder string, and
/// the images as canonical blocks in the order they were found. An invalid
/// marker, or one past `max`, becomes a placeholder saying why — a tool that
/// returns a bad frame should not fail the run.
pub fn extract(value: &Value, max: usize) -> (Value, Vec<Value>) {
    let mut images = Vec::new();
    let text = walk(value, &mut |marker| {
        if images.len() >= max {
            return format!("[image omitted: at most {max} images per tool result]");
        }
        match image_block(marker) {
            Ok(block) => {
                let line = format!("[image {}: {} — attached]", images.len() + 1, describe(&block));
                images.push(block);
                line
            }
            Err(e) => format!("[image omitted: {e}]"),
        }
    });
    (text, images)
}

/// The result as a step record or structured output keeps it: every marker's
/// payload replaced by its size, so megabytes of base64 do not land in the
/// response body, the audit log or session memory twice.
pub fn redact(value: &Value) -> Value {
    walk(value, &mut |marker| {
        let spec = marker.get(IMAGE_KEY).unwrap_or(&Value::Null);
        match spec.get("data").and_then(|d| d.as_str()) {
            Some(data) => json!({IMAGE_KEY: {
                "media_type": spec.get("media_type").cloned().unwrap_or(Value::Null),
                "bytes": decoded_len(data),
            }}),
            None => marker.clone(),
        }
    })
}

/// True when the value holds at least one image marker.
pub fn contains_images(value: &Value) -> bool {
    match value {
        v if is_marker(v) => true,
        Value::Array(items) => items.iter().any(contains_images),
        Value::Object(map) => map.values().any(contains_images),
        _ => false,
    }
}

fn walk<T: Into<Value>>(value: &Value, on_marker: &mut impl FnMut(&Value) -> T) -> Value {
    match value {
        v if is_marker(v) => on_marker(v).into(),
        Value::Array(items) => Value::Array(items.iter().map(|v| walk(v, on_marker)).collect()),
        Value::Object(map) => Value::Object(
            map.iter().map(|(k, v)| (k.clone(), walk(v, on_marker))).collect(),
        ),
        other => other.clone(),
    }
}

/// Keep only the `keep` most recent images in a conversation; older ones
/// become a line of text. Returns how many were dropped.
///
/// Message structure is untouched — a `tool_result` keeps its id and stays
/// paired with its `tool_use` — only image blocks are swapped for text.
pub fn prune(messages: &mut [crate::agent::Message], keep: usize) -> usize {
    let total: usize = messages.iter().map(|m| count(&m.content)).sum();
    if total <= keep {
        return 0;
    }
    let mut to_drop = total - keep;
    let dropped = to_drop;
    for m in messages.iter_mut() {
        if to_drop == 0 {
            break;
        }
        drop_oldest(&mut m.content, &mut to_drop);
    }
    dropped
}

/// Image blocks in one message's content, including inside tool results.
pub fn count(content: &Value) -> usize {
    match content {
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(|t| t.as_str()) {
                Some("image") => 1,
                Some("tool_result") => b.get("content").map(count).unwrap_or(0),
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

fn drop_oldest(content: &mut Value, to_drop: &mut usize) {
    let Value::Array(blocks) = content else { return };
    for b in blocks.iter_mut() {
        if *to_drop == 0 {
            return;
        }
        match b.get("type").and_then(|t| t.as_str()) {
            Some("image") => {
                *b = json!({"type": "text", "text": format!(
                    "[earlier image ({}) dropped from context to save tokens]",
                    describe(b)
                )});
                *to_drop -= 1;
            }
            Some("tool_result") => {
                if let Some(inner) = b.get_mut("content") {
                    drop_oldest(inner, to_drop);
                }
            }
            _ => {}
        }
    }
}

/// A canonical image block as an OpenAI `image_url` content part.
pub fn openai_part(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let url = match source.get("type").and_then(|t| t.as_str()) {
        Some("url") => source.get("url")?.as_str()?.to_string(),
        Some("base64") => format!(
            "data:{};base64,{}",
            source.get("media_type")?.as_str()?,
            source.get("data")?.as_str()?
        ),
        _ => return None,
    };
    Some(json!({"type": "image_url", "image_url": {"url": url}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;

    // A 1x1 transparent PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn marker() -> Value {
        json!({"$image": {"media_type": "image/png", "data": PNG}})
    }

    #[test]
    fn a_marker_becomes_a_canonical_block() {
        let block = image_block(&marker()).unwrap();
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/png");
    }

    #[test]
    fn bad_images_are_refused_with_a_reason() {
        assert!(image_block(&json!({"media_type": "image/bmp", "data": PNG})).unwrap_err().contains("unsupported"));
        assert!(image_block(&json!({"media_type": "image/png", "data": "!!"})).unwrap_err().contains("base64"));
        assert!(image_block(&json!({"url": "file:///etc/passwd"})).unwrap_err().contains("http"));
        assert!(image_block(&json!({"data": PNG})).unwrap_err().contains("media_type"));
    }

    #[test]
    fn urls_pass_through() {
        let block = image_block(&json!({"url": "https://example.com/a.jpg"})).unwrap();
        assert_eq!(block["source"]["url"], "https://example.com/a.jpg");
        assert_eq!(openai_part(&block).unwrap()["image_url"]["url"], "https://example.com/a.jpg");
    }

    #[test]
    fn extract_lifts_nested_markers_and_leaves_placeholders() {
        let result = json!({"frame": marker(), "objects": [{"label": "bolt", "crop": marker()}], "n": 1});
        let (text, images) = extract(&result, 8);
        assert_eq!(images.len(), 2);
        assert!(text["frame"].as_str().unwrap().starts_with("[image 1: image/png"));
        assert!(text["objects"][0]["crop"].as_str().unwrap().starts_with("[image 2:"));
        assert_eq!(text["n"], 1);
        assert!(!text.to_string().contains(PNG));
    }

    #[test]
    fn extract_respects_its_limit_and_survives_a_bad_marker() {
        let result = json!([marker(), {"$image": {"media_type": "image/png", "data": "%%"}}, marker(), marker()]);
        let (text, images) = extract(&result, 2);
        assert_eq!(images.len(), 2);
        assert!(text[1].as_str().unwrap().contains("not valid base64"));
        assert!(text[2].as_str().unwrap().starts_with("[image 2:"));
        assert!(text[3].as_str().unwrap().contains("at most 2"));
    }

    #[test]
    fn redact_keeps_the_size_not_the_pixels() {
        let r = redact(&json!({"frame": marker()}));
        assert_eq!(r["frame"]["$image"]["media_type"], "image/png");
        assert!(r["frame"]["$image"]["bytes"].as_u64().unwrap() > 0);
        assert!(r["frame"]["$image"].get("data").is_none());
        assert!(contains_images(&json!({"a": [marker()]})));
        assert!(!contains_images(&json!({"a": [1, "x"]})));
    }

    #[test]
    fn prune_drops_the_oldest_images_first() {
        let img = image_block(&marker()).unwrap();
        let mut messages = vec![
            Message { role: "user".into(), content: json!([img.clone(), {"type": "text", "text": "look"}]) },
            Message { role: "assistant".into(), content: json!([{"type": "tool_use", "id": "t1", "name": "cam", "input": {}}]) },
            Message { role: "user".into(), content: json!([{"type": "tool_result", "tool_use_id": "t1",
                "content": [{"type": "text", "text": "frame"}, img.clone()], "is_error": false}]) },
            Message { role: "user".into(), content: json!([img.clone()]) },
        ];
        assert_eq!(prune(&mut messages, 1), 2);
        assert_eq!(messages.iter().map(|m| count(&m.content)).sum::<usize>(), 1);
        assert!(messages[0].content[0]["text"].as_str().unwrap().contains("dropped from context"));
        assert_eq!(messages[2].content[0]["tool_use_id"], "t1", "pairing kept");
        assert_eq!(count(&messages[3].content), 1, "the newest survives");
        assert_eq!(prune(&mut messages, 1), 0);
    }

    #[test]
    fn input_images_are_validated_and_capped() {
        assert!(input_images(None).unwrap().is_empty());
        assert_eq!(input_images(Some(&json!([marker(), {"url": "https://x.test/a.png"}]))).unwrap().len(), 2);
        assert!(input_images(Some(&json!("nope"))).is_err());
        let many = Value::Array(vec![marker(); MAX_IMAGES + 1]);
        assert!(input_images(Some(&many)).is_err());
        assert!(input_images(Some(&json!([{"url": "ftp://x"}]))).unwrap_err().starts_with("images[0]"));
    }
}
