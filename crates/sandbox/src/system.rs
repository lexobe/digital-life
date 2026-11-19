use anyhow::{Context, Result, bail, ensure};
#[cfg(unix)]
use std::os::unix::{
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    io::AsRawFd,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) const RUNTIME_LOG_BYTES: u64 = 64 * 1024;

/// Manager callers hold the installation lock and validate the root-owned
/// parent directory. Check the inode before truncating; never follow a link.
pub(crate) fn runtime_log(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    opts.mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = opts.open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "runtime log must be a regular file");
    #[cfg(unix)]
    {
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1,
            "runtime log has unexpected owner or hard links"
        );
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.set_len(0)?;
    Ok(file)
}

pub(crate) fn runtime_log_tail(mut log: File) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    let len = log.metadata()?.len().min(RUNTIME_LOG_BYTES);
    log.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)))?;
    let mut bytes = Vec::new();
    log.take(16 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// youki writes to a private FIFO; only this bounded sink writes the host log.
/// No workload rlimit changes and no regular host log descriptor reaches youki.
pub(crate) struct RuntimeLog {
    fifo: PathBuf,
    _directory: tempfile::TempDir,
    log: Option<File>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}
impl RuntimeLog {
    #[cfg(unix)]
    pub(crate) fn new(path: &Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let log = runtime_log(path)?;
        let mut sink = log.try_clone()?;
        let directory = tempfile::Builder::new()
            .prefix(".youki-log-")
            .tempdir_in(path.parent().context("log parent")?)?;
        let fifo = directory.path().join("pipe");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0,
            "create runtime log FIFO: {}",
            std::io::Error::last_os_error()
        );
        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&fifo)?;
        let keepalive = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&fifo)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || -> Result<()> {
            let _keepalive = keepalive;
            let mut written = 0u64;
            let mut stopped_at = None;
            let mut buffer = [0u8; 8192];
            loop {
                if stopping.load(Ordering::Acquire)
                    && stopped_at.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(100)
                {
                    break;
                }
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(n) => {
                        let keep = n.min(RUNTIME_LOG_BYTES.saturating_sub(written) as usize);
                        sink.write_all(&buffer[..keep])?;
                        written += keep as u64;
                        // Continue draining beyond the retention cap so logging
                        // cannot block the runtime when its buffer fills.
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            sink.sync_all()?;
            Ok(())
        });
        Ok(Self {
            fifo,
            _directory: directory,
            log: Some(log),
            stop,
            worker: Some(worker),
        })
    }
    #[cfg(not(unix))]
    pub(crate) fn new(_: &Path) -> Result<Self> {
        bail!("runtime log capture requires Unix");
    }
    pub(crate) fn path(&self) -> &Path {
        &self.fifo
    }
    pub(crate) fn finish(mut self) -> Result<String> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.worker
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("runtime log capture panicked"))??;
        runtime_log_tail(self.log.take().unwrap())
    }
}
impl Drop for RuntimeLog {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn linux_root() -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "sandbox runtime requires Linux; no fallback runtime"
    );
    #[cfg(unix)]
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "sandbox runtime requires host root"
    );
    Ok(())
}

pub(crate) fn tool(name: &str) -> Result<PathBuf> {
    for prefix in ["/usr/sbin", "/usr/bin", "/sbin", "/bin"] {
        let p = Path::new(prefix).join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("required tool missing: {name}")
}

/// No shell, no caller-controlled PATH/loader environment, bounded wait, and no
/// pipe inherited by the container (youki create keeps its stdio open).
pub(crate) fn run(
    program: &Path,
    args: &[&str],
    input: Option<&str>,
    timeout: u64,
) -> Result<String> {
    run_stdio(program, args, input, timeout, false)
}
pub(crate) fn run_runtime(
    program: &Path,
    args: &[&str],
    timeout: u64,
    detached: bool,
) -> Result<String> {
    run_stdio(program, args, None, timeout, detached)
}
fn run_stdio(
    program: &Path,
    args: &[&str],
    input: Option<&str>,
    timeout: u64,
    detached: bool,
) -> Result<String> {
    let mut stdin = tempfile::tempfile()?;
    if let Some(value) = input {
        stdin.write_all(value.as_bytes())?;
    }
    use std::io::{Seek, SeekFrom};
    stdin.seek(SeekFrom::Start(0))?;
    let output = tempfile::tempfile()?;
    let error = tempfile::tempfile()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("LANG", "C")
        .stdin(if detached {
            Stdio::null()
        } else {
            Stdio::from(stdin)
        })
        // Workload stdout must not retain an unbounded anonymous host file.
        .stdout(if detached {
            Stdio::null()
        } else {
            Stdio::from(output.try_clone()?)
        })
        .stderr(if detached {
            Stdio::null()
        } else {
            Stdio::from(error.try_clone()?)
        });
    let mut child = command
        .spawn()
        .with_context(|| format!("launch {}", program.display()))?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(timeout) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} timed out; inspect runtime state before retry",
                program.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |mut f: File| -> Result<String> {
        f.seek(SeekFrom::Start(0))?;
        let mut s = String::new();
        f.take(1024 * 1024).read_to_string(&mut s)?;
        Ok(s)
    };
    ensure!(
        status.success(),
        "{} {} failed ({status}): {}",
        program.display(),
        args.first().unwrap_or(&""),
        read(error)?
    );
    read(output)
}

