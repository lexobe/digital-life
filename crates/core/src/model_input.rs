//! Separate transport size from the pre-dispatch hold for a desktop screenshot.
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

const JPEG_PREFIX: &str = "data:image/jpeg;base64,";
pub const MAX_FRAME_BYTES: usize = 128 * 1024;
/// An authorization estimate. Settlement always uses the provider's reported tokens.
const DESKTOP_FRAME_ALLOWANCE: u64 = 4096;

fn desktop_jpeg_payload(part: &Value, responses: bool) -> Option<usize> {
    let url = if responses && part["type"] == "input_image" {
        part["image_url"].as_str()?
    } else if !responses && part["type"] == "image_url" {
        part["image_url"]["url"].as_str()?
    } else {
        return None;
    };
    let encoded = url.strip_prefix(JPEG_PREFIX)?;
    if encoded.len() > MAX_FRAME_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let jpeg = STANDARD.decode(encoded).ok()?;
    is_desktop_jpeg(&jpeg).then_some(encoded.len())
}

pub fn is_desktop_jpeg(jpeg: &[u8]) -> bool {
    jpeg.len() <= MAX_FRAME_BYTES
        && jpeg.starts_with(&[0xff, 0xd8])
        && jpeg.ends_with(&[0xff, 0xd9])
        && imagesize::blob_size(jpeg)
            .is_ok_and(|s| (1..=1280).contains(&s.width) && (1..=720).contains(&s.height))
}

/// (serialized wire bytes, estimated input used for the energy reservation).
/// Only a real, bounded JPEG data URL in a user image part gets image accounting.
/// Strings in tool arguments, histories and text parts retain normal byte accounting.
pub fn model_request_size(request: &Value) -> Result<(usize, u64)> {
    let wire = serde_json::to_vec(request)?.len();
    let mut reservation = wire as u64;
    let (messages, responses) = match (request.get("input"), request.get("messages")) {
        (Some(input), None) => (input, true),
        (None, Some(messages)) => (messages, false),
        _ => return Ok((wire, reservation)),
    };
    for message in messages.as_array().into_iter().flatten() {
        if message["role"] != "user" {
            continue;
        }
        for part in message["content"].as_array().into_iter().flatten() {
            if let Some(encoded_len) = desktop_jpeg_payload(part, responses) {
                reservation = reservation - encoded_len as u64 + DESKTOP_FRAME_ALLOWANCE;
            }
        }
    }
    Ok((wire, reservation))
}

pub fn model_input_bytes_for_reservation(request: &Value) -> Result<u64> {
    Ok(model_request_size(request)?.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A 16 by 16 blue JPEG, used to exercise both protocol representations.
    const TILE: &str = "/9j/4AAQSkZJRgABAQAASABIAAD/4QBMRXhpZgAATU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAEKADAAQAAAABAAAAEAAAAAD/7QA4UGhvdG9zaG9wIDMuMAA4QklNBAQAAAAAAAA4QklNBCUAAAAAABDUHYzZjwCyBOmACZjs+EJ+/8AAEQgAEAAQAwEiAAIRAQMRAf/EAB8AAAEFAQEBAQEBAAAAAAAAAAABAgMEBQYHCAkKC//EALUQAAIBAwMCBAMFBQQEAAABfQECAwAEEQUSITFBBhNRYQcicRQygZGhCCNCscEVUtHwJDNicoIJChYXGBkaJSYnKCkqNDU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6g4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TFxsfIycrS09TV1tfY2drh4uPk5ebn6Onq8fLz9PX29/j5+v/EAB8BAAMBAQEBAQEBAQEAAAAAAAABAgMEBQYHCAkKC//EALURAAIBAgQEAwQHBQQEAAECdwABAgMRBAUhMQYSQVEHYXETIjKBCBRCkaGxwQkjM1LwFWJy0QoWJDThJfEXGBkaJicoKSo1Njc4OTpDREVGR0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoKDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uLj5OXm5+jp6vLz9PX29/j5+v/bAEMAAgICAgICAwICAwUDAwMFBgUFBQUGCAYGBgYGCAoICAgICAgKCgoKCgoKCgwMDAwMDA4ODg4ODw8PDw8PDw8PD//bAEMBAgICBAQEBwQEBxALCQsQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEP/dAAQAAf/aAAwDAQACEQMRAD8A5uiiiv8ATQ/yrP/Z";

    #[test]
    fn only_image_parts_receive_the_frame_allowance() {
        let image = format!("{JPEG_PREFIX}{TILE}");
        let responses =
            json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":image}]}]});
        let chat = json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":image}}]}]});
        for request in [responses, chat] {
            let (wire, estimate) = model_request_size(&request).unwrap();
            assert_eq!(
                estimate,
                wire as u64 - TILE.len() as u64 + DESKTOP_FRAME_ALLOWANCE
            );
        }
        let text =
            json!({"input":[{"role":"user","content":[{"type":"input_text","text":image}]}]});
        let bad = json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/jpeg;base64,YWJj"}]}]});
        for request in [text, bad] {
            let (wire, estimate) = model_request_size(&request).unwrap();
            assert_eq!(estimate, wire as u64);
        }
    }
}
