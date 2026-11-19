mod conversation;
mod desktop_upgrade;
mod life_store;
use crate::*;
use anyhow::{Context, Result, ensure};
use redb::{Database, ReadableDatabase, ReadableTable, Table, TableDefinition};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{collections::HashSet, path::Path, sync::Arc};

const RECORDS: TableDefinition<&str, &[u8]> = TableDefinition::new("records");
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

fn load<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<T> {
    let value = table
        .get(key)?
        .with_context(|| format!("not_found: {key}"))?;
    Ok(serde_json::from_slice(value.value())?)
}
fn optional<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<T>> {
    table
        .get(key)?
        .map(|v| serde_json::from_slice(v.value()).map_err(Into::into))
        .transpose()
}
fn put<T: Serialize>(
    table: &mut Table<'_, &'static str, &'static [u8]>,
    key: &str,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    table.insert(key, bytes.as_slice())?;
    Ok(())
}
fn scan<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    prefix: &str,
) -> Result<Vec<T>> {
    let mut records = Vec::new();
    for entry in table.range(prefix..)? {
        let (key, value) = entry?;
        if !key.value().starts_with(prefix) {
            break;
        }
        records.push(serde_json::from_slice(value.value())?);
    }
    Ok(records)
}
fn event(
    table: &mut Table<'_, &'static str, &'static [u8]>,
    agent_id: &str,
    kind: &str,
    data: serde_json::Value,
) -> Result<Event> {
    let seq = optional::<u64>(table, "meta:event")?
        .unwrap_or(0)
        .checked_add(1)
        .context("event sequence overflow")?;
    put(table, "meta:event", &seq)?;
    let value = Event {
        id: format!("{seq:020}"),
        agent_id: agent_id.into(),
        kind: kind.into(),
        data,
        created_at: now_ms(),
    };
    put(table, &format!("event:{agent_id}:{}", value.id), &value)?;
    Ok(value)
}

