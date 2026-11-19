//! Load deployment credentials before starting Tokio or any worker threads.
//!
//! The file is data, never a shell script: no interpolation, commands, or sourcing.
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeMap, path::Path};

pub const DEFAULT_ENV_FILE: &str = "/etc/survive-platform/environment";

pub fn parse(input: &str) -> Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    for (index, original) in input.lines().enumerate() {
        let line = original.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let invalid = || anyhow::anyhow!("invalid environment entry at line {}", index + 1);
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (key, raw) = line.split_once('=').ok_or_else(invalid)?;
        let key = key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .enumerate()
                .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        {
            return Err(invalid());
        }
        let raw = raw.trim();
        let value = if raw.starts_with(['\'', '"']) {
            let quote = raw.chars().next().unwrap();
            let mut result = String::new();
            let mut chars = raw[1..].char_indices();
            let mut closed = false;
            while let Some((offset, c)) = chars.next() {
                if c == quote {
                    let remainder = raw[offset + 2..].trim();
                    if !remainder.is_empty() && !remainder.starts_with('#') {
                        return Err(invalid());
                    }
                    closed = true;
                    break;
                }
                if c == '\\' && quote == '"' {
                    let (_, next) = chars.next().ok_or_else(invalid)?;
                    result.push(match next {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '"' => '"',
                        _ => return Err(invalid()),
                    });
                } else {
                    result.push(c);
                }
            }
            if !closed {
                return Err(invalid());
            }
            result
        } else {
            // A # inside a token is literal; whitespace followed by # is a comment.
            let end = raw.char_indices().find_map(|(i, c)| {
                (c == '#' && (i == 0 || raw[..i].ends_with(char::is_whitespace))).then_some(i)
            });
            raw[..end.unwrap_or(raw.len())].trim_end().to_owned()
        };
        if value.contains('\0') || entries.insert(key.to_owned(), value).is_some() {
            return Err(invalid());
        }
    }
    Ok(entries)
}

/// Read and validate the entire file before changing the process environment.
/// Must be called only from synchronous main, before any threads are created.
pub fn read(path: &Path) -> Result<BTreeMap<String, String>> {
    let metadata = std::fs::metadata(path).context("cannot read environment file metadata")?;
    ensure!(
        metadata.is_file(),
        "environment path must be a regular file"
    );
    ensure!(
        metadata.len() <= 1024 * 1024,
        "environment file exceeds 1 MiB"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "environment file contains credentials: set its permissions to 0600"
        );
    }
    let text = std::fs::read_to_string(path).context("cannot read environment file")?;
    parse(&text)
}

/// Select an explicit deployment file, then the system default, then a local .env.
pub fn select(explicit: Option<&Path>) -> Result<Option<std::path::PathBuf>> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("explicit environment file does not exist or is not a file");
        }
        return Ok(Some(path.to_owned()));
    }
    for path in [Path::new(DEFAULT_ENV_FILE), Path::new(".env")] {
        if path.exists() {
            return Ok(Some(path.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_deployment_values_without_shell_execution_or_expansion() {
        let vars = parse("# configuration\nAPI_KEY=abc=123#suffix\nEMPTY=\nexport MODEL='my model'\nURL=\"https://example.test/v1\" # comment\nLITERAL=$(touch /tmp/not-executed)\nOTHER=${API_KEY}\nUNICODE=\"模型\"\n").unwrap();
        assert_eq!(vars["API_KEY"], "abc=123#suffix");
        assert_eq!(vars["EMPTY"], "");
        assert_eq!(vars["MODEL"], "my model");
        assert_eq!(vars["URL"], "https://example.test/v1");
        assert_eq!(vars["LITERAL"], "$(touch /tmp/not-executed)");
        assert_eq!(vars["OTHER"], "${API_KEY}");
        assert_eq!(vars["UNICODE"], "模型");
    }

    #[test]
    fn invalid_values_never_leak_secrets_in_errors() {
        for value in [
            "API_KEY=\"super-secret",
            "API_KEY=\"super-secret\" garbage",
            "1KEY=super-secret",
            "API_KEY=super-secret\nAPI_KEY=duplicate",
            "API_KEY=\"super-secret\\x\"",
            "API_KEY=super-secret\0",
        ] {
            let error = parse(value).unwrap_err().to_string();
            assert!(!error.contains("super-secret"));
            assert!(error.contains("line"));
        }
    }

    #[test]
    fn explicit_missing_file_is_not_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(select(Some(&dir.path().join("missing"))).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn deployment_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environment");
        std::fs::write(&path, "API_KEY=private\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&path).unwrap()["API_KEY"], "private");
    }
}
