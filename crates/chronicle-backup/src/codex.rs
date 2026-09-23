//! Version-checked Codex index merge. Raw rollout files are authoritative.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const THREAD_REQUIRED: &[&str] = &[
    "id",
    "rollout_path",
    "created_at",
    "updated_at",
    "source",
    "model_provider",
    "cwd",
    "title",
];

pub(super) fn merge_index(
    source: &Path,
    target: &Path,
    path_map: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    let existed = target.exists();
    let res = (|| -> Result<()> {
        if let Some(parent) = target.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut target_conn = Connection::open(target)?;
        target_conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let transaction = target_conn.transaction()?;

        let normalized = path_map
            .iter()
            .map(|(from, to)| (normalize(from), to.clone()))
            .collect::<BTreeMap<_, _>>();
        ensure!(
            normalized.len() == path_map.len(),
            "duplicate Codex path mapping"
        );

        merge_table(
            &source,
            &transaction,
            "projects",
            &["id"],
            &normalized,
            None,
        )?;
        merge_table(
            &source,
            &transaction,
            "thread_sections",
            &["id"],
            &normalized,
            None,
        )?;
        merge_table(
            &source,
            &transaction,
            "threads",
            &["id"],
            &normalized,
            Some("rollout_path"),
        )?;
        merge_table(
            &source,
            &transaction,
            "thread_dynamic_tools",
            &["thread_id", "position"],
            &normalized,
            None,
        )?;
        merge_table(
            &source,
            &transaction,
            "thread_spawn_edges",
            &["child_thread_id"],
            &normalized,
            None,
        )?;

        // Validate foreign keys inside the transaction
        let mut violations = Vec::new();
        {
            let mut stmt = transaction.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                let rowid: i64 = row.get(1)?;
                let parent: String = row.get(2)?;
                let fkid: i64 = row.get(3)?;
                violations.push(format!(
                    "table '{table}' rowid {rowid} references missing parent in '{parent}' (fkid {fkid})"
                ));
            }
        }
        ensure!(
            violations.is_empty(),
            "missing dependency: Codex foreign key check failed: {}",
            violations.join("; ")
        );

        transaction.commit()?;
        Ok(())
    })();

    if res.is_err() && !existed && target.exists() {
        let _ = std::fs::remove_file(target);
        for suffix in ["-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", target.display())));
        }
    }

    res
}