fn reference_page(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
    wrap: bool,
) -> Result<Vec<(String, String)>> {
    let start = format!("{prefix}{}", after.unwrap_or(""));
    let mut out = Vec::new();
    let limit = limit.clamp(1, 1000);
    for entry in table.range(start.as_str()..)? {
        let (k, v) = entry?;
        if !k.value().starts_with(prefix) {
            break;
        }
        let cursor = &k.value()[prefix.len()..];
        if after == Some(cursor) {
            continue;
        }
        out.push((cursor.to_owned(), serde_json::from_slice(v.value())?));
        if out.len() == limit {
            return Ok(out);
        }
    }
    if wrap && after.is_some() {
        for entry in table.range(prefix..=start.as_str())? {
            let (k, v) = entry?;
            out.push((
                k.value()[prefix.len()..].to_owned(),
                serde_json::from_slice(v.value())?,
            ));
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::builder()
            .set_cache_size(64 * 1024 * 1024)
            .create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(RECORDS)?;
        tx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }
    fn read<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        load(&self.db.begin_read()?.open_table(RECORDS)?, key)
    }
    fn list<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<T>> {
        scan(&self.db.begin_read()?.open_table(RECORDS)?, prefix)
    }
    pub fn create_experiment(
        &self,
        owner: &str,
        input: ExperimentInput,
    ) -> Result<(Experiment, Vec<(Agent, String)>)> {
        validate_experiment(&input)?;
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let mut slot = optional::<u32>(&t, "meta:slot")?.unwrap_or(0);
        ensure!(
            slot.checked_add(input.count).is_some_and(|v| v <= 65532),
            "sandbox address pool exhausted"
        );
        let experiment = Experiment {
            id: new_id(),
            owner: owner.into(),
            reward_remaining: input.reward_budget,
            config: input,
            created_at: now_ms(),
        };
        let mut agents = Vec::new();
        for _ in 0..experiment.config.count {
            slot += 1;
            let token = format!(
                "sv_{}{}",
                new_id().replace('-', ""),
                new_id().replace('-', "")
            );
            let hash = token_hash(&token);
            let agent = Agent {
                id: new_id(),
                experiment_id: experiment.id.clone(),
                token_hash: hash.clone(),
                slot,
                source_ip: format!("10.203.{}.{}", (slot + 1) / 256, (slot + 1) % 256),
                state: AgentState::Queued,
                generation: 0,
                granted: experiment.config.initial_tokens,
                used: 0,
                reserved: 0,
                last_error: None,
                created_at: now_ms(),
            };
            put(&mut t, &format!("agent:{}", agent.id), &agent)?;
            put(&mut t, &format!("token:{hash}"), &agent.id)?;
            put(&mut t, &format!("credential:{}", agent.id), &token)?;
            event(
                &mut t,
                &agent.id,
                "created",
                json!({"initial_tokens": agent.granted}),
            )?;
            agents.push((agent, token));
        }
        put(&mut t, "meta:slot", &slot)?;
        put(
            &mut t,
            &format!("experiment:{}", experiment.id),
            &experiment,
        )?;
        drop(t);
        tx.commit()?;
        Ok((experiment, agents))
    }
    pub fn get_experiment(&self, id: &str) -> Result<Experiment> {
        self.read(&format!("experiment:{id}"))
    }
    pub fn list_experiments(&self, owner: &str) -> Result<Vec<Experiment>> {
        Ok(self
            .list::<Experiment>("experiment:")?
            .into_iter()
            .filter(|e| e.owner == owner)
            .collect())
    }
    pub fn get_agent(&self, id: &str) -> Result<Agent> {
        self.read(&format!("agent:{id}"))
    }
    /// Server-only bootstrap credential. Never expose this through observer APIs.
    pub fn agent_credential(&self, id: &str) -> Result<String> {
        self.read(&format!("credential:{id}"))
    }
    pub fn list_agents(&self, experiment_id: Option<&str>) -> Result<Vec<Agent>> {
        let mut agents: Vec<_> = self
            .list::<Agent>("agent:")?
            .into_iter()
            .filter(|a| experiment_id.is_none_or(|id| a.experiment_id == id))
            .collect();
        agents.sort_by_key(|a| a.slot);
        Ok(agents)
    }
    pub fn authenticate_agent(&self, token: &str) -> Result<Agent> {
        let id: String = self.read(&format!("token:{}", token_hash(token)))?;
        self.get_agent(&id)
    }
    pub fn set_agent_state(
        &self,
        id: &str,
        state: AgentState,
        error: Option<String>,
    ) -> Result<Agent> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let mut agent: Agent = load(&t, &format!("agent:{id}"))?;
        // A late award can race the external stop operation. Never overwrite its
        // queued wakeup with a stale depletion observation.
        let has_time = optional::<OrganismProfile>(&t, &format!("org2:{id}"))?
            .is_none_or(|p| p.remaining_lifetime_ms > 0);
        if state == AgentState::Depleted && agent.balance() > 0 && has_time {
            return Ok(agent);
        }
        let state = if agent.balance() == 0 || !has_time {
            AgentState::Depleted
        } else {
            state
        };
        if agent.state == state && agent.last_error == error {
            return Ok(agent);
        }
        if state == AgentState::Starting && agent.state != state {
            agent.generation += 1;
        }
        if agent.state != state || agent.last_error != error {
            event(
                &mut t,
                id,
                "state",
                json!({"state": state, "error": error, "generation": agent.generation}),
            )?;
        }
        agent.state = state;
        agent.last_error = error;
        put(&mut t, &format!("agent:{id}"), &agent)?;
        drop(t);
        tx.commit()?;
        Ok(agent)
    }
    pub fn reserve(
        &self,
        agent_id: &str,
        reservation_id: &str,
        provider: &str,
        amount: u64,
    ) -> Result<Reservation> {
        self.reserve_internal(agent_id, reservation_id, provider, amount, None, None)
    }
    pub fn reserve_priced(
        &self,
        agent_id: &str,
        reservation_id: &str,
        provider: &str,
        amount: u64,
        model: &str,
        price: EnergyPrice,
    ) -> Result<Reservation> {
        self.reserve_internal(
            agent_id,
            reservation_id,
            provider,
            amount,
            Some(model),
            Some(price),
        )
    }
    fn reserve_internal(
        &self,
        agent_id: &str,
        reservation_id: &str,
        provider: &str,
        amount: u64,
        model: Option<&str>,
        price: Option<EnergyPrice>,
    ) -> Result<Reservation> {
        ensure!(amount > 0, "reservation must be positive");
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("reservation:{reservation_id}");
        if let Some(existing) = optional::<Reservation>(&t, &key)? {
            ensure!(
                existing.agent_id == agent_id
                    && existing.amount == amount
                    && existing.provider == provider
                    && existing.model.as_deref() == model
                    && existing.price == price,
                "idempotency_conflict"
            );
            return Ok(existing);
        }
        let mut agent: Agent = load(&t, &format!("agent:{agent_id}"))?;
        if let Some(p) = optional::<OrganismProfile>(&t, &format!("org2:{agent_id}"))? {
            let (boot, mono) = runtime_clock()?;
            ensure!(p.remaining_at(&boot, mono) > 0, "lifetime_exhausted");
            let env: LifeEnvironment = load(&t, &format!("env2:{}", p.environment_id))?;
            ensure!(
                model.is_some_and(|m| env.permits_model(&p.birth, m)) && price.is_some(),
                "model not permitted"
            );
        }
        ensure!(agent.available() >= amount, "insufficient_tokens");
        ensure!(
            agent.state != AgentState::Depleted && agent.state != AgentState::Suspended,
            "agent_not_active"
        );
        agent.reserved = agent
            .reserved
            .checked_add(amount)
            .context("balance overflow")?;
        let reservation = Reservation {
            id: reservation_id.into(),
            agent_id: agent_id.into(),
            provider: provider.into(),
            model: model.map(str::to_owned),
            price,
            energy_charged: None,
            amount,
            status: ReservationStatus::Reserved,
            usage: None,
            upstream_id: None,
            created_at: now_ms(),
        };
        put(&mut t, &key, &reservation)?;
        put(
            &mut t,
            &format!("unsettled:{reservation_id}"),
            &reservation_id,
        )?;
        put(&mut t, &format!("agent:{agent_id}"), &agent)?;
        let entry = event(
            &mut t,
            agent_id,
            "tokens_reserved",
            json!({"reservation_id": reservation_id, "amount": amount}),
        )?;
        put(
            &mut t,
            &format!("ledger:{agent_id}:{}", entry.id),
            &reservation_id,
        )?;
        drop(t);
        tx.commit()?;
        Ok(reservation)
    }
    pub fn settle(
        &self,
        id: &str,
        usage: Usage,
        upstream_id: Option<String>,
    ) -> Result<Reservation> {
        usage.total().context("usage overflow")?;
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("reservation:{id}");
        let mut r: Reservation = load(&t, &key)?;
        let total = r
            .price
            .unwrap_or(EnergyPrice {
                input: 1,
                output: 1,
            })
            .charge(usage.input_tokens, usage.output_tokens)?;
        if r.status == ReservationStatus::Settled {
            let old = r.usage.as_ref().context("settlement missing usage")?;
            ensure!(
                old.input_tokens == usage.input_tokens && old.output_tokens == usage.output_tokens,
                "settlement_conflict"
            );
            return Ok(r);
        }
        ensure!(
            r.status != ReservationStatus::Released,
            "reservation already released"
        );
        let mut agent: Agent = load(&t, &format!("agent:{}", r.agent_id))?;
        agent.reserved = agent
            .reserved
            .checked_sub(r.amount)
            .context("invalid reserved balance")?;
        agent.used = agent.used.checked_add(total).context("usage overflow")?;
        if agent.balance() == 0 {
            agent.state = AgentState::Depleted;
        }
        r.energy_charged = Some(total);
        r.status = ReservationStatus::Settled;
        r.usage = Some(usage);
        r.upstream_id = upstream_id.or(r.upstream_id);
        put(&mut t, &key, &r)?;
        put(&mut t, &format!("agent:{}", r.agent_id), &agent)?;
        t.remove(format!("unsettled:{id}").as_str())?;
        t.remove(format!("recoverable:{id}").as_str())?;
        event(
            &mut t,
            &r.agent_id,
            "tokens_settled",
            json!({"reservation_id": id, "usage": r.usage, "model":r.model,"price":r.price,"energy_charged":total,"over_reservation": total.saturating_sub(r.amount)}),
        )?;
        drop(t);
        tx.commit()?;
        Ok(r)
    }
    pub fn mark_unknown(&self, id: &str, upstream_id: Option<String>) -> Result<Reservation> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("reservation:{id}");
        let mut r: Reservation = load(&t, &key)?;
        ensure!(
            !matches!(
                r.status,
                ReservationStatus::Settled | ReservationStatus::Released
            ),
            "reservation already finalized"
        );
        r.status = ReservationStatus::Unknown;
        r.upstream_id = upstream_id.or(r.upstream_id);
        put(&mut t, &key, &r)?;
        if r.upstream_id.as_ref().is_some_and(|id| !id.is_empty()) {
            put(&mut t, &format!("recoverable:{id}"), &id)?;
        }
        event(
            &mut t,
            &r.agent_id,
            "usage_unknown",
            json!({"reservation_id": id}),
        )?;
        drop(t);
        tx.commit()?;
        Ok(r)
    }
    pub fn release(&self, id: &str) -> Result<Reservation> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("reservation:{id}");
        let mut r: Reservation = load(&t, &key)?;
        if r.status == ReservationStatus::Released {
            return Ok(r);
        }
        ensure!(
            r.status == ReservationStatus::Reserved,
            "only known unprocessed attempts can be released"
        );
        let mut agent: Agent = load(&t, &format!("agent:{}", r.agent_id))?;
        agent.reserved = agent
            .reserved
            .checked_sub(r.amount)
            .context("invalid reserved balance")?;
        r.status = ReservationStatus::Released;
        put(&mut t, &key, &r)?;
        put(&mut t, &format!("agent:{}", r.agent_id), &agent)?;
        t.remove(format!("unsettled:{id}").as_str())?;
        event(
            &mut t,
            &r.agent_id,
            "tokens_released",
            json!({"reservation_id": id, "amount": r.amount}),
        )?;
        drop(t);
        tx.commit()?;
        Ok(r)
    }
    pub fn reservations(&self, agent_id: &str) -> Result<Vec<Reservation>> {
        self.list::<String>(&format!("ledger:{agent_id}:"))?
            .into_iter()
            .map(|id| self.read(&format!("reservation:{id}")))
            .collect()
    }
    pub fn reservation_page(
        &self,
        agent_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Reservation>, Option<String>)> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let refs = reference_page(
            &t,
            &format!("ledger:{agent_id}:"),
            after,
            limit.min(100),
            false,
        )?;
        let cursor = refs.last().map(|(cursor, _)| cursor.clone());
        let rows = refs
            .into_iter()
            .map(|(_, id)| load(&t, &format!("reservation:{id}")))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, cursor))
    }
    pub fn all_reservations(&self) -> Result<Vec<Reservation>> {
        self.list("reservation:")
    }
    pub fn unsettled_reservations(&self) -> Result<Vec<Reservation>> {
        self.list::<String>("unsettled:")?
            .into_iter()
            .map(|id| self.read(&format!("reservation:{id}")))
            .collect()
    }
    /// Bounded, cyclic traversal of only attempts with recoverable upstream IDs.
    pub fn unknown_reservations(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Reservation>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let prefix = "recoverable:";
        let start = format!("{prefix}{}", after.unwrap_or(""));
        let limit = limit.clamp(1, 1024);
        let mut result = Vec::new();
        for entry in t.range(start.as_str()..)? {
            let (k, v) = entry?;
            if !k.value().starts_with(prefix) {
                break;
            }
            let id: String = serde_json::from_slice(v.value())?;
            if after == Some(id.as_str()) {
                continue;
            }
            result.push(load(&t, &format!("reservation:{id}"))?);
            if result.len() == limit {
                return Ok(result);
            }
        }
        if after.is_some() {
            for entry in t.range(prefix..start.as_str())? {
                let (_, v) = entry?;
                let id: String = serde_json::from_slice(v.value())?;
                result.push(load(&t, &format!("reservation:{id}"))?);
                if result.len() == limit {
                    break;
                }
            }
        }
        Ok(result)
    }
    pub fn record_event(
        &self,
        agent_id: &str,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<Event> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let result = event(&mut t, agent_id, kind, data)?;
        drop(t);
        tx.commit()?;
        Ok(result)
    }
    pub fn events(&self, agent_id: &str, after: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let prefix = format!("event:{agent_id}:");
        let start = format!("{prefix}{}", after.unwrap_or(""));
        let mut out = Vec::new();
        let mut page_bytes = 0usize;
        for entry in t.range(start.as_str()..)? {
            let (k, v) = entry?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let mut event: Event = serde_json::from_slice(v.value())?;
            if v.value().len() > 131072 {
                event.data =
                    json!({"truncated":true,"bytes":v.value().len(),"detail_event_id":event.id});
            }
            let size = serde_json::to_vec(&event)?.len();
            if !out.is_empty() && page_bytes + size > 1024 * 1024 {
                break;
            }
            if after.is_none_or(|cursor| event.id.as_str() > cursor) {
                page_bytes += size;
                out.push(event);
            }
            if out.len() >= limit.clamp(1, 1000) {
                break;
            }
        }
        Ok(out)
    }
    pub fn event_chunk(
        &self,
        agent_id: &str,
        event_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<u8>, usize)> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let value = t
            .get(format!("event:{agent_id}:{event_id}").as_str())?
            .context("not_found: event")?;
        let bytes = value.value();
        let begin = offset.min(bytes.len());
        let end = begin.saturating_add(limit.clamp(1, 65536)).min(bytes.len());
        Ok((bytes[begin..end].to_vec(), bytes.len()))
    }
    /// Bounded newest-first activity previews for an observer console. The full
    /// immutable payload remains available through event_chunk.
    pub fn recent_events(&self, agent_id: &str, limit: usize) -> Result<Vec<Event>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let prefix = format!("event:{agent_id}:");
        let end = format!("{prefix}\u{10ffff}");
        let mut out = Vec::new();
        for entry in t
            .range(prefix.as_str()..end.as_str())?
            .rev()
            .take(limit.min(30))
        {
            let (_, value) = entry?;
            let mut item: Event = serde_json::from_slice(value.value())?;
            if value.value().len() > 8192 {
                item.data =
                    json!({"truncated":true,"bytes":value.value().len(),"detail_event_id":item.id});
            }
            out.push(item);
        }
        Ok(out)
    }
    /// Count statuses without allocating large frozen evidence documents.
    pub fn submission_counts(
        &self,
        agent_id: &str,
    ) -> Result<std::collections::BTreeMap<String, u64>> {
        #[derive(serde::Deserialize)]
        struct StatusOnly {
            status: SubmissionStatus,
        }
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let prefix = format!("submission_agent:{agent_id}:");
        let mut out = std::collections::BTreeMap::new();
        for entry in t.range(prefix.as_str()..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            let id: String = serde_json::from_slice(value.value())?;
            let item: StatusOnly = load(&t, &format!("submission:{id}"))?;
            let status = serde_json::to_value(item.status)?
                .as_str()
                .unwrap()
                .to_owned();
            *out.entry(status).or_insert(0) += 1;
        }
        Ok(out)
    }
    pub fn bind_response(&self, binding: ResponseBinding) -> Result<()> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("response:{}:{}", binding.agent_id, binding.response_id);
        if let Some(previous) = optional::<ResponseBinding>(&t, &key)? {
            ensure!(
                previous.provider == binding.provider,
                "response provider is immutable"
            );
        }
        put(&mut t, &key, &binding)?;
        drop(t);
        tx.commit()?;
        Ok(())
    }
    pub fn get_response(&self, agent_id: &str, response_id: &str) -> Result<ResponseBinding> {
        self.read(&format!("response:{agent_id}:{response_id}"))
    }
    pub fn lookup_submission(
        &self,
        agent_id: &str,
        input: &SubmissionInput,
    ) -> Result<Option<Submission>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let index = format!(
            "submission_key:{agent_id}:{}",
            token_hash(&input.idempotency_key)
        );
        if let Some(id) = optional::<String>(&t, &index)? {
            let old: Submission = load(&t, &format!("submission:{id}"))?;
            ensure!(
                serde_json::to_value(&old.input)? == serde_json::to_value(input)?,
                "idempotency_conflict"
            );
            return Ok(Some(old));
        }
        Ok(None)
    }
    pub fn accept_submission(
        &self,
        agent_id: &str,
        input: SubmissionInput,
        evidence: serde_json::Value,
    ) -> Result<Submission> {
        ensure!(
            !input.idempotency_key.is_empty() && input.idempotency_key.len() <= 128,
            "invalid idempotency_key"
        );
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let index = format!(
            "submission_key:{agent_id}:{}",
            token_hash(&input.idempotency_key)
        );
        if let Some(id) = optional::<String>(&t, &index)? {
            let old: Submission = load(&t, &format!("submission:{id}"))?;
            ensure!(
                serde_json::to_value(&old.input)? == serde_json::to_value(&input)?,
                "idempotency_conflict"
            );
            return Ok(old);
        }
        let agent: Agent = load(&t, &format!("agent:{agent_id}"))?;
        let exp: Experiment = load(&t, &format!("experiment:{}", agent.experiment_id))?;
        let rule = exp
            .config
            .rules
            .iter()
            .find(|r| r.id == input.rule_id)
            .context("unknown rule")?;
        ensure!(
            rule.max_awards == 1
                || input
                    .occurrence_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty()),
            "repeat awards require occurrence_id"
        );
        ensure!(
            optional::<String>(&t, &format!("pending_agent:{agent_id}"))?.is_none(),
            "submission_pending"
        );
        ensure!(
            optional::<u64>(&t, &format!("last_submission:{agent_id}"))?
                .is_none_or(|at| now_ms().saturating_sub(at) >= 30_000),
            "submission_rate_limited"
        );
        // Sampling timestamps/account balances are observations, not new artifacts.
        let has_files = evidence["files"].as_object().is_some_and(|f| !f.is_empty());
        let material = json!({"files":evidence["files"],"description":if has_files{serde_json::Value::Null}else{evidence["description"].clone()},"occurrence_id":input.occurrence_id});
        let hash = token_hash(&serde_json::to_string(&material)?);
        let fingerprint_key = format!("fingerprint:{agent_id}:{}:{hash}", input.rule_id);
        ensure!(
            optional::<bool>(&t, &fingerprint_key)?.is_none(),
            "duplicate_evidence"
        );
        let s = Submission {
            id: new_id(),
            agent_id: agent_id.into(),
            experiment_id: exp.id,
            input,
            evidence,
            evidence_hash: hash,
            status: SubmissionStatus::Pending,
            judge_result: None,
            reward: 0,
            attempts: 0,
            last_error: None,
            created_at: now_ms(),
        };
        put(&mut t, &format!("submission:{}", s.id), &s)?;
        put(&mut t, &index, &s.id)?;
        put(&mut t, &format!("pending_agent:{agent_id}"), &s.id)?;
        put(&mut t, &format!("pending_submission:{}", s.id), &s.id)?;
        put(
            &mut t,
            &format!("last_submission:{agent_id}"),
            &s.created_at,
        )?;
        put(&mut t, &fingerprint_key, &true)?;
        let entry = event(
            &mut t,
            agent_id,
            "submission_accepted",
            json!({"submission_id": s.id, "rule_id": s.input.rule_id}),
        )?;
        put(
            &mut t,
            &format!("submission_agent:{agent_id}:{}", entry.id),
            &s.id,
        )?;
        drop(t);
        tx.commit()?;
        Ok(s)
    }
    pub fn get_submission(&self, id: &str) -> Result<Submission> {
        self.read(&format!("submission:{id}"))
    }
    pub fn list_submissions(&self, agent_id: Option<&str>) -> Result<Vec<Submission>> {
        if let Some(id) = agent_id {
            return self
                .list::<String>(&format!("submission_agent:{id}:"))?
                .into_iter()
                .map(|id| self.get_submission(&id))
                .collect();
        }
        Ok(self
            .list::<Submission>("submission:")?
            .into_iter()
            .filter(|s| agent_id.is_none_or(|id| s.agent_id == id))
            .collect())
    }
    pub fn submission_page(
        &self,
        agent_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Submission>, Option<String>)> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let refs = reference_page(
            &t,
            &format!("submission_agent:{agent_id}:"),
            after,
            limit.min(8),
            false,
        )?;
        let cursor = refs.last().map(|(cursor, _)| cursor.clone());
        let rows = refs
            .into_iter()
            .map(|(_, id)| load(&t, &format!("submission:{id}")))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, cursor))
    }
    pub fn pending_submission_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Submission>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        reference_page(&t, "pending_submission:", after, limit.min(16), true)?
            .into_iter()
            .map(|(_, id)| load(&t, &format!("submission:{id}")))
            .collect()
    }
    pub fn pending_submission_count(&self) -> Result<usize> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let mut count = 0;
        for row in t.range("pending_submission:"..)? {
            if !row?.0.value().starts_with("pending_submission:") {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
    pub fn pending_submissions(&self) -> Result<Vec<Submission>> {
        self.list::<String>("pending_submission:")?
            .into_iter()
            .map(|id| self.get_submission(&id))
            .collect()
    }
    pub fn claim_submission(&self, id: &str) -> Result<bool> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("submission:{id}");
        let mut s: Submission = load(&t, &key)?;
        if s.status != SubmissionStatus::Pending {
            return Ok(false);
        }
        s.status = SubmissionStatus::Evaluating;
        s.attempts += 1;
        put(&mut t, &key, &s)?;
        drop(t);
        tx.commit()?;
        Ok(true)
    }
    pub fn retry_submission(&self, id: &str, error: String) -> Result<()> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("submission:{id}");
        let mut s: Submission = load(&t, &key)?;
        if s.status == SubmissionStatus::Evaluating {
            s.status = SubmissionStatus::Pending;
            s.last_error = Some(error);
            put(&mut t, &key, &s)?;
        }
        drop(t);
        tx.commit()?;
        Ok(())
    }
    pub fn finish_submission(
        &self,
        id: &str,
        approved: bool,
        result: serde_json::Value,
        expired: bool,
    ) -> Result<Submission> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("submission:{id}");
        let mut s: Submission = load(&t, &key)?;
        if matches!(
            s.status,
            SubmissionStatus::Awarded | SubmissionStatus::Rejected | SubmissionStatus::Expired
        ) {
            return Ok(s);
        }
        let mut exp: Experiment = load(&t, &format!("experiment:{}", s.experiment_id))?;
        let rule = exp
            .config
            .rules
            .iter()
            .find(|r| r.id == s.input.rule_id)
            .context("unknown rule")?
            .clone();
        let expired = expired
            || now_ms().saturating_sub(s.created_at) > rule.expires_after_secs.saturating_mul(1000);
        let award_key = format!("award_count:{}:{}", s.agent_id, rule.id);
        let count = optional::<u32>(&t, &award_key)?.unwrap_or(0);
        let occurrence_key = format!(
            "award_occurrence:{}:{}:{}",
            s.agent_id,
            rule.id,
            token_hash(s.input.occurrence_id.as_deref().unwrap_or("once"))
        );
        let used_occurrence = optional::<bool>(&t, &occurrence_key)?.unwrap_or(false);
        let can_award = approved
            && !expired
            && count < rule.max_awards
            && exp.reward_remaining >= rule.tokens
            && !used_occurrence;
        s.judge_result = Some(result);
        s.last_error = None;
        if can_award {
            let mut agent: Agent = load(&t, &format!("agent:{}", s.agent_id))?;
            agent.granted = agent
                .granted
                .checked_add(rule.tokens)
                .context("balance overflow")?;
            if agent.state == AgentState::Depleted && agent.balance() > 0 {
                agent.state = AgentState::Queued;
            }
            exp.reward_remaining -= rule.tokens;
            s.status = SubmissionStatus::Awarded;
            s.reward = rule.tokens;
            put(&mut t, &award_key, &(count + 1))?;
            put(&mut t, &occurrence_key, &true)?;
            put(&mut t, &format!("agent:{}", s.agent_id), &agent)?;
            put(&mut t, &format!("experiment:{}", exp.id), &exp)?;
        } else {
            s.status = if expired {
                SubmissionStatus::Expired
            } else {
                SubmissionStatus::Rejected
            };
            if approved && !expired {
                s.last_error =
                    Some("reward allowance exhausted or occurrence already awarded".into());
            }
        }
        put(&mut t, &key, &s)?;
        t.remove(format!("pending_agent:{}", s.agent_id).as_str())?;
        t.remove(format!("pending_submission:{}", s.id).as_str())?;
        event(
            &mut t,
            &s.agent_id,
            "submission_evaluated",
            json!({"submission_id": s.id, "status": s.status, "tokens": s.reward}),
        )?;
        drop(t);
        tx.commit()?;
        Ok(s)
    }
    pub fn register_service(&self, agent_id: &str, service: ServiceRegistration) -> Result<()> {
        ensure!(
            service.port > 0 && !service.name.is_empty() && service.name.len() <= 128,
            "invalid service"
        );
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        put(
            &mut t,
            &format!("service:{agent_id}:{}", service.port),
            &service,
        )?;
        drop(t);
        tx.commit()?;
        Ok(())
    }
    pub fn services(&self, agent_id: &str) -> Result<Vec<ServiceRegistration>> {
        self.list(&format!("service:{agent_id}:"))
    }
    pub fn external_event(
        &self,
        experiment_id: &str,
        source_id: &str,
        event_id: &str,
        value: serde_json::Value,
    ) -> Result<()> {
        let key = format!(
            "external:{experiment_id}:{source_id}:{}",
            token_hash(event_id)
        );
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        if let Some(old) = optional::<serde_json::Value>(&t, &key)? {
            ensure!(old == value, "event_conflict");
            return Ok(());
        }
        put(&mut t, &key, &value)?;
        drop(t);
        tx.commit()?;
        Ok(())
    }
    pub fn get_external_event(
        &self,
        experiment_id: &str,
        source_id: &str,
        event_id: &str,
    ) -> Result<serde_json::Value> {
        self.read(&format!(
            "external:{experiment_id}:{source_id}:{}",
            token_hash(event_id)
        ))
    }
}

