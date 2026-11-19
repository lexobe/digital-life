//! Verified, owner-scoped recovery of late usage. Retrieval is not an inference.
use super::*;
use survive_core::{Reservation, ReservationStatus};

impl Gateway {
    /// Recover verifiable late usage, returning the number of verified settlements.
    /// Missing credentials, bindings, IDs or usage leave reservations untouched.
    /// Calls on the same Gateway are serialized; a concurrent call returns zero.
    /// Examines at most `reconcile_max_attempts`, with a rotating cursor, and
    /// stops network work after `reconcile_time_budget_ms`. Durable Store calls
    /// finish normally. Uses only the bounded, indexed Store recovery query.
    pub async fn reconcile_unknown(&self) -> Result<usize, GatewayError> {
        let Ok(mut cursor) = self.inner.reconciliation.try_lock() else {
            return Ok(0);
        };
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(self.inner.config.reconcile_time_budget_ms);
        let after = cursor.clone();
        let limit = self.inner.config.reconcile_max_attempts;
        let reservations = self
            .inner
            .db(move |store| {
                let page = store.unknown_reservations(after.as_deref(), limit)?;
                // The cyclic Store query excludes the cursor itself. Retry an
                // unresolved singleton on the next pass instead of starving it.
                if page.is_empty() && after.is_some() {
                    return store.unknown_reservations(None, limit);
                }
                Ok(page)
            })
            .await?;
        let mut settled = 0;
        for reservation in reservations {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            // Advance even on unavailable/unverifiable entries to avoid starving
            // later reservations behind a permanently unrecoverable one.
            *cursor = Some(reservation.id.clone());
            if self
                .inner
                .live_attempts
                .lock()
                .map_err(|_| GatewayError::Internal)?
                .contains(&reservation.id)
            {
                continue;
            }
            // Reconciliation shares inference concurrency, without reserving tokens.
            let _permits = match self.inner.acquire(&reservation.agent_id) {
                Ok(permits) => permits,
                Err(GatewayError::Busy) => continue,
                Err(error) => return Err(error),
            };
            let agent = reservation.agent_id.clone();
            let id = reservation.upstream_id.clone().unwrap_or_default();
            let binding = match self.inner.db(move |s| s.get_response(&agent, &id)).await {
                Ok(binding) => binding,
                Err(_) => continue,
            };
            let Some(protocol) = recovery_protocol(&reservation, &binding) else {
                continue;
            };
            let Some(provider) = self.inner.providers.iter().find(|provider| {
                provider.available
                    && provider.config.name == reservation.provider
                    && provider.config.protocols.contains(&protocol)
            }) else {
                continue;
            };
            let Some(logical) = binding
                .body
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| provider.config.models.contains_key(*model))
            else {
                continue;
            };
            let mut url = provider.base.clone();
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| GatewayError::Internal)?;
                segments.pop_if_empty();
                match protocol {
                    Protocol::Responses => {
                        segments.push("responses");
                    }
                    Protocol::Chat => {
                        segments.push("chat").push("completions");
                    }
                }
                // Push a single encoded segment; IDs cannot supply another URL.
                segments.push(&binding.response_id);
            }
            let mut request = self
                .inner
                .client
                .get(url)
                .timeout(
                    Duration::from_millis(self.inner.config.request_timeout_ms.min(10_000))
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                )
                .header(header::ACCEPT, "application/json");
            if let Some(authorization) = &provider.authorization {
                request = request.header(header::AUTHORIZATION, authorization.clone());
            }
            let transport = async {
                let response = request.send().await.map_err(|_| GatewayError::Upstream)?;
                let status = response.status();
                let bytes = self.inner.read_body(response).await?;
                Ok::<_, GatewayError>((status, bytes))
            };
            let (status, bytes) = match tokio::time::timeout_at(deadline, transport).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => continue,
                Err(_) => break,
            };
            self.inner.event(&reservation.agent_id,"gateway.reconciliation",json!({"reservation_id":reservation.id,"status":status.as_u16(),"body":String::from_utf8_lossy(&bytes)})).await?;
            if !status.is_success() {
                continue;
            }
            let body = match serde_json::from_slice::<Value>(&bytes) {
                Ok(body) => body,
                Err(_) => continue,
            };
            if self
                .inner
                .recover(&reservation, &binding, protocol, logical, body)
                .await?
            {
                settled += 1;
            }
        }
        Ok(settled)
    }
}

fn recovery_protocol(reservation: &Reservation, binding: &ResponseBinding) -> Option<Protocol> {
    if reservation.status != ReservationStatus::Unknown
        || reservation.agent_id != binding.agent_id
        || reservation.provider != binding.provider
        || reservation.upstream_id.as_deref() != Some(binding.response_id.as_str())
        || binding.body.get("id").and_then(Value::as_str) != Some(binding.response_id.as_str())
    {
        return None;
    }
    match binding.body.get("object").and_then(Value::as_str) {
        Some("response") => Some(Protocol::Responses),
        Some("chat.completion")
            if binding
                .body
                .get("_survive_gateway_stored_chat")
                .and_then(Value::as_bool)
                == Some(true) =>
        {
            Some(Protocol::Chat)
        }
        _ => None,
    }
}

impl Inner {
    pub(super) async fn recover(
        &self,
        reservation: &Reservation,
        binding: &ResponseBinding,
        protocol: Protocol,
        logical: &str,
        mut body: Value,
    ) -> Result<bool, GatewayError> {
        if recovery_protocol(reservation, binding) != Some(protocol)
            || body.get("id").and_then(Value::as_str) != reservation.upstream_id.as_deref()
            || body.get("object").and_then(Value::as_str)
                != Some(match protocol {
                    Protocol::Responses => "response",
                    Protocol::Chat => "chat.completion",
                })
            || !valid_body(&body, protocol)
        {
            return Ok(false);
        }
        let Some(usage) = parse_usage(&body, protocol) else {
            return Ok(false);
        };
        if protocol == Protocol::Chat {
            body["_survive_gateway_stored_chat"] = json!(true);
        }
        self.bind(&reservation.agent_id, &reservation.provider, &body, logical)
            .await?;
        let id = reservation.id.clone();
        let upstream_id = reservation.upstream_id.clone();
        self.db(move |s| {
            // Store atomically verifies idempotency and rejects conflicting or
            // released settlements. A history scan here would be unbounded and
            // would not prevent a concurrent settlement between read and write.
            s.settle(&id, usage, upstream_id)?;
            Ok(true)
        })
        .await
    }
}
