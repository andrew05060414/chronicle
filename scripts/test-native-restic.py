"""Isolated real-Restic acceptance gate. Never reads an installed agent profile."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import sqlite3
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--restic', required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix='chronicle-restic-gate-'))
    password = root / 'password'
    password.write_text(secrets.token_urlsafe(32), encoding='utf-8')
    env = dict(os.environ, RESTIC_PASSWORD_FILE=str(password), RESTIC_CACHE_DIR=str(root / 'cache'))
    checks = []

    def run(repo, *argv, expected=0):
        start = time.monotonic()
        proc = subprocess.run([args.restic, '-r', str(repo), *argv], env=env,
                              capture_output=True, text=True, timeout=120)
        if proc.returncode != expected:
            raise RuntimeError(f'{argv}: exit {proc.returncode}: {proc.stderr[-2000:]}')
        checks.append({'command': argv[0], 'exit': proc.returncode,
                       'seconds': round(time.monotonic() - start, 3)})
        return proc.stdout

    source = root / 'fixture'
    source.mkdir()
    (source / 'rollout.jsonl').write_text('{"unknown_payload":{"v":1}}\n', encoding='utf-8')
    (source / 'attachment.bin').write_bytes(secrets.token_bytes(1024 * 1024))
    live = sqlite3.connect(root / 'live.db')
    live.execute('PRAGMA journal_mode=WAL')
    live.execute('CREATE TABLE steps(id INTEGER PRIMARY KEY, payload BLOB)')
    live.execute('INSERT INTO steps(payload) VALUES (?)', (b'unknown-protobuf\x00\xff',))
    live.commit()
    snapshot = sqlite3.connect(source / 'conversation.db')
    live.backup(snapshot)
    snapshot.close()
    repo, replica = root / 'repo', root / 'replica'
    run(repo, 'init')
    run(replica, 'init')
    run(repo, 'backup', str(source), '--json')
    first = json.loads(run(repo, 'snapshots', '--json'))[-1]['id']
    with (source / 'rollout.jsonl').open('a', encoding='utf-8') as handle:
        handle.write('{"tool_output":"new message"}\n')
    live.execute('INSERT INTO steps(payload) VALUES (?)', (b'next-step',))
    live.commit()
    snapshot = sqlite3.connect(source / 'conversation.db')
    live.backup(snapshot)
    snapshot.close()
    live.close()
    run(repo, 'backup', str(source), '--json')
    latest = json.loads(run(repo, 'snapshots', '--json'))[-1]['id']
    assert latest != first
    repeated = run(repo, 'backup', str(source), '--json')
    summary = [json.loads(line) for line in repeated.splitlines() if line.strip()][-1]
    assert summary['data_blobs'] == 0 and summary['files_changed'] == 0, summary
    expected = {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in source.rglob('*') if p.is_file()}
    # Interrupt a real backup, then retry. No lock deletion until the process exits.
    (source / 'interrupt.bin').write_bytes(secrets.token_bytes(64 * 1024 * 1024))
    interrupted = subprocess.Popen([args.restic, '-r', str(repo), 'backup', str(source),
                                    '--limit-upload', '1024'], env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.1)
    assert interrupted.poll() is None, 'interruption fixture finished before interruption'
    interrupted.kill()
    interrupted.wait(timeout=10)
    run(repo, 'unlock')
    run(repo, 'backup', str(source), '--json')
    run(repo, 'check', '--read-data')
    run(replica, 'copy', '--from-repo', str(repo), '--from-password-file', str(password))
    run(replica, 'check', '--read-data')
    copied = json.loads(run(replica, 'snapshots', '--json'))
    restored_id = next(s['id'] for s in copied if s.get('original', s['id']) == latest)
    output = root / 'restored'
    run(replica, 'restore', restored_id, '--target', str(output))
    found = list(output.rglob('rollout.jsonl'))
    assert len(found) == 1
    restored = found[0].parent
    actual = {str(p.relative_to(restored)): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in restored.rglob('*') if p.is_file()}
    assert actual == expected, (actual, expected)
    with sqlite3.connect(restored / 'conversation.db') as db:
        assert db.execute('PRAGMA integrity_check').fetchone() == ('ok',)
        assert db.execute('SELECT count(*) FROM steps').fetchone() == (2,)
    # A corrupt repository must never pass verification.
    packs = [p for p in (replica / 'data').rglob('*') if p.is_file()]
    assert packs
    with packs[0].open('r+b') as handle:
        value = handle.read(1)
        handle.seek(0)
        handle.write(bytes([value[0] ^ 255]))
    corruption = subprocess.run([args.restic, '-r', str(replica), 'check', '--read-data'],
                                 env=env, capture_output=True, timeout=120)
    assert corruption.returncode != 0
    checks.append({'command': 'corruption-detection', 'exit': corruption.returncode})
    report = {'passed': True, 'fixture_root': str(root), 'restic': args.restic,
              'checks': checks, 'restored_files': len(actual),
              'scope': 'synthetic local repositories; no real NAS or client UAT'}
    Path(args.output).write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
