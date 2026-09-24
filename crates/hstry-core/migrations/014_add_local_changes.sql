-- Monotonic local change tracking for incremental sync and recovery.
-- Logs local mutations so exports never miss historical imports or concurrent writes.

CREATE TABLE IF NOT EXISTS local_changes (
    change_id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    changed_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_local_changes_source ON local_changes(source_id, change_id);
CREATE INDEX IF NOT EXISTS idx_local_changes_conv ON local_changes(conversation_id);
CREATE INDEX IF NOT EXISTS idx_local_changes_change_id ON local_changes(change_id);

CREATE TRIGGER IF NOT EXISTS trg_conversations_local_changes_insert
AFTER INSERT ON conversations
BEGIN
    INSERT INTO local_changes(conversation_id, source_id, changed_at)
    VALUES (NEW.id, NEW.source_id, CAST((julianday('now') - 2440587.5)*86400000 AS INTEGER));
END;

CREATE TRIGGER IF NOT EXISTS trg_conversations_local_changes_update
AFTER UPDATE ON conversations
BEGIN
    INSERT INTO local_changes(conversation_id, source_id, changed_at)
    VALUES (NEW.id, NEW.source_id, CAST((julianday('now') - 2440587.5)*86400000 AS INTEGER));
END;

-- Backfill initial local_changes for existing conversations
INSERT INTO local_changes(conversation_id, source_id, changed_at)
SELECT id, source_id, COALESCE(updated_at, created_at) * 1000 FROM conversations;
