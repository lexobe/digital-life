use crate::{Agent, NetworkPolicy, ResourceLimits, default_model};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentInput {
    pub description: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LifeEnvironment {
    pub id: String,
    pub owner: String,
    pub description: String,
    pub name: String,
    pub note: String,
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    pub created_at: u64,
}

pub(crate) fn validate_allowed_models(models: Option<&[String]>) -> Result<()> {
    if let Some(list) = models {
        ensure!(
            (1..=128).contains(&list.len()) && list.iter().all(|m| !m.is_empty() && m.len() <= 256),
            "invalid allowed models"
        );
    }
    Ok(())
}

impl LifeEnvironment {
    fn allows(&self, model: &str) -> bool {
        self.allowed_models
            .as_ref()
            .is_none_or(|list| list.iter().any(|id| id == model))
    }
    pub fn permits_model(&self, birth: &BirthTraits, model: &str) -> bool {
        self.allows(model) && birth.allows(model)
    }
    pub(crate) fn validate_birth_models(&self, birth: &BirthTraits) -> Result<()> {
        ensure!(
            self.permits_model(birth, &birth.default_model)
                && birth
                    .allowed_models
                    .iter()
                    .flatten()
                    .all(|id| self.allows(id)),
            "birth models must be permitted by the environment"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BirthTraits {
    pub initial_energy: u64,
    pub lifetime_seconds: u64,
    #[serde(default)]
    pub dna: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub network: NetworkPolicy,
}
impl BirthTraits {
    pub fn allows(&self, model: &str) -> bool {
        self.allowed_models
            .as_ref()
            .is_none_or(|list| list.iter().any(|v| v == model))
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.initial_energy > 0
                && self.lifetime_seconds > 0
                && self.lifetime_seconds.checked_mul(1000).is_some(),
            "invalid initial energy or lifetime"
        );
        ensure!(
            serde_json::to_vec(&self.dna)?.len() <= 16384,
            "DNA exceeds 16 KiB"
        );
        ensure!(
            !self.default_model.is_empty()
                && self.default_model.len() <= 256
                && self.allows(&self.default_model),
            "default model must be permitted"
        );
        validate_allowed_models(self.allowed_models.as_deref())?;
        ensure!(
            self.limits.memory_bytes >= 32 * 1024 * 1024
                && (8..=16384).contains(&self.limits.pids)
                && (1..=1_000_000).contains(&self.limits.cpu_millis),
            "invalid resource limits"
        );
        Ok(())
    }
}
/// Partial batch overrides inherit the corresponding field from the birth defaults.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkOverride {
    pub internet: Option<bool>,
    pub peers: Option<bool>,
    pub services: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsOverride {
    pub memory_bytes: Option<u64>,
    pub pids: Option<u32>,
    pub cpu_millis: Option<u64>,
}
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BirthOverride {
    pub initial_energy: Option<u64>,
    pub lifetime_seconds: Option<u64>,
    pub dna: Option<String>,
    pub default_model: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub limits: Option<LimitsOverride>,
    pub network: Option<NetworkOverride>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BirthBatch {
    pub defaults: BirthTraits,
    pub count: u32,
    #[serde(default)]
    pub overrides: Vec<BirthOverride>,
}
impl BirthBatch {
    pub fn resolve(&self) -> Result<Vec<BirthTraits>> {
        ensure!(
            (1..=1000).contains(&self.count)
                && (self.overrides.is_empty() || self.overrides.len() == self.count as usize),
            "invalid batch count or overrides"
        );
        let mut values = Vec::with_capacity(self.count as usize);
        for i in 0..self.count as usize {
            let mut birth = self.defaults.clone();
            if let Some(o) = self.overrides.get(i) {
                if let Some(v) = o.initial_energy {
                    birth.initial_energy = v;
                }
                if let Some(v) = o.lifetime_seconds {
                    birth.lifetime_seconds = v;
                }
                if let Some(v) = &o.dna {
                    birth.dna.clone_from(v);
                }
                if let Some(v) = &o.default_model {
                    birth.default_model.clone_from(v);
                }
                if let Some(v) = &o.allowed_models {
                    birth.allowed_models = Some(v.clone());
                }
                if let Some(v) = &o.limits {
                    let base = &mut birth.limits;
                    base.memory_bytes = v.memory_bytes.unwrap_or(base.memory_bytes);
                    base.cpu_millis = v.cpu_millis.unwrap_or(base.cpu_millis);
                    base.pids = v.pids.unwrap_or(base.pids);
                }
                if let Some(v) = &o.network {
                    let base = &mut birth.network;
                    base.internet = v.internet.unwrap_or(base.internet);
                    base.peers = v.peers.unwrap_or(base.peers);
                    base.services = v.services.unwrap_or(base.services);
                }
            }
            birth.validate()?;
            values.push(birth);
        }
        Ok(values)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrganismProfile {
    pub id: String,
    pub environment_id: String,
    pub owner: String,
    pub name: String,
    pub birth: BirthTraits,
    pub created_at: u64,
    pub remaining_lifetime_ms: u64,
    pub last_heartbeat_at: Option<u64>,
    pub running_since_ms: Option<u64>,
    pub clock_boot_id: String,
}
impl OrganismProfile {
    pub fn remaining_at(&self, boot: &str, mono: u64) -> u64 {
        let elapsed = self
            .running_since_ms
            .filter(|_| self.clock_boot_id == boot)
            .map_or(0, |start| mono.saturating_sub(start));
        self.remaining_lifetime_ms.saturating_sub(elapsed)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LifeState {
    pub energy_balance: u64,
    pub energy_available: u64,
    pub energy_reserved: u64,
    pub remaining_lifetime_ms: u64,
    pub last_heartbeat_at: Option<u64>,
    pub stop_reasons: Vec<String>,
}
impl LifeState {
    pub fn from_parts(a: &Agent, p: &OrganismProfile, remaining: u64) -> Self {
        let mut reasons = vec![];
        if a.balance() == 0 {
            reasons.push("energy".into());
        }
        if remaining == 0 {
            reasons.push("lifetime".into());
        }
        Self {
            energy_balance: a.balance(),
            energy_available: a.available(),
            energy_reserved: a.reserved,
            remaining_lifetime_ms: remaining,
            last_heartbeat_at: p.last_heartbeat_at,
            stop_reasons: reasons,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GrantInput {
    pub idempotency_key: String,
    #[serde(default)]
    pub energy: u64,
    #[serde(default)]
    pub lifetime_seconds: u64,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub report_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub organism_id: String,
    pub owner: String,
    pub created_at: u64,
    #[serde(flatten)]
    pub input: GrantInput,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReportInput {
    pub idempotency_key: String,
    pub body: String,
    #[serde(default)]
    pub files: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LifeReport {
    pub id: String,
    pub organism_id: String,
    pub created_at: u64,
    #[serde(flatten)]
    pub input: ReportInput,
    pub snapshots: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnergyPrice {
    pub input: u64,
    pub output: u64,
}
impl EnergyPrice {
    pub fn charge(self, input_tokens: u64, output_tokens: u64) -> Result<u64> {
        input_tokens
            .checked_mul(self.input)
            .and_then(|i| output_tokens.checked_mul(self.output)?.checked_add(i))
            .context("energy charge overflow")
    }
}

pub fn runtime_clock() -> Result<(String, u64)> {
    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        static BOOT: OnceLock<String> = OnceLock::new();
        let boot = match BOOT.get() {
            Some(b) => b.clone(),
            None => {
                let b = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                    .trim()
                    .to_owned();
                let _ = BOOT.set(b.clone());
                b
            }
        };
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // The monotonic clock excludes suspended host time and survives service restarts.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((
            boot,
            ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000,
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::{sync::OnceLock, time::Instant};
        static START: OnceLock<(String, Instant)> = OnceLock::new();
        let (id, start) = START.get_or_init(|| (crate::new_id(), Instant::now()));
        Ok((id.clone(), start.elapsed().as_millis() as u64))
    }
}
