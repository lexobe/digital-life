use crate::*;
use serde_json::json;
use std::{collections::BTreeSet, fs};

fn config() -> SandboxConfig {
    toml::from_str(include_str!("../../../deploy/sandbox-config.example.toml")).unwrap()
}
fn spec(slot: u32, experiment: &str) -> SandboxSpec {
    SandboxSpec {
        agent_id: format!("agent-{slot}"),
        experiment_id: experiment.into(),
        slot,
        token: "test-secret".into(),
        limits: Default::default(),
        network: Default::default(),
    }
}

#[test]
fn source_ips_cover_contract_without_duplicates_or_gateway() {
    let mut all = BTreeSet::new();
    for slot in 1..=65532 {
        let ip = source_ip(slot).unwrap();
        assert!(all.insert(ip));
        assert_ne!(ip.to_string(), "10.203.0.1");
    }
    assert_eq!(source_ip(1).unwrap().to_string(), "10.203.0.2");
    assert_eq!(source_ip(255).unwrap().to_string(), "10.203.1.0");
    assert_eq!(source_ip(65532).unwrap().to_string(), "10.203.255.253");
    assert!(source_ip(0).is_err());
    assert!(source_ip(65533).is_err());
}

#[test]
fn configuration_fails_closed_for_host_network_paths_and_unbounded_resources() {
    let base = serde_json::to_value(config()).unwrap();
    for (field, replacement) in [
        ("gateway_url", json!("http://127.0.0.1:8089")),
        ("data_dir", json!("/var/lib/survive,lowerdir=/")),
        ("data_dir", json!("/var/lib/../etc")),
        ("cgroup_root", json!("/sys/fs/cgroup")),
        ("total_memory_bytes", json!(0)),
        ("disk_bytes", json!(0)),
        ("uid_base", json!(0)),
        ("denied_cidrs", json!(["0.0.0.0/0; flush ruleset"])),
    ] {
        let mut value = base.clone();
        value[field] = replacement;
        assert!(
            SandboxManager::new(serde_json::from_value(value).unwrap()).is_err(),
            "accepted {field}"
        );
    }
    assert!(SandboxManager::new(config()).is_ok());
}

#[test]
fn rejects_identifier_injection_and_invalid_limits() {
    let cfg = config();
    let mut s = spec(1, "e");
    for bad in ["", "../other", "a/b", "a\nadd rule", "x;", "☃"] {
        s.agent_id = bad.into();
        assert!(s.validate(&cfg).is_err());
    }
    s = spec(1, "e");
    s.limits.memory_bytes = cfg.total_memory_bytes + 1;
    assert!(s.validate(&cfg).is_err());
    s = spec(1, "e");
    s.limits.pids = 0;
    assert!(s.validate(&cfg).is_err());
    s = spec(1, "e");
    s.limits.cpu_millis = 0;
    assert!(s.validate(&cfg).is_err());
}

#[test]
fn uid_ranges_never_map_host_root_or_overlap() {
    let cfg = config();
    assert_eq!(spec(1, "e").uid(&cfg), 65536);
    for slot in 1..65532 {
        assert_eq!(
            spec(slot, "e").uid(&cfg) + 65536,
            spec(slot + 1, "e").uid(&cfg)
        );
    }
    assert!(spec(65532, "e").uid(&cfg).checked_add(65535).is_some());
}

#[test]
fn mutable_boot_is_reread_and_never_repaired() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("boot.json");
    assert!(BootManifest::read(&path).is_err());
    assert!(!path.exists());
    fs::write(&path, br#"{"argv":["/bin/sh","/work/recovery.sh"]}"#).unwrap();
    assert_eq!(BootManifest::read(&path).unwrap().argv[0], "/bin/sh");
    fs::write(&path, br#"{"argv":["/work/custom-core"]}"#).unwrap();
    assert_eq!(
        BootManifest::read(&path).unwrap().argv[0],
        "/work/custom-core"
    );
    for invalid in [
        br#"{"argv":[]}"#.as_slice(),
        br#"{"argv":["sh"]}"#,
        br#"{"argv":["/bin/sh"],"repair":true}"#,
        b"broken",
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(BootManifest::read(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), invalid);
    }
    fs::write(&path, vec![b' '; 65537]).unwrap();
    assert!(BootManifest::read(&path).is_err());
}

#[cfg(unix)]
#[test]
fn boot_symlinks_and_directories_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("boot.json");
    assert!(BootManifest::read(temp.path()).is_err());
    std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
    assert!(BootManifest::read(&path).is_err());
}