pub fn validate_experiment(input: &ExperimentInput) -> Result<()> {
    ensure!(
        input.rules.len() <= 64 && input.evidence_sources.len() <= 16,
        "at most 64 rules and 16 evidence sources per experiment"
    );
    ensure!(
        !input.name.trim().is_empty() && input.name.len() <= 256,
        "name required (max 256 bytes)"
    );
    ensure!(
        !input.goal.trim().is_empty() && input.goal.len() <= 32768,
        "goal required (max 32768 bytes)"
    );
    ensure!(
        !input.initial_prompt.trim().is_empty()
            && serde_json::to_vec(&input.initial_prompt)?.len() <= 32768,
        "initial_prompt required (max 32KiB JSON-encoded, leaving room for core tools/context)"
    );
    ensure!(
        (1..=1000).contains(&input.count),
        "count must be 1..1000 per request"
    );
    ensure!(
        input.individuals.is_empty() || input.individuals.len() == input.count as usize,
        "individual seeds must match count"
    );
    for seed in &input.individuals {
        ensure!(
            !seed.prompt.trim().is_empty() && serde_json::to_vec(&seed.prompt)?.len() <= 32768,
            "individual prompt required (max 32KiB JSON-encoded)"
        );
    }
    ensure!(
        input.initial_tokens > 0
            && input
                .initial_tokens
                .checked_mul(input.count as u64)
                .is_some(),
        "invalid initial_tokens"
    );
    ensure!(
        input
            .initial_tokens
            .checked_add(input.reward_budget)
            .is_some(),
        "token budget overflow"
    );
    ensure!(
        input.limits.memory_bytes >= 32 * 1024 * 1024
            && (8..=16384).contains(&input.limits.pids)
            && (1..=1_000_000).contains(&input.limits.cpu_millis),
        "invalid resource limits"
    );
    let mut source_ids = HashSet::new();
    for source in &input.evidence_sources {
        if let Some(hash) = &source.event_secret_hash {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "event_secret_hash must be lowercase SHA256 hex"
            );
        }
        ensure!(
            valid_id(&source.id) && source_ids.insert(source.id.clone()),
            "invalid or duplicate evidence source id"
        );
        ensure!(
            !source.url.is_empty() || source.event_secret_hash.is_some(),
            "evidence source needs url or event authentication"
        );
    }
    let mut rule_ids = HashSet::new();
    for rule in &input.rules {
        ensure!(
            rule.source_ids.len() <= 8 && rule.instructions.len() <= 16384,
            "rule evidence/instructions too large"
        );
        ensure!(
            valid_id(&rule.id) && rule_ids.insert(rule.id.clone()),
            "invalid or duplicate rule id"
        );
        ensure!(
            !rule.instructions.trim().is_empty()
                && rule.tokens > 0
                && rule.max_awards > 0
                && rule.expires_after_secs > 0,
            "invalid reward rule"
        );
        ensure!(
            rule.source_ids.iter().all(|id| source_ids.contains(id)),
            "unknown evidence source"
        );
        ensure!(
            rule.min_confidence.is_finite() && (0.0..=1.0).contains(&rule.min_confidence),
            "invalid confidence"
        );
        if rule.max_awards > 1 {
            ensure!(
                !rule.source_ids.is_empty()
                    && rule
                        .occurrence_path
                        .as_ref()
                        .is_some_and(|s| s.starts_with('/')),
                "repeat rewards require external occurrence_path"
            );
        }
        let max_score = match rule.kind {
            RewardKind::Noul => 1.0,
            RewardKind::Score => {
                let levels = rule
                    .criteria
                    .as_ref()
                    .and_then(|c| c.as_array())
                    .context("score requires criteria array")?;
                ensure!(
                    (2..=10).contains(&levels.len()),
                    "score criteria needs 2..10 levels"
                );
                (levels.len() - 1) as f64
            }
        };
        ensure!(
            rule.threshold.is_finite() && rule.threshold >= 0.0 && rule.threshold <= max_score,
            "invalid threshold"
        );
    }
    Ok(())
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    fn seed() -> ExperimentInput {
        serde_json::from_value(json!({"name":"test", "goal":"produce result", "initial_prompt":"act", "count":1, "initial_tokens":100,
            "reward_budget":50, "rules":[{"id":"done", "instructions":"valid?", "tokens":50}]})).unwrap()
    }
    #[test]
    fn atomic_reservations_and_durable_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = Store::open(&path).unwrap();
        let (_, agents) = store.create_experiment("owner", seed()).unwrap();
        let id = agents[0].0.id.clone();
        let threads: Vec<_> = (0..20)
            .map(|i| {
                let s = store.clone();
                let id = id.clone();
                std::thread::spawn(move || s.reserve(&id, &format!("r{i}"), "p", 10).is_ok())
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .filter_map(|t| t.join().ok())
                .filter(|v| *v)
                .count(),
            10
        );
        let r = store.reservations(&id).unwrap()[0].clone();
        store.mark_unknown(&r.id, None).unwrap();
        assert!(store.release(&r.id).is_err());
        assert_eq!(store.get_agent(&id).unwrap().balance(), 100);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_agent(&id).unwrap().available(), 0);
        store
            .settle(
                &r.id,
                Usage {
                    input_tokens: 4,
                    output_tokens: 2,
                },
                None,
            )
            .unwrap();
        store
            .settle(
                &r.id,
                Usage {
                    input_tokens: 4,
                    output_tokens: 2,
                },
                None,
            )
            .unwrap();
        assert_eq!(store.get_agent(&id).unwrap().used, 6);
        assert!(
            store
                .settle(
                    &r.id,
                    Usage {
                        input_tokens: 7,
                        output_tokens: 2
                    },
                    None
                )
                .is_err()
        );
    }
    #[test]
    fn delayed_reward_wakes_depleted_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("db")).unwrap();
        let (exp, agents) = s.create_experiment("owner", seed()).unwrap();
        let id = &agents[0].0.id;
        let sub = s
            .accept_submission(
                id,
                SubmissionInput {
                    rule_id: "done".into(),
                    idempotency_key: "a".into(),
                    description: "result".into(),
                    files: vec![],
                    occurrence_id: None,
                },
                json!({"result":"ok"}),
            )
            .unwrap();
        s.reserve(id, "r", "p", 100).unwrap();
        s.settle(
            "r",
            Usage {
                input_tokens: 50,
                output_tokens: 50,
            },
            None,
        )
        .unwrap();
        assert_eq!(s.get_agent(id).unwrap().state, AgentState::Depleted);
        let first = s
            .finish_submission(&sub.id, true, json!({"model":"jev-test"}), false)
            .unwrap();
        let second = s
            .finish_submission(&sub.id, true, json!({}), false)
            .unwrap();
        assert_eq!(first.reward, 50);
        assert_eq!(second.reward, 50);
        assert_eq!(s.get_agent(id).unwrap().state, AgentState::Queued);
        assert_eq!(s.get_agent(id).unwrap().balance(), 50);
        assert_eq!(s.get_experiment(&exp.id).unwrap().reward_remaining, 0);
    }
    #[test]
    fn experiment_validation_rejects_unverified_repeat_rewards() {
        let mut input = seed();
        input.rules[0].max_awards = 2;
        assert!(validate_experiment(&input).is_err());
    }
    #[test]
    fn experiment_rejects_memory_below_the_runtime_minimum_before_creation() {
        let mut input = seed();
        input.limits.memory_bytes = 16 * 1024 * 1024;
        assert!(validate_experiment(&input).is_err());
        input.limits.memory_bytes = 32 * 1024 * 1024;
        assert!(validate_experiment(&input).is_ok());
    }

    #[test]
    fn bounded_history_keeps_exact_payloads_and_scoped_ledger_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let (_, agents) = store.create_experiment("owner", seed()).unwrap();
        let id = &agents[0].0.id;
        let event = store
            .record_event(id, "large", json!({"text":"数据".repeat(40000)}))
            .unwrap();
        let page = store.events(id, None, 100).unwrap();
        let summary = page.iter().find(|e| e.id == event.id).unwrap();
        assert_eq!(summary.data["truncated"], true);
        let mut offset = 0;
        let mut raw = Vec::new();
        loop {
            let (chunk, total) = store.event_chunk(id, &event.id, offset, 65536).unwrap();
            assert!(chunk.len() <= 65536);
            offset += chunk.len();
            raw.extend(chunk);
            if offset == total {
                break;
            }
        }
        let restored: Event = serde_json::from_slice(&raw).unwrap();
        assert_eq!(restored.data, event.data);
        assert!(
            store
                .event_chunk("another-agent", &event.id, 0, 100)
                .is_err()
        );
        for n in 0..3 {
            store
                .reserve(id, &format!("ordered-{n}"), "mock", 10)
                .unwrap();
        }
        let (first, cursor) = store.reservation_page(id, None, 1).unwrap();
        assert_eq!(first[0].id, "ordered-0");
        let (rest, _) = store.reservation_page(id, cursor.as_deref(), 100).unwrap();
        assert_eq!(
            rest.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["ordered-1", "ordered-2"]
        );
        assert!(
            store
                .reservation_page("another-agent", None, 100)
                .unwrap()
                .0
                .is_empty()
        );
    }
}
