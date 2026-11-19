//! Narrow, root-controlled runtime views. Persistent metadata stays private.
use crate::{SandboxConfig, SandboxSpec, system};
#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) const BASE: &str = "/run/survive-platform";
pub(crate) const VIEWS: [&str; 4] = ["rootfs", "work", "assets", "resolv.conf"];

pub(crate) fn directory(spec: &SandboxSpec) -> PathBuf {
    Path::new(BASE).join(&spec.agent_id)
}

#[cfg(unix)]
fn check_directory(path: &Path, gid: u32, mode: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && directory_policy(metadata.uid(), metadata.gid(), metadata.mode(), gid, mode),
        "invalid staging directory ownership/mode: {} (expected root:{gid}, {mode:04o})",
        path.display()
    );
    Ok(())
}
#[cfg(not(unix))]
fn check_directory(_: &Path, _: u32, _: u32) -> Result<()> {
    bail!("Linux required")
}

fn directory_policy(uid: u32, gid: u32, mode: u32, expected_gid: u32, expected_mode: u32) -> bool {
    uid == 0 && gid == expected_gid && mode & 0o7777 == expected_mode
}

fn create_directory(path: &Path, gid: u32, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => return check_directory(path, gid, mode),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)?;
    system::chown(path, 0, gid)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    check_directory(path, gid, mode)
}

fn leaf(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "view must be a real {} without symlinks: {}",
        if directory { "directory" } else { "file" },
        path.display()
    );
    Ok(metadata)
}

fn same_object(source: &Path, target: &Path, directory: bool) -> Result<()> {
    let source_meta = leaf(source, directory)?;
    let target_meta = leaf(target, directory)?;
    #[cfg(unix)]
    ensure!(
        source_meta.dev() == target_meta.dev() && source_meta.ino() == target_meta.ino(),
        "runtime view mount identity differs from source: {}",
        target.display()
    );
    #[cfg(not(unix))]
    {
        let _ = (source_meta, target_meta);
        bail!("Linux required");
    }
    Ok(())
}

fn mounted(target: &Path, timeout: u64) -> Result<Option<Value>> {
    let output = system::command(
        "findmnt",
        &["--json", "--list", "--output", "TARGET,OPTIONS,PROPAGATION"],
        timeout,
    )?;
    let value: Value = serde_json::from_str(&output)?;
    let matches = value["filesystems"]
        .as_array()
        .context("findmnt filesystems missing")?
        .iter()
        .filter(|entry| entry["target"].as_str() == target.to_str())
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "stacked mounts at runtime view {}",
        target.display()
    );
    Ok(matches.first().map(|entry| (*entry).clone()))
}

fn verify_view(
    source: &Path,
    target: &Path,
    is_directory: bool,
    readonly: bool,
    timeout: u64,
) -> Result<()> {
    same_object(source, target, is_directory)?;
    let mount = mounted(target, timeout)?.context("runtime view is not a mount point")?;
    let flags = mount["options"]
        .as_str()
        .context("runtime mount options missing")?
        .split(',')
        .collect::<Vec<_>>();
    ensure!(
        flags.contains(&"nosuid")
            && flags.contains(&"nodev")
            && flags.contains(&if readonly { "ro" } else { "rw" }),
        "runtime view has incorrect mount flags: {}",
        target.display()
    );
    ensure!(
        mount["propagation"] == "private",
        "runtime view propagation must be private: {}",
        target.display()
    );
    Ok(())
}

