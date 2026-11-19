use std::{fs, process::Command};
use survive_core::Store;

#[test]
fn offline_upgrade_requires_an_existing_exclusively_available_database() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let config = include_str!("../../../config.example.toml")
        .replace("/var/lib/survive-platform", data.to_str().unwrap());
    fs::write(dir.path().join("config.toml"), config).unwrap();
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_survive-server"))
            .env_clear()
            .current_dir(dir.path())
            .args(["--config", "config.toml", "--finish-desktop-upgrade", "one"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    };
    assert!(run().contains("existing platform database required"));
    let path = data.join("platform.redb");
    assert!(!path.exists());
    let _service_store = Store::open(path).unwrap();
    assert!(run().contains("open database exclusively for offline upgrade"));
}
