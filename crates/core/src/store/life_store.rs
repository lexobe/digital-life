use super::*;
use serde_json::Value;

fn require_owner(actual: &str, expected: &str) -> Result<()> {
    ensure!(actual == expected, "forbidden");
    Ok(())
}
fn check_key(s: &str) -> Result<()> {
    ensure!(
        !s.trim().is_empty() && s.len() <= 256,
        "invalid idempotency key"
    );
    Ok(())
}

impl Store {
    pub fn environment(&self, id: &str) -> Result<LifeEnvironment> {
        self.read(&format!("env2:{id}"))
    }
    pub fn environments(&self, owner: &str) -> Result<Vec<LifeEnvironment>> {
        Ok(self
            .list::<LifeEnvironment>("env2:")?
            .into_iter()
            .filter(|e| e.owner == owner)
            .collect())
    }
    pub fn create_environment(
        &self,
        owner: &str,
        input: EnvironmentInput,
    ) -> Result<LifeEnvironment> {
        ensure!(
            !owner.is_empty()
                && !input.description.trim().is_empty()
                && serde_json::to_vec(&input.description)?.len() <= 16384,
            "environment description required (max 16 KiB)"
        );
        ensure!(
            input.name.len() <= 256 && input.note.len() <= 4096,
            "environment name or note too long"
        );
        validate_allowed_models(input.allowed_models.as_deref())?;
        let env = LifeEnvironment {
            id: new_id(),
            owner: owner.into(),
            description: input.description,
            name: input.name,
            note: input.note,
            allowed_models: input.allowed_models,
            created_at: now_ms(),
        };
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(RECORDS)?;
            put(&mut t, &format!("env2:{}", env.id), &env)?;
        }
        tx.commit()?;
        Ok(env)
    }
    pub fn organism_profile(&self, id: &str) -> Result<OrganismProfile> {
        self.read(&format!("org2:{id}"))
    }
    pub fn profiles(&self, owner: Option<&str>) -> Result<Vec<OrganismProfile>> {
        Ok(self
            .list::<OrganismProfile>("org2:")?
            .into_iter()
            .filter(|p| owner.is_none_or(|s| s == p.owner))
            .collect())
    }
    pub fn birth_organisms(
        &self,
        owner: &str,
        env_id: &str,
        batch: BirthBatch,
    ) -> Result<Vec<Agent>> {
        let traits = batch.resolve()?;
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let env: LifeEnvironment = load(&t, &format!("env2:{env_id}"))?;
        require_owner(&env.owner, owner)?;
        env.validate_birth_models(&batch.defaults)?;
        let mut slot = optional::<u32>(&t, "meta:slot")?.unwrap_or(0);
        ensure!(
            u64::from(slot) + traits.len() as u64 <= 65532,
            "sandbox address pool exhausted"
        );
        let mut agents = Vec::with_capacity(traits.len());
        for birth in traits {
            env.validate_birth_models(&birth)?;
            slot += 1;
            let id = new_id();
            let secret = format!(
                "sv_{}{}",
                new_id().replace('-', ""),
                new_id().replace('-', "")
            );
            let hash = token_hash(&secret);
            let a = Agent {
                id: id.clone(),
                experiment_id: env_id.into(),
                token_hash: hash.clone(),
                slot,
                source_ip: format!("10.203.{}.{}", (slot + 1) / 256, (slot + 1) % 256),
                state: AgentState::Queued,
                generation: 0,
                granted: birth.initial_energy,
                used: 0,
                reserved: 0,
                last_error: None,
                created_at: now_ms(),
            };
            let p = OrganismProfile {
                id: id.clone(),
                environment_id: env_id.into(),
                owner: owner.into(),
                name: String::new(),
                created_at: a.created_at,
                remaining_lifetime_ms: birth.lifetime_seconds * 1000,
                last_heartbeat_at: None,
                running_since_ms: None,
                clock_boot_id: String::new(),
                birth,
            };
            put(&mut t, &format!("agent:{id}"), &a)?;
            put(&mut t, &format!("org2:{id}"), &p)?;
            put(&mut t, &format!("credential:{id}"), &secret)?;
            put(&mut t, &format!("token:{hash}"), &id)?;
            event(
                &mut t,
                &id,
                "born",
                json!({"energy":a.granted,"lifetime_seconds":p.birth.lifetime_seconds,"environment_id":env_id}),
            )?;
            agents.push(a);
        }
        put(&mut t, "meta:slot", &slot)?;
        drop(t);
        tx.commit()?;
        Ok(agents)
    }
    pub fn life_state(&self, id: &str) -> Result<LifeState> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let a = load(&t, &format!("agent:{id}"))?;
        let p: OrganismProfile = load(&t, &format!("org2:{id}"))?;
        let (boot, mono) = runtime_clock()?;
        Ok(LifeState::from_parts(&a, &p, p.remaining_at(&boot, mono)))
    }
    pub fn observe_runtime(
        &self,
        id: &str,
        running: bool,
        boot: &str,
        mono: u64,
        wall: u64,
    ) -> Result<LifeState> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("org2:{id}");
        let mut p: OrganismProfile = load(&t, &key)?;
        let a: Agent = load(&t, &format!("agent:{id}"))?;
        let heartbeat =
            wall.saturating_sub(p.last_heartbeat_at.unwrap_or(p.created_at)) >= 3_600_000;
        if !running && p.running_since_ms.is_none() && !heartbeat {
            return Ok(LifeState::from_parts(&a, &p, p.remaining_lifetime_ms));
        }
        // If the process unexpectedly disappeared, only the last observed running
        // checkpoint is known. Host reboot likewise closes the previous interval.
        if running {
            p.remaining_lifetime_ms = p.remaining_at(boot, mono);
        }
        p.running_since_ms = running.then_some(mono);
        p.clock_boot_id = boot.into();
        if heartbeat {
            p.last_heartbeat_at = Some(wall);
        }
        let life = LifeState::from_parts(&a, &p, p.remaining_lifetime_ms);
        if heartbeat {
            event(&mut t, id, "heartbeat", serde_json::to_value(&life)?)?;
        }
        put(&mut t, &key, &p)?;
        drop(t);
        tx.commit()?;
        Ok(life)
    }
    /// Decide the next state after the supervisor confirmed the cgroup stopped.
    /// A concurrent grant must either precede this transaction (we requeue)
    /// or follow it (grant requeues), including when the old state was Running.
    pub fn confirm_resource_stop(&self, id: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("agent:{id}");
        let mut agent: Agent = load(&t, &key)?;
        let profile: OrganismProfile = load(&t, &format!("org2:{id}"))?;
        let state = if profile.remaining_lifetime_ms > 0 && agent.balance() > 0 {
            AgentState::Queued
        } else {
            AgentState::Depleted
        };
        if agent.state != state {
            agent.state = state;
            agent.last_error = None;
            event(
                &mut t,
                id,
                "state",
                json!({"state":agent.state,"generation":agent.generation}),
            )?;
            put(&mut t, &key, &agent)?;
        }
        drop(t);
        tx.commit()?;
        Ok(())
    }
    pub fn rename_organism(&self, id: &str, name: &str) -> Result<OrganismProfile> {
        ensure!(
            !name.trim().is_empty() && name.len() <= 256,
            "invalid organism name"
        );
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let key = format!("org2:{id}");
        let mut p: OrganismProfile = load(&t, &key)?;
        p.name = name.trim().into();
        put(&mut t, &key, &p)?;
        event(&mut t, id, "renamed", json!({"name":p.name}))?;
        drop(t);
        tx.commit()?;
        Ok(p)
    }
    pub fn grant(&self, owner: &str, id: &str, input: GrantInput) -> Result<Grant> {
        check_key(&input.idempotency_key)?;
        ensure!(
            input.energy > 0 || input.lifetime_seconds > 0,
            "positive grant required"
        );
        ensure!(
            input.note.as_ref().is_none_or(|s| s.len() <= 8192),
            "note too long"
        );
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let pkey = format!("org2:{id}");
        let mut p: OrganismProfile = load(&t, &pkey)?;
        require_owner(&p.owner, owner)?;
        let index = format!("grantkey:{id}:{}", token_hash(&input.idempotency_key));
        if let Some(grant_id) = optional::<String>(&t, &index)? {
            let g: Grant = load(&t, &format!("grant:{id}:{grant_id}"))?;
            ensure!(g.input == input, "idempotency_conflict");
            return Ok(g);
        }
        if let Some(report) = &input.report_id {
            let r: LifeReport = load(&t, &format!("report:{report}"))?;
            ensure!(r.organism_id == id, "report does not belong to organism");
        }
        let mut a: Agent = load(&t, &format!("agent:{id}"))?;
        let (boot, mono) = runtime_clock()?;
        if p.running_since_ms.is_some() && p.clock_boot_id == boot {
            p.remaining_lifetime_ms = p.remaining_at(&boot, mono);
            p.running_since_ms = Some(mono);
        }
        p.remaining_lifetime_ms = p
            .remaining_lifetime_ms
            .checked_add(
                input
                    .lifetime_seconds
                    .checked_mul(1000)
                    .context("lifetime overflow")?,
            )
            .context("lifetime overflow")?;
        a.granted = a
            .granted
            .checked_add(input.energy)
            .context("energy overflow")?;
        if a.state == AgentState::Depleted && a.balance() > 0 && p.remaining_lifetime_ms > 0 {
            a.state = AgentState::Queued;
            a.last_error = None;
        }
        let g = Grant {
            id: new_id(),
            organism_id: id.into(),
            owner: owner.into(),
            created_at: now_ms(),
            input,
        };
        put(&mut t, &format!("agent:{id}"), &a)?;
        put(&mut t, &pkey, &p)?;
        put(&mut t, &index, &g.id)?;
        put(&mut t, &format!("grant:{id}:{}", g.id), &g)?;
        let entry = event(
            &mut t,
            id,
            "grant",
            json!({"grant_id":g.id,"energy":g.input.energy,"lifetime_seconds":g.input.lifetime_seconds,"report_id":g.input.report_id}),
        )?;
        put(&mut t, &format!("grant_seq:{id}:{}", entry.id), &g.id)?;
        Self::increment_activity(&mut t, &format!("grant_count:{id}"))?;
        drop(t);
        tx.commit()?;
        Ok(g)
    }
    fn increment_activity(t: &mut Table<'_, &'static str, &'static [u8]>, key: &str) -> Result<()> {
        let count = optional::<u64>(t, key)?
            .unwrap_or(0)
            .checked_add(1)
            .context("activity count overflow")?;
        put(t, key, &count)
    }
    pub fn activity_counts(&self, id: &str) -> Result<(u64, u64)> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        Ok((
            optional(&t, &format!("report_count:{id}"))?.unwrap_or(0),
            optional(&t, &format!("grant_count:{id}"))?.unwrap_or(0),
        ))
    }
    pub fn grant_page(
        &self,
        id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Grant>, Option<String>)> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let limit = limit.clamp(1, 200);
        let mut refs = reference_page(&t, &format!("grant_seq:{id}:"), after, limit + 1, false)?;
        let more = refs.len() > limit;
        refs.truncate(limit);
        let cursor = refs.last().filter(|_| more).map(|v| v.0.clone());
        let rows = refs
            .into_iter()
            .map(|(_, key)| load(&t, &format!("grant:{id}:{key}")))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, cursor))
    }
    pub fn grants(&self, id: &str) -> Result<Vec<Grant>> {
        let mut g: Vec<Grant> = self.list(&format!("grant:{id}:"))?;
        g.sort_by_key(|r| r.created_at);
        Ok(g)
    }
    pub fn report(&self, id: &str) -> Result<LifeReport> {
        self.read(&format!("report:{id}"))
    }
    pub fn existing_report(&self, id: &str, input: &ReportInput) -> Result<Option<LifeReport>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        Self::lookup_report(&t, id, input)
    }
    fn lookup_report(
        t: &impl ReadableTable<&'static str, &'static [u8]>,
        id: &str,
        input: &ReportInput,
    ) -> Result<Option<LifeReport>> {
        let index = format!("reportkey:{id}:{}", token_hash(&input.idempotency_key));
        if let Some(report_id) = optional::<String>(t, &index)? {
            let r: LifeReport = load(t, &format!("report:{report_id}"))?;
            ensure!(&r.input == input, "idempotency_conflict");
            Ok(Some(r))
        } else {
            Ok(None)
        }
    }
    pub fn save_report(
        &self,
        id: &str,
        input: ReportInput,
        snapshots: Value,
    ) -> Result<LifeReport> {
        check_key(&input.idempotency_key)?;
        ensure!(
            !input.body.trim().is_empty() && input.body.len() <= 65536 && input.files.len() <= 16,
            "invalid report body or file count"
        );
        let tx = self.db.begin_write()?;
        let mut t = tx.open_table(RECORDS)?;
        let _: OrganismProfile = load(&t, &format!("org2:{id}"))?;
        if let Some(r) = Self::lookup_report(&t, id, &input)? {
            return Ok(r);
        }
        let r = LifeReport {
            id: new_id(),
            organism_id: id.into(),
            created_at: now_ms(),
            input,
            snapshots,
        };
        let e = event(&mut t, id, "report", json!({"report_id":r.id}))?;
        put(&mut t, &format!("report:{}", r.id), &r)?;
        put(
            &mut t,
            &format!("reportkey:{id}:{}", token_hash(&r.input.idempotency_key)),
            &r.id,
        )?;
        put(&mut t, &format!("reports:{id}:{}", e.id), &r.id)?;
        Self::increment_activity(&mut t, &format!("report_count:{id}"))?;
        drop(t);
        tx.commit()?;
        Ok(r)
    }
    pub fn reports(
        &self,
        id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<LifeReport>, Option<String>)> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(RECORDS)?;
        let refs = reference_page(&t, &format!("reports:{id}:"), after, limit.min(8), false)?;
        let cursor = refs.last().map(|v| v.0.clone());
        let rows = refs
            .into_iter()
            .map(|(_, key)| load(&t, &format!("report:{key}")))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, cursor))
    }
}
