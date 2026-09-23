// Comprehensive offline unit & regression tests for Grok web provider (grok.com).
// Covers message/thinking/tool parsing, DAG ordering, pagination & limits,
// fail-closed malformed responses, invalid ID detection, in-flight optimistic handling,
// load-responses error propagation, missing-ID detection, and watermark preservation.
// Usage: bun extension/test/grok.js

import assert from 'node:assert/strict';
import { syncGrok, grokInternals } from '../providers/grok.js';
import { NotLoggedInError } from '../lib/common.js';

let failures = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`PASS ${name}`);
  } catch (err) {
    console.log(`FAIL ${name}`);
    console.error(err);
    failures++;
  }
}
async function testAsync(name, fn) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (err) {
    console.log(`FAIL ${name}`);
    console.error(err);
    failures++;
  }
}

// ── 1. hasExtractableContent Strict Alignment with Canonical Extractor ───

test('hasExtractableContent strictly matches extractGrokMessage behavior without manual drift', () => {
  // Nested message.text and message.content are extractable
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: { text: 'nested text' } }), true);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: { content: 'nested content' } }), true);

  // query only counts when sender/role is user
  assert.equal(grokInternals.hasExtractableContent({ sender: 'human', query: 'user prompt' }), true);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', query: 'assistant prompt without message' }), false);

  // arbitrary steps[].text must NOT count unless extractor recognizes it as thinking or tool
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', steps: [{ type: 'custom', text: 'arbitrary' }] }), false);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', steps: [{ type: 'thinking', text: 'think' }] }), true);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', steps: [{ type: 'tool_call', toolName: 'calc', input: {} }] }), true);

  // Unknown card types and malformed card strings must NOT count as extractable
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', cardAttachmentsJson: ['{"unknownCard":123}'] }), false);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', cardAttachmentsJson: ['{bad json'] }), false);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', cardAttachmentsJson: ['{"webSearch":{"args":{"query":"hi"}}}'] }), true);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', cardAttachmentsJson: ['{"mcp":{"toolName":"t1","toolArgsJson":"{}"}}'] }), true);

  // Unmapped attachment metadata without text or recognized parts must NOT count as extractable
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', fileAttachmentsMetadata: [{ name: 'f.pdf' }] }), false);
  assert.equal(grokInternals.hasExtractableContent({ sender: 'assistant', message: '', imageAttachments: [{ url: 'http://img' }] }), false);
});

// ── 2. Message & Parts Extraction (including empty text tool/thinking nodes) ──

test('extractGrokMessage handles user, assistant and uppercase senders', () => {
  const userMsg = grokInternals.extractGrokMessage({
    sender: 'human',
    message: 'Hello from user',
    createTime: '2026-09-20T10:00:00Z',
  });
  assert.equal(userMsg.role, 'user');
  assert.equal(userMsg.content, 'Hello from user');
  assert.equal(userMsg.parts[0]?.type, 'text');

  const asstUpper = grokInternals.extractGrokMessage({
    sender: 'ASSISTANT',
    message: 'Answer text',
    createTime: '2026-09-20T10:01:00Z',
    model: 'grok-4',
  });
  assert.equal(asstUpper.role, 'assistant');
  assert.equal(asstUpper.content, 'Answer text');
  assert.equal(asstUpper.model, 'grok-4');
});

test('extractGrokMessage preserves thinking-only nodes without dropping them', () => {
  const thinkingOnly = grokInternals.extractGrokMessage({
    sender: 'assistant',
    message: '',
    thinkingTrace: 'Deliberating options...',
    createTime: '2026-09-20T10:02:00Z',
  });
  assert.ok(thinkingOnly, 'thinking-only node must not be dropped');
  assert.equal(thinkingOnly.role, 'assistant');
  assert.equal(thinkingOnly.parts.length, 1);
  assert.equal(thinkingOnly.parts[0].type, 'thinking');
  assert.equal(thinkingOnly.parts[0].text, 'Deliberating options...');
});

