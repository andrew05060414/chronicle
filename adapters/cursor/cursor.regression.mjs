// Regression test for GitHub issue #15 (cursor adapter).
//
// `loadAllCursorSessions` short-circuited collection once `limit` sessions
// were found (`full()`), and only sorted that subset — returning whichever
// sessions the filesystem walk happened to find first instead of the newest.
//
// Uses offline snapshot fixtures so no SQLite database is needed.
// Requires better-sqlite3 to be importable (the adapter returns [] without
// it): npm install --no-save better-sqlite3
//
// Run with: node adapters/cursor/cursor.regression.mjs
// (runs from the repo root)
import { strict as assert } from 'node:assert';
import { spawnSync } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm, utimes } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const ADAPTER = join(REPO_ROOT, 'adapters', 'cursor', 'adapter.ts');

function request(home, method, params = {}) {
  const r = spawnSync(
    'node',
    ['--experimental-strip-types', '--experimental-transform-types', ADAPTER],
    {
      env: {
        ...process.env,
        HOME: home,
        HSTRY_REQUEST: JSON.stringify({ method, params }),
      },
      encoding: 'utf-8',
    },
  );
  assert.equal(r.status, 0, `adapter exited ${r.status}: ${r.stderr.slice(0, 500)}`);
  return JSON.parse(r.stdout);
}

const home = await mkdtemp(join(tmpdir(), 'hstry-cursor15-'));
try {
  const root = join(home, 'cursor-data');
  const snapDir = join(root, 'snapshots');
  await mkdir(snapDir, { recursive: true });

  // createdAt recency: aaa oldest, zzz newest. File mtimes are set in the
  // OPPOSITE order so walk order != recency order (the old code collected in
  // walk order and stopped at `limit`).
  const sessions = [
    { id: 'aaa', createdAt: Date.parse('2026-01-01T10:00:00Z'), mtime: new Date('2026-03-10T10:00:00Z') },
    { id: 'mmm', createdAt: Date.parse('2026-02-01T10:00:00Z'), mtime: new Date('2026-03-09T10:00:00Z') },
    { id: 'zzz', createdAt: Date.parse('2026-03-01T10:00:00Z'), mtime: new Date('2026-03-08T10:00:00Z') },
  ];
  for (const s of sessions) {
    const p = join(snapDir, `${s.id}.json`);
    await writeFile(
      p,
      JSON.stringify({
        composerId: s.id,
        composerData: {
          name: `${s.id} session`,
          createdAt: s.createdAt,
          conversationMap: {
            b1: { type: 1, text: `hello ${s.id}`, createdAt: s.createdAt },
          },
        },
        bubbleEntries: {},
      }),
    );
    await utimes(p, s.mtime, s.mtime);
  }

  const limited = request(home, 'parse', { path: root, opts: { limit: 1 } });
  assert.equal(limited.length, 1);
  assert.equal(
    limited[0].externalId,
    'zzz',
    `limit=1 must return the newest session, got ${limited[0].externalId}`,
  );

  const two = request(home, 'parse', { path: root, opts: { limit: 2 } });
  assert.deepEqual(
    two.map(c => c.externalId),
    ['zzz', 'mmm'],
    'limit=2 must return the two newest, newest-first',
  );

  const all = request(home, 'parse', { path: root, opts: {} });
  assert.equal(all.length, 3, 'no limit must return everything');
} finally {
  await rm(home, { recursive: true, force: true });
}

console.log('cursor regression test passed (#15)');
