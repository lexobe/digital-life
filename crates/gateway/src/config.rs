use crate::Protocol;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use survive_core::EnergyPrice;

/// Gateway limits. Provider order is failover order; all limits must be positive.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    pub providers: Vec<ProviderConfig>,
    /// Energy units per input and output token, keyed by public logical model.
    pub prices: BTreeMap<String, EnergyPrice>,
    pub global_concurrency: usize,
    pub per_agent_concurrency: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_sse_event_bytes: usize,
    pub max_output_tokens: u64,
    pub default_output_tokens: u64,
    /// Minimum input reservation, including provider-side / multimodal context.
    /// The request's byte length and retained history can raise this amount.
    pub minimum_input_reservation: u64,
    /// Whole upstream attempt, including reading its body and draining a stream.
    pub request_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub stream_buffer_events: usize,
    /// Maximum unknown reservations examined per reconciliation pass (1..=1024).
    pub reconcile_max_attempts: usize,
    /// Per-pass network budget (1..=60000 ms); durable Store operations finish.
    pub reconcile_time_budget_ms: u64,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            providers: vec![],
            prices: BTreeMap::new(),
            global_concurrency: 16,
            per_agent_concurrency: 2,
            max_request_bytes: 256 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_sse_event_bytes: 256 * 1024,
            max_output_tokens: 8192,
            default_output_tokens: 1024,
            minimum_input_reservation: 1024,
            request_timeout_ms: 120_000,
            connect_timeout_ms: 10_000,
            stream_buffer_events: 8,
            reconcile_max_attempts: 16,
            reconcile_time_budget_ms: 20_000,
        }
    }
}

/// A trusted OpenAI-compatible endpoint. `base_url` includes `/v1` if required.
/// API key values are resolved once at construction from `api_key_env`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// A list of upstream model IDs. Existing installations may provide a routing table.
    #[serde(deserialize_with = "deserialize_models")]
    pub models: BTreeMap<String, String>,
    #[serde(default = "both_protocols")]
    pub protocols: Vec<Protocol>,
}

fn deserialize_models<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Models {
        Direct(Vec<String>),
        Mapped(BTreeMap<String, String>),
    }
    match Models::deserialize(deserializer)? {
        Models::Mapped(routes) => Ok(routes),
        Models::Direct(ids) => {
            let mut routes = BTreeMap::new();
            for id in ids {
                if id.trim().is_empty()
                    || id.trim() != id
                    || id.len() > 256
                    || id.chars().any(char::is_control)
                {
                    return Err(serde::de::Error::custom("invalid model ID"));
                }
                if routes.insert(id.clone(), id).is_some() {
                    return Err(serde::de::Error::custom("duplicate model ID"));
                }
            }
            Ok(routes)
        }
    }
}

fn both_protocols() -> Vec<Protocol> {
    vec![Protocol::Chat, Protocol::Responses]
}
