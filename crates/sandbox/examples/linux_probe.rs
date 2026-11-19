//! Workload for the opt-in Linux test. Never used by production sandboxes.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn atomic(name: &str, value: &Value) -> Result<()> {
    let path = Path::new("/work").join(name);
    let temp = path.with_extension("tmp");
    fs::write(&temp, serde_json::to_vec(value)?)?;
    fs::rename(temp, path)?;
    Ok(())
}
fn connected(addr: &str) -> bool {
    addr.parse::<SocketAddr>()
        .ok()
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok())
}
fn command(args: &[&str]) -> Result<()> {
    let result = Command::new(args[0]).args(&args[1..]).output()?;
    ensure!(
        result.status.success(),
        "{}: {}",
        args[0],
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}
fn compile_and_install() -> Result<()> {
    fs::write("/work/test.c", b"int main(void) { return 0; }\n")?;
    command(&["cc", "/work/test.c", "-o", "/work/c-test"])?;
    command(&["/work/c-test"])?;
    fs::write(
        "/work/test.rs",
        b"fn main() { std::thread::spawn(|| 1).join().unwrap(); }\n",
    )?;
    command(&[
        "rustc",
        "--edition=2024",
        "/work/test.rs",
        "-o",
        "/work/rust-test",
    ])?;
    command(&["/work/rust-test"])?;
    fs::create_dir_all("/work/pkg/DEBIAN")?;
    fs::create_dir_all("/work/pkg/usr/local/share")?;
    fs::write(
        "/work/pkg/DEBIAN/control",
        "Package: survive-sandbox-probe\nVersion: 1.0\nArchitecture: all\nMaintainer: Survive Test <nobody@example.invalid>\nDescription: Offline sandbox package install check\n",
    )?;
    fs::write("/work/pkg/usr/local/share/survive-probe", "installed\n")?;
    command(&["dpkg-deb", "--build", "/work/pkg", "/work/probe.deb"])?;
    command(&["dpkg", "--install", "/work/probe.deb"])?;
    ensure!(
        fs::read_to_string("/usr/local/share/survive-probe")? == "installed\n",
        "package missing"
    );
    Ok(())
}
fn fill_disk() -> Result<Value> {
    let mut f = fs::File::create("/work/quota-fill")?;
    let bytes = vec![0xa5; 1024 * 1024];
    let mut count = 0u64;
    let errno = loop {
        // Bound dirty page-cache pressure so this test isolates disk capacity.
        // A separate test intentionally exhausts memory and checks group OOM.
        match f.write_all(&bytes).and_then(|_| f.sync_data()) {
            Ok(()) => count += bytes.len() as u64,
            Err(e) => break e.raw_os_error(),
        }
    };
    let rootfs_bounded = fs::write("/var/tmp/quota-check", &bytes).is_err();
    drop(f);
    fs::remove_file("/work/quota-fill")?;
    let _ = fs::remove_file("/var/tmp/quota-check");
    Ok(json!({"errno":errno,"written":count,"rootfs_bounded":rootfs_bounded}))
}
fn pids_limit() -> Result<Value> {
    let mut children = Vec::new();
    let mut errno = None;
    for _ in 0..300 {
        match Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => children.push(child),
            Err(e) => {
                errno = e.raw_os_error();
                break;
            }
        }
    }
    let count = children.len();
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(json!({"errno":errno,"children":count}))
}
fn recovery() -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:9001")?;
    listener.set_nonblocking(true)?;
    let mut last = 0;
    let mut heartbeat = 0u64;
    loop {
        heartbeat += 1;
        atomic("heartbeat.json", &json!({"tick":heartbeat}))?;
        while let Ok((mut stream, peer)) = listener.accept() {
            stream.set_read_timeout(Some(Duration::from_millis(200)))?;
            let mut buffer = [0; 2048];
            let _ = stream.read(&mut buffer);
            let body = format!("peer={peer}\n");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
        if let Ok(bytes) = fs::read("/work/probe-request.json")
            && let Ok(req) = serde_json::from_slice::<Value>(&bytes)
        {
            let nonce = req["nonce"].as_u64().unwrap_or(0);
            if nonce > last {
                last = nonce;
                let result = match req["action"].as_str().unwrap_or("connect") {
                    "fill" => fill_disk()?,
                    "pids" => pids_limit()?,
                    "oom" => {
                        let mut chunks = Vec::new();
                        loop {
                            let mut data = vec![0; 8 * 1024 * 1024];
                            for b in &mut data {
                                *b = std::hint::black_box(0x99);
                            }
                            chunks.push(data);
                            std::hint::black_box(&chunks);
                        }
                    }
                    _ => {
                        json!({"connected":req["targets"].as_array().unwrap().iter().map(|v|connected(v.as_str().unwrap())).collect::<Vec<_>>()})
                    }
                };
                atomic(
                    "probe-response.json",
                    &json!({"nonce":nonce,"result":result}),
                )?;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn main() -> Result<()> {
    // A runtime logger or namespace handle must never reach an untrusted core.
    // read_dir itself holds one descriptor to /proc/self/fd during this loop.
    for entry in fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        let fd: i32 = entry.file_name().to_string_lossy().parse()?;
        if fd > 2 {
            let target = fs::read_link(entry.path())?;
            ensure!(
                target == Path::new(&format!("/proc/{}/fd", std::process::id())),
                "inherited descriptor {fd}: {}",
                target.display()
            );
        }
    }
    match std::env::args().nth(1).as_deref() {
        Some("recovery") => recovery(),
        Some("restart") => {
            ensure!(
                fs::read_to_string("/usr/local/share/survive-probe")? == "installed\n",
                "rootfs did not persist"
            );
            fs::write("/work/restarted", "mutable manifest used\n")?;
            recovery()
        }
        _ => {
            let child = Command::new("/work/probe").arg("recovery").spawn()?;
            atomic("recovery-pid.json", &json!({"pid":child.id()}))?;
            // Child deliberately outlives the initial core.
            let start = Instant::now();
            while !Path::new("/work/heartbeat.json").exists() {
                ensure!(
                    start.elapsed() < Duration::from_secs(10),
                    "recovery did not start"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            let result = compile_and_install();
            atomic(
                "compiled.json",
                &json!({"ok":result.is_ok(),"error":result.err().map(|e|e.to_string())}),
            )?;
            std::process::exit(42)
        }
    }
}
