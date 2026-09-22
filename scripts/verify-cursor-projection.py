"""Verify a filtered Cursor snapshot without printing keys or message values."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sqlite3


def verify(snapshot: Path):
    manifest = json.loads((snapshot / "manifest.json").read_text(encoding="utf-8"))
    results = []
    for entry in manifest["files"]:
        if not entry["relative"].startswith("cursor/"):
            continue
        assert entry["consistency"] == "sqlite-session-projection"
        path = (snapshot / entry["relative"]).resolve()
        assert path.is_relative_to(snapshot.resolve())
        with path.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        assert digest == entry["sha256"]
        assert path.stat().st_size == entry["bytes"]
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
            assert db.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
            tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            assert tables and tables <= {"ItemTable", "cursorDiskKV", "composerHeaders"}
            counts = {}
            for table in sorted(tables):
                if table == "composerHeaders":
                    counts[table] = db.execute("SELECT COUNT(*) FROM composerHeaders").fetchone()[0]
                    continue
                count = 0
                for (key,) in db.execute(f"SELECT key FROM {table}"):
                    valid = key in {"composer.composerHeaders", "workbench.panel.aichat.view.aichat.chatdata"}
                    valid = valid or isinstance(key, str) and re.fullmatch(r"(?:composerData:[a-zA-Z0-9_-]+|bubbleId:[a-zA-Z0-9_-]+:[a-zA-Z0-9_-]+)", key)
                    assert valid, "Non-session key found; no key or value printed"
                    count += 1
                counts[table] = count
            assert sum(counts.values()) > 0, "Projection contains no session records"
        results.append({"bytes": entry["bytes"], "sha256": digest, "tables": counts})
    assert results, "No Cursor projection found"
    return {"snapshot_id": manifest["snapshot_id"], "verified": True, "files": results}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.snapshot), indent=2))