test('extractGrokMessage preserves tool-only nodes with empty message text', () => {
  const mcpCard = JSON.stringify({
    toolUsageCardId: 'card-1',
    mcp: {
      toolName: 'myConnector___query',
      toolArgsJson: '{"limit": 5}',
    },
  });
  const toolNode = grokInternals.extractGrokMessage({
    sender: 'assistant',
    message: '',
    cardAttachmentsJson: [mcpCard],
  });
  assert.ok(toolNode, 'tool node with empty message must not be dropped');
  assert.equal(toolNode.role, 'assistant');
  const toolPart = toolNode.parts.find(p => p.type === 'tool_call');
  assert.ok(toolPart);
  assert.equal(toolPart.name, 'myConnector___query');
});

test('extractGrokMessage fallback for empty message with webSearchResults', () => {
  const searchOnly = grokInternals.extractGrokMessage({
    sender: 'assistant',
    message: '',
    webSearchResults: [{ title: 'Doc 1' }, { title: 'Doc 2' }],
  });
  assert.ok(searchOnly);
  assert.equal(searchOnly.content, '[Web Search: Doc 1, Doc 2]');
});

test('extractGrokMessage drops completely empty phantom entries', () => {
  const empty = grokInternals.extractGrokMessage({
    sender: 'assistant',
    message: '',
  });
  assert.equal(empty, null);
});

// ── 3. Response Ordering & DAG Preservation ───────────────────────────────

test('orderResponses reconstructs branch from parentResponseId DAG without dropping nodes', () => {
  const responses = [
    { responseId: 'n1', message: 'First', createTime: '2026-09-20T10:00:00Z' },
    { responseId: 'n3', parentResponseId: 'n2', message: 'Third', createTime: '2026-09-20T10:02:00Z' },
    { responseId: 'n2', parentResponseId: 'n1', message: 'Second', createTime: '2026-09-20T10:01:00Z' },
    { responseId: 'disconnected', message: 'Side message', createTime: '2026-09-20T10:03:00Z' },
  ];

  const ordered = grokInternals.orderResponses(responses);
  assert.equal(ordered.length, 4);
  assert.equal(ordered[0].responseId, 'n1');
  assert.equal(ordered[1].responseId, 'n2');
  assert.equal(ordered[2].responseId, 'n3');
  assert.equal(ordered[3].responseId, 'disconnected');
});

test('orderResponses preserves chronological order when no parent links exist', () => {
  const responses = [
    { responseId: 'b', message: 'Second', createTime: '2026-09-20T10:05:00Z' },
    { responseId: 'a', message: 'First', createTime: '2026-09-20T10:01:00Z' },
  ];
  const ordered = grokInternals.orderResponses(responses);
  assert.equal(ordered[0].responseId, 'a');
  assert.equal(ordered[1].responseId, 'b');
});

// ── 4. (1) Fail-Closed on Malformed 200 in listConversations ───────────────

