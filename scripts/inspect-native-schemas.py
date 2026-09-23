"""Read-only schema/credential-key audit; never prints stored values or transcripts."""
import json
from pathlib import Path
import re
import sqlite3
import argparse


def inspect(path):
    if not path.exists():
        return {'path': str(path), 'exists': False}
    with sqlite3.connect(f"file:{path.as_posix()}?mode=ro", uri=True) as db:
        tables = db.execute("SELECT name, sql FROM sqlite_master WHERE type='table'").fetchall()
        output = {'path': str(path), 'tables': [{'name': n, 'schema': s} for n, s in tables]}
        for table in ('ItemTable', 'cursorDiskKV'):
            if table not in [n for n, _ in tables]:
                continue
            keys = [r[0] for r in db.execute(f'SELECT key FROM "{table}"') if r[0] is not None]
            risky = [k for k in keys if re.search(r'auth|token|secret|password|cookie|credential', k, re.I)]
            output[table] = {'key_count': len(keys), 'credential_named_key_count': len(risky),
                             'credential_values_read': False}
        return output


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('paths', type=Path, nargs='+', help='Explicit databases to inspect read-only')
    args = parser.parse_args()
    print(json.dumps([inspect(p) for p in args.paths], ensure_ascii=True, indent=2))