pub(crate) fn command(name: &str, args: &[&str], timeout: u64) -> Result<String> {
    run(&tool(name)?, args, None, timeout)
}
pub(crate) fn ns_command(
    ns: &str,
    name: &str,
    args: &[&str],
    input: Option<&str>,
    timeout: u64,
) -> Result<String> {
    let executable = tool(name)?;
    let mut argv = vec![
        "netns",
        "exec",
        ns,
        executable.to_str().context("tool path")?,
    ];
    argv.extend_from_slice(args);
    run(&tool("ip")?, &argv, input, timeout)
}

/// Check every component; privileged metadata is never reached through a symlink.
pub(crate) fn trusted(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        let m = fs::symlink_metadata(&current)
            .with_context(|| format!("required path {}", current.display()))?;
        ensure!(
            !m.file_type().is_symlink(),
            "symlink in privileged path: {}",
            current.display()
        );
        #[cfg(unix)]
        ensure!(
            m.uid() == 0 && m.mode() & 0o022 == 0,
            "{} must be root owned and not group/world writable",
            current.display()
        );
    }
    Ok(())
}
pub(crate) fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir(path)?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    trusted(path)
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("parent")?)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

pub(crate) struct Lock(File);
impl Lock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        opts.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let f = opts.open(path)?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0,
            "manager lock failed"
        );
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn text(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

#[cfg(unix)]
pub(crate) fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    ensure!(
        unsafe { libc::lchown(p.as_ptr(), uid, gid) } == 0,
        "chown {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn chown(_: &Path, _: u32, _: u32) -> Result<()> {
    bail!("Linux required")
}

/// Only called on a fresh copy of the trusted template, before it is exposed.
pub(crate) fn shift_tree(path: &Path, base: u32) -> Result<()> {
    shift_tree_inner(path, base, &mut std::collections::HashSet::new())
}
fn shift_tree_inner(
    path: &Path,
    base: u32,
    seen: &mut std::collections::HashSet<(u64, u64)>,
) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        if !seen.insert((m.dev(), m.ino())) {
            return Ok(());
        }
        ensure!(
            m.uid() < 65536 && m.gid() < 65536,
            "template UID/GID must fit in 65536 IDs"
        );
        chown(path, base + m.uid(), base + m.gid())?;
    }
    if m.is_dir() {
        for entry in fs::read_dir(path)? {
            shift_tree_inner(&entry?.path(), base, seen)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runtime_logs_reject_symlinks_and_hardlinks_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"preserve").unwrap();
        let symlink = dir.path().join("symlink");
        std::os::unix::fs::symlink(&victim, &symlink).unwrap();
        assert!(runtime_log(&symlink).is_err());
        let hardlink = dir.path().join("hardlink");
        fs::hard_link(&victim, &hardlink).unwrap();
        assert!(runtime_log(&hardlink).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"preserve");
    }

    #[test]
    fn fifo_log_is_bounded_and_does_not_limit_workload_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.log");
        let capture = RuntimeLog::new(&path).unwrap();
        let artifact = dir.path().join("artifact");
        run_runtime(Path::new("/bin/sh"), &["-c", "dd if=/dev/zero of=\"$1\" bs=65536 count=2 2>/dev/null && dd if=/dev/zero of=\"$2\" bs=65536 count=2 2>/dev/null", "log-test", capture.path().to_str().unwrap(), artifact.to_str().unwrap()], 5, true).unwrap();
        let tail = capture.finish().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), RUNTIME_LOG_BYTES);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(&artifact).unwrap().len(),
            2 * RUNTIME_LOG_BYTES
        );
        assert_eq!(tail.len(), 16 * 1024);
    }
}
