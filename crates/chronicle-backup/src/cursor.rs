//! A deliberately limited session projection, never a full Cursor state backup.
use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
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

fn extract_bubbles_from_composer_data(
    composer_id: &str,
    value: &rusqlite::types::Value,
    referenced: &mut HashMap<String, Vec<String>>,
    inlined: &mut HashSet<(String, String)>,
) {
    let raw_bytes = match value {
        rusqlite::types::Value::Text(s) => s.as_bytes(),
        rusqlite::types::Value::Blob(b) => b.as_slice(),
        _ => return,
    };
    let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(raw_bytes) else {
        return;
    };
    let obj = match json_val {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
            .ok()
            .and_then(|v| match v {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            }),
        _ => None,
    };
    let Some(map) = obj else {
        return;
    };
    if let Some(headers) = map
        .get("fullConversationHeadersOnly")
        .and_then(|h| h.as_array())
    {
        for item in headers {
            let bubble_id = match item {
                serde_json::Value::Object(o) => o.get("bubbleId").and_then(|b| b.as_str()),
                serde_json::Value::String(s) => Some(s.as_str()),
                _ => None,
            };
            if let Some(b_id) = bubble_id {
                referenced
                    .entry(composer_id.to_string())
                    .or_default()
                    .push(b_id.to_string());
            }
        }
    }
    if let Some(conv_map) = map.get("conversationMap").and_then(|c| c.as_object()) {
        for b_id in conv_map.keys() {
            inlined.insert((composer_id.to_string(), b_id.clone()));
        }
    }
}

fn extract_composers_from_headers_json(
    value: &rusqlite::types::Value,
    known: &mut HashSet<String>,
) {
    let raw_bytes = match value {
        rusqlite::types::Value::Text(s) => s.as_bytes(),
        rusqlite::types::Value::Blob(b) => b.as_slice(),
        _ => return,
    };
    let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(raw_bytes) else {
        return;
    };
    let obj = match json_val {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
            .ok()
            .and_then(|v| match v {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            }),
        _ => None,
    };
    let Some(map) = obj else {
        return;
    };
    if let Some(all) = map.get("allComposers").and_then(|a| a.as_array()) {
        for item in all {
            if let Some(c_id) = item.get("composerId").and_then(|id| id.as_str()) {
                known.insert(c_id.to_string());
            }
        }
    }
}

fn audit_source_integrity(source: &Connection) -> Result<()> {
    let mut stmt = source.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        !tables.is_empty(),
        "Cursor projection contains no session tables"
    );
    for table in &tables {
        ensure!(
            matches!(
                table.as_str(),
                "ItemTable" | "cursorDiskKV" | "composerHeaders"
            ),
            "unsupported Cursor table '{table}': unverified/unsupported table in session projection"
        );
    }
    for table in ["ItemTable", "cursorDiskKV"] {
        if tables.iter().any(|t| t == table) {
            let cols = table_columns(source, table)?;
            ensure!(
                cols == ["key", "value"],
                "unsupported Cursor source table '{table}' schema: unverified/unsupported columns"
            );
        }
    }
    if tables.iter().any(|t| t == "composerHeaders") {
        let cols = table_columns(source, "composerHeaders")?;
        ensure!(
            cols == COMPOSER_HEADERS_COLUMNS,
            "unsupported Cursor source composerHeaders schema: unverified/unsupported fields"
        );
    }

    let mut known_composers = HashSet::new();
    let mut referenced_bubbles: HashMap<String, Vec<String>> = HashMap::new();
    let mut existing_bubbles = HashSet::new();
    let mut inlined_bubbles = HashSet::new();
    let mut total_records = 0;

    for table in ["ItemTable", "cursorDiskKV"] {
        if !tables.iter().any(|t| t == table) {
            continue;
        }
        let mut stmt = source.prepare(&format!("SELECT key, value FROM {table}"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let Some(key) = row.get::<_, Option<String>>(0)? else {
                bail!(
                    "unsupported Cursor session record: null key in table '{table}' is unverified/unsupported"
                );
            };
            ensure!(
                session_key(&key),
                "unsupported Cursor key '{key}' in table '{table}': unverified/unsupported session key"
            );
            total_records += 1;
            let val: rusqlite::types::Value = row.get(1)?;
            if let Some(id) = key.strip_prefix("composerData:") {
                known_composers.insert(id.to_string());
                extract_bubbles_from_composer_data(
                    id,
                    &val,
                    &mut referenced_bubbles,
                    &mut inlined_bubbles,
                );
            } else if let Some(suffix) = key.strip_prefix("bubbleId:") {
                if let Some((comp_id, b_id)) = suffix.split_once(':') {
                    existing_bubbles.insert((comp_id.to_string(), b_id.to_string()));
                }
            } else if key == "composer.composerHeaders" {
                extract_composers_from_headers_json(&val, &mut known_composers);
            }
        }
    }

    if tables.iter().any(|t| t == "composerHeaders") {
        let mut stmt = source.prepare("SELECT composerId, value FROM composerHeaders")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let comp_id: String = row.get(0)?;
            known_composers.insert(comp_id.clone());
            total_records += 1;
            let val: Option<String> = row.get(1)?;
            if let Some(v_str) = val {
                extract_bubbles_from_composer_data(
                    &comp_id,
                    &rusqlite::types::Value::Text(v_str),
                    &mut referenced_bubbles,
                    &mut inlined_bubbles,
                );
            }
        }
    }

    ensure!(
        total_records > 0,
        "Cursor projection contains no session records"
    );

    for (composer_id, bubbles) in &referenced_bubbles {
        for bubble_id in bubbles {
            let key = (composer_id.clone(), bubble_id.clone());
            ensure!(
                existing_bubbles.contains(&key) || inlined_bubbles.contains(&key),
                "Cursor session missing dependency: composer '{composer_id}' references bubble '{bubble_id}' which is missing from snapshot"
            );
        }
    }

    for (composer_id, bubble_id) in &existing_bubbles {
        ensure!(
            known_composers.contains(composer_id),
            "Cursor session missing dependency: orphan bubble '{bubble_id}' for missing composer '{composer_id}' in snapshot"
        );
    }

    Ok(())
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

