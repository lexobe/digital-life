//! Run only through deploy/sandbox-linux-integration.sh on an explicitly
//! provisioned Linux fixture. No tests mutate a host by default.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use survive_sandbox::{SandboxConfig, SandboxManager, SandboxSpec, SandboxStatus, source_ip};

fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
async fn until(mut predicate: impl FnMut() -> bool, seconds: u64) -> Result<()> {
    let start = Instant::now();
    while !predicate() {
        ensure!(
            start.elapsed() < Duration::from_secs(seconds),
            "integration wait timed out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}
async fn request(work: &Path, nonce: u64, mut body: Value) -> Result<Value> {
    body["nonce"] = json!(nonce);
    let tmp = work.join("probe-request.tmp");
    fs::write(&tmp, serde_json::to_vec(&body)?)?;
    fs::rename(tmp, work.join("probe-request.json"))?;
    until(
        || read(&work.join("probe-response.json")).is_ok_and(|v| v["nonce"] == nonce),
        90,
    )
    .await?;
    Ok(read(&work.join("probe-response.json"))?["result"].clone())
}
async fn proxy(ip: Ipv4Addr, source: Ipv4Addr) -> Result<tokio::net::TcpStream> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(source.into(), 0))?;
    Ok(tokio::time::timeout(
        Duration::from_secs(2),
        socket.connect(SocketAddr::new(ip.into(), 9001)),
    )
    .await??)
}
fn docker_rules() -> Result<Vec<String>> {
    let out = std::process::Command::new("iptables-save").output()?;
    ensure!(out.status.success(), "iptables-save failed");
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .filter(|l| l.contains("DOCKER"))
        .map(str::to_owned)
        .collect())
}

fn link_is_up(namespace: &str, device: &str) -> Result<bool> {
    let out = std::process::Command::new("ip")
        .args(["-j", "-n", namespace, "link", "show", "dev", device])
        .output()?;
    ensure!(
        out.status.success(),
        "inspect link {namespace}/{device} failed"
    );
    let links: Value = serde_json::from_slice(&out.stdout)?;
    let flags = links[0]["flags"].as_array().context("missing link flags")?;
    Ok(flags.iter().any(|flag| flag == "UP"))
}

