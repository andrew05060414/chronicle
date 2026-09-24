//! Capture rules for file-based hosts: Claude Code, Antigravity, and Grok.
//!
//! Host layouts come from the repository adapters and `default_sources` in lib.rs.
//! Unsupported items (credentials, runtime locks, sockets, transient caches) are
//! excluded from snapshots.

/// Check if a relative path corresponds to an unsupported file or directory for a host.
pub(super) fn is_unsupported_path(app: &str, rel_path: &str) -> Option<&'static str> {
    let s = rel_path.replace('\\', "/");
    let file_name = s.rsplit('/').next().unwrap_or(&s);

    if file_name.ends_with(".lock") || file_name.ends_with(".sock") || file_name.ends_with(".pid") {
        return Some("process lock or socket");
    }
    if file_name.ends_with("-wal") || file_name.ends_with("-shm") || file_name.ends_with("-journal")
    {
        return Some("sqlite lock or journal file");
    }

    match app {
        "claude-code" => {
            if file_name == ".claude.json" || file_name == "credentials.json" {
                return Some("credential file");
            }
            if s.split('/').any(|seg| {
                matches!(
                    seg,
                    "cache" | "telemetry" | "tmp" | "mcp-daemons" | "plugins"
                )
            }) {
                return Some("transient cache, telemetry or runtime daemon");
            }
        }
        "antigravity" => {
            let lower = file_name.to_ascii_lowercase();
            if lower.contains("credential") || lower.contains("token") || lower.contains("auth") {
                return Some("antigravity credential");
            }
            if s.contains("checkpoints/in-flight") {
                return Some("in-flight checkpoint");
            }
        }
        "grok" => {
            if s.split('/')
                .any(|seg| matches!(seg, "terminal" | "compaction"))
            {
                return Some("grok terminal buffer or compaction lock");
            }
            if file_name == "prompt_history.jsonl" {
                return Some("grok prompt history");
            }
            let lower = file_name.to_ascii_lowercase();
            if lower.contains("cookie") || lower.contains("auth") {
                return Some("grok auth");
            }
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::is_unsupported_path;
    use crate::{
        AppArgs, ComponentKind, NativeConfig, SourceConfig, capture_sources, extract_snapshot,
        hash_file, verify_local,
    };
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    /// Every original file must come back byte-identical under the extraction root.
    fn assert_extracted_matches(
        extract_dir: &Path,
        manifest: &crate::Manifest,
        originals: &BTreeMap<String, String>,
    ) {
        let extracted: Vec<(String, String)> = manifest
            .files
            .iter()
            .map(|file| {
                let (_, hash) = hash_file(&extract_dir.join(&file.relative)).unwrap();
                (file.relative.clone(), hash)
            })
            .collect();
        for (rel, expected_hash) in originals {
            let name = rel.rsplit('/').next().unwrap();
            assert!(
                extracted
                    .iter()
                    .any(|(path, hash)| path.ends_with(name) && hash == expected_hash),
                "missing or changed extracted file: {rel}"
            );
        }
    }

    fn setup_synthetic_claude_code(home: &Path) -> BTreeMap<String, String> {
        let projects_dir = home.join(".claude/projects/C--Users-Test-proj");
        let tasks_dir = home.join(".claude/tasks");
        let file_history_dir = home.join(".claude/file-history");

        fs::create_dir_all(&projects_dir).unwrap();
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::create_dir_all(&file_history_dir).unwrap();

        let session_file = projects_dir.join("session-alpha.jsonl");
        fs::write(
            &session_file,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        )
        .unwrap();

        let task_file = tasks_dir.join("task-101.json");
        fs::write(
            &task_file,
            "{\"id\":\"task-101\",\"title\":\"Synthetic task\"}",
        )
        .unwrap();

        let history_file = file_history_dir.join("fh-abc12345");
        fs::write(&history_file, "pub fn authenticate() -> bool { true }").unwrap();

        let mut hashes = BTreeMap::new();
        for path in [&session_file, &task_file, &history_file] {
            let (_, h) = hash_file(path).unwrap();
            let rel = path
                .strip_prefix(home)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            hashes.insert(rel, h);
        }
        hashes
    }

    fn setup_synthetic_antigravity(home: &Path) -> BTreeMap<String, String> {
        let app_conv_dir = home.join(".gemini/antigravity/conversations");
        let brain_dir =
            home.join(".gemini/antigravity-cli/brain/session-agy-001/.system_generated/logs");
        let ide_dir = home.join(".gemini/antigravity-ide");
        let tmp_dir = home.join(".gemini/tmp/chats");

        fs::create_dir_all(&app_conv_dir).unwrap();
        fs::create_dir_all(&brain_dir).unwrap();
        fs::create_dir_all(&ide_dir).unwrap();
        fs::create_dir_all(&tmp_dir).unwrap();

        let db_file = app_conv_dir.join("session-agy-001.db");
        {
            let conn = Connection::open(&db_file).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (id TEXT PRIMARY KEY, title TEXT);
                 INSERT INTO conversations VALUES ('session-agy-001', 'Test Conv');",
            )
            .unwrap();
            drop(conn);
            let temp_db = app_conv_dir.join("temp.db");
            crate::backup_sqlite(&db_file, &temp_db).unwrap();
            fs::rename(&temp_db, &db_file).unwrap();
        }

        let brain_file = brain_dir.join("transcript.jsonl");
        fs::write(
            &brain_file,
            "{\"type\":\"thought\",\"content\":\"processing\"}\n",
        )
        .unwrap();

        let ide_file = ide_dir.join("session-state.pb");
        fs::write(&ide_file, b"\x08\x96\x01\x12\x07agy-ide\x18\x01").unwrap();

        let tmp_file = tmp_dir.join("session-legacy.jsonl");
        fs::write(&tmp_file, "{\"sessionId\":\"legacy-001\"}\n").unwrap();

        let mut hashes = BTreeMap::new();
        for path in [&db_file, &brain_file, &ide_file, &tmp_file] {
            let (_, h) = hash_file(path).unwrap();
            let rel = path
                .strip_prefix(home)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            hashes.insert(rel, h);
        }
        hashes
    }

    fn setup_synthetic_grok(home: &Path) -> BTreeMap<String, String> {
        let session_dir = home.join(".grok/sessions/C%3A%5CWork/session-grok-888");
        let attachments_dir = session_dir.join("attachments");

        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&attachments_dir).unwrap();

        let chat_file = session_dir.join("chat_history.jsonl");
        fs::write(
            &chat_file,
            "{\"type\":\"system\",\"content\":\"Grok test harness\"}\n",
        )
        .unwrap();

        let summary_file = session_dir.join("summary.json");
        fs::write(
            &summary_file,
            "{\"info\":{\"id\":\"session-grok-888\",\"cwd\":\"/workspace\"}}",
        )
        .unwrap();

        let attachment_file = attachments_dir.join("diagram.png");
        fs::write(&attachment_file, b"\x89PNGfake").unwrap();

        let mut hashes = BTreeMap::new();
        for path in [&chat_file, &summary_file, &attachment_file] {
            let (_, h) = hash_file(path).unwrap();
            let rel = path
                .strip_prefix(home)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            hashes.insert(rel, h);
        }
        hashes
    }

    #[test]
    fn filehosts_unsupported_path_detection() {
        // Generic
        assert!(is_unsupported_path("claude-code", "test.lock").is_some());
        assert!(is_unsupported_path("any-app", "daemon.sock").is_some());
        assert!(is_unsupported_path("any-app", "process.pid").is_some());
        assert!(is_unsupported_path("any-app", "db.sqlite-wal").is_some());
        assert!(is_unsupported_path("any-app", "db.sqlite-shm").is_some());
        assert!(is_unsupported_path("any-app", "db.sqlite-journal").is_some());

        // Claude Code
        assert!(is_unsupported_path("claude-code", ".claude.json").is_some());
        assert!(is_unsupported_path("claude-code", "credentials.json").is_some());
        assert!(is_unsupported_path("claude-code", "cache/data.bin").is_some());
        assert!(is_unsupported_path("claude-code", "telemetry/events.json").is_some());
        assert!(is_unsupported_path("claude-code", "tmp/scratch.txt").is_some());
        assert!(is_unsupported_path("claude-code", "mcp-daemons/daemon.json").is_some());
        assert!(is_unsupported_path("claude-code", "plugins/plugin-a.json").is_some());
        assert!(is_unsupported_path("claude-code", "C--Users-Test-proj/session.jsonl").is_none());

        // Antigravity
        assert!(is_unsupported_path("antigravity", "oauth_credentials.json").is_some());
        assert!(is_unsupported_path("antigravity", "user_token.json").is_some());
        assert!(is_unsupported_path("antigravity", "auth_keys.bin").is_some());
        assert!(is_unsupported_path("antigravity", "checkpoints/in-flight/buf.bin").is_some());
        assert!(is_unsupported_path("antigravity", "conversations/sess.db").is_none());

        // Grok
        assert!(is_unsupported_path("grok", "terminal/pty.sock").is_some());
        assert!(is_unsupported_path("grok", "compaction/merge.lock").is_some());
        assert!(is_unsupported_path("grok", "prompt_history.jsonl").is_some());
        assert!(is_unsupported_path("grok", "cookies.json").is_some());
        assert!(is_unsupported_path("grok", "auth_tokens.txt").is_some());
        assert!(is_unsupported_path("grok", "C%3A%5CWork/sess/chat_history.jsonl").is_none());
    }

    #[test]
    fn filehosts_claude_code_capture_delete_and_restore_matches_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source_home");
        let backup_root = temp.path().join("backup_root");

        let original_hashes = setup_synthetic_claude_code(&source_home);
        assert!(!original_hashes.is_empty());

        let config = NativeConfig {
            sources: vec![
                SourceConfig {
                    app: "claude-code".into(),
                    component: ComponentKind::Sessions,
                    slot: "projects".into(),
                    path: source_home.join(".claude/projects"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "claude-code".into(),
                    component: ComponentKind::Sessions,
                    slot: "tasks".into(),
                    path: source_home.join(".claude/tasks"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "claude-code".into(),
                    component: ComponentKind::Sessions,
                    slot: "file-history".into(),
                    path: source_home.join(".claude/file-history"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let captured = capture_sources(
            &config,
            &backup_root,
            &AppArgs::default(),
            Some(&source_home),
        )
        .unwrap();
        let snapshot_id = captured["snapshot_id"].as_str().unwrap();

        // DELETE source directory completely
        fs::remove_dir_all(&source_home).unwrap();
        assert!(!source_home.exists());

        // Recover from the snapshot alone into an isolated directory.
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();
        let extract_dir = temp.path().join("extracted");
        extract_snapshot(&backup_root, snapshot_id, &extract_dir).unwrap();
        assert_extracted_matches(&extract_dir, &manifest, &original_hashes);
    }

    #[test]
    fn filehosts_antigravity_capture_delete_and_restore_matches_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source_home");
        let backup_root = temp.path().join("backup_root");

        let original_hashes = setup_synthetic_antigravity(&source_home);
        assert!(!original_hashes.is_empty());

        let config = NativeConfig {
            sources: vec![
                SourceConfig {
                    app: "antigravity".into(),
                    component: ComponentKind::Sessions,
                    slot: "app".into(),
                    path: source_home.join(".gemini/antigravity/conversations"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "antigravity".into(),
                    component: ComponentKind::Sessions,
                    slot: "brain".into(),
                    path: source_home.join(".gemini/antigravity-cli/brain"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "antigravity".into(),
                    component: ComponentKind::Sessions,
                    slot: "ide".into(),
                    path: source_home.join(".gemini/antigravity-ide"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "antigravity".into(),
                    component: ComponentKind::Sessions,
                    slot: "tmp".into(),
                    path: source_home.join(".gemini/tmp"),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let captured = capture_sources(
            &config,
            &backup_root,
            &AppArgs::default(),
            Some(&source_home),
        )
        .unwrap();
        let snapshot_id = captured["snapshot_id"].as_str().unwrap();

        // DELETE source directory completely
        fs::remove_dir_all(&source_home).unwrap();
        assert!(!source_home.exists());

        // Recover from the snapshot alone into an isolated directory.
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();
        let extract_dir = temp.path().join("extracted");
        extract_snapshot(&backup_root, snapshot_id, &extract_dir).unwrap();
        assert_extracted_matches(&extract_dir, &manifest, &original_hashes);
    }

    #[test]
    fn filehosts_grok_capture_delete_and_restore_matches_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source_home");
        let backup_root = temp.path().join("backup_root");

        let original_hashes = setup_synthetic_grok(&source_home);
        assert!(!original_hashes.is_empty());

        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "grok".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_home.join(".grok/sessions"),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let captured = capture_sources(
            &config,
            &backup_root,
            &AppArgs::default(),
            Some(&source_home),
        )
        .unwrap();
        let snapshot_id = captured["snapshot_id"].as_str().unwrap();

        // DELETE source directory completely
        fs::remove_dir_all(&source_home).unwrap();
        assert!(!source_home.exists());

        // Recover from the snapshot alone into an isolated directory.
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();
        let extract_dir = temp.path().join("extracted");
        extract_snapshot(&backup_root, snapshot_id, &extract_dir).unwrap();
        assert_extracted_matches(&extract_dir, &manifest, &original_hashes);
    }
}
