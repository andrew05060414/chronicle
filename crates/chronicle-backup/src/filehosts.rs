//! Restore rules and safety policies for file-based hosts:
//! Claude Code, Antigravity, and Grok.
//!
//! SAFETY RULES:
//! - Never reads or touches live profiles (~/.codex, ~/.cursor, ~/.claude, ~/.gemini, ~/.grok).
//! - All host layouts derived strictly from existing repository adapters and default_sources in lib.rs.
//! - Unknown host versions are extract-only: native install is refused.
//! - Blind cross-device path copying of encoded cwd is refused without explicit mapping.
//! - Unsupported items (credentials, runtime locks, sockets, transient caches) fail closed.

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

/// Identifies if a relative file path under a given host slot is located under
/// a path segment that encodes a working directory (e.g. claude-code projects/<encoded-cwd>/
/// or grok sessions/<url-encoded-workspace>/).
///
/// In Claude Code slot "projects" and Grok slot "sessions", the first path segment
/// of any nested file represents the working directory. If a relative path has a
/// further segment beneath the first, that first segment is returned.
pub(super) fn encoded_cwd_segment<'a>(app: &str, slot: &str, relative: &'a str) -> Option<&'a str> {
    let is_cwd_slot = match app {
        "claude-code" => slot == "projects",
        "grok" => slot == "sessions",
        _ => false,
    };
    if !is_cwd_slot {
        return None;
    }
    let trimmed = relative.trim_start_matches(['/', '\\']);
    let (first, rest) = trimmed.split_once(['/', '\\'])?;
    let rest_trimmed = rest.trim_start_matches(['/', '\\']);
    if !first.is_empty() && !rest_trimmed.is_empty() {
        Some(first)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{encoded_cwd_segment, is_unsupported_path};
    use crate::{
        AppArgs, ComponentKind, NativeConfig, SourceConfig, apply_install_plan, build_install_plan,
        capture_sources, extract_snapshot, hash_file, verify_local,
    };
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

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
    fn filehosts_encoded_cwd_segment_slot_matching() {
        assert_eq!(
            encoded_cwd_segment(
                "claude-code",
                "projects",
                "C--Users-Test-proj/session.jsonl"
            ),
            Some("C--Users-Test-proj")
        );
        assert_eq!(
            encoded_cwd_segment("claude-code", "projects", "-home-user-proj/session.jsonl"),
            Some("-home-user-proj")
        );
        assert_eq!(
            encoded_cwd_segment("claude-code", "projects", "session.jsonl"),
            None
        );
        assert_eq!(
            encoded_cwd_segment("claude-code", "tasks", "C--Users-Test-proj/task.json"),
            None
        );
        assert_eq!(
            encoded_cwd_segment("grok", "sessions", "C%3A%5CWork/s1/chat_history.jsonl"),
            Some("C%3A%5CWork")
        );
        assert_eq!(
            encoded_cwd_segment("grok", "sessions", "session.jsonl"),
            None
        );
        assert_eq!(
            encoded_cwd_segment("antigravity", "app", "C--Users-Test-proj/sess.db"),
            None
        );
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

        // Install only from snapshot into source_home (restoring the deleted source)
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();
        let plan = build_install_plan(
            &manifest,
            "claude-code",
            Some(&source_home),
            &[],
            &backup_root,
        )
        .unwrap();

        let report = apply_install_plan(&backup_root, plan, true).unwrap();
        assert_eq!(report["file_verification"], "passed");

        for (rel, expected_hash) in original_hashes {
            let restored = source_home.join(&rel);
            assert!(restored.is_file(), "missing restored file: {rel}");
            let (_, hash) = hash_file(&restored).unwrap();
            assert_eq!(hash, expected_hash, "hash mismatch on {rel}");
        }
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

        // Install only from snapshot into source_home (restoring the deleted source)
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();
        let plan = build_install_plan(
            &manifest,
            "antigravity",
            Some(&source_home),
            &[],
            &backup_root,
        )
        .unwrap();

        let report = apply_install_plan(&backup_root, plan, true).unwrap();
        assert_eq!(report["file_verification"], "passed");

        for (rel, expected_hash) in original_hashes {
            let restored = source_home.join(&rel);
            assert!(restored.is_file(), "missing restored file: {rel}");
            let (_, hash) = hash_file(&restored).unwrap();
            assert_eq!(hash, expected_hash, "hash mismatch on {rel}");
        }
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

        // Install only from snapshot into source_home (restoring the deleted source)
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();
        let plan =
            build_install_plan(&manifest, "grok", Some(&source_home), &[], &backup_root).unwrap();

        let report = apply_install_plan(&backup_root, plan, true).unwrap();
        assert_eq!(report["file_verification"], "passed");

        for (rel, expected_hash) in original_hashes {
            let restored = source_home.join(&rel);
            assert!(restored.is_file(), "missing restored file: {rel}");
            let (_, hash) = hash_file(&restored).unwrap();
            assert_eq!(hash, expected_hash, "hash mismatch on {rel}");
        }
    }

    #[test]
    fn filehosts_unknown_host_version_refuses_install_and_extract_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source_home");
        let backup_root = temp.path().join("backup_root");
        let extract_dir = temp.path().join("extract_dir");

        let original_hashes = setup_synthetic_claude_code(&source_home);

        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "claude-code".into(),
                component: ComponentKind::Sessions,
                slot: "projects".into(),
                path: source_home.join(".claude/projects"),
                host_version: Some("unknown-99.9.9".into()),
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
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();

        fs::remove_dir_all(&source_home).unwrap();
        assert!(!source_home.exists());

        // Install MUST be refused for unverified host version
        let err = build_install_plan(
            &manifest,
            "claude-code",
            Some(&source_home),
            &[],
            &backup_root,
        )
        .unwrap_err();
        let err_text = err.to_string();
        assert!(
            err_text.contains("native installation has not been verified")
                || err_text.contains("unknown host version"),
            "unexpected error message: {err_text}"
        );
        assert!(!source_home.exists());

        // Extract MUST succeed regardless of host version
        extract_snapshot(&backup_root, snapshot_id, &extract_dir).unwrap();
        for (rel, expected_hash) in original_hashes {
            if rel.starts_with(".claude/projects") {
                let sub = rel.strip_prefix(".claude/projects/").unwrap();
                let extracted = extract_dir.join("claude-code/projects").join(sub);
                assert!(extracted.is_file(), "missing extracted file: {sub}");
                let (_, hash) = hash_file(&extracted).unwrap();
                assert_eq!(hash, expected_hash);
            }
        }
    }

    #[test]
    fn filehosts_encoded_cwd_refused_without_map_and_installed_with_map() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source_home");
        let backup_root = temp.path().join("backup_root");
        let other_home = temp.path().join("other_home");

        // 1. Claude Code realistic encoded cwd
        let claude_cwd_seg = "C--Users-Test-proj";
        let project_dir = source_home.join(".claude/projects").join(claude_cwd_seg);
        fs::create_dir_all(&project_dir).unwrap();
        let session_file = project_dir.join("session.jsonl");
        fs::write(&session_file, b"{\"type\":\"session\"}\n").unwrap();
        let (_, original_hash) = hash_file(&session_file).unwrap();

        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "claude-code".into(),
                component: ComponentKind::Sessions,
                slot: "projects".into(),
                path: source_home.join(".claude/projects"),
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
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();

        // 1a. Cross-device restore without --map MUST be refused with exact naming
        let err = build_install_plan(
            &manifest,
            "claude-code",
            Some(&other_home),
            &[],
            &backup_root,
        )
        .unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("blind cross-device path copy refused"),
            "expected refusal, got: {err_msg}"
        );
        assert!(
            err_msg.contains(claude_cwd_seg),
            "expected error to name the segment, got: {err_msg}"
        );

        // 1b. Cross-device restore with explicit --map MUST succeed and install under mapped segment
        let map_arg = format!("{claude_cwd_seg}=mapped-project");
        let plan = build_install_plan(
            &manifest,
            "claude-code",
            Some(&other_home),
            &[map_arg],
            &backup_root,
        )
        .unwrap();

        let report = apply_install_plan(&backup_root, plan, true).unwrap();
        assert_eq!(report["file_verification"], "passed");

        let mapped_file = other_home.join(".claude/projects/mapped-project/session.jsonl");
        assert!(mapped_file.is_file(), "mapped file missing at target");
        let (_, hash) = hash_file(&mapped_file).unwrap();
        assert_eq!(hash, original_hash);

        // 1c. Same-location restore without --map keeps paths unchanged
        let same_plan = build_install_plan(
            &manifest,
            "claude-code",
            Some(&source_home),
            &[],
            &backup_root,
        )
        .unwrap();
        assert_eq!(
            same_plan.actions[0].target,
            source_home
                .join(".claude/projects")
                .join(claude_cwd_seg)
                .join("session.jsonl")
        );

        // 2. Grok realistic url-encoded workspace cwd
        let grok_home = temp.path().join("grok_home");
        let grok_other = temp.path().join("grok_other");
        let grok_enc = "C%3A%5CWork";
        let grok_sess = grok_home
            .join(".grok/sessions")
            .join(grok_enc)
            .join("sess1");
        fs::create_dir_all(&grok_sess).unwrap();
        let grok_file = grok_sess.join("chat_history.jsonl");
        fs::write(&grok_file, b"{\"type\":\"msg\"}\n").unwrap();
        let (_, grok_hash) = hash_file(&grok_file).unwrap();

        let grok_config = NativeConfig {
            sources: vec![SourceConfig {
                app: "grok".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: grok_home.join(".grok/sessions"),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let grok_cap = capture_sources(
            &grok_config,
            &backup_root,
            &AppArgs::default(),
            Some(&grok_home),
        )
        .unwrap();
        let grok_id = grok_cap["snapshot_id"].as_str().unwrap();
        let grok_manifest = verify_local(&backup_root, grok_id).unwrap();

        // 2a. Refused without --map
        let grok_err =
            build_install_plan(&grok_manifest, "grok", Some(&grok_other), &[], &backup_root)
                .unwrap_err();
        assert!(
            grok_err
                .to_string()
                .contains("blind cross-device path copy refused")
        );
        assert!(grok_err.to_string().contains(grok_enc));

        // 2b. Installed under mapped segment with --map
        let grok_plan = build_install_plan(
            &grok_manifest,
            "grok",
            Some(&grok_other),
            &[format!("{grok_enc}=mapped-grok-ws")],
            &backup_root,
        )
        .unwrap();
        let grok_rep = apply_install_plan(&backup_root, grok_plan, true).unwrap();
        assert_eq!(grok_rep["file_verification"], "passed");
        let grok_restored =
            grok_other.join(".grok/sessions/mapped-grok-ws/sess1/chat_history.jsonl");
        assert!(grok_restored.is_file());
        let (_, h) = hash_file(&grok_restored).unwrap();
        assert_eq!(h, grok_hash);

        // 2c. Same-location restore without --map keeps paths unchanged
        let grok_same_plan =
            build_install_plan(&grok_manifest, "grok", Some(&grok_home), &[], &backup_root)
                .unwrap();
        assert_eq!(
            grok_same_plan.actions[0].target,
            grok_home
                .join(".grok/sessions")
                .join(grok_enc)
                .join("sess1")
                .join("chat_history.jsonl")
        );
    }

    #[test]
    fn filehosts_unsupported_items_are_never_installed() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source_home");
        let backup_root = temp.path().join("backup_root");
        let restore_home = temp.path().join("restore_home");

        // 1. Claude Code: valid session + unsupported items
        let proj = source_home.join(".claude/projects/C--Users-Test-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("session.jsonl"), b"{\"type\":\"user\"}\n").unwrap();

        fs::write(
            source_home.join(".claude/projects/.claude.json"),
            b"secret_token",
        )
        .unwrap();
        fs::create_dir_all(source_home.join(".claude/projects/C--Users-Test-proj/cache")).unwrap();
        fs::write(
            source_home.join(".claude/projects/C--Users-Test-proj/cache/index.db"),
            b"cache",
        )
        .unwrap();
        fs::write(source_home.join(".claude/projects/daemon.lock"), b"lock").unwrap();

        // 2. Antigravity: valid session + unsupported items
        let agy_app = source_home.join(".gemini/antigravity/conversations");
        fs::create_dir_all(&agy_app).unwrap();
        let agy_db = agy_app.join("session.db");
        {
            let conn = Connection::open(&agy_db).unwrap();
            conn.execute_batch("CREATE TABLE conversations (id TEXT PRIMARY KEY);")
                .unwrap();
        }
        fs::write(agy_app.join("oauth_credentials.json"), b"auth_secret").unwrap();
        fs::write(agy_app.join("session.db-wal"), b"wal_data").unwrap();
        fs::write(agy_app.join("daemon.sock"), b"sock_data").unwrap();

        // 3. Grok: valid session + unsupported items
        let grok_sess = source_home.join(".grok/sessions/C%3A%5CWork/s1");
        fs::create_dir_all(&grok_sess).unwrap();
        fs::write(grok_sess.join("chat_history.jsonl"), b"{\"msg\":1}\n").unwrap();
        let grok_term = source_home.join(".grok/sessions/terminal");
        fs::create_dir_all(&grok_term).unwrap();
        fs::write(grok_term.join("term.sock"), b"pty").unwrap();
        fs::write(
            source_home.join(".grok/sessions/prompt_history.jsonl"),
            b"history",
        )
        .unwrap();
        let grok_compact = source_home.join(".grok/sessions/compaction");
        fs::create_dir_all(&grok_compact).unwrap();
        fs::write(grok_compact.join("compact.lock"), b"lock").unwrap();

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
                    app: "antigravity".into(),
                    component: ComponentKind::Sessions,
                    slot: "app".into(),
                    path: agy_app.clone(),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "grok".into(),
                    component: ComponentKind::Sessions,
                    slot: "sessions".into(),
                    path: source_home.join(".grok/sessions"),
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
        let manifest = verify_local(&backup_root, snapshot_id).unwrap();

        // Verify exclusions recorded unsupported items across all hosts
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains(".claude.json"))
        );
        assert!(manifest.exclusions.iter().any(|e| e.contains("cache")));
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("daemon.lock"))
        );
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("oauth_credentials.json"))
        );
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("session.db-wal"))
        );
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("daemon.sock"))
        );
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("terminal") || e.contains("term.sock"))
        );
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("prompt_history.jsonl"))
        );
        assert!(
            manifest
                .exclusions
                .iter()
                .any(|e| e.contains("compaction") || e.contains("compact.lock"))
        );

        // Install claude-code to restore_home (with map for cross-device)
        let claude_plan = build_install_plan(
            &manifest,
            "claude-code",
            Some(&restore_home),
            &["C--Users-Test-proj=mapped-proj".into()],
            &backup_root,
        )
        .unwrap();
        let report = apply_install_plan(&backup_root, claude_plan, true).unwrap();
        assert_eq!(report["file_verification"], "passed");

        // Verify valid session file was installed
        assert!(
            restore_home
                .join(".claude/projects/mapped-proj/session.jsonl")
                .is_file()
        );

        // Verify unsupported items were NEVER installed
        assert!(!restore_home.join(".claude/projects/.claude.json").exists());
        assert!(
            !restore_home
                .join(".claude/projects/mapped-proj/cache")
                .exists()
        );
        assert!(!restore_home.join(".claude/projects/daemon.lock").exists());

        // Install antigravity to restore_home
        let agy_plan = build_install_plan(
            &manifest,
            "antigravity",
            Some(&restore_home),
            &[],
            &backup_root,
        )
        .unwrap();
        apply_install_plan(&backup_root, agy_plan, true).unwrap();
        assert!(
            restore_home
                .join(".gemini/antigravity/conversations/session.db")
                .is_file()
        );
        assert!(
            !restore_home
                .join(".gemini/antigravity/conversations/oauth_credentials.json")
                .exists()
        );
        assert!(
            !restore_home
                .join(".gemini/antigravity/conversations/session.db-wal")
                .exists()
        );
        assert!(
            !restore_home
                .join(".gemini/antigravity/conversations/daemon.sock")
                .exists()
        );

        // Install grok to restore_home (with map for cross-device)
        let grok_plan = build_install_plan(
            &manifest,
            "grok",
            Some(&restore_home),
            &["C%3A%5CWork=mapped-grok".into()],
            &backup_root,
        )
        .unwrap();
        apply_install_plan(&backup_root, grok_plan, true).unwrap();
        assert!(
            restore_home
                .join(".grok/sessions/mapped-grok/s1/chat_history.jsonl")
                .is_file()
        );
        assert!(!restore_home.join(".grok/sessions/terminal").exists());
        assert!(
            !restore_home
                .join(".grok/sessions/prompt_history.jsonl")
                .exists()
        );
        assert!(!restore_home.join(".grok/sessions/compaction").exists());
    }
}
