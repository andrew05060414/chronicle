//! Version-checked Codex index merge. Raw rollout files are authoritative.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, types::Value};
use std::collections::BTreeMap;
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
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut target = Connection::open(target)?;
    let transaction = target.transaction()?;
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
        None,
        &normalized,
        None,
    )?;
    merge_table(
        &source,
        &transaction,
        "thread_sections",
        &["id"],
        Some("updated_at"),
        &normalized,
        None,
    )?;
    merge_table(
        &source,
        &transaction,
        "threads",
        &["id"],
        Some("updated_at"),
        &normalized,
        Some("rollout_path"),
    )?;
    merge_table(
        &source,
        &transaction,
        "thread_dynamic_tools",
        &["thread_id", "position"],
        None,
        &normalized,
        None,
    )?;
    merge_table(
        &source,
        &transaction,
        "thread_spawn_edges",
        &["child_thread_id"],
        None,
        &normalized,
        None,
    )?;
    transaction.commit()?;
    Ok(())
}

fn merge_table(
    source: &Connection,
    target: &Connection,
    table: &str,
    keys: &[&str],
    updated_column: Option<&str>,
    path_map: &BTreeMap<String, PathBuf>,
    mapped_path_column: Option<&str>,
) -> Result<()> {
    if !table_exists(source, table)? {
        return Ok(());
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
    if let Some(column) = updated_column {
        ensure!(
            columns.iter().any(|c| c == column),
            "Codex target schema missing {table}.{column}"
        );
    }
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
    let update_columns = columns
        .iter()
        .filter(|c| !keys.contains(&c.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let update_sql = update_columns
        .iter()
        .map(|c| format!("{}=excluded.{}", quote(c), quote(c)))
        .collect::<Vec<_>>()
        .join(",");
    let insert_sql = if update_columns.is_empty() {
        format!(
            "INSERT INTO {}({column_sql}) VALUES ({}) ON CONFLICT DO NOTHING",
            quote(table),
            (1..=columns.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    } else {
        format!(
            "INSERT INTO {}({column_sql}) VALUES ({}) ON CONFLICT({}) DO UPDATE SET {update_sql}",
            quote(table),
            (1..=columns.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(","),
            keys.iter().map(|k| quote(k)).collect::<Vec<_>>().join(",")
        )
    };
    let mut select = source.prepare(&format!("SELECT {column_sql} FROM {}", quote(table)))?;
    let mut rows = select.query([])?;
    let mut registered = 0;
    let mut verified = 0;
    let mut skipped = 0;
    while let Some(row) = rows.next()? {
        let mut values = (0..columns.len())
            .map(|index| row.get::<_, Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if table == "threads" {
            let path_index = columns
                .iter()
                .position(|c| Some(c.as_str()) == mapped_path_column)
                .unwrap();
            let Value::Text(path) = &values[path_index] else {
                skipped += 1;
                continue;
            };
            let Some(mapped) = path_map.get(&normalize(path)) else {
                skipped += 1;
                continue;
            };
            ensure!(
                mapped.is_file(),
                "Codex rollout target missing before index registration: {}",
                mapped.display()
            );
            verified += 1;
            values[path_index] = Value::Text(mapped.display().to_string());
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
            if let Some(column) = updated_column {
                let index = columns.iter().position(|c| c == column).unwrap();
                if value_i64(&target_values[index]) > value_i64(&values[index]) {
                    skipped += 1;
                    continue;
                }
            }
            ensure!(
                target_values == values || updated_column.is_some(),
                "same Codex {table} identity has different content"
            );
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
fn value_i64(value: &Value) -> i64 {
    match value {
        Value::Integer(v) => *v,
        _ => i64::MIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn merge_registers_only_mapped_rollouts_and_preserves_newer_target() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("state.sqlite");
        let target_path = temp.path().join("target.sqlite");
        let schema = "CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT NOT NULL, metadata TEXT NOT NULL, position INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL); CREATE TABLE thread_sections(id TEXT PRIMARY KEY,name TEXT,updated_at INTEGER); CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,source TEXT NOT NULL,model_provider TEXT NOT NULL,cwd TEXT NOT NULL,title TEXT NOT NULL); CREATE TABLE thread_dynamic_tools(thread_id TEXT,position INTEGER,name TEXT,PRIMARY KEY(thread_id,position)); CREATE TABLE thread_spawn_edges(child_thread_id TEXT PRIMARY KEY,parent_thread_id TEXT,status TEXT);";
        let source = Connection::open(&source_path).unwrap();
        source.execute_batch(schema).unwrap();
        source.execute_batch("INSERT INTO threads VALUES('mapped','C:/old/session.jsonl',1,2,'user','openai','C:/work','mapped'); INSERT INTO threads VALUES('ghost','C:/old/missing.jsonl',1,2,'user','openai','C:/work','ghost');").unwrap();
        let target = Connection::open(&target_path).unwrap();
        target.execute_batch(schema).unwrap();
        target.execute_batch("INSERT INTO threads VALUES('mapped','D:/new/session.jsonl',1,3,'user','openai','D:/work','newer target');").unwrap();
        let mapped = temp.path().join("new/session.jsonl");
        fs::create_dir_all(mapped.parent().unwrap()).unwrap();
        fs::write(&mapped, b"verified rollout").unwrap();
        let map = BTreeMap::from([("C:/old/session.jsonl".into(), mapped)]);
        merge_index(&source_path, &target_path, &map).unwrap();
        let target = Connection::open(&target_path).unwrap();
        let rows: (i64, i64) = target
            .query_row(
                "SELECT COUNT(*),SUM(title='newer target') FROM threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, (1, 1));
    }
}
