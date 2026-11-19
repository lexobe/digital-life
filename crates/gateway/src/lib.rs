//! OpenAI-compatible Chat Completions and Responses gateway.
//!
//! The server authenticates callers; this crate enforces the organism's allowed
//! models, bounded concurrency, output limits, accounting, and owner-scoped
//! Responses references. Configure with [`GatewayConfig`] and construct using
//! [`Gateway::new`]. See `example.toml` for an embeddable server configuration.
//!
//! Each upstream attempt has its own durable reservation. Explicit rejection or
//! connection failure releases it; ambiguous failures retain it as unknown.
//! Missing, partial, or overflowing usage never becomes zero usage. Request
//! tasks survive caller cancellation and stream workers drain disconnected
//! clients so final usage can still settle. A process crash leaves reservations
//! held. All synchronous Store operations use `spawn_blocking`.
//!
//! SSE events are persisted incrementally and IDs are durably bound before
//! forwarding. Memory is bounded by response/event limits and a bounded channel,
//! with no accumulated transcript. GET response reads the durable local snapshot
//! without another billable request. Background Responses and Conversations are
//! rejected because this API cannot account for their independent lifecycle.

mod config;
mod reconcile;
mod sse;
#[cfg(test)]
mod tests;

pub use config::{GatewayConfig, ProviderConfig};

use axum::{
    Json,
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    convert::Infallible,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use survive_core::{
    BirthTraits, EnergyPrice, LifeEnvironment, ResponseBinding, Store, Usage, new_id, now_ms,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Chat,
    Responses,
}

impl Protocol {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "chat/completions",
            Self::Responses => "responses",
        }
    }
}

#[derive(Clone, Debug)]
pub enum GatewayError {
    InvalidRequest(&'static str),
    ModelNotAllowed,
    NotFound,
    Busy,
    InsufficientTokens,
    Inactive,
    Unavailable,
    Upstream,
    Internal,
}

impl GatewayError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::ModelNotAllowed | Self::Inactive => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::InsufficientTokens => StatusCode::PAYMENT_REQUIRED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream => StatusCode::BAD_GATEWAY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn body(&self) -> Value {
        let (code, message) = match self {
            Self::InvalidRequest(message) => ("invalid_request", *message),
            Self::ModelNotAllowed => (
                "model_not_allowed",
                "Model is not permitted for this organism.",
            ),
            Self::NotFound => ("not_found", "Response or agent was not found."),
            Self::Busy => (
                "concurrency_limit",
                "Gateway concurrency limit reached; retry later.",
            ),
            Self::InsufficientTokens => (
                "insufficient_tokens",
                "Insufficient available energy for this request.",
            ),
            Self::Inactive => (
                "agent_not_active",
                "Agent cannot make model requests in its current state.",
            ),
            Self::Unavailable => (
                "provider_unavailable",
                "No eligible provider is available; check configured credentials and model routes.",
            ),
            Self::Upstream => (
                "upstream_error",
                "The model provider did not complete a valid response.",
            ),
            Self::Internal => (
                "gateway_error",
                "The gateway could not persist or process this request.",
            ),
        };
        json!({"error": {"message": message, "type": code, "code": code}})
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            self.body()["error"]["message"]
                .as_str()
                .unwrap_or("Gateway error")
        )
    }
}
impl std::error::Error for GatewayError {}
impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        (self.status(), Json(self.body())).into_response()
    }
}

#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    config: GatewayConfig,
    store: Store,
    client: reqwest::Client,
    providers: Vec<Provider>,
    redactions: Vec<String>,
    global: Arc<Semaphore>,
    agents: Mutex<HashMap<String, Weak<Semaphore>>>,
    live_attempts: Arc<Mutex<HashSet<String>>>,
    reconciliation: tokio::sync::Mutex<Option<String>>,
}

struct Provider {
    config: ProviderConfig,
    base: reqwest::Url,
    authorization: Option<HeaderValue>,
    available: bool,
}

struct Permits {
    _global: OwnedSemaphorePermit,
    _agent: OwnedSemaphorePermit,
}

struct Prepared {
    request: Value,
    logical_model: String,
    price: EnergyPrice,
    amount: u64,
    stream: bool,
    sticky: Option<String>,
    retryable: bool,
}

/// A live reservation; an unexpectedly dropped task never silently refunds it.
struct Attempt {
    store: Store,
    id: String,
    active: bool,
    upstream_id: Option<String>,
    live_attempts: Arc<Mutex<HashSet<String>>>,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if let Ok(mut live) = self.live_attempts.lock() {
            live.remove(&self.id);
        }
        if self.active {
            let store = self.store.clone();
            let id = self.id.clone();
            let upstream_id = self.upstream_id.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || {
                    let _ = store.mark_unknown(&id, upstream_id);
                });
            }
            // Without a runtime the durable Reserved balance remains held.
        }
    }
}

