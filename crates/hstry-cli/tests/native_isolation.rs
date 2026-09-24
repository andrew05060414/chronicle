use std::{fs, process::Command};

#[test]
fn corrupt_hstry_search_db_and_config_does_not_block_native_path() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let hstry_env_dir = temp.path().join("corrupt_hstry");
    let hstry_config_dir = hstry_env_dir.join("hstry");
    fs::create_dir_all(&hstry_config_dir)?;

    // 1. Write completely malformed/corrupt hstry config.toml and unparseable search DB
    let corrupt_config = hstry_config_dir.join("config.toml");
    fs::write(
        &corrupt_config,
        b"[[[invalid toml content@@@!!! not a valid config",
    )?;
    let corrupt_db = hstry_env_dir.join("corrupt_history.db");
    fs::write(&corrupt_db, b"GARBAGE_NOT_A_SQLITE_DATABASE_DATA")?;

    // 2. Verify that standard hstry CLI commands fail due to the corrupt config
    let standard_run = Command::new(env!("CARGO_BIN_EXE_hstry"))
        .arg("list")
        .env("XDG_CONFIG_HOME", &hstry_env_dir)
        .env("APPDATA", &hstry_env_dir)
        .env("HSTRY_NO_SERVICE", "1")
        .env("HSTRY_DATABASE", &corrupt_db)
        .output()?;
    assert!(
        !standard_run.status.success(),
        "standard hstry commands must fail when hstry config is corrupt"
    );

    // 3. Set up a synthetic native recovery environment in an isolated temp root
    let source_dir = temp.path().join("source_codex");
    fs::create_dir_all(source_dir.join("sessions"))?;
    let sample_session_content =
        b"{\"type\":\"session\",\"session_id\":\"synthetic-session-123\"}\n";
    fs::write(
        source_dir.join("sessions/session_123.jsonl"),
        sample_session_content,
    )?;

    let native_root = temp.path().join("native_backup_root");
    let native_config_path = temp.path().join("chronicle-native.toml");
    let native_config_content = format!(
        r#"
data_root = "{}"

[[sources]]
app = "codex"
component = "sessions"
slot = "sessions"
path = "{}"
host_version = "fixture-v1"
"#,
        native_root.display().to_string().replace('\\', "/"),
        source_dir
            .join("sessions")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    fs::write(&native_config_path, native_config_content)?;

    // 4. Capture a synthetic snapshot using `native capture`
    let capture_output = Command::new(env!("CARGO_BIN_EXE_hstry"))
        .args([
            "native",
            "--native-config",
            native_config_path.to_str().unwrap(),
            "--root",
            native_root.to_str().unwrap(),
            "capture",
        ])
        .env("XDG_CONFIG_HOME", &hstry_env_dir)
        .env("APPDATA", &hstry_env_dir)
        .env("HSTRY_DATABASE", &corrupt_db)
        .output()?;
    assert!(
        capture_output.status.success(),
        "native capture must succeed even when hstry config/db is garbage: {}",
        String::from_utf8_lossy(&capture_output.stderr)
    );

    let capture_json: serde_json::Value = serde_json::from_slice(&capture_output.stdout)?;
    assert_eq!(capture_json["ok"], true);
    assert_eq!(capture_json["result"]["status"], "captured");
    let snapshot_id = capture_json["result"]["snapshot_id"]
        .as_str()
        .expect("snapshot_id present")
        .to_string();

    // 5. Test `native status` succeeds despite garbage hstry config and search DB
    let status_output = Command::new(env!("CARGO_BIN_EXE_hstry"))
        .args([
            "native",
            "--native-config",
            native_config_path.to_str().unwrap(),
            "--root",
            native_root.to_str().unwrap(),
            "status",
        ])
        .env("XDG_CONFIG_HOME", &hstry_env_dir)
        .env("APPDATA", &hstry_env_dir)
        .env("HSTRY_DATABASE", &corrupt_db)
        .output()?;
    assert!(
        status_output.status.success(),
        "native status must succeed when hstry config/db is garbage: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );
    let status_json: serde_json::Value = serde_json::from_slice(&status_output.stdout)?;
    assert_eq!(status_json["ok"], true);
    assert_eq!(
        status_json["result"]["local_snapshot"].as_str().unwrap(),
        snapshot_id
    );

    // 6. Test `native restore --target <dir>` succeeds despite garbage hstry config and search DB
    let extract_dir = temp.path().join("extracted_target");
    let restore_output = Command::new(env!("CARGO_BIN_EXE_hstry"))
        .args([
            "native",
            "--native-config",
            native_config_path.to_str().unwrap(),
            "--root",
            native_root.to_str().unwrap(),
            "restore",
            &snapshot_id,
            "--target",
            extract_dir.to_str().unwrap(),
        ])
        .env("XDG_CONFIG_HOME", &hstry_env_dir)
        .env("APPDATA", &hstry_env_dir)
        .env("HSTRY_DATABASE", &corrupt_db)
        .output()?;
    assert!(
        restore_output.status.success(),
        "native restore must succeed when hstry config/db is garbage: {}",
        String::from_utf8_lossy(&restore_output.stderr)
    );
    let restore_json: serde_json::Value = serde_json::from_slice(&restore_output.stdout)?;
    assert_eq!(restore_json["ok"], true);

    // 7. Verify extracted file is byte-identical
    let restored_file = extract_dir.join("codex/sessions/session_123.jsonl");
    assert!(
        restored_file.exists(),
        "restored file must exist at {}",
        restored_file.display()
    );
    assert_eq!(
        fs::read(&restored_file)?,
        sample_session_content,
        "restored file must match captured content byte-for-byte"
    );

    Ok(())
}
