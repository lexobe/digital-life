use std::{fs, process::Command};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        r#"data_dir = {}
listen = "127.0.0.1:0"
env_listen = "127.0.0.1:0"
test_mode = true
[[creators]]
id = "test"
token_env = "SURVIVE_TEST_FILE_TOKEN"
[gateway]
[[gateway.providers]]
name = "mock"
base_url = "http://127.0.0.1:9/v1"
[gateway.providers.models]
default = "test"
"#,
        serde_json::to_string(&dir.path().join("data")).unwrap()
    );
    fs::write(dir.path().join("config.toml"), config).unwrap();
    dir
}
fn write_private(path: &std::path::Path, text: &str) {
    fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn server_loads_config_and_credentials_from_the_selected_file() {
    let dir = fixture();
    let path = dir.path().join("environment");
    write_private(
        &path,
        "SURVIVE_CONFIG=config.toml\nSURVIVE_TEST_FILE_TOKEN=synthetic-creator-token-at-least-32-characters\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_survive-server"))
        .env_clear()
        .current_dir(dir.path())
        .args(["--check", "--env-file"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("data/platform.redb").is_file());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("synthetic-creator-token"));
}

#[test]
fn process_environment_overrides_file_and_invalid_file_does_not_open_state() {
    let dir = fixture();
    let path = dir.path().join("environment");
    write_private(
        &path,
        "SURVIVE_CONFIG=config.toml\nSURVIVE_TEST_FILE_TOKEN=too-short\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_survive-server"))
        .env_clear()
        .env(
            "SURVIVE_TEST_FILE_TOKEN",
            "synthetic-override-token-at-least-32-characters",
        )
        .env("SURVIVE_ENV_FILE", &path)
        .current_dir(dir.path())
        .arg("--check")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let other = fixture();
    let private = other.path().join("environment");
    write_private(
        &private,
        "SURVIVE_CONFIG=config.toml\nSECRET=\"do-not-print-this-value\n",
    );
    let failed = Command::new(env!("CARGO_BIN_EXE_survive-server"))
        .env_clear()
        .current_dir(other.path())
        .args(["--check", "--env-file"])
        .arg(&private)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("do-not-print-this-value"));
    assert!(!other.path().join("data").exists());
}