fn merge_table(
    source: &Connection,
    target: &Connection,
    table: &str,
    keys: &[&str],
    path_map: &BTreeMap<String, PathBuf>,
    mapped_path_column: Option<&str>,
) -> Result<()> {
    if !table_exists(source, table)? {
        return Ok(());
    }

    if !table_exists(target, table)? {
        let create_sql: Option<String> = source
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(sql) = create_sql {
            target.execute_batch(&sql)?;
            let mut stmt = source.prepare(
                "SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL",
            )?;
            let mut index_rows = stmt.query([table])?;
            while let Some(row) = index_rows.next()? {
                let index_sql: String = row.get(0)?;
                target.execute_batch(&index_sql)?;
            }
        }
    }

    ensure!(
        table_exists(target, table)?,
        "Codex target schema missing {table}"
    );

    let source_columns = table_columns(source, table)?;
    let target_columns = table_columns(target, table)?;
    if table == "threads" {
        for required in THREAD_REQUIRED {
            ensure!(
                source_columns.iter().any(|c| c == required),
                "Codex source schema missing threads.{required}"
            );
        }
    }
    for key in keys {
        ensure!(
            source_columns.iter().any(|c| c == key),
            "Codex source schema missing {table}.{key}"
        );
    }
    let columns = source_columns
        .iter()
        .filter(|c| target_columns.contains(c))
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        keys.iter().all(|key| columns.iter().any(|c| c == key)),
        "Codex target schema missing keys for {table}"
    );

    let column_sql = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(",");
    let key_predicate = keys
        .iter()
        .map(|key| format!("{}=?", quote(key)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let key_positions = keys
        .iter()
        .map(|key| columns.iter().position(|c| c == key).unwrap() + 1)
        .collect::<Vec<_>>();

    let insert_sql = format!(
        "INSERT INTO {}({column_sql}) VALUES ({})",
        quote(table),
        (1..=columns.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut select = source.prepare(&format!("SELECT {column_sql} FROM {}", quote(table)))?;
    let mut rows = select.query([])?;
    let mut registered = 0;
    let mut verified = 0;
    let mut skipped = 0;

    while let Some(row) = rows.next()? {
        let mut values = (0..columns.len())
            .map(|index| row.get::<_, Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Remap cwd if present on exact path-component boundaries
        if let Some(cwd_index) = columns.iter().position(|c| c == "cwd")
            && let Value::Text(cwd_str) = &values[cwd_index]
            && let Some(mapped_cwd) = remap_path_on_boundary(cwd_str, path_map)
        {
            let mapped_str = mapped_cwd.display().to_string();
            let formatted = if cwd_str.contains('/') && !cwd_str.contains('\\') {
                mapped_str.replace('\\', "/")
            } else {
                mapped_str
            };
            values[cwd_index] = Value::Text(formatted);
        }

        if table == "threads" {
            let path_index = columns
                .iter()
                .position(|c| Some(c.as_str()) == mapped_path_column)
                .unwrap();
            let Value::Text(path) = &values[path_index] else {
                skipped += 1;
                continue;
            };
            let mapped = remap_path_on_boundary(path, path_map);
            let Some(mapped) = mapped else {
                skipped += 1;
                continue;
            };
            ensure!(
                mapped.is_file(),
                "missing dependency: Codex rollout target missing before index registration: {}",
                mapped.display()
            );

            // Hash-verify rollout file before any DB registration
            let (target_bytes, target_hash) = hash_file(&mapped).with_context(|| {
                format!("failed to hash Codex rollout file: {}", mapped.display())
            })?;
            ensure!(
                target_bytes > 0,
                "Codex rollout file is empty: {}",
                mapped.display()
            );

            let source_candidate = Path::new(path);
            if source_candidate.is_file() {
                let (_, source_hash) = hash_file(source_candidate).with_context(|| {
                    format!(
                        "failed to hash source rollout file: {}",
                        source_candidate.display()
                    )
                })?;
                ensure!(
                    source_hash == target_hash,
                    "Codex rollout file hash mismatch: source {} ({}) != target {} ({})",
                    source_candidate.display(),
                    source_hash,
                    mapped.display(),
                    target_hash
                );
            }

            verified += 1;
            let mapped_str = mapped.display().to_string();
            let formatted = if path.contains('/') && !path.contains('\\') {
                mapped_str.replace('\\', "/")
            } else {
                mapped_str
            };
            values[path_index] = Value::Text(formatted);
        }

        // Explicit dependency checks for child tables
        if table == "thread_dynamic_tools" {
            if let Some(thread_id_idx) = columns.iter().position(|c| c == "thread_id")
                && let Value::Text(thread_id) = &values[thread_id_idx]
            {
                let thread_exists: bool = target.query_row(
                    "SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?1)",
                    [thread_id],
                    |r| r.get(0),
                )?;
                ensure!(
                    thread_exists,
                    "missing dependency: thread_dynamic_tools references missing thread '{thread_id}'"
                );
            }
        } else if table == "thread_spawn_edges" {
            if let Some(child_idx) = columns.iter().position(|c| c == "child_thread_id")
                && let Value::Text(child_id) = &values[child_idx]
            {
                let child_exists: bool = target.query_row(
                    "SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?1)",
                    [child_id],
                    |r| r.get(0),
                )?;
                ensure!(
                    child_exists,
                    "missing dependency: thread_spawn_edges references missing child thread '{child_id}'"
                );
            }
            if let Some(parent_idx) = columns.iter().position(|c| c == "parent_thread_id")
                && let Value::Text(parent_id) = &values[parent_idx]
                && !parent_id.is_empty()
            {
                let parent_exists: bool = target.query_row(
                    "SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?1)",
                    [parent_id],
                    |r| r.get(0),
                )?;
                ensure!(
                    parent_exists,
                    "missing dependency: thread_spawn_edges references missing parent thread '{parent_id}'"
                );
            }
        }

        let key_values = key_positions
            .iter()
            .map(|position| &values[position - 1])
            .collect::<Vec<_>>();

        let target_exists: bool = target.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {} WHERE {key_predicate})",
                quote(table)
            ),
            rusqlite::params_from_iter(key_values.clone()),
            |row| row.get(0),
        )?;

        if target_exists {
            let target_values = target.query_row(
                &format!(
                    "SELECT {column_sql} FROM {} WHERE {key_predicate}",
                    quote(table)
                ),
                rusqlite::params_from_iter(key_values.clone()),
                |row| {
                    (0..columns.len())
                        .map(|index| row.get::<_, Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                },
            )?;

            ensure!(
                values_equal(table, &columns, &target_values, &values),
                "conflict: same Codex {table} identity has different content"
            );
            skipped += 1;
            continue;
        }

        target.execute(&insert_sql, rusqlite::params_from_iter(values.iter()))?;
        registered += 1;
    }

    ensure!(
        table != "threads" || verified > 0,
        "Codex snapshot has no verified rollout files"
    );
    let _ = (registered, skipped);
    Ok(())
}

fn values_equal(_table: &str, columns: &[String], a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (i, (va, vb)) in a.iter().zip(b.iter()).enumerate() {
        if va == vb {
            continue;
        }
        let col = &columns[i];
        if (col == "rollout_path" || col == "cwd")
            && let (Value::Text(sa), Value::Text(sb)) = (va, vb)
            && normalize(sa) == normalize(sb)
        {
            continue;
        }
        return false;
    }
    true
}

fn remap_path_on_boundary(path_str: &str, path_map: &BTreeMap<String, PathBuf>) -> Option<PathBuf> {
    let norm_path = normalize(path_str);
    let norm_trimmed = norm_path.trim_end_matches('/');

    let mut candidates: Vec<_> = path_map.iter().collect();
    candidates.sort_by_key(|(b, _)| std::cmp::Reverse(b.len()));

    for (from, to) in candidates {
        let from_norm = normalize(from);
        let from_trimmed = from_norm.trim_end_matches('/');
        if from_trimmed.is_empty() {
            continue;
        }

        if norm_trimmed == from_trimmed {
            return Some(to.clone());
        }

        if let Some(remainder) = norm_trimmed.strip_prefix(from_trimmed)
            && let Some(stripped) = remainder.strip_prefix('/')
        {
            let orig_slash = path_str.replace('\\', "/");
            let orig_trimmed = orig_slash.trim_end_matches('/');
            let rel = if orig_trimmed.len() > from_trimmed.len() + 1 {
                &orig_trimmed[from_trimmed.len() + 1..]
            } else {
                stripped
            };
            return Some(to.join(rel));
        }
    }
    None
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

fn table_columns(connection: &Connection, name: &str) -> Result<Vec<String>> {
    let statement = &format!("PRAGMA table_info({})", quote(name));
    let mut statement = connection.prepare(statement)?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?)
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    let hash = format!("{:x}", hasher.finalize());
    Ok((bytes, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CODEX_SCHEMA: &str = r#"
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    metadata TEXT NOT NULL,
    position INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE thread_sections (
    id TEXT PRIMARY KEY,
    name TEXT,
    updated_at INTEGER
);

CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    rollout_path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    source TEXT NOT NULL,
    model_provider TEXT NOT NULL,
    cwd TEXT NOT NULL,
    title TEXT NOT NULL
);

CREATE TABLE thread_dynamic_tools (
    thread_id TEXT,
    position INTEGER,
    name TEXT,
    PRIMARY KEY(thread_id, position),
    FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
);

CREATE TABLE thread_spawn_edges (
    child_thread_id TEXT PRIMARY KEY,
    parent_thread_id TEXT,
    status TEXT,
    FOREIGN KEY(child_thread_id) REFERENCES threads(id) ON DELETE CASCADE
);
"#;

    fn setup_source_db(path: &PathBuf) -> (PathBuf, PathBuf) {
        let source_dir = path.parent().unwrap();
        let rollout_1 = source_dir.join("rollouts/thread-1.jsonl");
        let rollout_2 = source_dir.join("rollouts/thread-2.jsonl");
        fs::create_dir_all(rollout_1.parent().unwrap()).unwrap();
        fs::write(
            &rollout_1,
            b"{\"type\":\"session_meta\",\"custom_unknown_byte\":42}\n{\"type\":\"turn_context\",\"cwd\":\"C:/work/app\"}\n",
        )
        .unwrap();
        fs::write(
            &rollout_2,
            b"{\"type\":\"session_meta\",\"feature\":true}\n",
        )
        .unwrap();

        let conn = Connection::open(path).unwrap();
        conn.execute_batch(CODEX_SCHEMA).unwrap();

        conn.execute(
            "INSERT INTO projects VALUES ('proj-1', 'Project 1', '{}', 0, 1000, 2000)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO thread_sections VALUES ('sec-1', 'Main Section', 2000)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO threads VALUES ('t-1', ?1, 100, 200, 'user', 'openai', 'C:/work/app', 'Title 1')",
            [rollout_1.display().to_string()],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO threads VALUES ('t-2', ?1, 150, 250, 'user', 'openai', 'C:/workspace/other', 'Title 2')",
            [rollout_2.display().to_string()],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO thread_dynamic_tools VALUES ('t-1', 0, 'bash')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO thread_spawn_edges VALUES ('t-2', 't-1', 'active')",
            [],
        )
        .unwrap();

        (rollout_1, rollout_2)
    }

    #[test]
    fn test_merge_index_absent_target() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let target_db = temp.path().join("target_dir/non_existent_state.sqlite");
        let t_rollout_1 = temp.path().join("target_dir/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target_dir/rollouts/t2.jsonl");
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::copy(&s_rollout_1, &t_rollout_1).unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1.clone()),
            (s_rollout_2.display().to_string(), t_rollout_2.clone()),
            ("C:/work".into(), PathBuf::from("D:/restored_work")),
        ]);

        assert!(!target_db.exists());

        merge_index(&source_db, &target_db, &path_map).unwrap();

        assert!(target_db.is_file());
        let target_conn = Connection::open(&target_db).unwrap();
        let thread_count: i64 = target_conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(thread_count, 2);

        // Verify cwd mapping applied on component boundary
        let cwd_1: String = target_conn
            .query_row("SELECT cwd FROM threads WHERE id='t-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cwd_1, "D:/restored_work/app");

        // Verify cwd mapping did NOT apply to "C:/workspace" (not component boundary)
        let cwd_2: String = target_conn
            .query_row("SELECT cwd FROM threads WHERE id='t-2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cwd_2, "C:/workspace/other");

        let tools_count: i64 = target_conn
            .query_row("SELECT COUNT(*) FROM thread_dynamic_tools", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(tools_count, 1);

        let edges_count: i64 = target_conn
            .query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges_count, 1);
    }

    #[test]
    fn test_merge_index_existing_target_preserves_unrelated_rows() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let target_db = temp.path().join("target/state.sqlite");
        fs::create_dir_all(target_db.parent().unwrap()).unwrap();
        let target_conn = Connection::open(&target_db).unwrap();
        target_conn.execute_batch(CODEX_SCHEMA).unwrap();
        target_conn
            .execute(
                "INSERT INTO threads VALUES ('existing-t', 'D:/live/rollout.jsonl', 500, 600, 'user', 'openai', 'D:/project', 'Existing Thread')",
                [],
            )
            .unwrap();
        drop(target_conn);

        let t_rollout_1 = temp.path().join("target/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target/rollouts/t2.jsonl");
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::copy(&s_rollout_1, &t_rollout_1).unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1),
            (s_rollout_2.display().to_string(), t_rollout_2),
        ]);

        merge_index(&source_db, &target_db, &path_map).unwrap();

        let target_conn = Connection::open(&target_db).unwrap();
        let count: i64 = target_conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "should have 1 pre-existing + 2 merged threads");

        let existing_title: String = target_conn
            .query_row("SELECT title FROM threads WHERE id='existing-t'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(existing_title, "Existing Thread");
    }

    #[test]
    fn test_merge_index_idempotent_skip() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let target_db = temp.path().join("target/state.sqlite");
        let t_rollout_1 = temp.path().join("target/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target/rollouts/t2.jsonl");
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::copy(&s_rollout_1, &t_rollout_1).unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1),
            (s_rollout_2.display().to_string(), t_rollout_2),
        ]);

        // First merge
        merge_index(&source_db, &target_db, &path_map).unwrap();

        // Second merge: identical content -> must succeed as idempotent skip
        merge_index(&source_db, &target_db, &path_map).unwrap();

        let target_conn = Connection::open(&target_db).unwrap();
        let thread_count: i64 = target_conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(thread_count, 2);
    }

    #[test]
    fn test_merge_index_refuses_conflict_with_different_content() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let target_db = temp.path().join("target/state.sqlite");
        fs::create_dir_all(target_db.parent().unwrap()).unwrap();
        let target_conn = Connection::open(&target_db).unwrap();
        target_conn.execute_batch(CODEX_SCHEMA).unwrap();
        // Insert conflicting thread with same ID 't-1' but different title and updated_at
        target_conn
            .execute(
                "INSERT INTO threads VALUES ('t-1', 'D:/other/r.jsonl', 100, 999, 'user', 'openai', 'C:/work/app', 'Conflicting Local Title')",
                [],
            )
            .unwrap();
        drop(target_conn);

        let t_rollout_1 = temp.path().join("target/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target/rollouts/t2.jsonl");
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::copy(&s_rollout_1, &t_rollout_1).unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1),
            (s_rollout_2.display().to_string(), t_rollout_2),
        ]);

        // Must be refused with conflict error
        let err = merge_index(&source_db, &target_db, &path_map).unwrap_err();
        assert!(
            err.to_string().contains("conflict"),
            "expected conflict error, got: {err}"
        );

        // Target must be untouched (nothing written)
        let target_conn = Connection::open(&target_db).unwrap();
        let title: String = target_conn
            .query_row("SELECT title FROM threads WHERE id='t-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "Conflicting Local Title");

        // t-2 must NOT have been written either (transaction was rolled back)
        let t2_exists: bool = target_conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE id='t-2')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!t2_exists);
    }

    #[test]
    fn test_merge_index_hash_verification_and_missing_rollout() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let target_db = temp.path().join("target/state.sqlite");
        let t_rollout_1 = temp.path().join("target/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target/rollouts/t2.jsonl");

        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1.clone()),
            (s_rollout_2.display().to_string(), t_rollout_2.clone()),
        ]);

        // 1. Missing target rollout file -> missing dependency error
        let err = merge_index(&source_db, &target_db, &path_map).unwrap_err();
        assert!(
            err.to_string().contains("missing dependency")
                || err
                    .to_string()
                    .contains("missing before index registration"),
            "expected missing rollout error, got: {err}"
        );

        // 2. Hash mismatch between source rollout and target rollout
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::write(&t_rollout_1, b"corrupted bytes different from source").unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let err = merge_index(&source_db, &target_db, &path_map).unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected hash mismatch error, got: {err}"
        );

        // 3. Empty target rollout file
        fs::write(&t_rollout_1, b"").unwrap();
        let err = merge_index(&source_db, &target_db, &path_map).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected empty rollout error, got: {err}"
        );
    }

    #[test]
    fn test_remap_path_boundary_rules() {
        let path_map = BTreeMap::from([
            ("C:/work".to_string(), PathBuf::from("D:/new_work")),
            ("C:/old/sessions".to_string(), PathBuf::from("D:/sessions")),
        ]);

        // Exact match
        assert_eq!(
            remap_path_on_boundary("C:/work", &path_map),
            Some(PathBuf::from("D:/new_work"))
        );
        assert_eq!(
            remap_path_on_boundary("C:/work/", &path_map),
            Some(PathBuf::from("D:/new_work"))
        );

        // Component boundary prefix match
        assert_eq!(
            remap_path_on_boundary("C:/work/app/sub", &path_map),
            Some(PathBuf::from("D:/new_work/app/sub"))
        );
        assert_eq!(
            remap_path_on_boundary("C:\\work\\app\\sub", &path_map),
            Some(PathBuf::from("D:/new_work/app/sub"))
        );

        // Non-boundary string match must NOT match
        assert_eq!(remap_path_on_boundary("C:/workspace", &path_map), None);
        assert_eq!(
            remap_path_on_boundary("C:/work_extra/project", &path_map),
            None
        );

        // Substring in middle must NOT match
        assert_eq!(
            remap_path_on_boundary("C:/other/C:/work/app", &path_map),
            None
        );
    }

    #[test]
    fn test_missing_child_dependency_reported_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let t_rollout_1 = temp.path().join("target/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target/rollouts/t2.jsonl");
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::copy(&s_rollout_1, &t_rollout_1).unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let conn = Connection::open(&source_db).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Insert a dynamic tool for a non-existent thread 'non-existent-thread'
        conn.execute(
            "INSERT INTO thread_dynamic_tools VALUES ('non-existent-thread', 0, 'python')",
            [],
        )
        .unwrap();
        drop(conn);

        let target_db = temp.path().join("target/state.sqlite");
        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1),
            (s_rollout_2.display().to_string(), t_rollout_2),
        ]);

        let err = merge_index(&source_db, &target_db, &path_map).unwrap_err();
        assert!(
            err.to_string().contains("missing dependency")
                && err.to_string().contains("non-existent-thread"),
            "expected explicit missing dependency reporting the thread name, got: {err}"
        );
    }

    #[test]
    fn test_foreign_key_violation_reported_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, s_rollout_2) = setup_source_db(&source_db);

        let t_rollout_1 = temp.path().join("target/rollouts/t1.jsonl");
        let t_rollout_2 = temp.path().join("target/rollouts/t2.jsonl");
        fs::create_dir_all(t_rollout_1.parent().unwrap()).unwrap();
        fs::copy(&s_rollout_1, &t_rollout_1).unwrap();
        fs::copy(&s_rollout_2, &t_rollout_2).unwrap();

        let conn = Connection::open(&source_db).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO thread_spawn_edges VALUES ('child-without-thread', 'parent-without-thread', 'active')",
            [],
        )
        .unwrap();
        drop(conn);

        let target_db = temp.path().join("target/state.sqlite");
        let path_map = BTreeMap::from([
            (s_rollout_1.display().to_string(), t_rollout_1),
            (s_rollout_2.display().to_string(), t_rollout_2),
        ]);

        let err = merge_index(&source_db, &target_db, &path_map).unwrap_err();
        assert!(
            err.to_string().contains("missing dependency")
                && (err.to_string().contains("child-without-thread")
                    || err.to_string().contains("foreign key check failed")),
            "expected explicit FK or missing dependency reporting child, got: {err}"
        );
    }

    #[test]
    fn test_absent_target_cleaned_up_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let source_db = temp.path().join("source/state.sqlite");
        fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        let (s_rollout_1, _) = setup_source_db(&source_db);

        // Target does not exist initially
        let target_db = temp.path().join("target/absent_state.sqlite");
        assert!(!target_db.exists());

        // Call merge_index with missing rollout mapping for t-2 -> will fail
        let path_map = BTreeMap::from([(
            s_rollout_1.display().to_string(),
            temp.path().join("missing.jsonl"),
        )]);

        let err = merge_index(&source_db, &target_db, &path_map);
        assert!(err.is_err());

        // Target must NOT be left on disk
        assert!(
            !target_db.exists(),
            "target file should have been cleaned up on failure when it was originally absent"
        );
    }
}