pub(super) fn audit_projection(path: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    audit_source_integrity(&connection)
}

fn table_columns(connection: &Connection, name: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({name})"))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?)
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
    fn unknown_tables_keys_fields_produce_explicit_integrity_status() {
        let temp = tempfile::tempdir().unwrap();

        // 1. Unknown table in source
        let s_table = temp.path().join("s_table.db");
        let db = Connection::open(&s_table).unwrap();
        db.execute_batch(
            "CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value BLOB);
             CREATE TABLE credentials(secret TEXT);
             INSERT INTO ItemTable VALUES('composerData:c1', 'data');
             INSERT INTO credentials VALUES('secret');",
        )
        .unwrap();
        drop(db);
        let err = audit_projection(&s_table).unwrap_err();
        assert!(err.to_string().contains("unverified/unsupported table"));

        // 2. Unknown key in source ItemTable
        let s_key = temp.path().join("s_key.db");
        let db = Connection::open(&s_key).unwrap();
        db.execute_batch(
            "CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO ItemTable VALUES('cursor.accessToken', 'stolen');",
        )
        .unwrap();
        drop(db);
        let err = audit_projection(&s_key).unwrap_err();
        assert!(
            err.to_string()
                .contains("unverified/unsupported session key")
        );

        // 3. Unknown columns in composerHeaders in source
        let s_cols = temp.path().join("s_cols.db");
        let db = Connection::open(&s_cols).unwrap();
        db.execute_batch(
            "CREATE TABLE composerHeaders(composerId TEXT PRIMARY KEY, unknownCol TEXT);
             INSERT INTO composerHeaders VALUES('c1', 'val');",
        )
        .unwrap();
        drop(db);
        let err = audit_projection(&s_cols).unwrap_err();
        assert!(err.to_string().contains("unverified/unsupported fields"));

        // 4. Unknown columns in ItemTable in source
        let s_kv_cols = temp.path().join("s_kv_cols.db");
        let db = Connection::open(&s_kv_cols).unwrap();
        db.execute_batch(
            "CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value BLOB, extraCol TEXT);
             INSERT INTO ItemTable VALUES('composerData:c1', 'val', 'extra');",
        )
        .unwrap();
        drop(db);
        let err = audit_projection(&s_kv_cols).unwrap_err();
        assert!(err.to_string().contains("unverified/unsupported columns"));
    }

    #[test]
    fn backup_sessions_succeeds_for_orphan_bubble_and_zero_session_keys() {
        let temp = tempfile::tempdir().unwrap();

        // (a) Profile with an orphan bubble
        let source_a = temp.path().join("source_a.db");
        let dest_a = temp.path().join("dest_a.db");
        let db_a = Connection::open(&source_a).unwrap();
        db_a.execute_batch(
            "CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO ItemTable VALUES('bubbleId:orphan-comp:b1', '{\"text\":\"orphan\"}');",
        )
        .unwrap();
        drop(db_a);

        let omitted_a = backup_sessions(&source_a, &dest_a).unwrap();
        assert_eq!(omitted_a, 0);
        assert!(dest_a.exists());
        let audit_err_a = audit_projection(&dest_a).unwrap_err();
        assert!(audit_err_a.to_string().contains("missing dependency"));
        assert!(audit_err_a.to_string().contains("orphan bubble"));

        // (b) Profile with ItemTable/cursorDiskKV but zero session keys
        let source_b = temp.path().join("source_b.db");
        let dest_b = temp.path().join("dest_b.db");
        let db_b = Connection::open(&source_b).unwrap();
        db_b.execute_batch(
            "CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value BLOB);
             CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO ItemTable VALUES('cursor.accessToken', 'FAKE_ACCESS');
             INSERT INTO cursorDiskKV VALUES('unrelated.key', 'FAKE_VALUE');",
        )
        .unwrap();
        drop(db_b);

        let omitted_b = backup_sessions(&source_b, &dest_b).unwrap();
        assert_eq!(omitted_b, 2);
        assert!(dest_b.exists());
        let audit_err_b = audit_projection(&dest_b).unwrap_err();
        assert!(
            audit_err_b
                .to_string()
                .contains("contains no session records")
        );
    }
}