await testAsync('listConversations throws fail-closed when conversations array is missing', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({ error: 'something went wrong', status: 'error' });

  try {
    await assert.rejects(
      async () => {
        await grokInternals.listConversations(null);
      },
      /missing conversations array/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('syncGrok fails closed and preserves watermark when listConversations is malformed', async () => {
  const originalFetch = globalThis.fetch;
  const oldWatermark = Date.now() - 500_000;
  globalThis.fetch = async () => Response.json({ result: 'unexpected shape without conversations' });

  try {
    await assert.rejects(
      async () => {
        await syncGrok({
          state: { lastSyncMs: oldWatermark },
          push: async () => 0,
          log: () => {},
        });
      },
      /missing conversations array/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 5. (2) MaxPages Limit Reached with Remaining Pages ────────────────────

await testAsync('listConversations detects truncation when maxPages limit reached', async () => {
  const originalFetch = globalThis.fetch;
  let page = 0;
  globalThis.fetch = async () => {
    page++;
    return Response.json({
      conversations: [{ conversationId: `conv-${page}`, createTime: new Date().toISOString() }],
      nextPageToken: `tok-${page + 1}`,
    });
  };

  try {
    const list = await grokInternals.listConversations(null, { maxPages: 3 });
    assert.equal(list.length, 3);
    assert.equal(list.truncated, true, 'must mark truncated = true');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('syncGrok pushes partial batch when truncated but preserves old watermark', async () => {
  const originalFetch = globalThis.fetch;
  const now = Date.now();
  const oldWatermark = now - 500_000;
  let page = 0;

  globalThis.fetch = async (url) => {
    if (url.includes('/rest/app-chat/conversations?')) {
      page++;
      return Response.json({
        conversations: [
          { conversationId: `conv-p${page}`, title: `Chat P${page}`, modifyTime: new Date(now - 1000).toISOString() },
        ],
        nextPageToken: `tok-${page + 1}`, // infinite pagination
      });
    }
    if (url.includes('/responses')) {
      return Response.json({
        responses: [
          { responseId: 'r1', sender: 'human', message: 'Hello' },
          { responseId: 'r2', sender: 'assistant', message: 'World' },
        ],
      });
    }
    return new Response('not found', { status: 404 });
  };

  const pushed = [];
  const logs = [];
  try {
    const res = await syncGrok({
      state: { lastSyncMs: oldWatermark, maxPages: 2 },
      push: async (_s, _a, batch) => {
        pushed.push(...batch);
        return batch.length;
      },
      log: msg => logs.push(msg),
    });

    // 1. Partial conversations were pushed (2 pages)
    assert.equal(pushed.length, 2);
    assert.equal(res.conversations, 2);

    // 2. Truncation failure was logged
    assert.ok(logs.some(l => l.includes('reached page limit')));

    // 3. Watermark was NOT advanced
    assert.equal(res.state.lastSyncMs, oldWatermark, 'watermark must remain oldWatermark');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 6. (A) /responses Malformed 200 Handling & Valid Fallback ────────────

await testAsync('fetchConversationResponses recovers via /response-node when /responses is malformed', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    if (url.endsWith('/responses')) {
      // 200 OK but malformed body (missing responses array)
      return Response.json({ ok: true, unexpected: 'shape' });
    }
    if (url.includes('/response-node')) {
      return Response.json({
        responseNodes: [
          { responseId: 'u1', sender: 'human', createTime: '2026-09-20T10:00:00Z' },
          { responseId: 'a1', sender: 'assistant', createTime: '2026-09-20T10:01:00Z' },
        ],
      });
    }
    if (url.endsWith('/load-responses')) {
      return Response.json({
        responses: [
          { responseId: 'u1', sender: 'human', message: 'Hello recovered' },
          { responseId: 'a1', sender: 'assistant', message: 'Answer recovered' },
        ],
      });
    }
    return new Response('not found', { status: 404 });
  };

  try {
    const responses = await grokInternals.fetchConversationResponses('conv-recover');
    assert.equal(responses.length, 2);
    assert.equal(responses[0].message, 'Hello recovered');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('fetchConversationResponses throws when both /responses and /response-node are malformed', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({ bad: 'json' });

  try {
    await assert.rejects(
      async () => {
        await grokInternals.fetchConversationResponses('conv-both-malformed');
      },
      /missing responses array/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 7. (B) Invalid IDs and In-Progress Optimistic Placeholders ─────────────

await testAsync('fetchConversationResponses throws when entries lack valid responseId', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    if (url.endsWith('/responses')) {
      return Response.json({
        responses: [{ sender: 'human', message: 'Invalid entry with no responseId' }],
      });
    }
    return new Response('not found', { status: 404 });
  };

  try {
    await assert.rejects(
      async () => {
        await grokInternals.fetchConversationResponses('conv-invalid-id');
      },
      /missing or invalid responseId/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('fetchConversationResponses throws for in-progress optimistic-only conversations', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    if (url.endsWith('/responses')) {
      return Response.json({
        responses: [
          { responseId: 'optimistic_pending_user_msg', sender: 'human', message: 'Thinking...' },
        ],
      });
    }
    return new Response('not found', { status: 404 });
  };

  try {
    await assert.rejects(
      async () => {
        await grokInternals.fetchConversationResponses('conv-in-progress');
      },
      /in-progress with only optimistic placeholder/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 8. (3 & C) loadResponses Failures, Missing IDs & Strict Alignment ─────

await testAsync('loadResponses propagates network/server failures without swallowing', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('Server Error', { status: 500 });

  try {
    await assert.rejects(
      async () => {
        await grokInternals.loadResponses('conv-err', ['id-1'], new Map());
      },
      /500/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('loadResponses rejects when server omits unhydrated responseIds (empty content)', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({
    responses: [{ responseId: 'id-1', message: 'Text 1' }],
  });

  const rawById = new Map([
    ['id-1', { responseId: 'id-1', message: '' }],
    ['id-2', { responseId: 'id-2', message: '' }], // empty in raw
  ]);

  try {
    await assert.rejects(
      async () => {
        await grokInternals.loadResponses('conv-missing', ['id-1', 'id-2'], rawById);
      },
      /omitted 1 unhydrated responseId\(s\) \(id-2\)/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('loadResponses rejects when server omits responseId that only has unknown/malformed card', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({
    responses: [{ responseId: 'ok-1', message: 'Valid' }],
  });

  const rawById = new Map([
    ['ok-1', { responseId: 'ok-1', message: '' }],
    ['bad-card', {
      responseId: 'bad-card',
      message: '',
      cardAttachmentsJson: ['{"unknownSpecialCard": 999}'], // unknown card: cannot be extracted
    }],
  ]);

  try {
    await assert.rejects(
      async () => {
        await grokInternals.loadResponses('conv-unknown-card', ['ok-1', 'bad-card'], rawById);
      },
      /omitted 1 unhydrated responseId\(s\) \(bad-card\)/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('loadResponses rejects when server omits responseId that only has unmapped attachments', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({
    responses: [{ responseId: 'ok-1', message: 'Valid' }],
  });

  const rawById = new Map([
    ['ok-1', { responseId: 'ok-1', message: '' }],
    ['att-only', {
      responseId: 'att-only',
      message: '',
      fileAttachmentsMetadata: [{ name: 'file.pdf' }], // unmapped attachment: cannot be extracted without hydration
    }],
  ]);

  try {
    await assert.rejects(
      async () => {
        await grokInternals.loadResponses('conv-att-only', ['ok-1', 'att-only'], rawById);
      },
      /omitted 1 unhydrated responseId\(s\) \(att-only\)/
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('loadResponses accepts omission of responseId if raw entry has proven extractable content (nested message text or tool)', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({
    responses: [{ responseId: 'text-1', message: 'Answer' }],
  });

  const rawById = new Map([
    ['text-1', { responseId: 'text-1', message: '' }],
    ['nested-text', {
      responseId: 'nested-text',
      message: { text: 'already available nested text' }, // extractable!
    }],
    ['tool-1', {
      responseId: 'tool-1',
      message: '',
      cardAttachmentsJson: ['{"webSearch":{"args":{"query":"x"}}}'], // extractable tool!
    }],
  ]);

  try {
    // Must NOT throw: nested-text and tool-1 are proven extractable in rawList
    const map = await grokInternals.loadResponses('conv-proven', ['text-1', 'nested-text', 'tool-1'], rawById);
    assert.equal(map.get('text-1')?.message, 'Answer');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await testAsync('syncGrok pushes healthy conversation but preserves watermark when one conversation fails loadResponses', async () => {
  const originalFetch = globalThis.fetch;
  const now = Date.now();
  const oldWatermark = now - 200_000;

  globalThis.fetch = async (url) => {
    if (url.includes('/rest/app-chat/conversations?')) {
      return Response.json({
        conversations: [
          { conversationId: 'c-healthy', title: 'Healthy', modifyTime: new Date(now - 1000).toISOString() },
          { conversationId: 'c-corrupted', title: 'Corrupted', modifyTime: new Date(now - 2000).toISOString() },
        ],
      });
    }
    if (url.includes('/c-healthy/responses')) {
      return Response.json({
        responses: [
          { responseId: 'h1', sender: 'human', message: 'Hello healthy' },
          { responseId: 'h2', sender: 'assistant', message: 'Hi healthy' },
        ],
      });
    }
    if (url.includes('/c-corrupted/responses')) {
      return Response.json({
        responses: [
          { responseId: 'x1', sender: 'human', message: 'Hello corrupted' },
          { responseId: 'x2', sender: 'assistant', message: '' },
        ],
      });
    }
    if (url.includes('/c-corrupted/load-responses')) {
      return new Response('Hydration Error', { status: 500 });
    }
    return new Response('not found', { status: 404 });
  };

  const pushed = [];
  const logs = [];
  try {
    const res = await syncGrok({
      state: { lastSyncMs: oldWatermark },
      push: async (_s, _a, batch) => {
        pushed.push(...batch);
        return batch.length;
      },
      log: msg => logs.push(msg),
    });

    assert.equal(pushed.length, 1);
    assert.equal(pushed[0].externalId, 'c-healthy');
    assert.equal(res.conversations, 1);
    assert.ok(logs.some(l => l.includes('skipping conversation c-corrupted')));
    assert.equal(res.state.lastSyncMs, oldWatermark, 'watermark must remain oldWatermark');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 9. Legitimate Empty Conversation Handled Gracefully ────────────────────

await testAsync('readConversation handles legitimate 0-message conversation without error', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    if (url.includes('/responses')) {
      return Response.json({ responses: [] });
    }
    return new Response('not found', { status: 404 });
  };

  try {
    const conv = await grokInternals.readConversation({ conversationId: 'c-empty', title: 'Empty Chat' });
    assert.equal(conv, null, 'empty conversation evaluates to null without throwing');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 10. Clean Sync Advances Watermark ──────────────────────────────────────

await testAsync('syncGrok advances watermark when entire run is clean', async () => {
  const originalFetch = globalThis.fetch;
  const now = Date.now();
  const oldWatermark = now - 200_000;

  globalThis.fetch = async (url) => {
    if (url.includes('/rest/app-chat/conversations?')) {
      return Response.json({
        conversations: [
          { conversationId: 'c-clean', title: 'Clean', modifyTime: new Date(now - 1000).toISOString() },
        ],
      });
    }
    if (url.includes('/c-clean/responses')) {
      return Response.json({
        responses: [
          { responseId: 'c1', sender: 'human', message: 'Clean query' },
          { responseId: 'c2', sender: 'assistant', message: 'Clean reply' },
        ],
      });
    }
    return new Response('not found', { status: 404 });
  };

  try {
    const res = await syncGrok({
      state: { lastSyncMs: oldWatermark },
      push: async (_s, _a, batch) => batch.length,
      log: () => {},
    });

    assert.equal(res.conversations, 1);
    assert.ok(res.state.lastSyncMs > oldWatermark, 'watermark advanced');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ── 11. Auth Failure 401/403 ───────────────────────────────────────────────

await testAsync('syncGrok throws NotLoggedInError on 401', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('Unauthorized', { status: 401 });

  try {
    await assert.rejects(
      async () => {
        await syncGrok({
          state: {},
          push: async () => {},
          log: () => {},
        });
      },
      (err) => err instanceof NotLoggedInError && err.message.includes('grok.com')
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

if (failures > 0) {
  console.error(`${failures} test(s) failed`);
  process.exit(1);
} else {
  console.log('ALL GROK UNIT & REGRESSION TESTS PASSED');
}