impl Gateway {
    pub fn new(mut config: GatewayConfig, store: Store) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.global_concurrency > 0 && config.global_concurrency <= Semaphore::MAX_PERMITS,
            "invalid gateway global concurrency"
        );
        anyhow::ensure!(
            config.per_agent_concurrency > 0
                && config.per_agent_concurrency <= Semaphore::MAX_PERMITS,
            "invalid gateway per-agent concurrency"
        );
        anyhow::ensure!(
            config.max_request_bytes > 0
                && config.max_response_bytes > 0
                && config.max_sse_event_bytes > 0,
            "gateway byte limits must be positive"
        );
        anyhow::ensure!(
            config.max_sse_event_bytes <= config.max_response_bytes,
            "SSE event limit exceeds response limit"
        );
        anyhow::ensure!(
            config.default_output_tokens > 0
                && config.default_output_tokens <= config.max_output_tokens,
            "invalid gateway output token limits"
        );
        anyhow::ensure!(
            config.minimum_input_reservation > 0
                && config
                    .max_output_tokens
                    .checked_add(config.minimum_input_reservation)
                    .is_some(),
            "invalid gateway input reservation"
        );
        anyhow::ensure!(
            config.request_timeout_ms > 0 && config.connect_timeout_ms > 0,
            "gateway timeouts must be positive"
        );
        anyhow::ensure!(
            config.stream_buffer_events > 0 && config.stream_buffer_events <= 1024,
            "stream_buffer_events must be 1..1024"
        );
        anyhow::ensure!(
            (1..=1024).contains(&config.reconcile_max_attempts),
            "reconcile_max_attempts must be 1..1024"
        );
        anyhow::ensure!(
            (1..=60_000).contains(&config.reconcile_time_budget_ms),
            "reconcile_time_budget_ms must be 1..60000"
        );
        let mut providers = Vec::new();
        let mut names = HashSet::new();
        let mut redactions = Vec::new();
        for pc in &config.providers {
            anyhow::ensure!(
                !pc.name.is_empty()
                    && pc.name.len() <= 128
                    && pc
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    && names.insert(pc.name.clone()),
                "provider names must be unique identifiers"
            );
            let mut base = reqwest::Url::parse(&pc.base_url)
                .map_err(|_| anyhow::anyhow!("invalid provider base URL"))?;
            anyhow::ensure!(
                matches!(base.scheme(), "http" | "https")
                    && base.host_str().is_some()
                    && base.username().is_empty()
                    && base.password().is_none()
                    && base.query().is_none()
                    && base.fragment().is_none(),
                "invalid provider base URL"
            );
            anyhow::ensure!(
                !pc.models.is_empty()
                    && pc
                        .models
                        .iter()
                        .all(|(k, v)| !k.trim().is_empty() && !v.trim().is_empty())
                    && !pc.protocols.is_empty(),
                "provider needs model routes and protocols"
            );
            base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
            redactions.push(pc.base_url.trim_end_matches('/').to_owned());
            redactions.push(base.origin().ascii_serialization());
            let (authorization, available) = match &pc.api_key_env {
                None => (None, true),
                Some(name) => {
                    anyhow::ensure!(
                        !name.is_empty() && !name.contains('='),
                        "invalid provider API key environment variable name"
                    );
                    match std::env::var(name)
                        .ok()
                        .filter(|key| !key.trim().is_empty())
                    {
                        Some(key) => {
                            let mut header = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| anyhow::anyhow!("provider API key environment variable contains invalid header bytes"))?;
                            header.set_sensitive(true);
                            redactions.push(key);
                            (Some(header), true)
                        }
                        None => (None, false),
                    }
                }
            };
            providers.push(Provider {
                config: pc.clone(),
                base,
                authorization,
                available,
            });
        }
        let aliases: BTreeSet<_> = config
            .providers
            .iter()
            .flat_map(|p| p.models.keys().cloned())
            .collect();
        if config.prices.is_empty() {
            // Existing installations used one energy unit per raw token.
            config.prices.extend(aliases.iter().cloned().map(|alias| {
                (
                    alias,
                    EnergyPrice {
                        input: 1,
                        output: 1,
                    },
                )
            }));
        }
        anyhow::ensure!(
            config.prices.keys().eq(aliases.iter()),
            "gateway.prices must quote exactly the configured logical models"
        );
        for (alias, price) in &config.prices {
            anyhow::ensure!(
                price.input > 0 && price.output > 0,
                "model {alias} needs positive input and output prices"
            );
            price.charge(
                config
                    .minimum_input_reservation
                    .max(config.max_request_bytes as u64),
                config.max_output_tokens,
            )?;
        }
        redactions.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .map_err(|_| anyhow::anyhow!("could not initialize gateway HTTP client"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                global: Arc::new(Semaphore::new(config.global_concurrency)),
                agents: Mutex::new(HashMap::new()),
                live_attempts: Arc::new(Mutex::new(HashSet::new())),
                reconciliation: tokio::sync::Mutex::new(None),
                config,
                store,
                client,
                providers,
                redactions,
            }),
        })
    }

    pub async fn completion(
        &self,
        agent_id: &str,
        protocol: Protocol,
        request: Value,
    ) -> Result<Response, GatewayError> {
        let permits = self.inner.acquire(agent_id)?;
        let inner = self.inner.clone();
        let agent_id = agent_id.to_owned();
        let (tx, rx) = oneshot::channel();
        // Once launched, cancellation of the HTTP handler cannot cancel accounting.
        tokio::spawn(async move {
            let result = inner.execute(agent_id, protocol, request, permits).await;
            let _ = tx.send(result);
        });
        rx.await.map_err(|_| GatewayError::Internal)?
    }

    /// Returns the owner's last durably stored Responses snapshot. No model call.
    pub async fn response(
        &self,
        agent_id: &str,
        response_id: &str,
    ) -> Result<Response, GatewayError> {
        let _permits = self.inner.acquire(agent_id)?;
        let agent = agent_id.to_owned();
        let id = response_id.to_owned();
        let binding = self
            .inner
            .db(move |s| s.get_response(&agent, &id))
            .await
            .map_err(|_| GatewayError::NotFound)?;
        // Item references share the binding index but are not GET response IDs.
        if binding.body.get("id").and_then(Value::as_str) != Some(response_id)
            || binding.body.get("object").and_then(Value::as_str) != Some("response")
        {
            return Err(GatewayError::NotFound);
        }
        let mut body = binding.body;
        self.inner.sanitize(&mut body, None);
        Ok(Json(body).into_response())
    }

    /// Logical aliases, current energy quotes and route availability, sorted by alias.
    pub fn catalog(&self, allowed_models: Option<&[String]>) -> Vec<Value> {
        self.catalog_matching(|id| allowed_models.is_none_or(|acl| acl.iter().any(|m| m == id)))
    }

    pub fn organism_catalog(&self, env: &LifeEnvironment, birth: &BirthTraits) -> Vec<Value> {
        self.catalog_matching(|id| env.permits_model(birth, id))
    }

    fn catalog_matching(&self, permits: impl Fn(&str) -> bool) -> Vec<Value> {
        self.inner.config.prices.iter()
            .filter(|(id,_)| permits(id))
            .map(|(id, price)| {
                let available = self.inner.providers.iter().any(|p| p.available && p.config.models.contains_key(id));
                json!({"id":id,"object":"model","created":0,"owned_by":"survive","input":price.input,"output":price.output,"available":available})
            }).collect()
    }

    /// Available logical aliases in the OpenAI list envelope.
    pub fn models(&self) -> Value {
        let rows: Vec<_> = self
            .catalog(None)
            .into_iter()
            .filter(|row| row["available"] == true)
            .collect();
        json!({"object":"list","data":rows})
    }
}

