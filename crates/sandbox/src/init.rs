use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootManifest {
    pub argv: Vec<String>,
}
impl BootManifest {
    pub fn read(path: &Path) -> Result<Self> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        }
        let file = opts.open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "boot manifest must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "boot manifest too large");
        let manifest: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            !manifest.argv.is_empty() && manifest.argv.len() <= 256,
            "boot argv must contain 1..=256 entries"
        );
        ensure!(
            Path::new(&manifest.argv[0]).is_absolute(),
            "boot executable must be absolute"
        );
        ensure!(
            manifest.argv.iter().all(|v| !v.contains('\0')),
            "NUL in boot argv"
        );
        Ok(manifest)
    }
}

#[cfg(target_os = "linux")]
fn isolate_init_fds() -> std::io::Result<()> {
    // No Rust file objects exist yet. Close the entire inherited range, even
    // descriptors above the new RLIMIT_NOFILE, before touching mutable work.
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn spawn_child(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            let err = libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            if err != 0 {
                return Err(std::io::Error::from_raw_os_error(err));
            }
            Ok(())
        });
    }
    cmd.spawn()
}

#[cfg(target_os = "linux")]
pub fn run_init() -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    ensure!(
        unsafe { libc::getpid() } == 1,
        "survive-init must run as PID 1 inside a sandbox"
    );
    isolate_init_fds()?;
    // Block before spawning: a fast child exit cannot race handler installation.
    let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signals);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut signals, signal);
        }
        ensure!(
            libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) == 0,
            "block init signals"
        );
    }
    let work = Path::new("/work");
    let report = |value: serde_json::Value| {
        // This is untrusted advisory telemetry, never an authorization signal.
        // Persist atomically; a malicious workspace cannot make init follow a symlink.
        if let Err(e) = crate::system::atomic_write(
            &work.join(".survive-init-status.json"),
            &serde_json::to_vec(&value).unwrap(),
        ) {
            eprintln!("init status: {e}");
        }
    };
    let home = work.join(".home");
    if fs::create_dir(&home).is_ok() {
        // The browser may traverse this directory to reach its own private home.
        let _ = fs::set_permissions(&home, fs::Permissions::from_mode(0o711));
    }
    let mut desktop = crate::desktop::Desktop::default();
    desktop.start();
    let spawned = (|| -> Result<std::process::Child> {
        let boot = BootManifest::read(&work.join("boot.json"))?;
        let mut cmd = Command::new(&boot.argv[0]);
        cmd.args(&boot.argv[1..]).current_dir(work);
        Ok(spawn_child(&mut cmd)?)
    })();
    let mut core = match spawned {
        Ok(child) => {
            let pid = child.id() as i32;
            report(serde_json::json!({"state":"running","pid":pid}));
            Some(pid)
        }
        Err(e) => {
            report(serde_json::json!({"state":"launch_failed","error":e.to_string()}));
            None
        }
    };
    loop {
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            desktop.exited(pid, status);
            if Some(pid) == core {
                core = None;
                report(
                    serde_json::json!({"state":"core_exited","pid":pid,"exit_code":if libc::WIFEXITED(status) {Some(libc::WEXITSTATUS(status))}else{None},"signal":if libc::WIFSIGNALED(status) {Some(libc::WTERMSIG(status))}else{None}}),
                );
            }
            // Reap all adopted children; NEVER kill or restart on a core exit.
        }
        let mut signal = 0;
        let err = unsafe { libc::sigwait(&signals, &mut signal) };
        ensure!(err == 0, "sigwait failed: {err}");
        if signal == libc::SIGTERM || signal == libc::SIGINT {
            // Only an explicit shutdown kills children. Parent uses cgroup.kill
            // for an immediate, complete token-exhaustion shutdown.
            unsafe {
                libc::kill(-1, libc::SIGTERM);
            }
        }
        // SIGHUP does not reload or revert mutable agent state. Explicit restart
        // is stop/start, at which point the manifest is read again from disk.
    }
}
#[cfg(not(target_os = "linux"))]
pub fn run_init() -> Result<()> {
    anyhow::bail!("survive-init requires Linux")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn init_closes_inherited_fds_above_the_current_fd_limit() {
        let file = tempfile::tempfile().unwrap();
        let inherited = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 64) };
        assert!(inherited >= 64);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // This child uses only direct syscalls and exits without touching
            // inherited test-runner state. Parent descriptors/limits are intact.
            let mut fd_limit: libc::rlimit = unsafe { std::mem::zeroed() };
            if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut fd_limit) } != 0 {
                unsafe {
                    libc::_exit(10);
                }
            }
            fd_limit.rlim_cur = 32;
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &fd_limit) } != 0 {
                unsafe {
                    libc::_exit(11);
                }
            }
            if isolate_init_fds().is_err() {
                unsafe {
                    libc::_exit(12);
                }
            }
            let closed = unsafe { libc::fcntl(inherited, libc::F_GETFD) } == -1;
            unsafe {
                libc::_exit(if closed { 0 } else { 14 });
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        unsafe {
            libc::close(inherited);
        }
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } >= 0);
    }
}
