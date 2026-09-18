// Regression tests for GitHub issues #10 and #15 (grok adapter).
//
// #10: legacy transcripts (chat_format_version 0, role-keyed records) parsed
//      to zero messages and vanished from the archive. Also covers the fields
//      dropped in the same rewrite: parentExternalId/forkType,
//      imageAttachments, backend_tool_call.
// #15: `--limit` broke out of the session loop before sorting, returning the
//      alphabetically-first sessions instead of the newest.
//
// Run with: node adapters/grok/grok.regression.mjs
// (runs from the repo root; spawns the adapter under node since bun is not
// installed in this environment)
import { strict as assert } from 'node:assert';
import { spawnSync } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const ADAPTER = join(REPO_ROOT, 'adapters', 'grok', 'adapter.ts');

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

async function makeSession(root, id, summary, lines) {
  const dir = join(root, 'ws', id);
  await mkdir(dir, { recursive: true });
  await writeFile(join(dir, 'summary.json'), JSON.stringify(summary));
  await writeFile(join(dir, 'chat_history.jsonl'), lines.map(l => JSON.stringify(l)).join('\n') + '\n');
  return dir;
}

// ---- #10: legacy format must parse, with fork lineage ----
{
  const home = await mkdtemp(join(tmpdir(), 'hstry-grok10-'));
  try {
    const root = join(home, '.grok', 'sessions');
    await makeSession(
      root,
      'legacy-1',
      {
        info: { id: 'legacy-1', cwd: '/work/repo' },
        created_at: '2025-06-01T10:00:00Z',
        updated_at: '2025-06-01T10:02:00Z',
        current_model_id: 'grok-old',
        chat_format_version: 0,
        parent_session_id: 'parent-9',
      },
      [
        { role: 'user', content: 'Legacy hello world' },
        {
          role: 'assistant',
          content: 'Legacy reply',
          tool_calls: [{ id: 'c1', function: { name: 'read', arguments: '{"f":"a"}' } }],
        },
        { role: 'tool', content: 'file data', tool_call_id: 'c1', name: 'read' },
      ],
    );
    const [c] = request(home, 'parse', { path: root, opts: {} });
    assert.ok(c, 'legacy session must not vanish');
    assert.equal(c.externalId, 'legacy-1');
    assert.equal(c.parentExternalId, 'parent-9');
    assert.equal(c.forkType, 'fork');
    assert.equal(c.metadata.chatFormatVersion, 0);
    assert.ok(
      c.messages.some(m => m.role === 'user' && m.content === 'Legacy hello world'),
      'legacy user message kept',
    );
    const assistant = c.messages.find(m => m.role === 'assistant');
    assert.ok(assistant?.toolCalls?.some(t => t.toolName === 'read'), 'legacy tool call kept');
    assert.ok(c.messages.some(m => m.role === 'tool'), 'legacy tool message kept');
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

// ---- #10 (v1): restored fields ----
{
  const home = await mkdtemp(join(tmpdir(), 'hstry-grok10b-'));
  try {
    const root = join(home, '.grok', 'sessions');
    await makeSession(
      root,
      'grok-1',
      {
        info: { id: 'grok-1', cwd: '/work/repo' },
        generated_title: 'File server',
        created_at: '2026-01-01T10:00:00Z',
        updated_at: '2026-01-01T10:02:00Z',
        current_model_id: 'grok-code',
        chat_format_version: 1,
        parent_session_id: 'parent-1',
      },
      [
        { type: 'user', content: [{ type: 'text', text: 'Build it' }, { type: 'image', url: 'file:///tmp/a.png' }] },
        { type: 'assistant', content: 'Reading.' },
        { type: 'backend_tool_call', kind: { tool_type: 'web_search', id: 'b1' } },
      ],
    );
    const [c] = request(home, 'parse', { path: root, opts: {} });
    assert.equal(c.parentExternalId, 'parent-1');
    assert.equal(c.forkType, 'fork');
    assert.ok(
      c.messages.find(m => m.role === 'user')?.attachments?.length,
      'v1 image attachments kept',
    );
    assert.ok(
      c.messages.some(m => m.toolCalls?.some(t => t.toolName === 'web_search')),
      'v1 backend_tool_call kept',
    );
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

// ---- #10: v1 records without a version marker must not vanish either ----
{
  const home = await mkdtemp(join(tmpdir(), 'hstry-grok10c-'));
  try {
    const root = join(home, '.grok', 'sessions');
    await makeSession(
      root,
      'noversion-1',
      {
        // NOTE: no chat_format_version — mirrors testdata/grok fixtures.
        info: { id: 'noversion-1', cwd: '/ws' },
        created_at: '2026-01-01T10:00:00Z',
        updated_at: '2026-01-01T10:02:00Z',
        current_model_id: 'grok-code',
      },
      [
        { type: 'user', content: 'Hello without version' },
        { type: 'assistant', content: 'Hi there' },
      ],
    );
    const [c] = request(home, 'parse', { path: root, opts: {} });
    assert.ok(c, 'v1 session without version marker must not vanish');
    assert.equal(c.externalId, 'noversion-1');
    assert.ok(c.messages.length >= 2, 'v1 messages kept without version marker');
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

// ---- #15: limit applies after sorting by recency ----
{
  const home = await mkdtemp(join(tmpdir(), 'hstry-grok15-'));
  try {
    const root = join(home, '.grok', 'sessions');
    // Alphabetical order (aaa < zzz) is the REVERSE of recency here.
    for (const [id, created] of [
      ['aaa', '2026-01-01T10:00:00Z'],
      ['mmm', '2026-02-01T10:00:00Z'],
      ['zzz', '2026-03-01T10:00:00Z'],
    ]) {
      await makeSession(
        root,
        id,
        {
          info: { id, cwd: '/ws' },
          created_at: created,
          updated_at: created,
          current_model_id: 'grok-code',
          chat_format_version: 1,
        },
        [{ type: 'user', content: `hi ${id}` }],
      );
    }
    const limited = request(home, 'parse', { path: root, opts: { limit: 1 } });
    assert.equal(limited.length, 1);
    assert.equal(limited[0].externalId, 'zzz', 'limit=1 must return the newest session');
    const two = request(home, 'parse', { path: root, opts: { limit: 2 } });
    assert.deepEqual(
      two.map(c => c.externalId),
      ['zzz', 'mmm'],
      'limit=2 must return the two newest, newest-first',
    );
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

console.log('grok regression tests passed (#10, #15)');