#[test]
fn oci_has_real_isolation_and_no_host_credentials_or_sockets() {
    let cfg = config();
    let s = spec(1, "e");
    let v = crate::oci::bundle(&cfg, &s);
    assert_eq!(v["root"]["path"], "/run/survive-platform/agent-1/rootfs");
    let mounts = v["mounts"].as_array().unwrap();
    for (destination, source) in [
        ("/work", "work"),
        ("/opt/survive", "assets"),
        ("/etc/resolv.conf", "resolv.conf"),
    ] {
        assert_eq!(
            mounts
                .iter()
                .find(|mount| mount["destination"] == destination)
                .unwrap()["source"],
            format!("/run/survive-platform/agent-1/{source}")
        );
    }
    assert!(
        !v.to_string().contains(cfg.data_dir.to_str().unwrap()),
        "private data directory leaked into runtime filesystem sources"
    );
    let names = v["linux"]["namespaces"].as_array().unwrap();
    for name in ["pid", "mount", "ipc", "uts", "user", "cgroup", "network"] {
        assert!(names.iter().any(|v| v["type"] == name));
    }
    assert_eq!(
        names.iter().find(|v| v["type"] == "network").unwrap(),
        &json!({"type":"network"})
    );
    assert_eq!(v["process"]["noNewPrivileges"], true);
    assert_eq!(v["linux"]["uidMappings"][0]["hostID"], 65536);
    for bad in [
        "CAP_SYS_ADMIN",
        "CAP_NET_ADMIN",
        "CAP_NET_RAW",
        "CAP_SYS_PTRACE",
        "CAP_SYS_MODULE",
        "CAP_MKNOD",
    ] {
        assert!(
            !v["process"]["capabilities"]["bounding"]
                .as_array()
                .unwrap()
                .contains(&json!(bad))
        );
    }
    assert!(!v.to_string().contains("docker.sock"));
    assert_eq!(
        v["linux"]["resources"]["memory"]["swap"],
        s.limits.memory_bytes
    );
    assert_eq!(v["linux"]["resources"]["unified"]["memory.swap.max"], "0");
    assert_eq!(v["linux"]["resources"]["pids"]["limit"], s.limits.pids);
    assert_eq!(
        v["linux"]["resources"]["cpu"]["quota"],
        s.limits.cpu_millis * 100
    );
    assert_eq!(v["root"]["readonly"], false);
    assert_eq!(v["linux"]["seccomp"]["defaultAction"], "SCMP_ACT_ERRNO");
    let calls = v["linux"]["seccomp"]["syscalls"][0]["names"]
        .as_array()
        .unwrap();
    for denied in [
        "mount",
        "umount2",
        "unshare",
        "setns",
        "bpf",
        "ptrace",
        "reboot",
        "kexec_load",
        "open_by_handle_at",
        "io_uring_setup",
        "clone",
        "clone3",
        "socket",
    ] {
        assert!(
            !calls.contains(&json!(denied)),
            "unconditional syscall {denied}"
        );
    }
    assert_eq!(
        v["linux"]["seccomp"]["syscalls"][1]["args"][0]["value"],
        0x02000000
    );
    assert_eq!(v["linux"]["seccomp"]["syscalls"][2]["errnoRet"], 38);
}

#[test]
fn peer_policy_is_mutual_experiment_scoped_and_source_bound() {
    let cfg = config();
    let a = spec(1, "one");
    let b = spec(2, "one");
    let c = spec(3, "two");
    let mut d = spec(4, "one");
    d.network.peers = false;
    d.network.internet = false;
    d.network.services = false;
    let rules = crate::network::router_rules(&cfg, &[a, b, c, d]).unwrap();
    assert!(rules.contains("peers0 { type ipv4_addr; elements = { 10.203.0.2, 10.203.0.3 }"));
    assert!(rules.contains("peers1 { type ipv4_addr; elements = { 10.203.0.4 }"));
    assert!(!rules.contains("agent4 oifname \"sa*\""));
    assert!(!rules.contains("agent4 oifname uplink accept"));
    for slot in 1..=4 {
        let ip = source_ip(slot).unwrap();
        assert!(rules.contains(&format!("iifname \"sa{slot}\" ip saddr != {ip} drop")));
    }
    assert!(rules.contains("meta nfproto != ipv4 drop"));
    assert!(rules.contains("policy drop"));
    assert!(rules.contains("169.254.0.0/16"));
}

#[test]
fn host_service_paths_require_exact_source_and_enabled_destination() {
    let a = spec(1, "e");
    let mut b = spec(2, "e");
    b.network.services = false;
    let rules = crate::network::host_rules(&[a.clone(), b]).unwrap();
    assert!(rules.contains("hook output priority -150;"));
    assert!(rules.contains("tcp dport 8089 iifname != \"sv-uplink\" drop"));
    assert!(rules.contains("services { type ipv4_addr; elements = { 10.203.0.2 }; }"));
    assert!(rules.contains("ip saddr 10.203.0.1 ip daddr @services meta l4proto tcp accept"));
    assert!(
        rules.contains("ct direction reply ct original ip saddr 10.203.0.1 ct state established")
    );
    assert!(!rules.contains("DOCKER"));
    assert!(!rules.contains("flush ruleset"));
    assert!(!rules.contains("dport 80 "));
    assert!(!rules.contains("dport 443 "));
    let r = crate::network::router_rules(&config(), &[a]).unwrap();
    assert!(r.contains("ip saddr 10.203.0.1 ip daddr 10.203.0.2 meta l4proto tcp accept"));
}

#[tokio::test]
#[cfg(not(target_os = "linux"))]
async fn unsupported_host_never_falls_back() {
    let m = SandboxManager::new(config()).unwrap();
    assert!(m.doctor().await.unwrap_err().to_string().contains("Linux"));
    assert!(m.prepare(&spec(1, "e")).await.is_err());
    assert!(m.start("agent-1").await.is_err());
    assert!(run_init().is_err());
}
