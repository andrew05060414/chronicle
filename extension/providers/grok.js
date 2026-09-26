// Grok web sync: syncs conversations and messages from grok.com via its web API,
// authenticated with browser session cookies (credentials: 'include').

import {
  NotLoggedInError,
  fetchJson,
  sleep,
  textPart,
  thinkingPart,
  toolCallPart,
  toMs,
} from '../lib/common.js';

const BASE = 'https://grok.com';
const PAGE_SIZE = 50;
const OVERLAP_MS = 5 * 60 * 1000;
const THROTTLE_MS = 200;
const LOAD_BATCH_SIZE = 40;

function grokHeaders() {
  const headers = {
    accept: 'application/json',
    'content-type': 'application/json',
  };
  if (typeof globalThis.crypto?.randomUUID === 'function') {
    headers['x-xai-request-id'] = globalThis.crypto.randomUUID();
  }
  return headers;
}

/**
 * Extract a single canonical message from a Grok response entry.
 * Preserves text, reasoning/thinking traces, and recognized tool metadata.
 * Returns null if the entry cannot be transformed into canonical content.
 */
function extractGrokMessage(entry, defaultTime = Date.now()) {
  if (!entry || typeof entry !== 'object') return null;

  // Sender mapping: human / user -> 'user', tool -> 'tool', system -> 'system', else 'assistant'
  const rawSender = String(entry.sender ?? entry.role ?? '').toLowerCase();
  let role = 'assistant';
  if (rawSender === 'human' || rawSender === 'user') {
    role = 'user';
  } else if (rawSender === 'tool') {
    role = 'tool';
  } else if (rawSender === 'system') {
    role = 'system';
  }

  // Text content extraction
  let text = '';
  if (typeof entry.message === 'string') {
    text = entry.message;
  } else if (typeof entry.text === 'string') {
    text = entry.text;
  } else if (typeof entry.content === 'string') {
    text = entry.content;
  } else if (typeof entry.query === 'string' && role === 'user') {
    text = entry.query;
  } else if (entry.message && typeof entry.message === 'object') {
    text = entry.message.text || entry.message.content || '';
  }

  const parts = [];

  // Extract thinking / reasoning trace
  if (typeof entry.thinkingTrace === 'string' && entry.thinkingTrace.trim()) {
    parts.push(thinkingPart(entry.thinkingTrace.trim()));
  } else if (Array.isArray(entry.steps)) {
    for (const step of entry.steps) {
      const think = step?.thinking || (step?.type === 'thinking' && (step?.text || step?.message));
      if (typeof think === 'string' && think.trim()) {
        parts.push(thinkingPart(think.trim()));
      }
    }
  }

  // Extract text part
  if (text.trim()) {
    parts.push(textPart(text.trim()));
  }

  // Tool usage cards from cardAttachmentsJson (only recognized formats)
  if (Array.isArray(entry.cardAttachmentsJson)) {
    for (const cardStr of entry.cardAttachmentsJson) {
      try {
        const card = typeof cardStr === 'string' ? JSON.parse(cardStr) : cardStr;
        if (card?.mcp && typeof card.mcp.toolName === 'string') {
          const callId = card.toolUsageCardId || card.mcp.toolName;
          parts.push(toolCallPart(callId, card.mcp.toolName, card.mcp.toolArgsJson));
        } else if (card?.webSearch && card.webSearch.args) {
          parts.push(
            toolCallPart(card.toolUsageCardId || 'web_search', 'web_search', card.webSearch.args)
          );
        }
      } catch {
        // Non-JSON or unrecognized card format: ignore, not a recognized part
      }
    }
  }

  // Tool steps
  if (Array.isArray(entry.steps)) {
    for (const step of entry.steps) {
      if (step?.type === 'tool_call' || step?.toolName) {
        const name = step.toolName || 'tool';
        const callId = step.id || step.toolCallId || name;
        parts.push(toolCallPart(callId, name, step.input || step.args));
      }
    }
  }

  // Web search results fallback when text is empty
  if (
    !text.trim() &&
    parts.length === 0 &&
    Array.isArray(entry.webSearchResults) &&
    entry.webSearchResults.length > 0
  ) {
    const titles = entry.webSearchResults
      .map(r => r?.title || r?.url)
      .filter(Boolean)
      .join(', ');
    if (titles) {
      text = `[Web Search: ${titles}]`;
      parts.push(textPart(text));
    }
  }

  // Do not emit empty message if there is no text and no recognized canonical parts
  if (!text.trim() && parts.length === 0) {
    return null;
  }

  const createdAt = toMs(entry.createTime ?? entry.createdAt) ?? defaultTime;

  return {
    role,
    content: text.trim(),
    createdAt,
    model: entry.model ?? null,
    parts,
  };
}

