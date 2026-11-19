use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub fn default_model() -> String {
    "default".into()
}
fn one() -> u32 {
    1
}
fn threshold() -> f64 {
    0.9
}
fn memory() -> u64 {
    256 * 1024 * 1024
}
fn pids() -> u32 {
    64
}
fn cpu() -> u64 {
    500
}
fn yes() -> bool {
    true
}
fn expiry() -> u64 {
    86400
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentInput {
    pub name: String,
    pub goal: String,
    pub initial_prompt: String,
    #[serde(default = "one")]
    pub count: u32,
    pub initial_tokens: u64,
    pub reward_budget: u64,
    #[serde(default = "default_model")]
    pub model: String,
    pub rules: Vec<RewardRule>,
    #[serde(default)]
    pub evidence_sources: Vec<EvidenceSource>,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub individuals: Vec<IndividualSeed>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndividualSeed {
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardRule {
    pub id: String,
    pub instructions: String,
    #[serde(default = "threshold")]
    pub threshold: f64,
    #[serde(default = "threshold")]
    pub min_confidence: f64,
    pub tokens: u64,
    #[serde(default = "one")]
    pub max_awards: u32,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub criteria: Option<serde_json::Value>,
    #[serde(default)]
    pub kind: RewardKind,
    #[serde(default)]
    pub occurrence_path: Option<String>,
    #[serde(default = "expiry")]
    pub expires_after_secs: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RewardKind {
    #[default]
    Noul,
    Score,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSource {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub event_secret_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default = "memory")]
    pub memory_bytes: u64,
    #[serde(default = "pids")]
    pub pids: u32,
    #[serde(default = "cpu")]
    pub cpu_millis: u64,
}
impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_bytes: memory(),
            pids: pids(),
            cpu_millis: cpu(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicy {
    #[serde(default = "yes")]
    pub internet: bool,
    #[serde(default = "yes")]
    pub peers: bool,
    #[serde(default = "yes")]
    pub services: bool,
}
impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            internet: true,
            peers: true,
            services: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    pub id: String,
    pub owner: String,
    pub config: ExperimentInput,
    pub reward_remaining: u64,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub experiment_id: String,
    pub token_hash: String,
    pub slot: u32,
    pub source_ip: String,
    pub state: AgentState,
    pub generation: u64,
    pub granted: u64,
    pub used: u64,
    pub reserved: u64,
    pub last_error: Option<String>,
    pub created_at: u64,
}
impl Agent {
    pub fn available(&self) -> u64 {
        self.granted
            .saturating_sub(self.used)
            .saturating_sub(self.reserved)
    }
    pub fn balance(&self) -> u64 {
        self.granted.saturating_sub(self.used)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Queued,
    Starting,
    Running,
    Depleted,
    Failed,
    Suspended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
impl Usage {
    pub fn total(&self) -> Option<u64> {
        self.input_tokens.checked_add(self.output_tokens)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub price: Option<crate::EnergyPrice>,
    #[serde(default)]
    pub energy_charged: Option<u64>,
    pub id: String,
    pub agent_id: String,
    pub provider: String,
    pub amount: u64,
    pub status: ReservationStatus,
    pub usage: Option<Usage>,
    pub upstream_id: Option<String>,
    pub created_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Reserved,
    Unknown,
    Settled,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub agent_id: String,
    pub kind: String,
    pub data: serde_json::Value,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmissionInput {
    pub rule_id: String,
    pub idempotency_key: String,
    pub description: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub occurrence_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub agent_id: String,
    pub experiment_id: String,
    pub input: SubmissionInput,
    pub evidence: serde_json::Value,
    pub evidence_hash: String,
    pub status: SubmissionStatus,
    pub judge_result: Option<serde_json::Value>,
    pub reward: u64,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionStatus {
    Pending,
    Evaluating,
    Awarded,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseBinding {
    pub agent_id: String,
    pub response_id: String,
    pub provider: String,
    pub body: serde_json::Value,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRegistration {
    pub port: u16,
    pub name: String,
}