impl Inner {
    async fn db<T, F>(&self, operation: F) -> Result<T, GatewayError>
    where
        T: Send + 'static,
        F: FnOnce(Store) -> anyhow::Result<T> + Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || operation(store))
            .await
            .map_err(|_| GatewayError::Internal)?
            .map_err(|_| GatewayError::Internal)
    }

    fn acquire(&self, agent_id: &str) -> Result<Permits, GatewayError> {
        let global = self
            .global
            .clone()
            .try_acquire_owned()
            .map_err(|_| GatewayError::Busy)?;
        let mut agents = self.agents.lock().map_err(|_| GatewayError::Internal)?;
        agents.retain(|_, weak| weak.strong_count() > 0);
        let semaphore = agents
            .get(agent_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let value = Arc::new(Semaphore::new(self.config.per_agent_concurrency));
                agents.insert(agent_id.to_owned(), Arc::downgrade(&value));
                value
            });
        let agent = semaphore
            .try_acquire_owned()
            .map_err(|_| GatewayError::Busy)?;
        Ok(Permits {
            _global: global,
            _agent: agent,
        })
    }

    async fn prepare(
        &self,
        agent_id: &str,
        protocol: Protocol,
        mut request: Value,
    ) -> Result<Prepared, GatewayError> {
        let invalid = GatewayError::InvalidRequest;
        if serde_json::to_vec(&request)
            .map_err(|_| invalid("Invalid JSON request."))?
            .len()
            > self.config.max_request_bytes
        {
            return Err(invalid("Request exceeds the configured byte limit."));
        }
        let obj = request
            .as_object_mut()
            .ok_or(invalid("Request must be a JSON object."))?;
        let logical_model = obj
            .get("model")
            .and_then(Value::as_str)
            .ok_or(invalid("A model is required."))?
            .to_owned();
        let price = self
            .config
            .prices
            .get(&logical_model)
            .copied()
            .ok_or(GatewayError::ModelNotAllowed)?;
        let agent = agent_id.to_owned();
        let model = logical_model.clone();
        let allowed = self
            .db(move |store| match store.organism_profile(&agent) {
                Ok(profile) => {
                    let env = store.environment(&profile.environment_id)?;
                    Ok(env.permits_model(&profile.birth, &model))
                }
                Err(e) if e.to_string().starts_with("not_found:") => {
                    let experiment =
                        store.get_experiment(&store.get_agent(&agent)?.experiment_id)?;
                    Ok(experiment.config.model == model)
                }
                Err(e) => Err(e),
            })
            .await
            .map_err(|_| GatewayError::NotFound)?;
        if !allowed {
            return Err(GatewayError::ModelNotAllowed);
        }
        let stream = match obj.get("stream") {
            None => false,
            Some(v) => v.as_bool().ok_or(invalid("stream must be boolean."))?,
        };
        let key = match protocol {
            Protocol::Chat => {
                if !obj.get("messages").is_some_and(Value::is_array) {
                    return Err(invalid("messages must be an array."));
                }
                if obj.contains_key("max_tokens") && obj.contains_key("max_completion_tokens") {
                    return Err(invalid("Specify only one Chat output token limit."));
                }
                if obj.contains_key("max_tokens") {
                    "max_tokens"
                } else {
                    "max_completion_tokens"
                }
            }
            Protocol::Responses => {
                if !obj
                    .get("input")
                    .is_some_and(|v| v.is_string() || v.is_array())
                {
                    return Err(invalid("input must be a string or array."));
                }
                if obj
                    .get("background")
                    .is_some_and(|v| v != &Value::Bool(false))
                {
                    return Err(invalid("Background Responses are unsupported."));
                }
                if obj.get("conversation").is_some_and(|v| !v.is_null()) {
                    return Err(invalid(
                        "Use an owned previous_response_id instead of conversation.",
                    ));
                }
                "max_output_tokens"
            }
        };
        let output = match obj.get(key) {
            None => self.config.default_output_tokens,
            Some(value) => value
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or(invalid("Output token limit must be a positive integer."))?,
        };
        let count = if protocol == Protocol::Chat {
            match obj.get("n") {
                None => 1,
                Some(v) => v
                    .as_u64()
                    .filter(|v| *v > 0)
                    .ok_or(invalid("n must be a positive integer."))?,
            }
        } else {
            1
        };
        let output_total = output
            .checked_mul(count)
            .filter(|v| *v <= self.config.max_output_tokens)
            .ok_or(invalid(
                "Output token limit exceeds the configured maximum.",
            ))?;
        obj.insert(key.into(), json!(output));
        if protocol == Protocol::Chat && stream {
            let options = obj.entry("stream_options").or_insert_with(|| json!({}));
            options
                .as_object_mut()
                .ok_or(invalid("stream_options must be an object."))?
                .insert("include_usage".into(), json!(true));
        }
        let mut refs = Vec::new();
        if protocol == Protocol::Responses {
            if let Some(value) = request.get("previous_response_id").filter(|v| !v.is_null()) {
                refs.push((
                    value
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or(invalid("Invalid previous_response_id."))?
                        .to_owned(),
                    true,
                ));
            }
            if let Some(input) = request.get("input").and_then(Value::as_array) {
                let inline_calls: HashSet<&str> = input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("function_call" | "custom_tool_call")
                        )
                    })
                    .filter_map(|item| item.get("call_id").and_then(Value::as_str))
                    .collect();
                for item in input {
                    if let Some(id) = item.get("id") {
                        refs.push((
                            id.as_str()
                                .filter(|s| !s.is_empty())
                                .ok_or(invalid("Invalid input item reference."))?
                                .to_owned(),
                            false,
                        ));
                    } else if item.get("type").and_then(Value::as_str) == Some("item_reference") {
                        return Err(invalid("item_reference requires an id."));
                    }
                    if matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call_output" | "custom_tool_call_output")
                    ) {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .ok_or(invalid("Tool outputs require a call_id."))?;
                        if !inline_calls.contains(call_id) {
                            refs.push((call_id.to_owned(), false));
                        }
                    }
                }
            }
        }
        let mut sticky = None;
        let mut history = 0u64;
        let mut seen = HashSet::new();
        for (id, is_response) in refs {
            if !seen.insert(id.clone()) {
                continue;
            }
            let agent = agent_id.to_owned();
            let lookup = id.clone();
            let binding = self
                .db(move |s| s.get_response(&agent, &lookup))
                .await
                .map_err(|_| GatewayError::NotFound)?;
            if binding.body.get("object").and_then(Value::as_str) != Some("response")
                || (is_response
                    && binding.body.get("id").and_then(Value::as_str) != Some(id.as_str()))
            {
                return Err(GatewayError::NotFound);
            }
            if sticky.as_ref().is_some_and(|p| p != &binding.provider) {
                return Err(invalid("References belong to different providers."));
            }
            sticky = Some(binding.provider);
            let size = parse_usage(&binding.body, Protocol::Responses)
                .and_then(|v| v.total())
                .unwrap_or_else(|| binding.body.to_string().len() as u64);
            history = history
                .checked_add(size)
                .ok_or(invalid("Referenced context is too large."))?;
        }
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| invalid("Invalid JSON request."))?
            .len();
        if bytes > self.config.max_request_bytes {
            return Err(invalid("Request exceeds the configured byte limit."));
        }
        let input = (bytes as u64)
            .checked_add(history)
            .ok_or(invalid("Input reservation overflow."))?
            .max(self.config.minimum_input_reservation);
        let amount = price
            .charge(input, output_total)
            .map_err(|_| invalid("Energy reservation overflow."))?;
        // Hosted tools or provider resources may cause side effects or carry state.
        let hosted_tools = request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                tools.iter().any(|t| {
                    !matches!(
                        t.get("type").and_then(Value::as_str),
                        Some("function" | "custom")
                    )
                })
            });
        let retryable = sticky.is_none() && !hosted_tools && request.get("prompt").is_none();
        Ok(Prepared {
            request,
            logical_model,
            price,
            amount,
            stream,
            sticky,
            retryable,
        })
    }

    async fn event(
        &self,
        agent: &str,
        kind: &'static str,
        mut data: Value,
    ) -> Result<(), GatewayError> {
        // The parent exposes Store events to agents. Redact before persistence,
        // including escaped JSON strings in upstream HTTP bodies.
        if let Some(raw) = data.get_mut("body")
            && let Some(mut body) = raw
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            self.sanitize(&mut body, None);
            *raw = Value::String(body.to_string());
        }
        self.sanitize(&mut data, None);
        let agent = agent.to_owned();
        self.db(move |s| s.record_event(&agent, kind, data).map(|_| ()))
            .await
    }

    async fn reserve(
        &self,
        agent: &str,
        provider: &str,
        amount: u64,
        logical_model: &str,
        price: EnergyPrice,
    ) -> Result<Attempt, GatewayError> {
        let store = self.store.clone();
        let agent = agent.to_owned();
        let provider = provider.to_owned();
        let id = new_id();
        let reservation_id = id.clone();
        let logical_model = logical_model.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            store.reserve_priced(
                &agent,
                &reservation_id,
                &provider,
                amount,
                &logical_model,
                price,
            )
        })
        .await
        .map_err(|_| GatewayError::Internal)?;
        match result {
            Ok(_) => {
                self.live_attempts
                    .lock()
                    .map_err(|_| GatewayError::Internal)?
                    .insert(id.clone());
                Ok(Attempt {
                    store: self.store.clone(),
                    id,
                    active: true,
                    upstream_id: None,
                    live_attempts: self.live_attempts.clone(),
                })
            }
            Err(error) if error.to_string().contains("insufficient_tokens") => {
                Err(GatewayError::InsufficientTokens)
            }
            Err(error) if error.to_string().contains("model not permitted") => {
                Err(GatewayError::ModelNotAllowed)
            }
            Err(error)
                if matches!(
                    error.to_string().as_str(),
                    "agent_not_active" | "lifetime_exhausted"
                ) =>
            {
                Err(GatewayError::Inactive)
            }
            Err(_) => Err(GatewayError::Internal),
        }
    }

    async fn finish(
        &self,
        attempt: &mut Attempt,
        usage: Option<Usage>,
        upstream_id: Option<String>,
        unprocessed: bool,
    ) -> Result<(), GatewayError> {
        let id = attempt.id.clone();
        let upstream_id = upstream_id.or_else(|| attempt.upstream_id.clone());
        self.db(move |s| {
            if unprocessed {
                s.release(&id)?;
            } else if let Some(usage) = usage {
                s.settle(&id, usage, upstream_id)?;
            } else {
                s.mark_unknown(&id, upstream_id)?;
            }
            Ok(())
        })
        .await?;
        attempt.active = false;
        Ok(())
    }

    async fn identify(&self, attempt: &mut Attempt, upstream_id: &str) -> Result<(), GatewayError> {
        let id = attempt.id.clone();
        attempt.upstream_id = Some(upstream_id.to_owned());
        let upstream_id = attempt.upstream_id.clone();
        self.db(move |s| s.mark_unknown(&id, upstream_id).map(|_| ()))
            .await
    }

    async fn execute(
        self: Arc<Self>,
        agent: String,
        protocol: Protocol,
        request: Value,
        permits: Permits,
    ) -> Result<Response, GatewayError> {
        let prepared = self.prepare(&agent, protocol, request).await?;
        let candidates: Vec<usize> = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.available
                    && p.config.protocols.contains(&protocol)
                    && p.config.models.contains_key(&prepared.logical_model)
                    && prepared
                        .sticky
                        .as_ref()
                        .is_none_or(|name| name == &p.config.name)
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return Err(GatewayError::Unavailable);
        }
        self.event(
            &agent,
            "gateway.request",
            json!({"protocol":protocol, "request":prepared.request}),
        )
        .await?;
        for (position, index) in candidates.iter().enumerate() {
            let provider = &self.providers[*index];
            let can_retry = prepared.retryable && position + 1 < candidates.len();
            let mut body = prepared.request.clone();
            body["model"] = json!(provider.config.models[&prepared.logical_model]);
            let url = provider
                .base
                .join(protocol.path())
                .map_err(|_| GatewayError::Internal)?;
            let mut builder = self.client.post(url).json(&body).header(
                header::ACCEPT,
                if prepared.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            );
            if let Some(authorization) = &provider.authorization {
                builder = builder.header(header::AUTHORIZATION, authorization.clone());
            }
            // Building cannot send bytes and precedes the reservation.
            let request = builder.build().map_err(|_| GatewayError::Internal)?;
            let mut attempt = self
                .reserve(
                    &agent,
                    &provider.config.name,
                    prepared.amount,
                    &prepared.logical_model,
                    prepared.price,
                )
                .await?;
            let upstream = match self.client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    self.finish(&mut attempt, None, None, error.is_connect())
                        .await?;
                    if can_retry {
                        continue;
                    }
                    return Err(GatewayError::Upstream);
                }
            };
            let status = upstream.status();
            if !status.is_success() {
                let raw = self.read_body(upstream).await;
                if let Ok(bytes) = &raw {
                    self.event(&agent, "gateway.upstream_error", json!({"reservation_id":attempt.id, "status":status.as_u16(), "body":String::from_utf8_lossy(bytes)})).await?;
                }
                // A complete explicit rejection is positive evidence of no inference.
                let unprocessed = raw.is_ok()
                    && matches!(
                        status.as_u16(),
                        400 | 401 | 403 | 404 | 405 | 413 | 415 | 422 | 429
                    );
                self.finish(&mut attempt, None, None, unprocessed).await?;
                if can_retry
                    && (status.is_server_error()
                        || matches!(status.as_u16(), 401 | 403 | 404 | 408 | 429))
                {
                    continue;
                }
                return Err(GatewayError::Upstream);
            }
            if prepared.stream {
                if !upstream
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| {
                        v.split(';')
                            .next()
                            .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
                    })
                {
                    self.finish(&mut attempt, None, None, false).await?;
                    if can_retry {
                        continue;
                    }
                    return Err(GatewayError::Upstream);
                }
                let (tx, rx) = mpsc::channel(self.config.stream_buffer_events);
                let inner = self.clone();
                let provider_name = provider.config.name.clone();
                let logical = prepared.logical_model.clone();
                let stored_chat = protocol == Protocol::Chat
                    && prepared.request.get("store").and_then(Value::as_bool) == Some(true);
                tokio::spawn(async move {
                    let _permits = permits;
                    let mut sender = Some(tx);
                    let result = inner
                        .stream(
                            &agent,
                            protocol,
                            &provider_name,
                            &logical,
                            stored_chat,
                            upstream,
                            &mut attempt,
                            &mut sender,
                        )
                        .await;
                    if let Err(error) = result {
                        if attempt.active {
                            let _ = inner.finish(&mut attempt, None, None, false).await;
                        }
                        let _ = inner
                            .event(
                                &agent,
                                "gateway.stream_error",
                                json!({"reservation_id":attempt.id, "error":error.body()}),
                            )
                            .await;
                        send_event(
                            &mut sender,
                            format!("event: error\ndata: {}\n\n", error.body()),
                        )
                        .await;
                    }
                });
                let downstream = futures_util::stream::unfold(rx, |mut rx| async {
                    rx.recv().await.map(|value| (value, rx))
                });
                return Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header("x-accel-buffering", "no")
                    .body(Body::from_stream(downstream))
                    .map_err(|_| GatewayError::Internal);
            }
            let bytes = match self.read_body(upstream).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.finish(&mut attempt, None, None, false).await?;
                    if can_retry {
                        continue;
                    }
                    return Err(error);
                }
            };
            self.event(
                &agent,
                "gateway.upstream_body",
                json!({"reservation_id":attempt.id, "body":String::from_utf8_lossy(&bytes)}),
            )
            .await?;
            let body = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .filter(|v| valid_body(v, protocol));
            let Some(mut body) = body else {
                self.finish(&mut attempt, None, None, false).await?;
                if can_retry {
                    continue;
                }
                return Err(GatewayError::Upstream);
            };
            let id = body.get("id").and_then(Value::as_str).map(str::to_owned);
            attempt.upstream_id = id.clone();
            if protocol == Protocol::Responses {
                self.bind(
                    &agent,
                    &provider.config.name,
                    &body,
                    &prepared.logical_model,
                )
                .await?;
            } else if prepared.request.get("store").and_then(Value::as_bool) == Some(true) {
                let mut binding_body = body.clone();
                binding_body["_survive_gateway_stored_chat"] = json!(true);
                self.bind(
                    &agent,
                    &provider.config.name,
                    &binding_body,
                    &prepared.logical_model,
                )
                .await?;
            }
            self.finish(&mut attempt, parse_usage(&body, protocol), id, false)
                .await?;
            self.sanitize(&mut body, Some(&prepared.logical_model));
            return Ok(Json(body).into_response());
        }
        Err(GatewayError::Upstream)
    }

    async fn read_body(&self, response: reqwest::Response) -> Result<Vec<u8>, GatewayError> {
        if response
            .content_length()
            .is_some_and(|length| length > self.config.max_response_bytes as u64)
        {
            return Err(GatewayError::Upstream);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| GatewayError::Upstream)?;
            if chunk.len() > self.config.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(GatewayError::Upstream);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn bind(
        &self,
        agent: &str,
        provider: &str,
        body: &Value,
        logical: &str,
    ) -> Result<(), GatewayError> {
        let response_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(GatewayError::Upstream)?;
        let mut snapshot = body.clone();
        snapshot["model"] = json!(logical);
        self.sanitize(&mut snapshot, None);
        let mut bindings = vec![(response_id.to_owned(), snapshot)];
        if let Some(items) = body.get("output").and_then(Value::as_array) {
            for item in items {
                for id in [item.get("id"), item.get("call_id")]
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if id.is_empty() || id == response_id {
                        return Err(GatewayError::Upstream);
                    }
                    // Keep one full response snapshot. Alias rows only need their
                    // item and context estimate, avoiding quadratic storage.
                    let mut alias = json!({"id":response_id,"object":"response","status":body.get("status"),"model":logical,"usage":body.get("usage"),"output":[item]});
                    self.sanitize(&mut alias, None);
                    bindings.push((id.to_owned(), alias));
                }
            }
        }
        let agent = agent.to_owned();
        let provider = provider.to_owned();
        // Provider payload is separately persisted in full, redacted events.
        self.db(move |s| {
            for (id, body) in bindings {
                s.bind_response(ResponseBinding {
                    agent_id: agent.clone(),
                    response_id: id,
                    provider: provider.clone(),
                    body,
                    created_at: now_ms(),
                })?;
            }
            Ok(())
        })
        .await
    }

    fn scrub(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.redactions {
            if !secret.is_empty() {
                text = text.replace(secret, "[redacted]");
            }
        }
        text
    }

    fn sanitize(&self, value: &mut Value, logical: Option<&str>) {
        match value {
            Value::String(text) => *text = self.scrub(text),
            Value::Array(items) => {
                for item in items {
                    self.sanitize(item, logical);
                }
            }
            Value::Object(object) => {
                let old = std::mem::take(object);
                for (key, mut value) in old {
                    if key == "model" && value.is_string() && logical.is_some() {
                        value = json!(logical);
                    }
                    self.sanitize(&mut value, logical);
                    object.insert(self.scrub(&key), value);
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream(
        &self,
        agent: &str,
        protocol: Protocol,
        provider: &str,
        logical: &str,
        stored_chat: bool,
        upstream: reqwest::Response,
        attempt: &mut Attempt,
        sender: &mut Option<mpsc::Sender<Result<Bytes, Infallible>>>,
    ) -> Result<(), GatewayError> {
        let mut upstream = upstream.bytes_stream();
        let mut decoder = sse::Decoder::default();
        let mut received = 0usize;
        let mut usage: Option<Usage> = None;
        let mut response_id: Option<String> = None;
        while let Some(chunk) = upstream.next().await {
            let chunk = chunk.map_err(|_| GatewayError::Upstream)?;
            received = received
                .checked_add(chunk.len())
                .filter(|size| *size <= self.config.max_response_bytes)
                .ok_or(GatewayError::Upstream)?;
            // Bound the temporary list of decoded frames, even for a large read.
            for part in chunk.chunks(8192) {
                let frames = decoder
                    .push(part, self.config.max_sse_event_bytes)
                    .map_err(|_| GatewayError::Upstream)?;
                for frame in frames {
                    let mut persisted_raw = self.scrub(&frame.raw);
                    if let Some(mut value) = frame
                        .data
                        .as_deref()
                        .and_then(|data| serde_json::from_str::<Value>(data).ok())
                    {
                        self.sanitize(&mut value, None);
                        persisted_raw = self.frame_wire(&frame, &value);
                    }
                    self.event(
                        agent,
                        "gateway.sse",
                        json!({"reservation_id":attempt.id, "raw":persisted_raw}),
                    )
                    .await?;
                    let Some(data) = frame.data.as_deref() else {
                        send_event(sender, self.scrub(&frame.raw)).await;
                        continue;
                    };
                    if data.trim() == "[DONE]" {
                        if protocol != Protocol::Chat {
                            return Err(GatewayError::Upstream);
                        }
                        self.finish(attempt, usage, response_id, false).await?;
                        send_event(sender, "data: [DONE]\n\n".into()).await;
                        return Ok(());
                    }
                    let mut value: Value =
                        serde_json::from_str(data).map_err(|_| GatewayError::Upstream)?;
                    if !value.is_object() {
                        return Err(GatewayError::Upstream);
                    }
                    let event_type = value
                        .get("type")
                        .and_then(Value::as_str)
                        .or(frame.event.as_deref())
                        .unwrap_or("")
                        .to_owned();
                    if event_type == "error" || value.get("error").is_some_and(|e| !e.is_null()) {
                        return Err(GatewayError::Upstream);
                    }
                    let mut terminal = false;
                    match protocol {
                        Protocol::Chat => {
                            if let Some(id) = value.get("id").and_then(Value::as_str) {
                                if response_id.as_deref().is_some_and(|old| old != id) {
                                    return Err(GatewayError::Upstream);
                                }
                                response_id = Some(id.to_owned());
                                if attempt.upstream_id.as_deref() != Some(id) {
                                    if stored_chat {
                                        self.bind(agent, provider, &json!({"id":id,"object":"chat.completion","_survive_gateway_stored_chat":true}), logical).await?;
                                    }
                                    self.identify(attempt, id).await?;
                                }
                            }
                            if value.get("usage").is_some_and(|u| !u.is_null()) {
                                let reported = parse_usage(&value, Protocol::Chat)
                                    .ok_or(GatewayError::Upstream)?;
                                if usage.as_ref().is_some_and(|old| {
                                    old.input_tokens != reported.input_tokens
                                        || old.output_tokens != reported.output_tokens
                                }) {
                                    return Err(GatewayError::Upstream);
                                }
                                usage = Some(reported);
                            }
                        }
                        Protocol::Responses => {
                            let response = value.get("response");
                            let id = response
                                .and_then(|r| r.get("id"))
                                .or_else(|| value.get("response_id"))
                                .and_then(Value::as_str);
                            if let Some(id) = id {
                                if id.is_empty()
                                    || response_id.as_deref().is_some_and(|old| old != id)
                                {
                                    return Err(GatewayError::Upstream);
                                }
                                response_id = Some(id.to_owned());
                            }
                            let id = response_id.as_ref().ok_or(GatewayError::Upstream)?;
                            if let Some(body) = response {
                                if body.get("id").and_then(Value::as_str) != Some(id.as_str())
                                    || body.get("object").and_then(Value::as_str)
                                        != Some("response")
                                    || (matches!(
                                        event_type.as_str(),
                                        "response.completed"
                                            | "response.incomplete"
                                            | "response.failed"
                                    ) && !valid_body(body, Protocol::Responses))
                                {
                                    return Err(GatewayError::Upstream);
                                }
                                self.bind(agent, provider, body, logical).await?;
                            } else if value.get("response_id").is_some() {
                                let agent = agent.to_owned();
                                let id = id.clone();
                                let provider = provider.to_owned();
                                let logical = logical.to_owned();
                                self.db(move |s| {
                                    if s.get_response(&agent, &id).is_err() {
                                        s.bind_response(ResponseBinding { agent_id:agent, response_id:id.clone(), provider, body:json!({"id":id,"object":"response","status":"in_progress","model":logical,"output":[]}), created_at:now_ms() })?;
                                    }
                                    Ok(())
                                }).await?;
                            }
                            if attempt.upstream_id.as_deref() != Some(id) {
                                self.identify(attempt, id).await?;
                            }
                            if value.get("item").is_some()
                                || value.get("item_id").is_some()
                                || value.get("call_id").is_some()
                            {
                                let item = value.get("item").cloned().unwrap_or_else(|| json!({"id":value.get("item_id"),"call_id":value.get("call_id")}));
                                let mut ids = Vec::new();
                                if let Some(id) = item.get("id").and_then(Value::as_str) {
                                    ids.push(id.to_owned());
                                }
                                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                                    ids.push(id.to_owned());
                                }
                                let agent = agent.to_owned();
                                let provider = provider.to_owned();
                                let mut body = json!({"id":id,"object":"response","status":"in_progress","model":logical,"output":[item]});
                                self.sanitize(&mut body, None);
                                self.db(move |s| {
                                    for id in ids {
                                        s.bind_response(ResponseBinding {
                                            agent_id: agent.clone(),
                                            response_id: id,
                                            provider: provider.clone(),
                                            body: body.clone(),
                                            created_at: now_ms(),
                                        })?;
                                    }
                                    Ok(())
                                })
                                .await?;
                            }
                            if matches!(
                                event_type.as_str(),
                                "response.completed" | "response.incomplete" | "response.failed"
                            ) {
                                let body = response.ok_or(GatewayError::Upstream)?;
                                let expected = event_type.strip_prefix("response.").unwrap_or("");
                                if body.get("status").and_then(Value::as_str) != Some(expected) {
                                    return Err(GatewayError::Upstream);
                                }
                                usage = parse_usage(body, Protocol::Responses);
                                terminal = true;
                            }
                        }
                    }
                    self.sanitize(&mut value, Some(logical));
                    if terminal {
                        self.finish(attempt, usage.take(), response_id.clone(), false)
                            .await?;
                    }
                    // Preserve SSE metadata/comments, replacing only data lines.
                    send_event(sender, self.frame_wire(&frame, &value)).await;
                    if terminal {
                        return Ok(());
                    }
                }
            }
        }
        // Even clean EOF is ambiguous without a protocol terminal marker.
        let _incomplete_frame = !decoder.is_empty();
        Err(GatewayError::Upstream)
    }

    fn frame_wire(&self, frame: &sse::Frame, value: &Value) -> String {
        let mut wire = String::new();
        for line in frame.raw.lines() {
            if !line.is_empty() && line != "data" && !line.starts_with("data:") {
                wire.push_str(&self.scrub(line));
                wire.push('\n');
            }
        }
        wire.push_str(&format!("data: {value}\n\n"));
        wire
    }
}

fn valid_body(body: &Value, protocol: Protocol) -> bool {
    body.get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
        && match protocol {
            Protocol::Chat => {
                body.get("error").is_none_or(Value::is_null)
                    && body.get("choices").is_some_and(Value::is_array)
            }
            Protocol::Responses => {
                body.get("object").and_then(Value::as_str) == Some("response")
                    && matches!(
                        body.get("status").and_then(Value::as_str),
                        Some("completed" | "incomplete" | "failed")
                    )
                    && body.get("output").is_some_and(Value::is_array)
            }
        }
}

fn parse_usage(body: &Value, protocol: Protocol) -> Option<Usage> {
    let usage = body.get("usage")?;
    let (input, output) = match protocol {
        Protocol::Chat => ("prompt_tokens", "completion_tokens"),
        Protocol::Responses => ("input_tokens", "output_tokens"),
    };
    let usage_value = Usage {
        input_tokens: usage.get(input)?.as_u64()?,
        output_tokens: usage.get(output)?.as_u64()?,
    };
    let total = usage_value.total()?;
    if usage
        .get("total_tokens")
        .is_some_and(|v| v.as_u64() != Some(total))
    {
        return None;
    }
    Some(usage_value)
}

async fn send_event(sender: &mut Option<mpsc::Sender<Result<Bytes, Infallible>>>, data: String) {
    if let Some(tx) = sender {
        // Slow clients cannot keep an inference slot forever. Close their stream
        // and continue draining/persisting the upstream instead of buffering it.
        if !matches!(
            tokio::time::timeout(Duration::from_secs(1), tx.send(Ok(Bytes::from(data)))).await,
            Ok(Ok(()))
        ) {
            *sender = None;
        }
    }
}
