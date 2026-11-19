//! Rootful Linux youki backend. Construction is side-effect free; provisioning is explicit.
//! The manager never starts a workload with missing isolation prerequisites.
mod config;
#[cfg(target_os = "linux")]
mod desktop;
mod init;
mod manager;
mod network;
mod oci;
mod staging;
mod system;

pub use config::{SandboxConfig, SandboxSpec, SandboxStatus, source_ip};
pub use init::{BootManifest, run_init};
pub use manager::SandboxManager;

#[cfg(test)]
mod tests;
