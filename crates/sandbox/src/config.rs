use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::Ipv4Addr,
    path::{Component, Path, PathBuf},
};
use survive_core::{NetworkPolicy, ResourceLimits};

pub const ROUTER: &str = "survive-router";
pub const HOST_LINK: &str = "sv-uplink";
pub const GATEWAY: &str = "10.203.0.1";
pub const GATEWAY_PORT: u16 = 8089;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub data_dir: PathBuf,
    pub rootfs: PathBuf,
    pub youki: PathBuf,
    pub init_binary: PathBuf,
    pub agent_binary: PathBuf,
    pub gateway_url: String,
    pub total_memory_bytes: u64,
    #[serde(default = "cgroup_root")]
    pub cgroup_root: PathBuf,
    /// Reserve 65536 host UIDs/GIDs per slot, starting at this base.
    #[serde(default = "uid_base")]
    pub uid_base: u32,
    /// Public management/control addresses must be included here too.
    #[serde(default)]
    pub denied_cidrs: Vec<String>,
    #[serde(default = "dns")]
    pub dns_servers: Vec<Ipv4Addr>,
    #[serde(default = "timeout")]
    pub command_timeout_secs: u64,
    /// Hard per-agent writable filesystem capacity, preallocated on the host.
    #[serde(default = "disk_bytes")]
    pub disk_bytes: u64,
}
fn cgroup_root() -> PathBuf {
    "/sys/fs/cgroup/survive".into()
}
fn uid_base() -> u32 {
    65536
}
fn dns() -> Vec<Ipv4Addr> {
    vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)]
}
fn timeout() -> u64 {
    30
}
fn disk_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    pub agent_id: String,
    pub experiment_id: String,
    pub slot: u32,
    pub token: String,
    pub limits: ResourceLimits,
    pub network: NetworkPolicy,
}

// Deliberately no Debug: a spec contains the environment credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    Missing,
    Created,
    Running,
    Stopped,
}

pub fn source_ip(slot: u32) -> Result<Ipv4Addr> {
    ensure!((1..=65532).contains(&slot), "slot must be in 1..=65532");
    Ok(Ipv4Addr::new(
        10,
        203,
        ((slot + 1) / 256) as u8,
        ((slot + 1) % 256) as u8,
    ))
}

pub(crate) fn identifier(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid sandbox identifier"
    );
    Ok(())
}
pub(crate) fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path != Path::new("/")
            && path
                .components()
                .all(|p| matches!(p, Component::RootDir | Component::Normal(_))),
        "path must be absolute without traversal: {}",
        path.display()
    );
    ensure!(path.to_str().is_some(), "paths must be UTF-8");
    ensure!(
        !path
            .to_str()
            .unwrap()
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | ':' | '\\')),
        "paths cannot contain whitespace or mount-option separators"
    );
    Ok(())
}
pub(crate) fn cidr(value: &str) -> Result<()> {
    let (ip, prefix) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("expected IPv4 CIDR"))?;
    let _: Ipv4Addr = ip.parse()?;
    ensure!(prefix.parse::<u8>()? <= 32, "invalid IPv4 prefix");
    Ok(())
}
impl SandboxConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        for p in [
            &self.data_dir,
            &self.rootfs,
            &self.youki,
            &self.init_binary,
            &self.agent_binary,
            &self.cgroup_root,
        ] {
            absolute(p)?;
        }
        ensure!(
            self.cgroup_root == Path::new("/sys/fs/cgroup/survive"),
            "only the dedicated /sys/fs/cgroup/survive hierarchy is supported"
        );
        ensure!(
            self.gateway_url.trim_end_matches('/') == "http://10.203.0.1:8089",
            "gateway must be http://10.203.0.1:8089"
        );
        ensure!(
            self.total_memory_bytes >= 64 * 1024 * 1024
                && self.total_memory_bytes <= i64::MAX as u64,
            "invalid total memory limit"
        );
        ensure!(
            self.uid_base >= 65536 && (self.uid_base as u64 + 65532 * 65536) <= u32::MAX as u64,
            "invalid reserved UID range"
        );
        ensure!(
            (1..=120).contains(&self.command_timeout_secs),
            "command timeout must be 1..=120 seconds"
        );
        ensure!(
            (256 * 1024 * 1024..=1024 * 1024 * 1024 * 1024).contains(&self.disk_bytes)
                && self.disk_bytes.is_multiple_of(4096),
            "disk_bytes must be 256 MiB..=1 TiB, aligned to 4096 bytes"
        );
        ensure!(
            !self.dns_servers.is_empty()
                && self.dns_servers.iter().all(|ip| !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && !ip.is_private()
                    && !ip.is_link_local()),
            "DNS must use public IPv4 addresses"
        );
        for c in &self.denied_cidrs {
            cidr(c)?;
        }
        ensure!(
            !self.data_dir.starts_with(&self.rootfs) && !self.rootfs.starts_with(&self.data_dir),
            "rootfs and data_dir must be separate trees"
        );
        Ok(())
    }
}
impl SandboxSpec {
    pub(crate) fn validate(&self, config: &SandboxConfig) -> Result<()> {
        identifier(&self.agent_id)?;
        identifier(&self.experiment_id)?;
        source_ip(self.slot)?;
        ensure!(
            !self.token.is_empty()
                && self.token.len() <= 4096
                && !self
                    .token
                    .bytes()
                    .any(|b| b == 0 || b == b'\n' || b == b'\r'),
            "invalid environment token"
        );
        ensure!(
            (16 * 1024 * 1024..=config.total_memory_bytes).contains(&self.limits.memory_bytes),
            "invalid agent memory limit"
        );
        ensure!(
            (8..=16384).contains(&self.limits.pids),
            "pids limit must be 8..=16384"
        );
        ensure!(
            (1..=1_000_000).contains(&self.limits.cpu_millis),
            "invalid CPU limit"
        );
        Ok(())
    }
    pub(crate) fn runtime_id(&self) -> String {
        format!("survive-{}", self.agent_id)
    }
    pub(crate) fn hostname(&self) -> String {
        format!("agent-{}", self.slot)
    }
    pub(crate) fn netns(&self) -> String {
        format!("sv-{}", self.slot)
    }
    pub(crate) fn link(&self) -> String {
        format!("sa{}", self.slot)
    }
    pub(crate) fn uid(&self, config: &SandboxConfig) -> u32 {
        config.uid_base + (self.slot - 1) * 65536
    }
}