/**
 * Check whether a response entry yields a non-null canonical message when extracted.
 * Strictly derives from extractGrokMessage to guarantee zero divergence.
 * Unknown cards and unmapped attachments cannot be used as exemptions.
 */
function hasExtractableContent(entry) {
  return extractGrokMessage(entry, 0) !== null;
}

/**
 * List updated conversations from Grok's REST endpoint.
 * Paginates using `pageToken` until reaching conversations older than `sinceMs`
 * or until no more pages exist.
 *
 * Fail-closed: Throws on malformed responses missing conversations array.
 * Sets `threads.truncated = true` if maxPages is reached while remaining pages exist.
 */
async function listConversations(sinceMs, { maxPages = 200 } = {}) {
  const threads = [];
  let pageToken = null;
  let truncated = false;

  for (let page = 0; page < maxPages; page++) {
    let url = `${BASE}/rest/app-chat/conversations?pageSize=${PAGE_SIZE}`;
    if (pageToken) {
      url += `&pageToken=${encodeURIComponent(pageToken)}`;
    }
    const data = await fetchJson(url, { headers: grokHeaders() });
    if (!data || typeof data !== 'object' || !Array.isArray(data.conversations)) {
      throw new Error('Grok returned malformed conversations response: missing conversations array');
    }

    const items = data.conversations;
    let reachedOld = false;

    for (const item of items) {
      const updatedAt = toMs(item.modifyTime ?? item.createTime);
      if (sinceMs && updatedAt && updatedAt <= sinceMs) {
        reachedOld = true;
        continue;
      }
      threads.push({
        ...item,
        conversationId: String(item.conversationId ?? item.id ?? ''),
        updatedAt,
      });
    }

    if (reachedOld || !data.nextPageToken || items.length === 0) {
      pageToken = null;
      break;
    }
    pageToken = data.nextPageToken;
  }

  if (pageToken) {
    truncated = true;
  }

  const result = threads.filter(t => t.conversationId);
  result.truncated = truncated;
  return result;
}

/**
 * Batch-load full message contents via POST /load-responses.
 * Propagates network/server failures without swallowing.
 * Rejects if any requested ID is omitted UNLESS the raw entry already has extractable content.
 */
async function loadResponses(conversationId, responseIds, rawById = new Map()) {
  const map = new Map();
  if (!responseIds.length) return map;

  for (let i = 0; i < responseIds.length; i += LOAD_BATCH_SIZE) {
    const chunk = responseIds.slice(i, i + LOAD_BATCH_SIZE);
    const data = await fetchJson(
      `${BASE}/rest/app-chat/conversations/${encodeURIComponent(conversationId)}/load-responses`,
      {
        method: 'POST',
        headers: grokHeaders(),
        body: JSON.stringify({ responseIds: chunk }),
      }
    );

    const responses = Array.isArray(data)
      ? data
      : Array.isArray(data?.responses)
      ? data.responses
      : null;

    if (!responses) {
      throw new Error(`Grok load-responses returned malformed response for ${conversationId}`);
    }

    for (const resp of responses) {
      if (resp?.responseId) {
        map.set(resp.responseId, resp);
      }
    }
  }

  // Check omitted IDs: only accept omission if the raw entry already produces extractable canonical content
  const omittedIds = responseIds.filter(id => !map.has(id));
  if (omittedIds.length > 0) {
    const unrecoverable = omittedIds.filter(id => !hasExtractableContent(rawById.get(id)));
    if (unrecoverable.length > 0) {
      throw new Error(
        `Grok load-responses omitted ${unrecoverable.length} unhydrated responseId(s) (${unrecoverable.join(', ')}) for ${conversationId}`
      );
    }
  }

  return map;
}

/**
 * Fetch raw response entries for a conversation.
 * Tries GET /responses first. If unavailable, falls back to GET /response-node.
 * Hydrates missing message contents via POST /load-responses.
 * Fail-closed on malformed responses or invalid IDs; flags optimistic placeholders.
 */
