use crate::{SandboxConfig, SandboxSpec, SandboxStatus, source_ip};
use crate::{
    config::{GATEWAY, HOST_LINK, ROUTER, identifier},
    network, oci, staging,
    system::{self, Lock},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct SandboxManager {
    config: Arc<SandboxConfig>,
}

impl SandboxManager {
    /// Validate configuration only. No host mutation and no runtime fallback.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config: Arc::new(config),
        })
    }
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }
    pub fn work_dir(&self, id: &str) -> Result<PathBuf> {
        identifier(id)?;
        Ok(self.dir(id).join("work"))
    }
    pub async fn prepare(&self, spec: &SandboxSpec) -> Result<()> {
        let this = self.clone();
        let spec = spec.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = this.lock()?;
            this.prepare_sync(&spec)
        })
        .await?
    }
    pub async fn start(&self, id: &str) -> Result<()> {
        identifier(id)?;
        let this = self.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let _lock = this.lock()?;
            this.start_sync(&id)
        })
        .await?
    }
    pub async fn stop(&self, id: &str) -> Result<()> {
        identifier(id)?;
        let this = self.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let _lock = this.lock()?;
            this.stop_sync(&id)
        })
        .await?
    }
    pub async fn status(&self, id: &str) -> Result<SandboxStatus> {
        identifier(id)?;
        let this = self.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let _lock = this.lock()?;
            this.status_sync(&id)
        })
        .await?
    }
    /// Keep other sandbox operations excluded while an offline operator commits
    /// maintenance metadata for a verified, stopped individual.
    pub async fn with_stopped_spec<T: Send + 'static>(
        &self,
        id: &str,
        apply: impl FnOnce(&SandboxSpec) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        identifier(id)?;
        let this = self.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let _lock = this.lock()?;
            ensure!(
                this.status_sync(&id)? == SandboxStatus::Stopped,
                "sandbox must be stopped"
            );
            let spec = this.spec(&id)?;
            let cgroup = this.cgroup(&spec);
            if cgroup.try_exists()? {
                ensure!(
                    system::text(&cgroup.join("cgroup.events"))?
                        .lines()
                        .any(|l| l == "populated 0"),
                    "stopped sandbox cgroup must be empty"
                );
            }
            apply(&spec)
        })
        .await?
    }
    /// Read all prepared individuals under one installation lock. Youki 0.7's
    /// `list` command emits a human table and does not refresh process state, so
    /// use the authoritative state operation for extant runtime records. Stopped
    /// records are deleted by stop and need no child process on subsequent ticks.
    pub async fn statuses(&self) -> Result<std::collections::BTreeMap<String, SandboxStatus>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = this.lock()?;
            let mut statuses = std::collections::BTreeMap::new();
            for spec in this.all_specs()? {
                statuses.insert(spec.agent_id.clone(), this.status_sync(&spec.agent_id)?);
            }
            Ok(statuses)
        })
        .await?
    }
    /// Youki shares the host kernel; only configured individual RAM is reserved.
    pub fn memory_reservation(&self, limits: &survive_core::ResourceLimits) -> u64 {
        limits.memory_bytes
    }
    /// The server authenticates observers and validates service registration.
    /// This route uses the individual's directly routed namespace address.
    pub fn service_endpoint(&self, id: &str, port: u16) -> Result<String> {
        ensure!(port != 0, "invalid service port");
        let spec = self.spec(id)?;
        ensure!(spec.network.services, "services disabled");
        Ok(format!("http://{}:{port}/", source_ip(spec.slot)?))
    }
    /// Read-only preflight. Does not install packages, change sysctls or firewall.
    pub async fn doctor(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.doctor_sync()).await?
    }
    /// Explicit operator action; never called by prepare/start/doctor.
    pub async fn setup(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.setup_sync()).await?
    }
    /// Rebuild volatile isolation after a proven host reboot. Never starts agents.
    pub async fn restore(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.restore_sync()).await?
    }
    fn dir(&self, id: &str) -> PathBuf {
        self.config.data_dir.join("agents").join(id)
    }
    fn runtime_root(&self) -> PathBuf {
        self.config.data_dir.join("youki")
    }
    fn cgroup(&self, spec: &SandboxSpec) -> PathBuf {
        self.config.cgroup_root.join(spec.runtime_id())
    }
    fn timeout(&self) -> u64 {
        self.config.command_timeout_secs
    }
    fn command(&self, name: &str, args: &[&str]) -> Result<String> {
        system::command(name, args, self.timeout())
    }
    fn network_command(&self, stage: &str, args: &[&str]) -> Result<()> {
        self.command("ip", args)
            .with_context(|| format!("{stage}: ip {}", args.join(" ")))?;
        Ok(())
    }
    fn router(&self, name: &str, args: &[&str]) -> Result<String> {
        system::ns_command(ROUTER, name, args, None, self.timeout())
    }
    fn runtime(&self, spec: &SandboxSpec, args: &[&str]) -> Result<String> {
        let operation = args.first().context("missing runtime operation")?;
        ensure!(
            matches!(*operation, "create" | "start" | "state" | "kill" | "delete"),
            "unexpected runtime operation"
        );
        let dir = self.dir(&spec.agent_id);
        system::trusted(&dir)?;
        let log_path = dir.join(format!("youki-{operation}.log"));
        let log = system::RuntimeLog::new(&log_path)?;
        let result = self.runtime_capture(spec, args, &log);
        let captured = log.finish();
        match result {
            Ok(output) => {
                captured?;
                Ok(output)
            }
            Err(error) => {
                let tail =
                    captured.unwrap_or_else(|e| format!("cannot capture runtime error log: {e}"));
                let context = format!(
                    "youki {operation} failed for {}; log {} (max {} bytes): {}",
                    spec.agent_id,
                    log_path.display(),
                    system::RUNTIME_LOG_BYTES,
                    tail.trim()
                );
                Err(error.context(context))
            }
        }
    }
    fn runtime_capture(
        &self,
        spec: &SandboxSpec,
        args: &[&str],
        log: &system::RuntimeLog,
    ) -> Result<String> {
        let operation = args.first().context("missing runtime operation")?;
        let root = self.runtime_root();
        let mut argv = vec![
            "--root",
            root.to_str().unwrap(),
            "--log",
            log.path().to_str().unwrap(),
            "--log-level",
            "error",
            "--log-format",
            "json",
        ];
        argv.extend_from_slice(args);
        system::run_runtime(
            &self.config.youki,
            &argv,
            self.timeout(),
            *operation == "create",
        )
        .with_context(|| format!("youki {operation} for {}", spec.agent_id))
    }
    fn lock(&self) -> Result<Lock> {
        system::linux_root()?;
        system::trusted(&self.config.data_dir)?;
        Lock::acquire(&self.config.data_dir.join("manager.lock"))
    }
    fn spec(&self, id: &str) -> Result<SandboxSpec> {
        identifier(id)?;
        let path = self.dir(id).join("spec.json");
        system::trusted(&path)?;
        let s: SandboxSpec = serde_json::from_slice(&fs::read(path)?)?;
        s.validate(&self.config)?;
        ensure!(s.agent_id == id, "metadata identity mismatch");
        Ok(s)
    }
    fn all_specs(&self) -> Result<Vec<SandboxSpec>> {
        let mut specs = Vec::new();
        for entry in fs::read_dir(self.config.data_dir.join("agents"))? {
            let entry = entry?;
            if entry.path().join("spec.json").exists() {
                specs.push(
                    self.spec(
                        entry
                            .file_name()
                            .to_str()
                            .context("invalid agent directory")?,
                    )?,
                );
            }
        }
        specs.sort_by_key(|s| s.slot);
        ensure!(
            specs.windows(2).all(|s| s[0].slot != s[1].slot),
            "duplicate source-IP slot in metadata"
        );
        Ok(specs)
    }
    fn base_checks(&self) -> Result<()> {
        system::linux_root()?;
        let data_mount = if self.config.data_dir.exists() {
            &self.config.data_dir
        } else {
            self.config.data_dir.parent().context("data parent")?
        };
        let fs_type = self.command(
            "findmnt",
            &["-n", "-o", "FSTYPE", "-T", data_mount.to_str().unwrap()],
        )?;
        ensure!(
            matches!(fs_type.trim(), "ext4" | "xfs"),
            "data_dir must use ext4 or XFS to guarantee preallocated image disk bounds"
        );
        for tool in [
            "ip",
            "nft",
            "iptables",
            "mount",
            "umount",
            "findmnt",
            "cp",
            "fallocate",
            "mkfs.ext4",
            "sysctl",
        ] {
            system::tool(tool)?;
        }
        for path in [
            &self.config.rootfs,
            &self.config.youki,
            &self.config.init_binary,
            &self.config.agent_binary,
        ] {
            system::trusted(path)?;
        }
        for path in [
            "/bin/sh",
            "/usr/bin/cc",
            "/usr/bin/cargo",
            "/usr/bin/rustc",
            "/usr/bin/python3",
            "/usr/bin/curl",
            "/usr/bin/Xvfb",
            "/usr/bin/openbox",
            "/usr/bin/chromium",
            "/usr/bin/xdotool",
            "/usr/bin/xclip",
            "/usr/bin/scrot",
            "/usr/bin/xterm",
        ] {
            ensure!(
                rootfs_path(&self.config.rootfs, Path::new(path)).is_ok(),
                "rootfs lacks compilation/runtime tool {path}"
            );
        }
        ensure!(
            system::text(Path::new("/proc/sys/net/ipv4/ip_forward"))? == "1",
            "host IPv4 forwarding must already be enabled; doctor/setup do not change global forwarding"
        );
        let controllers = system::text(Path::new("/sys/fs/cgroup/cgroup.controllers"))?;
        for name in ["cpu", "memory", "pids"] {
            ensure!(
                controllers.split_whitespace().any(|s| s == name),
                "cgroup v2 {name} controller missing"
            );
        }
        let features: Value = serde_json::from_str(&system::run(
            &self.config.youki,
            &["features"],
            None,
            self.timeout(),
        )?)?;
        ensure!(
            features.pointer("/linux/cgroup/v2") == Some(&Value::Bool(true)),
            "youki must support cgroup v2"
        );
        // Some youki releases omit seccomp from `features`; their version output
        // carries the compile-time feature instead. Actual filter activation is
        // independently verified on /proc/PID/status before networking is up.
        let version = system::run(&self.config.youki, &["--version"], None, self.timeout())?;
        ensure!(
            features.pointer("/linux/seccomp/enabled") == Some(&Value::Bool(true))
                || version.lines().any(|l| l.starts_with("libseccomp:")),
            "youki must be built with seccomp"
        );
        for ns in ["mount", "pid", "network", "ipc", "uts", "user", "cgroup"] {
            ensure!(
                features
                    .pointer("/linux/namespaces")
                    .and_then(Value::as_array)
                    .is_some_and(|n| n.iter().any(|v| v == ns)),
                "youki lacks {ns} namespace support"
            );
        }
        ensure!(
            self.command("iptables", &["--version"])?
                .contains("nf_tables"),
            "iptables-nft is required; do not migrate a live Docker host automatically"
        );
        Ok(())
    }
    fn firewall_snapshot(&self, router: bool) -> Result<String> {
        if router {
            self.router("nft", &["-s", "list", "table", "inet", "survive"])
        } else {
            self.command("nft", &["-s", "list", "table", "inet", "survive_host"])
        }
    }
    fn save_router_snapshot(&self) -> Result<()> {
        system::atomic_write(
            &self.config.data_dir.join("host-firewall.expected"),
            self.firewall_snapshot(false)?.as_bytes(),
        )?;
        system::atomic_write(
            &self.config.data_dir.join("router-firewall.expected"),
            self.firewall_snapshot(true)?.as_bytes(),
        )
    }
    fn check_forwarding_jumps(&self) -> Result<()> {
        let rules = self.command("iptables", &["-w", "5", "-S", "FORWARD"])?;
        let top: Vec<_> = rules
            .lines()
            .filter(|l| l.starts_with("-A "))
            .take(2)
            .collect();
        ensure!(
            top.len() == 2
                && top.contains(&"-A FORWARD -i sv-uplink -j SURVIVE-FWD")
                && top.contains(&"-A FORWARD -o sv-uplink -j SURVIVE-FWD"),
            "sandbox FORWARD jumps missing/reordered; fail closed, inspect firewall manually"
        );
        let chain = self.command("iptables", &["-w", "5", "-S", "SURVIVE-FWD"])?;
        let expected = "-N SURVIVE-FWD\n-A SURVIVE-FWD -s 10.203.0.0/16 -i sv-uplink -j ACCEPT\n-A SURVIVE-FWD -d 10.203.0.0/16 -o sv-uplink -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT\n-A SURVIVE-FWD -j DROP\n";
        ensure!(chain == expected, "owned forwarding chain changed");
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-C",
                "INPUT",
                "-i",
                HOST_LINK,
                "-s",
                "10.203.0.0/16",
                "-d",
                "10.203.0.1/32",
                "-p",
                "tcp",
                "--dport",
                "8089",
                "-j",
                "ACCEPT",
            ],
        )?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-C",
                "INPUT",
                "-i",
                HOST_LINK,
                "-s",
                "10.203.0.0/16",
                "-d",
                "10.203.0.1/32",
                "-p",
                "tcp",
                "-m",
                "conntrack",
                "--ctstate",
                "ESTABLISHED",
                "--ctdir",
                "REPLY",
                "-j",
                "ACCEPT",
            ],
        )?;
        Ok(())
    }
    fn doctor_sync(&self) -> Result<()> {
        self.base_checks()?;
        system::trusted(&self.config.data_dir)?;
        ensure!(
            system::text(&self.config.data_dir.join("boot-id"))? == boot_id()?,
            "host reboot detected: use explicit restore, then start agents individually"
        );
        let installed: SandboxConfig = serde_json::from_slice(
            &fs::read(self.config.data_dir.join("installation.json"))
                .context("run explicit sandbox setup first")?,
        )?;
        ensure!(
            serde_json::to_value(installed)? == serde_json::to_value(&*self.config)?,
            "config differs from installed configuration; manual maintenance required"
        );
        for name in ["agents", "youki"] {
            system::trusted(&self.config.data_dir.join(name))?;
        }
        for (name, router) in [
            ("host-firewall.expected", false),
            ("router-firewall.expected", true),
        ] {
            let path = self.config.data_dir.join(name);
            system::trusted(&path)?;
            ensure!(
                fs::read_to_string(path)? == self.firewall_snapshot(router)?,
                "sandbox firewall drift detected ({name})"
            );
        }
        self.check_forwarding_jumps()?;
        ensure!(
            self.router("sysctl", &["-n", "net.ipv4.ip_forward"])?
                .trim()
                == "1",
            "router forwarding disabled"
        );
        let cg = &self.config.cgroup_root;
        ensure!(
            system::text(&cg.join("memory.max"))? == self.config.total_memory_bytes.to_string(),
            "aggregate memory limit mismatch"
        );
        ensure!(
            system::text(&cg.join("memory.swap.max"))? == "0",
            "aggregate swap must be disabled"
        );
        ensure!(
            cg.join("cgroup.kill").is_file(),
            "kernel must support cgroup.kill"
        );
        ensure!(
            system::text(&cg.join("cgroup.procs"))?.is_empty(),
            "parent cgroup must contain no processes"
        );
        let enabled = system::text(&cg.join("cgroup.subtree_control"))?;
        for name in ["cpu", "memory", "pids"] {
            ensure!(
                enabled.split_whitespace().any(|s| s == name),
                "parent {name} controller not enabled"
            );
        }
        let addresses: Value =
            serde_json::from_str(&self.command("ip", &["-j", "addr", "show", "dev", HOST_LINK])?)?;
        for ip in ["10.204.0.1", GATEWAY] {
            ensure!(
                addresses
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v["addr_info"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v["local"] == ip)))),
                "host sandbox address {ip} missing"
            );
        }
        Ok(())
    }
    fn setup_sync(&self) -> Result<()> {
        self.base_checks()?;
        if self.config.data_dir.join("installation.json").exists() {
            return self.doctor_sync();
        }
        if !self.config.data_dir.exists() {
            system::trusted(
                self.config
                    .data_dir
                    .parent()
                    .context("data directory parent")?,
            )?;
            system::private_dir(&self.config.data_dir)?;
        }
        let _lock = self.lock()?;
        ensure!(
            !self.config.data_dir.join("setup-started").exists(),
            "incomplete setup retained; inspect manually, no automatic rollback or retry"
        );
        self.check_no_volatile_resources()?;
        system::atomic_write(
            &self.config.data_dir.join("setup-started"),
            b"Partial failure requires manual inspection.\n",
        )?;
        self.provision()?;
        self.finish_installation()?;
        self.doctor_sync()
    }
    fn restore_sync(&self) -> Result<()> {
        self.base_checks()?;
        let _lock = self.lock()?;
        let installed: SandboxConfig =
            serde_json::from_slice(&fs::read(self.config.data_dir.join("installation.json"))?)?;
        ensure!(
            serde_json::to_value(installed)? == serde_json::to_value(&*self.config)?,
            "restore config differs from installation"
        );
        let old_boot = system::text(&self.config.data_dir.join("boot-id"))?;
        identifier(&old_boot)?;
        let current_boot = boot_id()?;
        ensure!(
            old_boot != current_boot,
            "restore is only permitted after a proven reboot; same-boot firewall drift requires manual investigation"
        );
        ensure!(
            !self
                .config
                .data_dir
                .join(format!("restore-{current_boot}"))
                .exists(),
            "incomplete restore retained; inspect manually"
        );
        self.check_no_volatile_resources()?;
        let specs = self.all_specs()?;
        for spec in &specs {
            ensure!(
                self.dir(&spec.agent_id).join("prepared").exists(),
                "incomplete agent prepare prevents restore"
            );
        }
        system::atomic_write(
            &self.config.data_dir.join(format!("restore-{current_boot}")),
            b"1\n",
        )?;
        // Old youki records contain dead/reusable host PIDs. Quarantine them;
        // NEVER run delete/kill against records from a previous boot.
        fs::rename(
            self.runtime_root(),
            self.config.data_dir.join(format!("youki-{old_boot}")),
        )?;
        self.provision()?;
        for spec in &specs {
            self.mount_storage(spec)?;
            self.mount_overlay(spec)?;
        }
        network::apply(&self.config, &specs)?;
        self.finish_installation()?;
        self.doctor_sync()
    }
    fn check_no_volatile_resources(&self) -> Result<()> {
        ensure!(
            !Path::new("/run/netns").join(ROUTER).exists()
                && !Path::new("/sys/class/net").join(HOST_LINK).exists(),
            "sandbox namespace/interface name already exists"
        );
        ensure!(
            !self.config.cgroup_root.exists(),
            "dedicated cgroup already exists without installation record"
        );
        let tables: Value = serde_json::from_str(&self.command("nft", &["-j", "list", "tables"])?)?;
        ensure!(
            !tables["nftables"]
                .as_array()
                .context("nft tables")?
                .iter()
                .any(|t| t["table"]["name"] == "survive_host"),
            "sandbox firewall table already exists"
        );
        let routes: Value =
            serde_json::from_str(&self.command("ip", &["-j", "route", "show", "table", "all"])?)?;
        for route in routes.as_array().context("routes")? {
            if let Some(dst) = route["dst"].as_str() {
                ensure!(
                    !overlaps_reserved(dst),
                    "existing route overlaps sandbox range: {dst}"
                );
            }
        }
        // Existing owned jumps/chains indicate partial/persisted configuration;
        // do not duplicate them or replace an operator's firewall on restore.
        let rules = self.command("iptables", &["-w", "5", "-S"])?;
        ensure!(
            !rules.contains("SURVIVE-FWD") && !rules.contains(HOST_LINK),
            "owned iptables state already exists; inspect before provisioning"
        );
        Ok(())
    }
    fn provision(&self) -> Result<()> {
        for name in ["agents", "youki"] {
            system::private_dir(&self.config.data_dir.join(name))?;
        }
        fs::write(
            "/sys/fs/cgroup/cgroup.subtree_control",
            "+cpu +memory +pids",
        )?;
        fs::create_dir(&self.config.cgroup_root)?;
        fs::write(
            self.config.cgroup_root.join("memory.max"),
            self.config.total_memory_bytes.to_string(),
        )?;
        fs::write(self.config.cgroup_root.join("memory.swap.max"), "0")?;
        fs::write(
            self.config.cgroup_root.join("cgroup.subtree_control"),
            "+cpu +memory +pids",
        )?;
        self.command("ip", &["netns", "add", ROUTER])?;
        self.router(
            "sysctl",
            &[
                "-q",
                "-w",
                "net.ipv4.ip_forward=1",
                "net.ipv6.conf.all.disable_ipv6=1",
                "net.ipv6.conf.default.disable_ipv6=1",
                "net.ipv4.conf.all.accept_redirects=0",
                "net.ipv4.conf.default.accept_redirects=0",
                "net.ipv4.conf.all.send_redirects=0",
                "net.ipv4.conf.default.send_redirects=0",
            ],
        )?;
        self.command(
            "ip",
            &[
                "link",
                "add",
                HOST_LINK,
                "type",
                "veth",
                "peer",
                "name",
                "sv-router-up",
            ],
        )?;
        self.command("ip", &["link", "set", "sv-router-up", "netns", ROUTER])?;
        self.command(
            "ip",
            &[
                "-n",
                ROUTER,
                "link",
                "set",
                "sv-router-up",
                "name",
                "uplink",
            ],
        )?;
        self.command("ip", &["addr", "add", "10.204.0.1/30", "dev", HOST_LINK])?;
        self.command("ip", &["addr", "add", "10.203.0.1/32", "dev", HOST_LINK])?;
        self.command(
            "ip",
            &[
                "-n",
                ROUTER,
                "addr",
                "add",
                "10.204.0.2/30",
                "dev",
                "uplink",
            ],
        )?;
        self.command("ip", &["-n", ROUTER, "link", "set", "lo", "up"])?;
        // Install all drops before raising the uplink.
        self.command("nft", &["add", "table", "inet", "survive_host"])?;
        system::run(
            &system::tool("nft")?,
            &["-f", "-"],
            Some(&network::host_rules(&[])?),
            self.timeout(),
        )?;
        self.router("nft", &["add", "table", "inet", "survive"])?;
        network::apply(&self.config, &[])?;
        self.command("iptables", &["-w", "5", "-N", "SURVIVE-FWD"])?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-A",
                "SURVIVE-FWD",
                "-i",
                HOST_LINK,
                "-s",
                "10.203.0.0/16",
                "-j",
                "ACCEPT",
            ],
        )?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-A",
                "SURVIVE-FWD",
                "-o",
                HOST_LINK,
                "-d",
                "10.203.0.0/16",
                "-m",
                "conntrack",
                "--ctstate",
                "ESTABLISHED,RELATED",
                "-j",
                "ACCEPT",
            ],
        )?;
        self.command("iptables", &["-w", "5", "-A", "SURVIVE-FWD", "-j", "DROP"])?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-I",
                "FORWARD",
                "1",
                "-i",
                HOST_LINK,
                "-j",
                "SURVIVE-FWD",
            ],
        )?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-I",
                "FORWARD",
                "1",
                "-o",
                HOST_LINK,
                "-j",
                "SURVIVE-FWD",
            ],
        )?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-I",
                "INPUT",
                "1",
                "-i",
                HOST_LINK,
                "-s",
                "10.203.0.0/16",
                "-d",
                "10.203.0.1/32",
                "-p",
                "tcp",
                "--dport",
                "8089",
                "-j",
                "ACCEPT",
            ],
        )?;
        self.command(
            "iptables",
            &[
                "-w",
                "5",
                "-I",
                "INPUT",
                "1",
                "-i",
                HOST_LINK,
                "-s",
                "10.203.0.0/16",
                "-d",
                "10.203.0.1/32",
                "-p",
                "tcp",
                "-m",
                "conntrack",
                "--ctstate",
                "ESTABLISHED",
                "--ctdir",
                "REPLY",
                "-j",
                "ACCEPT",
            ],
        )?;
        self.command("ip", &["link", "set", HOST_LINK, "up"])?;
        self.command("ip", &["-n", ROUTER, "link", "set", "uplink", "up"])?;
        self.command(
            "ip",
            &["-n", ROUTER, "route", "add", "default", "via", "10.204.0.1"],
        )?;
        self.command(
            "ip",
            &[
                "route",
                "add",
                "10.203.0.0/16",
                "via",
                "10.204.0.2",
                "dev",
                HOST_LINK,
            ],
        )?;
        Ok(())
    }
    fn finish_installation(&self) -> Result<()> {
        self.save_router_snapshot()?;
        system::atomic_write(
            &self.config.data_dir.join("installation.json"),
            &serde_json::to_vec_pretty(&*self.config)?,
        )?;
        system::atomic_write(&self.config.data_dir.join("boot-id"), boot_id()?.as_bytes())
    }
    fn prepare_sync(&self, spec: &SandboxSpec) -> Result<()> {
        spec.validate(&self.config)?;
        self.doctor_sync()?;
        let dir = self.dir(&spec.agent_id);
        let mut specs = self.all_specs()?;
        if let Some(existing) = specs.iter().find(|s| s.agent_id == spec.agent_id) {
            ensure!(
                serde_json::to_value(existing)? == serde_json::to_value(spec)?,
                "prepared sandbox spec is immutable; use a new identity for changed policy/limits"
            );
            ensure!(
                dir.join("prepared").is_file(),
                "incomplete prepare retained; inspect manually, no automatic rollback"
            );
            // Only the platform-owned seed executable follows platform updates. A custom
            // boot entry, writable sources and journal remain under the organism's control.
            if self.status_sync(&spec.agent_id)? == SandboxStatus::Stopped {
                refresh_platform_seed(&self.config.agent_binary, &dir.join("assets"))?;
            }
            return Ok(());
        }
        ensure!(
            !specs.iter().any(|s| s.slot == spec.slot),
            "source IP slot already reserved"
        );
        system::private_dir(&dir)?;
        system::atomic_write(&dir.join("spec.json"), &serde_json::to_vec_pretty(spec)?)?;
        // Preserve all parent-seeded work, including boot.json. No default boot
        // is installed: a missing/broken manifest is reported by init unchanged.
        let work = dir.join("work");
        if !work.exists() {
            system::private_dir(&work)?;
        }
        ensure!(
            !fs::symlink_metadata(&work)?.file_type().is_symlink(),
            "work must be a real directory"
        );
        for name in ["assets", "lower", "storage", "rootfs"] {
            system::private_dir(&dir.join(name))?;
        }
        #[cfg(unix)]
        fs::set_permissions(dir.join("assets"), fs::Permissions::from_mode(0o755))?;
        self.create_storage(spec)?;
        for (source, name) in [
            (&self.config.init_binary, "survive-init"),
            (&self.config.agent_binary, "survive-agent"),
        ] {
            fs::copy(source, dir.join("assets").join(name))?;
            #[cfg(unix)]
            fs::set_permissions(
                dir.join("assets").join(name),
                fs::Permissions::from_mode(0o555),
            )?;
        }
        // The read-only template is never modified. Reflinks avoid copying file
        // data where supported. Shift the private lower tree before exposure;
        // ownership preserves non-root package users inside the UID namespace.
        let template = format!("{}/.", self.config.rootfs.display());
        system::command(
            "cp",
            &[
                "-a",
                "--reflink=auto",
                &template,
                dir.join("lower").to_str().unwrap(),
            ],
            // ext4 must physically copy the entire desktop template at birth.
            self.timeout().max(180),
        )?;
        scaffold(&dir.join("lower"))?;
        #[cfg(unix)]
        install_guest_hosts(&dir, &spec.hostname())?;
        system::shift_tree(&dir.join("lower"), spec.uid(&self.config))?;
        let resolv = self
            .config
            .dns_servers
            .iter()
            .map(|ip| format!("nameserver {ip}\n"))
            .collect::<String>();
        system::atomic_write(&dir.join("resolv.conf"), resolv.as_bytes())?;
        #[cfg(unix)]
        fs::set_permissions(dir.join("resolv.conf"), fs::Permissions::from_mode(0o444))?;
        self.mount_overlay(spec)?;
        specs.push(spec.clone());
        specs.sort_by_key(|s| s.slot);
        network::apply(&self.config, &specs)?;
        self.save_router_snapshot()?;
        system::atomic_write(
            &dir.join("config.json"),
            &serde_json::to_vec_pretty(&oci::bundle(&self.config, spec))?,
        )?;
        system::atomic_write(&dir.join("prepared"), b"1\n")?;
        Ok(())
    }
    fn mount_record(&self, target: &Path) -> Result<Option<Value>> {
        let mounts: Value = serde_json::from_str(&self.command(
            "findmnt",
            &["--json", "--list", "--output", "TARGET,SOURCE,FSTYPE"],
        )?)?;
        Ok(mounts["filesystems"]
            .as_array()
            .and_then(|a| a.iter().find(|m| m["target"] == target.to_str().unwrap()))
            .cloned())
    }
    fn create_storage(&self, spec: &SandboxSpec) -> Result<()> {
        let dir = self.dir(&spec.agent_id);
        let image = dir.join("storage.ext4");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&image)?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        drop(file);
        // A sparse file would let untrusted writes exhaust the host later.
        // Reserve the full capacity now, before admitting any workload.
        self.command(
            "fallocate",
            &[
                "-l",
                &self.config.disk_bytes.to_string(),
                image.to_str().unwrap(),
            ],
        )?;
        self.command(
            "mkfs.ext4",
            &[
                "-q",
                "-F",
                "-m",
                "0",
                "-E",
                "nodiscard,lazy_itable_init=0,lazy_journal_init=0",
                image.to_str().unwrap(),
            ],
        )?;
        self.command(
            "mount",
            &[
                "-t",
                "ext4",
                "-o",
                "loop,nosuid,nodev,noatime",
                image.to_str().unwrap(),
                dir.join("storage").to_str().unwrap(),
            ],
        )?;
        for name in ["work", "upper", "overlay-work"] {
            system::private_dir(&dir.join("storage").join(name))?;
        }
        // Overlay root metadata comes from the upper root. Set ownership once
        // on a new image; never chown a live view or existing agent filesystem.
        system::chown(
            &dir.join("storage/upper"),
            spec.uid(&self.config),
            spec.uid(&self.config),
        )?;
        #[cfg(unix)]
        fs::set_permissions(
            dir.join("storage/upper"),
            fs::Permissions::from_mode(fs::metadata(&self.config.rootfs)?.mode() & 0o777),
        )?;
        let seeds = format!("{}/.", dir.join("work").display());
        self.command(
            "cp",
            &[
                "-a",
                "--reflink=auto",
                &seeds,
                dir.join("storage/work").to_str().unwrap(),
            ],
        )?;
        shift_seed(&dir.join("storage/work"), spec.uid(&self.config))?;
        // The guest browser runs as nobody. It needs search permission to reach
        // its own home under /work. Parent host staging remains slot-private.
        #[cfg(unix)]
        {
            let work = dir.join("storage/work");
            let mode = fs::metadata(&work)?.mode() & 0o7777;
            fs::set_permissions(work, fs::Permissions::from_mode(mode | 0o001))?;
        }
        self.mount_storage(spec)
    }
    fn mount_storage(&self, spec: &SandboxSpec) -> Result<()> {
        let dir = self.dir(&spec.agent_id);
        let image = dir.join("storage.ext4");
        system::trusted(&image)?;
        ensure!(
            fs::metadata(&image)?.is_file()
                && fs::metadata(&image)?.len() == self.config.disk_bytes,
            "storage image capacity mismatch"
        );
        #[cfg(unix)]
        ensure!(
            fs::metadata(&image)?.blocks().saturating_mul(512) >= self.config.disk_bytes,
            "storage image is sparse or lost preallocated backing blocks"
        );
        if let Some(mount) = self.mount_record(&dir.join("storage"))? {
            ensure!(mount["fstype"] == "ext4", "storage must be ext4");
        } else {
            self.command(
                "mount",
                &[
                    "-t",
                    "ext4",
                    "-o",
                    "loop,nosuid,nodev,noatime",
                    image.to_str().unwrap(),
                    dir.join("storage").to_str().unwrap(),
                ],
            )?;
        }
        for path in [dir.join("work"), dir.join("storage/work")] {
            ensure!(
                fs::symlink_metadata(&path)?.is_dir(),
                "work must be a real directory"
            );
        }
        if self.mount_record(&dir.join("work"))?.is_none() {
            self.command(
                "mount",
                &[
                    "--bind",
                    dir.join("storage/work").to_str().unwrap(),
                    dir.join("work").to_str().unwrap(),
                ],
            )?;
        }
        #[cfg(unix)]
        {
            let work = fs::metadata(dir.join("work"))?;
            let storage = fs::metadata(dir.join("storage/work"))?;
            ensure!(
                work.dev() == storage.dev() && work.ino() == storage.ino(),
                "work bind does not point into bounded storage"
            );
        }
        Ok(())
    }
    fn mount_overlay(&self, spec: &SandboxSpec) -> Result<()> {
        let dir = self.dir(&spec.agent_id);
        let target = dir.join("rootfs");
        let mounts = self.command(
            "findmnt",
            &["--json", "--list", "--output", "TARGET,FSTYPE"],
        )?;
        let mounts: Value = serde_json::from_str(&mounts)?;
        if let Some(mount) = mounts["filesystems"]
            .as_array()
            .and_then(|a| a.iter().find(|v| v["target"] == target.to_str().unwrap()))
        {
            ensure!(
                mount["fstype"] == "overlay",
                "rootfs mount is not an overlay"
            );
        } else {
            system::trusted(&dir)?;
            for name in ["upper", "overlay-work"] {
                ensure!(
                    fs::symlink_metadata(dir.join("storage").join(name))?.is_dir(),
                    "overlay directories must be real directories"
                );
            }
            let options = format!(
                "lowerdir={},upperdir={},workdir={},index=off,nosuid,nodev",
                dir.join("lower").display(),
                dir.join("storage/upper").display(),
                dir.join("storage/overlay-work").display()
            );
            self.command(
                "mount",
                &[
                    "-t",
                    "overlay",
                    "overlay",
                    "-o",
                    &options,
                    target.to_str().unwrap(),
                ],
            )?;
        }
        // Mutable rootfs is inspected only while the cgroup is empty. Never
        // traverse attacker-owned symlinks as host root during a restart.
        for path in [
            "proc",
            "dev",
            "dev/pts",
            "dev/shm",
            "sys",
            "tmp",
            "run",
            "work",
            "opt",
            "opt/survive",
            "etc",
            "etc/resolv.conf",
        ] {
            let mut p = target.clone();
            for part in Path::new(path).components() {
                p.push(part);
                ensure!(
                    !fs::symlink_metadata(&p)
                        .with_context(|| format!("rootfs mount target {} missing", p.display()))?
                        .file_type()
                        .is_symlink(),
                    "symlink in rootfs mount target {}",
                    p.display()
                );
            }
        }
        Ok(())
    }
    fn create_network(&self, spec: &SandboxSpec, pid: u32) -> Result<()> {
        let ns = spec.netns();
        let link = spec.link();
        let guest = format!("sc{}", spec.slot);
        ensure!(
            !Path::new("/run/netns").join(&ns).exists(),
            "agent namespace already exists"
        );
        self.network_command(
            "start: pin OCI-created network namespace",
            &["netns", "attach", &ns, &pid.to_string()],
        )?;
        system::ns_command(
            &ns,
            "sysctl",
            &[
                "-q",
                "-w",
                "net.ipv6.conf.all.disable_ipv6=1",
                "net.ipv6.conf.default.disable_ipv6=1",
                "net.ipv4.conf.all.accept_redirects=0",
                "net.ipv4.conf.default.accept_redirects=0",
            ],
            None,
            self.timeout(),
        )
        .with_context(|| format!("start: wire paused init: configure network sysctls in {ns}"))?;
        self.network_command(
            "start: wire paused init: create agent veth pair",
            &[
                "-n", ROUTER, "link", "add", &link, "type", "veth", "peer", "name", &guest,
            ],
        )?;
        self.network_command(
            "start: wire paused init: move guest veth into namespace",
            &["-n", ROUTER, "link", "set", &guest, "netns", &ns],
        )?;
        self.network_command(
            "start: wire paused init: rename guest veth",
            &["-n", &ns, "link", "set", &guest, "name", "eth0"],
        )?;
        self.network_command(
            "start: wire paused init: assign router gateway address",
            &[
                "-n",
                ROUTER,
                "addr",
                "add",
                "169.254.203.1/32",
                "dev",
                &link,
            ],
        )?;
        let ip = format!("{}/32", source_ip(spec.slot)?);
        self.network_command(
            "start: wire paused init: assign guest source address",
            &["-n", &ns, "addr", "add", &ip, "dev", "eth0"],
        )?;
        // Linux requires an administratively UP next-hop device. The router
        // end stays DOWN, so raising eth0 cannot enable external traffic.
        self.network_command(
            "start: wire paused init: enable guest link",
            &["-n", &ns, "link", "set", "eth0", "up"],
        )?;
        self.network_command(
            "start: wire paused init: install guest gateway link route",
            &["-n", &ns, "route", "add", "169.254.203.1/32", "dev", "eth0"],
        )?;
        self.network_command(
            "start: wire paused init: install guest default route",
            &["-n", &ns, "route", "add", "default", "via", "169.254.203.1"],
        )?;
        self.network_command(
            "start: wire paused init: enable guest loopback",
            &["-n", &ns, "link", "set", "lo", "up"],
        )?;
        // The router link stays DOWN with no route to this agent. Only verified
        // start may raise that link and install the router-side /32 route.
        Ok(())
    }
    fn runtime_state(&self, spec: &SandboxSpec) -> Result<Option<Value>> {
        // Missing runtime directory is distinguishable from a broken youki
        // command. Never interpret permission/JSON/runtime errors as "stopped".
        if !self.runtime_root().join(spec.runtime_id()).exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(
            &self.runtime(spec, &["state", &spec.runtime_id()])?,
        )?))
    }
    fn created_pid(&self, spec: &SandboxSpec) -> Result<u32> {
        let state = self
            .runtime_state(spec)?
            .context("runtime missing after create")?;
        ensure!(
            state["status"] == "created" && state["id"] == spec.runtime_id(),
            "runtime is not the expected paused container"
        );
        let pid = state["pid"]
            .as_u64()
            .filter(|pid| *pid > 1 && *pid <= i32::MAX as u64)
            .context("paused init PID missing or invalid")? as u32;
        #[cfg(unix)]
        for ns in ["net", "user"] {
            ensure!(
                fs::metadata(format!("/proc/{pid}/ns/{ns}"))?.ino()
                    != fs::metadata(format!("/proc/self/ns/{ns}"))?.ino(),
                "paused init shares host {ns} namespace"
            );
        }
        ensure!(
            system::text(&self.cgroup(spec).join("cgroup.procs"))?
                .lines()
                .any(|value| value == pid.to_string()),
            "paused init is outside its dedicated cgroup"
        );
        Ok(pid)
    }
    fn status_sync(&self, id: &str) -> Result<SandboxStatus> {
        if !self.dir(id).join("spec.json").exists() {
            return Ok(SandboxStatus::Missing);
        }
        let spec = self.spec(id)?;
        if system::text(&self.config.data_dir.join("boot-id"))? != boot_id()? {
            return Ok(SandboxStatus::Stopped);
        }
        match self.runtime_state(&spec)? {
            None => Ok(if self.dir(id).join("started").exists() {
                SandboxStatus::Stopped
            } else {
                SandboxStatus::Created
            }),
            Some(state) => match state["status"].as_str() {
                Some("running") => Ok(SandboxStatus::Running),
                Some("created") => Ok(SandboxStatus::Created),
                Some("stopped") => Ok(SandboxStatus::Stopped),
                _ => bail!("unexpected youki runtime state"),
            },
        }
    }
    fn disconnected(&self, spec: &SandboxSpec) -> Result<()> {
        if !self.agent_link_exists(spec)? {
            return Ok(());
        }
        self.network_command(
            "disconnect: disable router-side agent link",
            &["-n", ROUTER, "link", "set", &spec.link(), "down"],
        )
    }
    fn agent_link_exists(&self, spec: &SandboxSpec) -> Result<bool> {
        // A successful full link listing distinguishes an absent agent link
        // from a missing router namespace, permissions failure or broken ip.
        let output = self
            .command("ip", &["-j", "-n", ROUTER, "link", "show"])
            .context("query router links before agent network operation")?;
        has_named_link(&output, &spec.link())
    }
    fn remove_network(&self, spec: &SandboxSpec) -> Result<()> {
        // Caller has killed and emptied the old cgroup. Never reuse an old
        // pinned netns: a new OCI user namespace must own its new network ns.
        if self.agent_link_exists(spec)? {
            self.network_command(
                "retire: delete old router-side agent veth",
                &["-n", ROUTER, "link", "delete", &spec.link()],
            )?;
        }
        let pinned = Path::new("/run/netns").join(spec.netns());
        match fs::symlink_metadata(&pinned) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "agent namespace pin must not be a symlink"
                );
                self.network_command(
                    "retire: delete old agent namespace pin",
                    &["netns", "delete", &spec.netns()],
                )?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect old agent namespace pin"),
        }
        Ok(())
    }
    fn kill_cgroup(&self, spec: &SandboxSpec) -> Result<()> {
        let cg = self.cgroup(spec);
        if !cg.exists() {
            return Ok(());
        }
        fs::write(cg.join("cgroup.kill"), "1")?;
        let started = Instant::now();
        while !system::text(&cg.join("cgroup.events"))?
            .lines()
            .any(|l| l == "populated 0")
        {
            ensure!(
                started.elapsed() < Duration::from_secs(self.timeout()),
                "cgroup failed to empty"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    fn stop_sync(&self, id: &str) -> Result<()> {
        if !self.dir(id).join("spec.json").exists() {
            return Ok(());
        }
        let spec = self.spec(id)?;
        ensure!(
            system::text(&self.config.data_dir.join("boot-id"))? == boot_id()?,
            "refusing stale runtime PIDs after reboot; explicit restore required"
        );
        let confirmed = self.dir(id).join("stop-confirmed");
        if confirmed.is_file() {
            system::trusted(&confirmed)?;
            let same_boot = system::text(&confirmed)? == boot_id()?;
            let empty = if self.cgroup(&spec).exists() {
                system::text(&self.cgroup(&spec).join("cgroup.events"))?
                    .lines()
                    .any(|line| line == "populated 0")
            } else {
                true
            };
            // Marker alone is not evidence: a live runtime, populated cgroup,
            // or newly pinned namespace always takes the real revocation path.
            if same_boot
                && empty
                && !self.runtime_root().join(spec.runtime_id()).try_exists()?
                && !Path::new("/run/netns").join(spec.netns()).try_exists()?
            {
                return Ok(());
            }
        }
        // Do not gate emergency stop on doctor or working firewall/runtime.
        let disconnect = self.disconnected(&spec);
        let killed = self.kill_cgroup(&spec);
        if let Err(e) = killed {
            let _ = self.runtime(&spec, &["kill", "--all", &spec.runtime_id(), "KILL"]);
            return Err(e.context("cgroup shutdown could not be confirmed"));
        }
        if self.runtime_root().join(spec.runtime_id()).exists() {
            self.runtime(&spec, &["delete", "--force", &spec.runtime_id()])?;
        }
        let retired = self.remove_network(&spec);
        system::atomic_write(&self.dir(id).join("started"), b"1\n")?;
        disconnect.context("processes stopped, but network disconnect could not be confirmed")?;
        retired.context("processes stopped, but old agent network removal failed")?;
        system::atomic_write(&confirmed, boot_id()?.as_bytes())
    }
    fn verify_runtime(&self, spec: &SandboxSpec) -> Result<()> {
        let state = self
            .runtime_state(spec)?
            .context("runtime missing after create")?;
        let pid = state["pid"]
            .as_u64()
            .filter(|p| *p > 1)
            .context("runtime PID missing")?;
        #[cfg(unix)]
        {
            for ns in ["mnt", "pid", "net", "ipc", "uts", "user", "cgroup"] {
                ensure!(
                    fs::metadata(format!("/proc/{pid}/ns/{ns}"))?.ino()
                        != fs::metadata(format!("/proc/self/ns/{ns}"))?.ino(),
                    "runtime shares host {ns} namespace"
                );
            }
            ensure!(
                fs::metadata(format!("/proc/{pid}/ns/net"))?.ino()
                    == fs::metadata(format!("/run/netns/{}", spec.netns()))?.ino(),
                "runtime joined wrong network namespace"
            );
        }
        let uid_map = system::text(Path::new(&format!("/proc/{pid}/uid_map")))?;
        let expected = vec![
            "0".to_string(),
            spec.uid(&self.config).to_string(),
            "65536".to_string(),
        ];
        ensure!(
            uid_map.split_whitespace().collect::<Vec<_>>() == expected,
            "runtime UID map mismatch"
        );
        let cg = self.cgroup(spec);
        for (name, expected) in [
            ("memory.max", spec.limits.memory_bytes.to_string()),
            ("memory.swap.max", "0".into()),
            ("pids.max", spec.limits.pids.to_string()),
            (
                "cpu.max",
                format!("{} 100000", spec.limits.cpu_millis * 100),
            ),
            ("memory.oom.group", "1".into()),
        ] {
            ensure!(
                system::text(&cg.join(name))? == expected,
                "runtime {name} mismatch"
            );
        }
        ensure!(
            system::text(&cg.join("cgroup.procs"))?
                .lines()
                .any(|p| p == pid.to_string()),
            "init is outside the dedicated cgroup"
        );
        let proc_status = fs::read_to_string(format!("/proc/{pid}/status"))?;
        for name in ["CapEff:", "CapBnd:"] {
            let bits = u64::from_str_radix(
                proc_status
                    .lines()
                    .find_map(|l| l.strip_prefix(name))
                    .context("missing capability mask")?
                    .trim(),
                16,
            )?;
            let permitted = (1u64 << 0)
                | (1 << 1)
                | (1 << 3)
                | (1 << 4)
                | (1 << 5)
                | (1 << 6)
                | (1 << 7)
                | (1 << 10)
                | (1 << 18)
                | (1 << 31);
            ensure!(bits & !permitted == 0, "runtime has extra capabilities");
        }
        ensure!(
            proc_status.lines().any(|l| l == "NoNewPrivs:\t1")
                && proc_status.lines().any(|l| l == "Seccomp:\t2"),
            "runtime lacks no_new_privs/seccomp"
        );
        Ok(())
    }
    fn start_sync(&self, id: &str) -> Result<()> {
        self.doctor_sync()?;
        let spec = self.spec(id)?;
        ensure!(
            self.dir(id).join("prepared").exists(),
            "sandbox prepare is incomplete"
        );
        if self.status_sync(id)? == SandboxStatus::Running {
            self.verify_runtime(&spec)?;
            return Ok(());
        }
        self.disconnected(&spec)?;
        self.kill_cgroup(&spec)?;
        if self.runtime_root().join(spec.runtime_id()).exists() {
            self.runtime(&spec, &["delete", "--force", &spec.runtime_id()])?;
        }
        self.remove_network(&spec)?;
        match fs::remove_file(self.dir(id).join("stop-confirmed")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.mount_storage(&spec)?;
        self.mount_overlay(&spec)?;
        staging::prepare(&self.config, &spec)?;
        // Regenerate only root-owned runtime configuration, so pre-existing
        // prepared bundles also request an OCI-created netns on their next start.
        system::atomic_write(
            &self.dir(id).join("config.json"),
            &serde_json::to_vec_pretty(&oci::bundle(&self.config, &spec))?,
        )?;
        let create_log_path = self.dir(id).join("youki-create.log");
        // Keep draining the create logger while its paused init is wired and
        // started. The untrusted core never receives the host log file itself.
        let create_log = system::RuntimeLog::new(&create_log_path)?;
        let result = (|| -> Result<()> {
            self.runtime_capture(
                &spec,
                &[
                    "create",
                    "--bundle",
                    self.dir(id).to_str().unwrap(),
                    &spec.runtime_id(),
                ],
                &create_log,
            )?;
            let pid = self.created_pid(&spec)?;
            self.create_network(&spec, pid)?;
            self.runtime(&spec, &["start", &spec.runtime_id()])?;
            // The router-side link stays down while PID 1 starts; verify the
            // actual runtime before granting any external connectivity.
            self.verify_runtime(&spec)?;
            self.network_command(
                "start: enable guest link after runtime verification",
                &["-n", &spec.netns(), "link", "set", "eth0", "up"],
            )?;
            self.network_command(
                "start: enable router-side agent link after runtime verification",
                &["-n", ROUTER, "link", "set", &spec.link(), "up"],
            )?;
            // Linux rejects a route through a down next-hop device. This must
            // follow link-up; replace also handles routes retained across stop.
            let ip = format!("{}/32", source_ip(spec.slot)?);
            self.network_command(
                "start: install router-side agent route",
                &["-n", ROUTER, "route", "replace", &ip, "dev", &spec.link()],
            )?;
            system::atomic_write(&self.dir(id).join("started"), b"1\n")?;
            Ok(())
        })();
        let (result, create_diagnostics) = match create_log.finish() {
            Ok(tail) => (result, tail),
            Err(log_error) => (
                match result {
                    Ok(()) => Err(log_error),
                    Err(error) => {
                        Err(error.context(format!("create log capture failed: {log_error}")))
                    }
                },
                String::new(),
            ),
        };
        if let Err(error) = result {
            let disconnected = self.disconnected(&spec);
            let killed = self.kill_cgroup(&spec);
            let context = format!(
                "start failed closed; network_down={}, cgroup_empty={}; workspace/rootfs retained; {error}; create log {}: {}",
                disconnected.is_ok(),
                killed.is_ok(),
                create_log_path.display(),
                create_diagnostics.trim()
            );
            return Err(error.context(context));
        }
        Ok(())
    }
}

fn has_named_link(output: &str, name: &str) -> Result<bool> {
    let links: Vec<Value> = serde_json::from_str(output).context("parse router link listing")?;
    let mut found = false;
    for link in links {
        let ifname = link["ifname"]
            .as_str()
            .context("router link listing lacks interface name")?;
        found |= ifname == name;
    }
    Ok(found)
}

fn boot_id() -> Result<String> {
    system::text(Path::new("/proc/sys/kernel/random/boot_id"))
}

#[cfg(unix)]
fn install_guest_hosts(dir: &Path, hostname: &str) -> Result<()> {
    // Xlib and local servers may look up the guest hostname before accepting
    // connections. These local names must resolve even with egress disabled.
    let asset = dir.join("assets/hosts");
    let hosts = format!("127.0.0.1\tlocalhost\n127.0.1.1\t{hostname}\n");
    system::atomic_write(&asset, hosts.as_bytes())?;
    fs::set_permissions(&asset, fs::Permissions::from_mode(0o444))?;
    // scaffold checked the parent, and the new private lower has never run.
    // Unlink template symlinks themselves, never write through their targets.
    let path = dir.join("lower/etc/hosts");
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::os::unix::fs::symlink("/opt/survive/hosts", path)?;
    Ok(())
}

fn refresh_platform_seed(source: &Path, assets: &Path) -> Result<()> {
    system::trusted(assets)?;
    let destination = assets.join("survive-agent");
    system::trusted(&destination)?;
    let contents = fs::read(source)?;
    if fs::read(&destination)? == contents {
        return Ok(());
    }
    let mut staged = tempfile::NamedTempFile::new_in(assets)?;
    std::io::Write::write_all(staged.as_file_mut(), &contents)?;
    #[cfg(unix)]
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o555))?;
    staged.as_file().sync_all()?;
    staged.persist(destination)?;
    fs::File::open(assets)?.sync_all()?;
    Ok(())
}
fn scaffold(root: &Path) -> Result<()> {
    for name in [
        "proc",
        "dev",
        "dev/pts",
        "dev/shm",
        "sys",
        "tmp",
        "run",
        "work",
        "opt",
        "opt/survive",
        "etc",
    ] {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(m) => ensure!(
                m.is_dir(),
                "template mount target must be a real directory: {name}"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let resolv = root.join("etc/resolv.conf");
    if !resolv.exists() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&resolv)?;
    }
    ensure!(
        fs::symlink_metadata(resolv)?.is_file(),
        "template resolv.conf must be a regular file"
    );
    Ok(())
}

/// Resolve absolute symlinks *inside* the template, never against the host root.
fn rootfs_path(root: &Path, path: &Path) -> Result<PathBuf> {
    use std::collections::VecDeque;
    use std::path::Component;
    let mut pending = path
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect::<VecDeque<_>>();
    let mut relative = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part)
            .components()
            .next()
            .context("empty path component")?
        {
            Component::RootDir => {
                relative.clear();
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                ensure!(relative.pop(), "template symlink escapes root");
                continue;
            }
            Component::Normal(_) => {}
            _ => bail!("invalid template path"),
        }
        let candidate = root.join(&relative).join(&part);
        if fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
            links += 1;
            ensure!(links <= 40, "template symlink loop");
            let target = fs::read_link(candidate)?;
            let components = target
                .components()
                .map(|c| c.as_os_str().to_owned())
                .collect::<Vec<_>>();
            for component in components.into_iter().rev() {
                pending.push_front(component);
            }
        } else {
            relative.push(part);
        }
    }
    let resolved = root.join(relative);
    ensure!(
        fs::metadata(&resolved)?.is_file(),
        "template executable must be a file"
    );
    Ok(resolved)
}