/// Called only after the old cgroup is empty, before youki create. Never chown
/// mounted views: their leaf ownership is the persistent source's ownership.
pub(crate) fn prepare(config: &SandboxConfig, spec: &SandboxSpec) -> Result<PathBuf> {
    system::linux_root()?;
    let cgroup = config.cgroup_root.join(spec.runtime_id());
    match fs::read_to_string(cgroup.join("cgroup.events")) {
        Ok(events) => ensure!(
            events.lines().any(|line| line == "populated 0"),
            "runtime views require an empty cgroup"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !cgroup.try_exists()? => {}
        Err(e) => return Err(e).context("verify empty cgroup before staging"),
    }
    system::trusted(&config.data_dir)?;
    #[cfg(unix)]
    ensure!(
        fs::metadata(&config.data_dir)?.mode() & 0o7777 == 0o700,
        "data_dir must remain private mode 0700"
    );
    let source_dir = config.data_dir.join("agents").join(&spec.agent_id);
    system::trusted(&source_dir)?;
    system::trusted(Path::new("/run"))?;
    create_directory(Path::new(BASE), 0, 0o711)?;
    let target_dir = directory(spec);
    create_directory(&target_dir, spec.uid(config), 0o710)?;
    // Only the four filesystem views may appear here, never controls or tokens.
    for entry in fs::read_dir(&target_dir)? {
        let entry = entry?;
        ensure!(
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| VIEWS.contains(&name)),
            "unexpected entry in runtime staging directory"
        );
    }
    for name in VIEWS {
        let source = source_dir.join(name);
        let target = target_dir.join(name);
        let is_directory = name != "resolv.conf";
        let readonly = name == "assets" || name == "resolv.conf";
        let metadata = leaf(&source, is_directory)?;
        #[cfg(unix)]
        {
            if readonly {
                ensure!(
                    metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
                    "trusted runtime asset source has unsafe ownership/mode"
                );
            } else {
                let ids = spec.uid(config)..=spec.uid(config) + 65535;
                ensure!(
                    ids.contains(&metadata.uid()) && ids.contains(&metadata.gid()),
                    "rootfs/work source ownership is outside this agent's mapped IDs: {}",
                    source.display()
                );
            }
        }
        if mounted(&target, config.command_timeout_secs)?.is_none() {
            match fs::symlink_metadata(&target) {
                Ok(metadata) => {
                    #[cfg(unix)]
                    ensure!(
                        metadata.uid() == 0 && metadata.gid() == 0 && metadata.mode() & 0o022 == 0,
                        "unsafe unmounted view placeholder"
                    );
                    leaf(&target, is_directory)?;
                    if is_directory {
                        ensure!(
                            fs::read_dir(&target)?.next().is_none(),
                            "view placeholder must be empty"
                        );
                    } else {
                        ensure!(metadata.len() == 0, "view placeholder must be empty");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if is_directory {
                        create_directory(&target, 0, 0o700)?;
                    } else {
                        let mut options = fs::OpenOptions::new();
                        options.write(true).create_new(true);
                        #[cfg(unix)]
                        options
                            .mode(0o600)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
                        options.open(&target)?;
                    }
                }
                Err(e) => return Err(e.into()),
            }
            system::command(
                "mount",
                &["--bind", source.to_str().unwrap(), target.to_str().unwrap()],
                config.command_timeout_secs,
            )
            .with_context(|| format!("bind runtime view {name}"))?;
            same_object(&source, &target, is_directory)?;
            system::command(
                "mount",
                &["--make-private", target.to_str().unwrap()],
                config.command_timeout_secs,
            )?;
            let options = if readonly {
                "remount,bind,ro,nosuid,nodev"
            } else {
                "remount,bind,rw,nosuid,nodev"
            };
            system::command(
                "mount",
                &["-o", options, target.to_str().unwrap()],
                config.command_timeout_secs,
            )?;
        }
        verify_view(
            &source,
            &target,
            is_directory,
            readonly,
            config.command_timeout_secs,
        )?;
    }
    check_directory(Path::new(BASE), 0, 0o711)?;
    check_directory(&target_dir, spec.uid(config), 0o710)?;
    Ok(target_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staging_permissions_do_not_allow_other_agents_to_traverse() {
        assert!(directory_policy(0, 65536, 0o40710, 65536, 0o710));
        for (uid, gid, mode) in [
            (65536, 65536, 0o710),
            (0, 65537, 0o710),
            (0, 65536, 0o711),
            (0, 65536, 0o750),
            (0, 65536, 0o770),
            (0, 65536, 0o2710),
        ] {
            assert!(!directory_policy(uid, gid, mode, 65536, 0o710));
        }
    }
    #[test]
    #[cfg(unix)]
    fn view_identity_rejects_copies_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let copy = dir.path().join("copy");
        let link = dir.path().join("link");
        let hardlink = dir.path().join("same-inode");
        fs::write(&source, b"same bytes").unwrap();
        fs::copy(&source, &copy).unwrap();
        std::os::unix::fs::symlink(&source, &link).unwrap();
        fs::hard_link(&source, &hardlink).unwrap();
        assert!(same_object(&source, &copy, false).is_err());
        assert!(same_object(&source, &link, false).is_err());
        assert!(same_object(&source, &hardlink, false).is_ok());
    }
}