async function fetchConversationResponses(conversationId) {
  let rawList = [];
  let primaryError = null;

  // Primary: GET /responses
  try {
    const data = await fetchJson(
      `${BASE}/rest/app-chat/conversations/${encodeURIComponent(conversationId)}/responses`,
      { headers: grokHeaders() }
    );
    if (Array.isArray(data)) {
      rawList = data;
    } else if (data && typeof data === 'object' && Array.isArray(data.responses)) {
      rawList = data.responses;
    } else {
      primaryError = new Error(
        `Grok /responses returned malformed response for ${conversationId}: missing responses array`
      );
    }
  } catch (err) {
    if (err instanceof NotLoggedInError) throw err;
    primaryError = err;
  }

  // Fallback: GET /response-node?includeThreads=true
  if (rawList.length === 0 && primaryError) {
    try {
      const data = await fetchJson(
        `${BASE}/rest/app-chat/conversations/${encodeURIComponent(conversationId)}/response-node?includeThreads=true`,
        { headers: grokHeaders() }
      );
      const nodes = Array.isArray(data?.responseNodes)
        ? data.responseNodes
        : Array.isArray(data)
        ? data
        : null;
      if (nodes) {
        rawList = nodes;
        primaryError = null; // Successfully fell back to valid response-node
      } else {
        throw new Error(
          `Grok /response-node returned malformed response for ${conversationId}: missing responseNodes array`
        );
      }
    } catch (err) {
      if (err instanceof NotLoggedInError) throw err;
      throw new Error(
        `Failed to fetch responses for ${conversationId}: primary (${primaryError.message}), fallback (${err.message})`
      );
    }
  }

  if (rawList.length === 0) {
    if (primaryError) throw primaryError;
    // Legitimate empty conversation (0 messages on valid 200 OK)
    return [];
  }

  const valid = [];
  const placeholders = [];
  const invalidEntries = [];

  for (const r of rawList) {
    if (!r || typeof r !== 'object' || typeof r.responseId !== 'string' || !r.responseId.trim()) {
      invalidEntries.push(r);
      continue;
    }
    const id = r.responseId;
    if (id.startsWith('optimistic_') || id.startsWith('streaming_in_progress_')) {
      placeholders.push(r);
      continue;
    }
    valid.push(r);
  }

  if (invalidEntries.length > 0) {
    throw new Error(
      `Grok returned ${invalidEntries.length} response entries with missing or invalid responseId for ${conversationId}`
    );
  }

  if (valid.length === 0) {
    if (placeholders.length > 0) {
      throw new Error(
        `Conversation ${conversationId} is in-progress with only optimistic placeholder responses; retrying later`
      );
    }
    return [];
  }

  // Build raw lookup map
  const rawById = new Map(valid.map(r => [r.responseId, r]));

  // Check if any response entries require hydration (missing message text)
  const missingIds = valid
    .filter(r => !r.message || (typeof r.message === 'string' && !r.message.trim()))
    .map(r => r.responseId);

  if (missingIds.length > 0) {
    const hydrated = await loadResponses(conversationId, missingIds, rawById);
    return valid.map(r => ({ ...r, ...(hydrated.get(r.responseId) ?? {}) }));
  }

  return valid;
}

/**
 * Reconstruct response order from parentResponseId DAG when present.
 * Uses topological sort with timestamp awareness so parents always precede children,
 * and disconnected or branching nodes are never silently dropped.
 */
function orderResponses(responses) {
  if (!responses || !responses.length) return [];

  const compareEntries = (a, b) => {
    const ta = toMs(a.createTime ?? a.createdAt) ?? 0;
    const tb = toMs(b.createTime ?? b.createdAt) ?? 0;
    return ta - tb || String(a.responseId ?? '').localeCompare(String(b.responseId ?? ''));
  };

  const byId = new Map(responses.map(r => [r.responseId, r]));
  const children = new Map();
  const indegree = new Map(responses.map(r => [r.responseId, 0]));

  for (const r of responses) {
    const parentId = r.parentResponseId;
    if (parentId && parentId !== r.responseId && byId.has(parentId)) {
      indegree.set(r.responseId, 1);
      const siblings = children.get(parentId) ?? [];
      siblings.push(r.responseId);
      children.set(parentId, siblings);
    }
  }

  const ready = responses.filter(r => indegree.get(r.responseId) === 0).sort(compareEntries);
  const ordered = [];

  while (ready.length > 0) {
    const node = ready.shift();
    ordered.push(node);
    for (const childId of children.get(node.responseId) ?? []) {
      const remainingIn = indegree.get(childId) - 1;
      indegree.set(childId, remainingIn);
      if (remainingIn === 0) {
        ready.push(byId.get(childId));
        ready.sort(compareEntries);
      }
    }
  }

  if (ordered.length < responses.length) {
    const orderedIds = new Set(ordered.map(r => r.responseId));
    ordered.push(...responses.filter(r => !orderedIds.has(r.responseId)).sort(compareEntries));
  }

  return ordered;
}

