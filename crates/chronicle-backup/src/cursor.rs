//! A deliberately limited session projection, never a full Cursor state backup.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

const COMPOSER_HEADERS_COLUMNS: &[&str] = &[
    "composerId",
    "workspaceId",
    "createdAt",
    "lastUpdatedAt",
    "isArchived",
    "isSubagent",
    "recency",
    "checkpointAt",
    "value",
    "subagentTypeName",
];

fn session_key(key: &str) -> bool {
    matches!(
        key,
        "composer.composerHeaders" | "workbench.panel.aichat.view.aichat.chatdata"
    ) || key.strip_prefix("composerData:").is_some_and(valid_id)
        || key.strip_prefix("bubbleId:").is_some_and(|suffix| {
            suffix
                .split_once(':')
                .is_some_and(|(composer, bubble)| valid_id(composer) && valid_id(bubble))
        })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

pub(super) fn backup_sessions(source: &Path, destination: &Path) -> Result<u64> {
    ensure!(
        !destination.exists(),
        "Cursor projection destination already exists"
    );
    let result = write_projection(source, destination);
    if result.is_err() {
        // Only the newly created projection is removed; never touch the source.
        let _ = std::fs::remove_file(destination);
    }
    result
}

pub(super) fn merge_sessions(source: &Path, target: &Path) -> Result<u64> {
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut target = Connection::open(target)?;
    let transaction = target.transaction()?;
    let mut copied = 0;
    for table in ["ItemTable", "cursorDiskKV"] {
        if !table_exists(&source, table)? {
            continue;
        }
        ensure!(
            table_exists(&transaction, table)? || create_kv_table(&transaction, table)?,
            "table creation failed"
        );
        let columns = table_columns(&transaction, table)?;
        ensure!(
            columns == ["key", "value"],
            "unsupported Cursor target {table} schema"
        );
        let mut statement = source.prepare(&format!("SELECT key,value FROM {table}"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let Some(key) = row.get::<_, Option<String>>(0)? else {
                continue;
            };
            if !session_key(&key) {
                continue;
            }
            let value: rusqlite::types::Value = row.get(1)?;
            transaction.execute(&format!("INSERT INTO {table}(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value"),rusqlite::params![key,value])?;
            copied += 1;
        }
    }
    if table_exists(&source, "composerHeaders")? {
        if !table_exists(&transaction, "composerHeaders")? {
            transaction.execute_batch("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT)")?;
        }
        ensure!(
            table_columns(&transaction, "composerHeaders")? == COMPOSER_HEADERS_COLUMNS,
            "unsupported Cursor target composerHeaders schema"
        );
        let insert = format!(
            "INSERT INTO composerHeaders({}) VALUES ({}) ON CONFLICT(composerId) DO UPDATE SET {}",
            COMPOSER_HEADERS_COLUMNS.join(","),
            (1..=COMPOSER_HEADERS_COLUMNS.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(","),
            COMPOSER_HEADERS_COLUMNS
                .iter()
                .skip(1)
                .map(|c| format!("{c}=excluded.{c}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut statement = source.prepare(&format!(
            "SELECT {} FROM composerHeaders",
            COMPOSER_HEADERS_COLUMNS.join(",")
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let values = (0..COMPOSER_HEADERS_COLUMNS.len())
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            transaction.execute(&insert, rusqlite::params_from_iter(values.iter()))?;
            copied += 1;
        }
    }
    ensure!(copied > 0, "Cursor projection contains no session records");
    transaction.commit()?;
    Ok(copied)
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

fn table_columns(connection: &Connection, name: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({name})"))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?)
}

fn create_kv_table(connection: &Connection, name: &str) -> Result<bool> {
    connection.execute_batch(&format!(
        "CREATE TABLE {name} (key TEXT PRIMARY KEY, value BLOB)"
    ))?;
    Ok(true)
}

fn write_projection(source: &Path, destination: &Path) -> Result<u64> {
    let mut from = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let read = from.transaction()?;
    let mut to = Connection::open(destination)?;
    let write = to.transaction()?;
    let mut recognized = false;
    let mut omitted = 0;
    for table in ["ItemTable", "cursorDiskKV"] {
        let exists: bool = read.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            continue;
        }
        recognized = true;
        // Fixed table names only. Build fresh pages so deleted credentials and
        // unrelated tables cannot survive in free pages of the output database.
        write.execute_batch(&format!(
            "CREATE TABLE {table} (key TEXT PRIMARY KEY, value BLOB)"
        ))?;
        let total: u64 = read.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        let mut copied = 0;
        // Let the key index reject unrelated records before loading their
        // potentially large values. Validate suffixes again in Rust.
        let mut statement = read.prepare(&format!("SELECT key, value FROM {table} WHERE key IN ('composer.composerHeaders','workbench.panel.aichat.view.aichat.chatdata') OR key GLOB 'composerData:*' OR key GLOB 'bubbleId:*'"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let Some(key) = row.get::<_, Option<String>>(0)? else {
                continue;
            };
            if !session_key(&key) {
                continue;
            }
            let value: rusqlite::types::Value = row.get(1)?;
            write.execute(
                &format!("INSERT INTO {table}(key,value) VALUES (?1,?2)"),
                rusqlite::params![key, value],
            )?;
            copied += 1;
        }
        omitted += total - copied;
    }
    let composer_exists: bool = read.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='composerHeaders')",
        [],
        |row| row.get(0),
    )?;
    if composer_exists {
        recognized = true;
        let mut columns = Vec::new();
        let mut statement = read.prepare("PRAGMA table_info(composerHeaders)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in rows {
            columns.push(column?);
        }
        ensure!(
            columns == COMPOSER_HEADERS_COLUMNS,
            "unsupported Cursor composerHeaders schema"
        );
        write.execute_batch("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT)")?;
        let insert = format!(
            "INSERT INTO composerHeaders({}) VALUES ({})",
            COMPOSER_HEADERS_COLUMNS.join(","),
            (1..=COMPOSER_HEADERS_COLUMNS.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let select = format!(
            "SELECT {} FROM composerHeaders",
            COMPOSER_HEADERS_COLUMNS.join(",")
        );
        let mut statement = read.prepare(&select)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let mut values = Vec::new();
            for index in 0..COMPOSER_HEADERS_COLUMNS.len() {
                values.push(row.get::<_, rusqlite::types::Value>(index)?);
            }
            write.execute(&insert, rusqlite::params_from_iter(values.iter()))?;
        }
    }
    ensure!(
        recognized,
        "unsupported Cursor database schema: no known session table"
    );
    write.commit()?;
    read.commit()?;
    Ok(omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_preserves_session_values_and_excludes_auth_and_free_pages() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.vscdb");
        let destination = temp.path().join("sessions.vscdb");
        let db = Connection::open(&source).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT); CREATE TABLE auth(secret TEXT); INSERT INTO auth VALUES ('FAKE_UNRELATED_SECRET'); INSERT INTO composerHeaders VALUES ('composer-1','workspace-1',1,2,0,0,3,4,'FAKE_HEADER_SESSION_VALUE',NULL);").unwrap();
        for table in ["ItemTable", "cursorDiskKV"] {
            for (key, value) in [
                ("composerData:abc-123", "{\"text\":\"hello\"}"),
                ("bubbleId:abc-123:def_456", "{\"text\":\"world\"}"),
                ("cursor.accessToken", "FAKE_ACCESS_SECRET"),
                ("unknown", "FAKE_UNKNOWN_SECRET"),
            ] {
                db.execute(&format!("INSERT INTO {table} VALUES (?1,?2)"), [key, value])
                    .unwrap();
            }
        }
        db.execute(
            "INSERT INTO ItemTable VALUES (NULL,'FAKE_NULL_KEY_SECRET')",
            [],
        )
        .unwrap();
        assert_eq!(backup_sessions(&source, &destination).unwrap(), 5);
        let output = Connection::open(&destination).unwrap();
        for table in ["ItemTable", "cursorDiskKV"] {
            let values: Vec<(String, String)> = output
                .prepare(&format!("SELECT key,value FROM {table} ORDER BY key"))
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                values,
                vec![
                    (
                        "bubbleId:abc-123:def_456".into(),
                        "{\"text\":\"world\"}".into()
                    ),
                    ("composerData:abc-123".into(), "{\"text\":\"hello\"}".into())
                ]
            );
        }
        let headers: (String, String) = output
            .query_row("SELECT composerId,value FROM composerHeaders", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(
            headers,
            ("composer-1".into(), "FAKE_HEADER_SESSION_VALUE".into())
        );
        let bytes = std::fs::read(&destination).unwrap();
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(!rendered.contains("FAKE_ACCESS_SECRET"));
        assert!(!rendered.contains("FAKE_UNRELATED_SECRET"));
        assert!(backup_sessions(&source, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    }

    #[test]
    fn unknown_schema_fails_without_publishing_output() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("unknown.db");
        Connection::open(&source)
            .unwrap()
            .execute_batch("CREATE TABLE credentials(value TEXT)")
            .unwrap();
        let destination = temp.path().join("output.db");
        assert!(backup_sessions(&source, &destination).is_err());
        assert!(!destination.exists());
        for key in [
            "composerData:",
            "composerData:abc:token",
            "bubbleId:abc:",
            "bubbleId:abc:def:ghi",
            "composer.composerHeaders.accessToken",
        ] {
            assert!(!session_key(key));
        }
    }

    #[test]
    fn merge_preserves_target_authentication_and_only_replaces_session_keys() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let target = temp.path().join("target.db");
        let source_db = Connection::open(&source).unwrap();
        source_db.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT); INSERT INTO ItemTable VALUES('composerData:session-1','restored'); INSERT INTO composerHeaders VALUES('session-1','workspace-1',1,2,0,0,3,4,'header',NULL);").unwrap();
        let target_db = Connection::open(&target).unwrap();
        target_db.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT); INSERT INTO ItemTable VALUES('cursor.accessToken','FAKE_TARGET_AUTH'); INSERT INTO ItemTable VALUES('composerData:session-1','stale'); INSERT INTO composerHeaders VALUES('session-1','old-workspace',5,6,1,0,7,8,'old-header',NULL);").unwrap();
        drop(target_db);
        assert_eq!(merge_sessions(&source, &target).unwrap(), 2);
        let target_db = Connection::open(&target).unwrap();
        assert_eq!(
            target_db
                .query_row(
                    "SELECT value FROM ItemTable WHERE key='cursor.accessToken'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "FAKE_TARGET_AUTH"
        );
        assert_eq!(
            target_db
                .query_row(
                    "SELECT value FROM ItemTable WHERE key='composerData:session-1'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "restored"
        );
        assert_eq!(
            target_db
                .query_row("SELECT workspaceId,value FROM composerHeaders", [], |r| Ok(
                    (r.get::<_, String>(0)?, r.get::<_, String>(1)?)
                ))
                .unwrap(),
            ("workspace-1".into(), "header".into())
        );
    }
}