fn overlaps_reserved(cidr: &str) -> bool {
    if cidr == "default" {
        return false;
    }
    let (addr, bits) = cidr.split_once('/').unwrap_or((cidr, "32"));
    let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return true;
    };
    if bits > 32 {
        return true;
    }
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    [
        (u32::from(std::net::Ipv4Addr::new(10, 203, 0, 0)), 16),
        (u32::from(std::net::Ipv4Addr::new(10, 204, 0, 0)), 30),
    ]
    .into_iter()
    .any(|(network, prefix)| {
        let common = if bits.min(prefix) == 0 {
            0
        } else {
            u32::MAX << (32 - bits.min(prefix))
        };
        (u32::from(ip) & mask & common) == (network & common)
    })
}
fn shift_seed(path: &Path, base: u32) -> Result<()> {
    // Parent-provided files have not yet been exposed to a workload. Repeated
    // prepare never touches an existing workspace's ownership or contents.
    let m = fs::symlink_metadata(path)?;
    system::chown(path, base, base)?;
    if m.is_dir() {
        for child in fs::read_dir(path)? {
            shift_seed(&child?.path(), base)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn local_guest_names_do_not_depend_on_dns_or_follow_template_hosts_links() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("lower/etc")).unwrap();
        fs::create_dir(dir.join("assets")).unwrap();
        let original = dir.join("template");
        fs::write(&original, "preserve").unwrap();
        let hosts = dir.join("lower/etc/hosts");
        std::os::unix::fs::symlink(&original, &hosts).unwrap();
        install_guest_hosts(dir, "agent-47").unwrap();
        let asset = dir.join("assets/hosts");
        assert_eq!(
            fs::read_to_string(&asset).unwrap(),
            "127.0.0.1\tlocalhost\n127.0.1.1\tagent-47\n"
        );
        assert_eq!(fs::metadata(asset).unwrap().mode() & 0o777, 0o444);
        assert_eq!(
            fs::read_link(hosts).unwrap(),
            Path::new("/opt/survive/hosts")
        );
        assert_eq!(fs::read_to_string(original).unwrap(), "preserve");
    }

    #[test]
    fn absent_link_requires_valid_router_query_output() {
        assert!(!has_named_link(r#"[{"ifname":"lo"},{"ifname":"uplink"}]"#, "sa1").unwrap());
        assert!(has_named_link(r#"[{"ifname":"lo"},{"ifname":"sa1"}]"#, "sa1").unwrap());
        for invalid in [
            "",
            "Cannot open network namespace",
            r#"{"error":"permission denied"}"#,
            "[{}]",
        ] {
            assert!(has_named_link(invalid, "sa1").is_err());
        }
    }
    #[test]
    fn reserved_routes_reject_docker_conflicts_but_not_default() {
        for route in ["10.203.4.0/24", "10.204.0.1/32", "10.0.0.0/8"] {
            assert!(overlaps_reserved(route));
        }
        for route in ["default", "172.17.0.0/16", "10.205.0.0/16"] {
            assert!(!overlaps_reserved(route));
        }
    }
    #[test]
    #[cfg(unix)]
    fn template_absolute_symlinks_are_jailed_to_template() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("usr/bin")).unwrap();
        fs::create_dir_all(t.path().join("usr/local/cargo/bin")).unwrap();
        fs::write(t.path().join("usr/local/cargo/bin/cargo"), b"test").unwrap();
        std::os::unix::fs::symlink("/usr/local/cargo/bin/cargo", t.path().join("usr/bin/cargo"))
            .unwrap();
        assert_eq!(
            rootfs_path(t.path(), Path::new("/usr/bin/cargo")).unwrap(),
            t.path().join("usr/local/cargo/bin/cargo")
        );
        std::os::unix::fs::symlink("/etc/passwd", t.path().join("usr/bin/escape")).unwrap();
        assert!(rootfs_path(t.path(), Path::new("/usr/bin/escape")).is_err());
    }
}