/**
 * Build parsed Conversation object.
 */
function toParsedConversation(summary, responses) {
  const ordered = orderResponses(responses);
  const defaultTime = summary.updatedAt ?? toMs(summary.createTime) ?? Date.now();
  const messages = ordered
    .map(entry => extractGrokMessage(entry, defaultTime))
    .filter(Boolean);

  if (messages.length === 0) return null;

  const externalId = String(summary.conversationId);
  const firstUser = messages.find(m => m.role === 'user');
  const title =
    typeof summary.title === 'string' && summary.title.trim()
      ? summary.title.trim()
      : firstUser?.content
      ? firstUser.content.slice(0, 80).replace(/\n/g, ' ').trim()
      : 'Untitled conversation';

  const lastAssistant = [...messages].reverse().find(m => m.role === 'assistant' && m.model);
  const model = lastAssistant?.model ?? summary.model ?? null;

  return {
    externalId,
    title,
    createdAt: toMs(summary.createTime) ?? messages[0]?.createdAt ?? Date.now(),
    updatedAt:
      summary.updatedAt ??
      toMs(summary.modifyTime) ??
      messages[messages.length - 1]?.createdAt ??
      Date.now(),
    model,
    provider: 'xai',
    messages,
    metadata: {
      url: `${BASE}/c/${externalId}`,
      ...(summary.starred !== undefined ? { starred: summary.starred } : {}),
      ...(summary.temporary !== undefined ? { temporary: summary.temporary } : {}),
      ...(Array.isArray(summary.workspaces) && summary.workspaces.length > 0
        ? { workspaces: summary.workspaces }
        : {}),
    },
  };
}

async function readConversation(summary) {
  const responses = await fetchConversationResponses(summary.conversationId);
  return toParsedConversation(summary, responses);
}

/**
 * Main provider sync entry point.
 */
export async function syncGrok({
  state,
  push,
  register = async () => {},
  log = console.log,
  report = async () => {},
}) {
  await report({ phase: 'discovering' });
  const lastSyncMs = state?.lastSyncMs ?? null;
  await register('grok-web', 'grok');
  const since = lastSyncMs ? lastSyncMs - OVERLAP_MS : null;
  const runStartedMs = Date.now();

  const maxPages = state?.maxPages ?? 200;
  const summaries = await listConversations(since, { maxPages });
  await report({ phase: 'importing', detected: summaries.length, processed: 0 });

  let total = 0;
  let failures = 0;
  let processed = 0;
  let batch = [];
  let first = true;

  if (summaries.truncated) {
    failures++;
    log('grok: conversation list reached page limit with remaining pages; preserving watermark');
  }

  for (const summary of summaries) {
    if (!first) await sleep(THROTTLE_MS);
    first = false;

    try {
      const conv = await readConversation(summary);
      if (conv) batch.push(conv);
    } catch (err) {
      failures++;
      log(`grok: skipping conversation ${summary.conversationId}: ${err.message}`);
    }

    processed++;
    await report({ phase: 'importing', detected: summaries.length, processed });

    if (batch.length >= 10) {
      total += await push('grok-web', 'grok', batch);
      batch = [];
    }
  }

  if (batch.length > 0) {
    total += await push('grok-web', 'grok', batch);
  }

  if (failures > 0) {
    log(`grok: ${failures} issue(s) during sync; keeping watermark to retry next run`);
  }

  await report({ phase: 'complete', detected: summaries.length, processed });

  return {
    state: { lastSyncMs: failures > 0 ? lastSyncMs : runStartedMs },
    conversations: total,
  };
}

export const grokInternals = {
  hasExtractableContent,
  listConversations,
  fetchConversationResponses,
  loadResponses,
  orderResponses,
  extractGrokMessage,
  toParsedConversation,
  readConversation,
  grokHeaders,
};