fn router_agent_routes(slot: u32) -> Result<Vec<Value>> {
    let prefix = format!("{}/32", source_ip(slot)?);
    let out = std::process::Command::new("ip")
        .args([
            "-j",
            "-n",
            "survive-router",
            "route",
            "show",
            "exact",
            &prefix,
        ])
        .output()?;
    ensure!(
        out.status.success(),
        "inspect router route for {prefix} failed"
    );
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn router_has_agent_link(slot: u32) -> Result<bool> {
    let out = std::process::Command::new("ip")
        .args(["-j", "-n", "survive-router", "link", "show"])
        .output()?;
    ensure!(out.status.success(), "query router links failed");
    let links: Vec<Value> = serde_json::from_slice(&out.stdout)?;
    Ok(links
        .iter()
        .any(|link| link["ifname"] == format!("sa{slot}")))
}

#[cfg(unix)]
fn verify_runtime_views(config: &SandboxConfig, spec: &SandboxSpec) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let base = Path::new("/run/survive-platform");
    let stage = base.join(&spec.agent_id);
    let uid = config.uid_base + (spec.slot - 1) * 65536;
    let metadata = fs::symlink_metadata(base)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.mode() & 0o7777 == 0o711,
        "invalid staging base permissions"
    );
    let metadata = fs::symlink_metadata(&stage)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == 0
            && metadata.gid() == uid
            && metadata.mode() & 0o7777 == 0o710,
        "staging must be root-owned and searchable only by the mapped group"
    );
    let mut entries = fs::read_dir(&stage)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    ensure!(
        entries == ["assets", "resolv.conf", "rootfs", "work"],
        "staging exposes unexpected control files"
    );
    for name in entries {
        let source = fs::symlink_metadata(
            config
                .data_dir
                .join("agents")
                .join(&spec.agent_id)
                .join(&name),
        )?;
        let view = fs::symlink_metadata(stage.join(&name))?;
        ensure!(
            !view.file_type().is_symlink()
                && (view.dev(), view.ino()) == (source.dev(), source.ino()),
            "runtime view identity mismatch: {name}"
        );
        ensure!(
            (view.uid(), view.gid()) == (source.uid(), source.gid()),
            "view changed persistent ownership"
        );
    }
    ensure!(
        fs::metadata(&config.data_dir)?.mode() & 0o7777 == 0o700,
        "private data directory permissions changed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "root Linux fixture only; explicit opt-in required"]
async fn real_youki_isolation_and_lifecycle() -> Result<()> {
    ensure!(
        std::env::var("SURVIVE_LINUX_TEST").as_deref() == Ok("YES"),
        "set SURVIVE_LINUX_TEST=YES"
    );
    let config: SandboxConfig = toml::from_str(&fs::read_to_string(std::env::var(
        "SURVIVE_SANDBOX_CONFIG",
    )?)?)?;
    let binary = std::env::var("SURVIVE_PROBE_BINARY")?;
    let manager = SandboxManager::new(config.clone())?;
    manager.doctor().await?;
    let before = docker_rules()?;
    // This test requires port 8089 free, and never touches ports 80/443.
    let gateway = std::net::TcpListener::bind("10.203.0.1:8089")
        .context("stop the fixture environment server before this test")?;
    let denied_listener = std::net::TcpListener::bind("10.203.0.1:18080")?;
    gateway.set_nonblocking(true)?;
    let peers = Arc::new(Mutex::new(Vec::<IpAddr>::new()));
    let seen = peers.clone();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let responder = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Ok((mut c, peer)) = gateway.accept() {
                seen.lock().unwrap().push(peer.ip());
                let _ = c.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    });
    let mut specs = Vec::new();
    let prefix = format!("integration-{}", std::process::id());
    for index in 0..4 {
        let s = SandboxSpec {
            agent_id: format!("{prefix}-{index}"),
            experiment_id: if index == 2 {
                format!("{prefix}-b")
            } else {
                format!("{prefix}-a")
            },
            slot: 60000 + index,
            token: "integration-only-not-a-real-credential".into(),
            limits: survive_core::ResourceLimits {
                memory_bytes: 256 * 1024 * 1024,
                pids: 128,
                cpu_millis: 1000,
            },
            network: survive_core::NetworkPolicy {
                internet: index != 3,
                peers: index != 3,
                services: index < 2,
            },
        };
        let work = manager.work_dir(&s.agent_id)?;
        ensure!(!work.exists(), "test identity exists");
        fs::create_dir_all(&work)?;
        fs::copy(&binary, work.join("probe"))?;
        fs::write(
            work.join("boot.json"),
            br#"{"argv":["/work/probe","core"]}"#,
        )?;
        fs::write(work.join("prompt.md"), "keep this seed\n")?;
        manager.prepare(&s).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mapped = config.uid_base + (s.slot - 1) * 65536;
            let mode = fs::metadata(&config.rootfs)?.mode() & 0o777;
            for name in ["storage/upper", "rootfs"] {
                let metadata = fs::metadata(work.parent().unwrap().join(name))?;
                ensure!(
                    (metadata.uid(), metadata.gid(), metadata.mode() & 0o777)
                        == (mapped, mapped, mode),
                    "new upper/merged root ownership or mode is wrong: {name}"
                );
            }
        }
        ensure!(
            !Path::new(&format!("/run/netns/sv-{}", s.slot)).try_exists()?,
            "prepare must not precreate or pin an agent network namespace"
        );
        ensure!(
            !router_has_agent_link(s.slot)?,
            "prepare must not create an agent veth"
        );
        ensure!(
            router_agent_routes(s.slot)?.is_empty(),
            "prepare installed a router-side route before link-up"
        );
        manager.start(&s.agent_id).await?;
        #[cfg(unix)]
        verify_runtime_views(&config, &s)?;
        ensure!(
            link_is_up(&format!("sv-{}", s.slot), "eth0")?
                && link_is_up("survive-router", &format!("sa{}", s.slot))?,
            "start did not enable the verified runtime network"
        );
        let routes = router_agent_routes(s.slot)?;
        ensure!(
            routes.len() == 1 && routes[0]["dev"] == format!("sa{}", s.slot),
            "start did not install the agent route on its router-side link"
        );
        for operation in ["create", "start", "state"] {
            let metadata = fs::symlink_metadata(
                work.parent()
                    .unwrap()
                    .join(format!("youki-{operation}.log")),
            )?;
            ensure!(
                metadata.is_file() && metadata.len() <= 65536,
                "runtime log is not bounded"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.uid() == 0 && metadata.mode() & 0o777 == 0o600,
                    "runtime log is not private to host root"
                );
            }
        }
        specs.push(s);
    }
    for s in &specs {
        let work = manager.work_dir(&s.agent_id)?;
        until(
            || {
                read(&work.join(".survive-init-status.json"))
                    .is_ok_and(|v| v["state"] == "core_exited")
            },
            180,
        )
        .await?;
        ensure!(
            read(&work.join("compiled.json"))?["ok"] == true,
            "compiler/package install failed: {}",
            read(&work.join("compiled.json"))?
        );
        ensure!(
            read(&work.join(".survive-init-status.json"))?["exit_code"] == 42,
            "core exit not recorded"
        );
        ensure!(
            manager.status(&s.agent_id).await? == SandboxStatus::Running,
            "core exit killed recovery/PID1"
        );
        let first = read(&work.join("heartbeat.json"))?["tick"]
            .as_u64()
            .unwrap();
        until(
            || {
                read(&work.join("heartbeat.json"))
                    .is_ok_and(|v| v["tick"].as_u64().unwrap_or(0) > first)
            },
            5,
        )
        .await?;
        ensure!(
            fs::read_to_string(work.join("prompt.md"))? == "keep this seed\n",
            "seed prompt changed"
        );
    }
    let ips = specs
        .iter()
        .map(|s| source_ip(s.slot).unwrap())
        .collect::<Vec<_>>();
    let work = manager.work_dir(&specs[0].agent_id)?;
    let public = std::env::var("SURVIVE_PUBLIC_TEST_ADDR").unwrap_or_else(|_| "1.1.1.1:443".into());
    let targets = json!([
        "10.203.0.1:8089",
        "10.203.0.1:18080",
        format!("{}:9001", ips[1]),
        format!("{}:9001", ips[2]),
        format!("{}:9001", ips[3]),
        "169.254.169.254:80",
        "172.17.0.1:80",
        public
    ]);
    let result = request(&work, 1, json!({"targets":targets})).await?;
    ensure!(
        result["connected"] == json!([true, false, true, false, false, false, false, true]),
        "network matrix failed: {result}"
    );
    let disabled = manager.work_dir(&specs[3].agent_id)?;
    let result = request(
        &disabled,
        1,
        json!({"targets":["10.203.0.1:8089",format!("{}:9001",ips[0]),public]}),
    )
    .await?;
    ensure!(
        result["connected"] == json!([true, false, false]),
        "disabled network policy failed: {result}"
    );
    until(
        || {
            let peers = peers.lock().unwrap();
            peers.contains(&IpAddr::V4(ips[0])) && peers.contains(&IpAddr::V4(ips[3]))
        },
        5,
    )
    .await?;
    let host = Ipv4Addr::new(10, 203, 0, 1);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut service = proxy(ips[0], host).await?;
    service
        .write_all(b"GET / HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = String::new();
    service.read_to_string(&mut response).await?;
    ensure!(
        response.contains("peer=10.203.0.1:"),
        "proxy source address wrong"
    );
    ensure!(
        proxy(ips[2], host).await.is_err(),
        "disabled service reachable"
    );
    ensure!(
        proxy(ips[0], Ipv4Addr::new(10, 204, 0, 1)).await.is_err(),
        "wrong host proxy source accepted"
    );
    // Give a *host-side* attacker the ability to spoof in this test namespace;
    // the real workload lacks CAP_NET_ADMIN. Router still must reject the IP.
    let ns = format!("sv-{}", specs[0].slot);
    let spoof = format!("{}/32", ips[2]);
    ensure!(
        std::process::Command::new("ip")
            .args(["-n", &ns, "addr", "add", &spoof, "dev", "eth0"])
            .status()?
            .success(),
        "add test alias"
    );
    let status = std::process::Command::new("ip")
        .args([
            "netns",
            "exec",
            &ns,
            "curl",
            "--noproxy",
            "*",
            "--silent",
            "--max-time",
            "2",
            "--interface",
            &ips[2].to_string(),
            "http://10.203.0.1:8089/",
        ])
        .status()?;
    ensure!(!status.success(), "spoofed source reached host gateway");
    ensure!(
        std::process::Command::new("ip")
            .args(["-n", &ns, "addr", "del", &spoof, "dev", "eth0"])
            .status()?
            .success(),
        "remove test alias"
    );
    let result = request(&work, 2, json!({"action":"pids"})).await?;
    ensure!(
        result["errno"] == libc::EAGAIN && result["children"].as_u64().unwrap() < 128,
        "pids limit not enforced: {result}"
    );
    let result = request(&work, 3, json!({"action":"fill"})).await?;
    ensure!(
        result["errno"] == libc::ENOSPC
            && result["rootfs_bounded"] == true
            && result["written"].as_u64().unwrap() <= config.disk_bytes,
        "shared hard disk bound failed: {result}"
    );
    let namespace_path = format!("/run/netns/sv-{}", specs[0].slot);
    // Hold the old inode so even kernel inode reuse cannot hide a stale-netns restart.
    let old_namespace = fs::File::open(&namespace_path)?;
    manager.stop(&specs[0].agent_id).await?;
    ensure!(
        !Path::new(&namespace_path).try_exists()? && !router_has_agent_link(specs[0].slot)?,
        "stop did not retire the old namespace pin and veth"
    );
    let tick = read(&work.join("heartbeat.json"))?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    ensure!(
        read(&work.join("heartbeat.json"))? == tick,
        "recovery survived cgroup stop"
    );
    fs::write(
        work.join("boot.json"),
        br#"{"argv":["/work/probe","restart"]}"#,
    )?;
    manager.start(&specs[0].agent_id).await?;
    #[cfg(unix)]
    verify_runtime_views(&config, &specs[0])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            fs::metadata(&namespace_path)?.ino() != old_namespace.metadata()?.ino(),
            "restart reused the old user-namespace-owned network namespace"
        );
    }
    drop(old_namespace);
    until(|| work.join("restarted").exists(), 10).await?;
    // OOM-group must stop the whole individual, including its init.
    fs::write(
        disabled.join("probe-request.json"),
        br#"{"nonce":2,"action":"oom"}"#,
    )?;
    let start = Instant::now();
    while manager.status(&specs[3].agent_id).await? == SandboxStatus::Running {
        ensure!(
            start.elapsed() < Duration::from_secs(30),
            "memory limit not enforced"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for s in &specs {
        manager.stop(&s.agent_id).await?;
        ensure!(
            manager.status(&s.agent_id).await? == SandboxStatus::Stopped,
            "stop failed"
        );
    }
    ensure!(docker_rules()? == before, "existing Docker rules changed");
    ensure!(
        manager.restore().await.is_err(),
        "same-boot restore should reject firewall drift/rebuild"
    );
    done.store(true, Ordering::Relaxed);
    responder.join().unwrap();
    drop(denied_listener);
    eprintln!(
        "Linux integration passed; retained fixture IDs {prefix}-0..3, slots 60000..60003. No automatic cleanup/rollback."
    );
    Ok(())
}
