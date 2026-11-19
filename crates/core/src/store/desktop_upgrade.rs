use super::*;

impl Store {
    /// The offline caller holds the sandbox installation lock and has confirmed a
    /// stopped runtime with a validated host spec. Only this organism may change.
    pub fn upgrade_desktop_limits(
        &self,
        id: &str,
        environment_id: &str,
        slot: u32,
        credential_hash: &str,
        network: &NetworkPolicy,
        limits: &ResourceLimits,
    ) -> Result<AgentState> {
        let tx = self.db.begin_write()?;
        let mut table = tx.open_table(RECORDS)?;
        let agent_key = format!("agent:{id}");
        let profile_key = format!("org2:{id}");
        let mut agent: Agent = load(&table, &agent_key)?;
        let mut profile: OrganismProfile = load(&table, &profile_key)?;
        ensure!(
            agent.id == id
                && profile.id == id
                && agent.slot == slot
                && agent.experiment_id == environment_id
                && profile.environment_id == environment_id
                && agent.token_hash == credential_hash,
            "prepared sandbox identity does not match the stored organism"
        );
        ensure!(
            profile.birth.network == *network,
            "prepared network policy differs from birth"
        );
        ensure!(
            profile.running_since_ms.is_none(),
            "runtime clock must already be paused"
        );
        if let Some(previous) = &profile.runtime_limits {
            ensure!(
                previous == limits,
                "desktop upgrade conflict: different limits already applied"
            );
            return Ok(agent.state);
        }
        let old_state = agent.state.clone();
        agent.state = if agent.available() > 0 && profile.remaining_lifetime_ms > 0 {
            AgentState::Queued
        } else {
            AgentState::Depleted
        };
        agent.last_error = None;
        profile.runtime_limits = Some(limits.clone());
        put(&mut table, &agent_key, &agent)?;
        put(&mut table, &profile_key, &profile)?;
        event(
            &mut table,
            id,
            "desktop_upgrade",
            json!({"old_limits": profile.birth.limits, "limits": limits,
                "old_state": old_state, "state": agent.state, "generation": agent.generation}),
        )?;
        drop(table);
        tx.commit()?;
        Ok(agent.state)
    }
}
