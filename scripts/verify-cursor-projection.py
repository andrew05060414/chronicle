"""Verify a filtered Cursor snapshot without printing keys or message values."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys

COMPOSER_HEADERS_COLUMNS = [
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
]


def session_key_valid(key):
    if not isinstance(key, str):
        return False
    if key in ("composer.composerHeaders", "workbench.panel.aichat.view.aichat.chatdata"):
        return True
    return bool(re.fullmatch(r"^(?:composerData:[a-zA-Z0-9_-]+|bubbleId:[a-zA-Z0-9_-]+:[a-zA-Z0-9_-]+)$", key))


def extract_composer_data_bubbles(c_id, val, referenced, inlined):
    if val is None:
        return
    data = None
    try:
        raw = val.decode("utf-8") if isinstance(val, bytes) else str(val)
        data = json.loads(raw)
        if isinstance(data, str):
            data = json.loads(data)
    except Exception:
        return
    if not isinstance(data, dict):
        return
    headers = data.get("fullConversationHeadersOnly")
    if isinstance(headers, list):
        for item in headers:
            b_id = item.get("bubbleId") if isinstance(item, dict) else (item if isinstance(item, str) else None)
            if b_id:
                referenced.setdefault(c_id, []).append(str(b_id))
    conv_map = data.get("conversationMap")
    if isinstance(conv_map, dict):
        for b_id in conv_map.keys():
            inlined.add((c_id, str(b_id)))


def extract_all_composers(val, known):
    if val is None:
        return
    try:
        raw = val.decode("utf-8") if isinstance(val, bytes) else str(val)
        data = json.loads(raw)
        if isinstance(data, str):
            data = json.loads(data)
        if isinstance(data, dict) and isinstance(data.get("allComposers"), list):
            for item in data["allComposers"]:
                if isinstance(item, dict) and item.get("composerId"):
                    known.add(str(item["composerId"]))
    except Exception:
        pass


def verify(snapshot: Path):
    manifest_path = snapshot / "manifest.json"
    if not manifest_path.is_file():
        return {
            "snapshot_id": "unknown",
            "verified": False,
            "violations": [f"manifest.json missing at {snapshot}"],
            "files": [],
        }
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    results = []
    violations = []
    cursor_found = False

    for entry in manifest.get("files", []):
        if not entry.get("relative", "").startswith("cursor/"):
            continue
        cursor_found = True
        if entry.get("consistency") != "sqlite-session-projection":
            violations.append(f"invalid consistency for {entry.get('relative')}: expected sqlite-session-projection")
            continue
        path = (snapshot / entry["relative"]).resolve()
        if not path.is_relative_to(snapshot.resolve()):
            violations.append(f"path traversal detected in relative path: {entry.get('relative')}")
            continue
        if not path.is_file():
            violations.append(f"file missing: {path}")
            continue
        with path.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        if digest != entry.get("sha256"):
            violations.append(f"sha256 mismatch for {entry['relative']}")
        if path.stat().st_size != entry.get("bytes"):
            violations.append(f"size mismatch for {entry['relative']}")

        db_uri = path.as_uri() + "?mode=ro"
        with sqlite3.connect(db_uri, uri=True) as db:
            integrity = db.execute("PRAGMA integrity_check").fetchall()
            if integrity != [("ok",)]:
                violations.append(f"sqlite integrity_check failed on {entry['relative']}: {integrity}")

            tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")}
            if not tables:
                violations.append(f"Cursor projection contains no session tables in {entry['relative']}")
            unsupported_tables = tables - {"ItemTable", "cursorDiskKV", "composerHeaders"}
            if unsupported_tables:
                violations.append(f"unverified/unsupported tables in {entry['relative']}: {sorted(unsupported_tables)}")

            if "composerHeaders" in tables:
                cols = [r[1] for r in db.execute("PRAGMA table_info(composerHeaders)").fetchall()]
                if cols != COMPOSER_HEADERS_COLUMNS:
                    violations.append(f"unverified/unsupported fields in composerHeaders for {entry['relative']}: {cols}")

            for t in ("ItemTable", "cursorDiskKV"):
                if t in tables:
                    cols = [r[1] for r in db.execute(f"PRAGMA table_info({t})").fetchall()]
                    if cols != ["key", "value"]:
                        violations.append(f"unverified/unsupported columns in {t} for {entry['relative']}: {cols}")

            counts = {}
            known_composers = set()
            referenced_bubbles = {}
            existing_bubbles = set()
            inlined_bubbles = set()

            if "composerHeaders" in tables:
                counts["composerHeaders"] = db.execute("SELECT COUNT(*) FROM composerHeaders").fetchone()[0]
                for (c_id, val) in db.execute("SELECT composerId, value FROM composerHeaders"):
                    if c_id:
                        known_composers.add(str(c_id))
                    extract_composer_data_bubbles(str(c_id), val, referenced_bubbles, inlined_bubbles)

            for table in sorted(tables & {"ItemTable", "cursorDiskKV"}):
                count = 0
                for (key, val) in db.execute(f"SELECT key, value FROM {table}"):
                    if not session_key_valid(key):
                        violations.append(f"unverified/unsupported session key in {table}: non-session key found")
                        continue
                    count += 1
                    if key.startswith("composerData:"):
                        c_id = key.split(":", 1)[1]
                        known_composers.add(c_id)
                        extract_composer_data_bubbles(c_id, val, referenced_bubbles, inlined_bubbles)
                    elif key.startswith("bubbleId:"):
                        parts = key.split(":")
                        if len(parts) == 3:
                            existing_bubbles.add((parts[1], parts[2]))
                    elif key == "composer.composerHeaders":
                        extract_all_composers(val, known_composers)
                counts[table] = count

            if sum(counts.values()) == 0:
                violations.append(f"projection {entry['relative']} contains no session records")

            for c_id, bubbles in referenced_bubbles.items():
                for b_id in bubbles:
                    if (c_id, b_id) not in existing_bubbles and (c_id, b_id) not in inlined_bubbles:
                        violations.append(f"missing dependency: composer '{c_id}' references missing bubble '{b_id}'")

            for (c_id, b_id) in existing_bubbles:
                if c_id not in known_composers:
                    violations.append(f"missing dependency: orphan bubble '{b_id}' for missing composer '{c_id}'")

            results.append({"bytes": entry.get("bytes"), "sha256": digest, "tables": counts})

    if not cursor_found:
        violations.append("No Cursor projection found in manifest")

    verified = len(violations) == 0
    return {
        "snapshot_id": manifest.get("snapshot_id"),
        "verified": verified,
        "violations": violations,
        "files": results,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    args = parser.parse_args()
    summary = verify(args.snapshot)
    print(json.dumps(summary, indent=2))
    if not summary.get("verified", False):
        sys.exit(1)
    sys.exit(0)
