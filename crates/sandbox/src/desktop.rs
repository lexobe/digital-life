use crate::init::spawn_child;
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    fs, io,
    os::unix::{
        fs::PermissionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

const PROFILE: &str = "/work/.home/chromium";
const BROWSER_UID: u32 = 65534;

fn desktop_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_remove("SURVIVE_TOKEN");
    command
}

fn private_directory(path: &str, uid: u32) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            std::os::unix::fs::chown(path, Some(uid), Some(uid))?;
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.is_dir(),
                "{path} is not a directory"
            );
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct Desktop {
    children: Vec<(&'static str, Child)>,
}
impl Desktop {
    fn report(&self, state: &str, error: Option<String>) {
        let processes: std::collections::BTreeMap<_, _> = self
            .children
            .iter()
            .map(|(name, child)| (*name, child.id()))
            .collect();
        let status = json!({"state":state, "display":":0", "processes":processes, "error":error});
        let _ = crate::system::atomic_write(
            Path::new("/work/.survive-desktop-status.json"),
            status.to_string().as_bytes(),
        );
    }

    fn launch(&mut self, name: &'static str, cmd: &mut Command) -> Result<()> {
        let child = spawn_child(cmd).with_context(|| format!("launch {name}"))?;
        self.children.push((name, child));
        Ok(())
    }

    fn wait_for(&mut self, args: &[&str], description: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for index in 0..self.children.len() {
                if let Some(status) = self.children[index].1.try_wait()? {
                    let (name, _) = self.children.remove(index);
                    bail!("{name} exited during startup ({status})");
                }
            }
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            // Mutable guest binaries or a frozen X server must not stall PID 1
            // forever before it reads the individual's mutable boot manifest.
            let mut probe = desktop_command("/usr/bin/xdotool");
            probe.args(args).stdout(Stdio::null()).stderr(Stdio::null());
            let mut probe = spawn_child(&mut probe)?;
            loop {
                if let Some(status) = probe.try_wait()? {
                    if status.success() {
                        return Ok(());
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = probe.kill();
                    let _ = probe.wait();
                    bail!("timed out waiting for {description}");
                }
                sleep(Duration::from_millis(25));
            }
            sleep(Duration::from_millis(75));
        }
    }

    fn launch_session(&mut self) -> Result<()> {
        private_directory("/run/survive-desktop", 0)?;
        private_directory("/run/survive-browser", BROWSER_UID)?;
        private_directory(PROFILE, BROWSER_UID)?;
        self.launch(
            "display",
            desktop_command("/usr/bin/Xvfb").args([
                ":0",
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
                "-noreset",
                "-extension",
                "MIT-SHM",
            ]),
        )?;
        self.wait_for(&["getdisplaygeometry"], "X display")?;
        self.launch("window_manager", &mut desktop_command("/usr/bin/openbox"))?;
        self.wait_for(&["get_num_desktops"], "window manager")?;

        // The outer youki policy disallows setuid elevation and nested user
        // namespaces. Keep that boundary and run Chromium as a capability-free
        // guest user, with only its own persistent profile and ephemeral runtime dir.
        let has_home = Path::new("/opt/survive/home.html").is_file();
        self.launch(
            "browser",
            desktop_command("/usr/bin/chromium")
                .uid(BROWSER_UID)
                .gid(BROWSER_UID)
                .current_dir(PROFILE)
                .env("HOME", PROFILE)
                .env("XDG_RUNTIME_DIR", "/run/survive-browser")
                .args([
                    "--no-sandbox",
                    "--ozone-platform=x11",
                    "--disable-gpu",
                    "--disable-dev-shm-usage",
                    "--disable-background-networking",
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--password-store=basic",
                    "--renderer-process-limit=2",
                    "--start-maximized",
                    "--hide-crash-restore-bubble",
                ])
                .arg(format!("--user-data-dir={PROFILE}"))
                .arg(if has_home {
                    "file:///opt/survive/home.html"
                } else {
                    "about:blank"
                }),
        )?;
        self.wait_for(
            &["search", "--onlyvisible", "--class", "^chromium$"],
            "Chromium window",
        )?;
        if has_home {
            self.wait_for(
                &["search", "--onlyvisible", "--name", "生物环境"],
                "environment page",
            )?;
            // Chromium maps its window before the first page paint. Let the compositor
            // present the loaded page before releasing the observer and the seed.
            sleep(Duration::from_millis(350));
        }
        Ok(())
    }

    pub(crate) fn start(&mut self) {
        self.report("starting", None);
        match self.launch_session() {
            Ok(()) => {
                self.report("ready", None);
                // This independent guest worker authenticates to the gateway.
                // PID 1 reaps it normally and the sandbox cgroup owns its lifetime.
                if let Err(error) = spawn_child(
                    Command::new("/opt/survive/survive-agent").arg("--desktop-observer"),
                ) {
                    eprintln!("desktop observer: {error}");
                }
            }
            Err(error) => self.report("failed", Some(format!("{error:#}"))),
        }
    }

    pub(crate) fn exited(&mut self, pid: i32, status: i32) {
        if let Some(index) = self
            .children
            .iter()
            .position(|(_, child)| child.id() == pid as u32)
        {
            let (name, _) = self.children.remove(index);
            let status = std::process::ExitStatus::from_raw(status);
            self.report("exited", Some(format!("{name} exited ({status})")));
        }
    }
}
