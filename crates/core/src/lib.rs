mod model_input;
pub use model_input::{
    MAX_FRAME_BYTES, is_desktop_jpeg, model_input_bytes_for_reservation, model_request_size,
};
pub mod life;
pub use life::*;
pub mod models;
pub mod store;
pub use models::*;
pub use store::Store;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn token_hash(token: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

/// The same immutable environment message is used to budget seed requests and
/// to enforce the organism's actual environment at the model gateway.
pub fn environment_context_message(description: &str) -> serde_json::Value {
    serde_json::json!({"role":"system","content":format!("Environment description:\n{description}")})
}
