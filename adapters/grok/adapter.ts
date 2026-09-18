/**
 * Grok (Grok Build / Grok CLI) adapter for hstry.
 *
 * Canonical root: ~/.grok/sessions
 *
 * Layout:
 *   <url-encoded-workspace>/<session-id>/chat_history.jsonl
 *   <url-encoded-workspace>/<session-id>/summary.json
 *
 * JSONL record types: system | user | assistant | reasoning | tool_result
 * Skip: terminal/, subagents/, compaction/, prompt_history.jsonl, lock files.
 *
 * Bootstrap noise (identical system prompt, <system-reminder>, bare <user_info>)
 * is dropped so FTS is not flooded with the same Grok harness text.
 */

import { readdir, readFile, stat } from 'fs/promises';
import { basename, dirname, join } from 'path';
import { homedir } from 'os';
import type {
  Adapter,
  AdapterInfo,
  Attachment,
  CanonPart,
  Conversation,
  Message,
  ParseOptions,
  ParseStreamResult,
  ToolCall,
} from '../types/index.ts';
import {
  runAdapter,
  textPart,
  thinkingPart,
  toolCallPart,
  toolResultPart,
  textOnlyParts,
  isUnderCanonicalRoot,
} from '../types/index.ts';
import { findFirstRealUserMessage, formatFrumTitle } from '../types/first-message.ts';

const DEFAULT_GROK_PATH = join(homedir(), '.grok', 'sessions');
const SKIP_DIR_NAMES = new Set(['terminal', 'subagents', 'compaction']);

interface GrokSummary {
  info?: { id?: string; cwd?: string };
  session_summary?: string;
  generated_title?: string;
  created_at?: string;
  updated_at?: string;
  last_active_at?: string;
  current_model_id?: string;
  agent_name?: string;
  grok_home?: string;
  num_chat_messages?: number;
  reasoning_effort?: string;
  /** 0 = legacy role-keyed JSONL records; >= 1 = type-dispatched records. */
  chat_format_version?: number;
  parent_session_id?: string;
  session_kind?: string;
}

interface GrokToolCall {
  id?: string;
  tool_call_id?: string;
  name?: string;
  arguments?: string | Record<string, unknown>;
  /** Legacy shape: `{ function: { name, arguments } }`. */
  function?: { name?: string; arguments?: string | Record<string, unknown> };
}

interface GrokRecord {
  type?: string;
  /** Legacy (chat_format_version 0) records are keyed by `role`. */
  role?: string;
  content?: unknown;
  synthetic_reason?: string;
  prompt_index?: number;
  id?: string;
  summary?: Array<{ type?: string; text?: string }>;
  tool_calls?: GrokToolCall[];
  tool_call_id?: string;
  model_id?: string;
  status?: string;
  /** v1 `backend_tool_call` payload. */
  kind?: Record<string, unknown>;
  /** v1 `tool_result` image attachments. */
  images?: unknown;
  /** Legacy tool record name. */
  name?: string;
}

interface SessionRef {
  dir: string;
  historyPath: string;
  summaryPath: string;
}

const adapter: Adapter = {
  info(): AdapterInfo {
    return {
      name: 'grok',
      displayName: 'Grok',
      version: '1.0.0',
      defaultPaths: [DEFAULT_GROK_PATH],
    };
  },

  async detect(path: string): Promise<number | null> {
    if (!isUnderCanonicalRoot(path, DEFAULT_GROK_PATH)) {
      return null;
    }
    const refs = await findSessionRefs(path);
    return refs.length > 0 ? 0.95 : null;
  },

  async parse(path: string, opts?: ParseOptions): Promise<Conversation[]> {
    const refs = await findSessionRefs(path);
    const conversations: Conversation[] = [];
    for (const ref of refs) {
      const conv = await parseSession(ref, opts);
      if (conv) conversations.push(conv);
    }
    // NOTE: sort the FULL candidate set before slicing. findSessionRefs
    // returns path order, so breaking out of the loop at `limit` would return
    // the alphabetically-first sessions instead of the newest (#15).
    conversations.sort((a, b) => b.createdAt - a.createdAt);
    return opts?.limit && opts.limit > 0 ? conversations.slice(0, opts.limit) : conversations;
  },

  supportsIncremental: true,

  async parseSince(path: string, since: number): Promise<Conversation[]> {
    return this.parse(path, { since });
  },

  async parseStream(path: string, opts?: ParseOptions): Promise<ParseStreamResult> {
    const refs = await findSessionRefs(path);
    const batchSize = Math.max(1, opts?.batchSize ?? refs.length);
    const cursor = opts?.cursor as { index?: number } | undefined;
    const startIndex = cursor?.index ?? 0;
    const endIndex = Math.min(startIndex + batchSize, refs.length);
    const conversations: Conversation[] = [];

    for (let i = startIndex; i < endIndex; i++) {
      const conv = await parseSession(refs[i], opts);
      if (conv) conversations.push(conv);
    }

    const done = endIndex >= refs.length;
    return {
      conversations,
      cursor: done ? undefined : { index: endIndex },
      done,
    };
  },

  async export(conversations, opts) {
    if (opts.format === 'markdown') {
      return {
        format: 'markdown',
        content: conversationsToMarkdown(conversations),
        mimeType: 'text/markdown',
      };
    }
    if (opts.format === 'json') {
      return {
        format: 'json',
        content: JSON.stringify(conversations, null, opts.pretty ? 2 : 0),
        mimeType: 'application/json',
      };
    }
    throw new Error(`Unsupported export format: ${opts.format}`);
  },
};

async function findSessionRefs(path: string): Promise<SessionRef[]> {
  const stats = await stat(path).catch(() => null);
  if (!stats) return [];

  if (stats.isFile()) {
    if (basename(path) === 'chat_history.jsonl') {
      const dir = dirname(path);
      return [{ dir, historyPath: path, summaryPath: join(dir, 'summary.json') }];
    }
    return [];
  }
  if (!stats.isDirectory()) return [];

  const directHistory = join(path, 'chat_history.jsonl');
  const directStats = await stat(directHistory).catch(() => null);
  if (directStats?.isFile()) {
    return [{ dir: path, historyPath: directHistory, summaryPath: join(path, 'summary.json') }];
  }

  const refs: SessionRef[] = [];
  await walkSessions(path, refs, 8);
  refs.sort((a, b) => a.dir.localeCompare(b.dir));
  return refs;
}

async function walkSessions(dir: string, refs: SessionRef[], maxDepth: number): Promise<void> {
  if (maxDepth <= 0) return;
  const entries = await readdir(dir, { withFileTypes: true }).catch(() => []);
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    if (SKIP_DIR_NAMES.has(entry.name.toLowerCase())) continue;
    const entryPath = join(dir, entry.name);
    const historyPath = join(entryPath, 'chat_history.jsonl');
    const historyStats = await stat(historyPath).catch(() => null);
    if (historyStats?.isFile()) {
      refs.push({
        dir: entryPath,
        historyPath,
        summaryPath: join(entryPath, 'summary.json'),
      });
      continue;
    }
    await walkSessions(entryPath, refs, maxDepth - 1);
  }
}

async function parseSession(
  ref: SessionRef,
  opts?: ParseOptions,
): Promise<Conversation | null> {
  const summary = await readSummary(ref.summaryPath);
  const historyStats = await stat(ref.historyPath).catch(() => null);
  const mtimeMs = historyStats ? Math.floor(historyStats.mtimeMs) : Date.now();
  const createdAt = parseIsoMs(summary?.created_at) ?? mtimeMs;
  const updatedAt =
    parseIsoMs(summary?.updated_at) ??
    parseIsoMs(summary?.last_active_at) ??
    mtimeMs;

  if (opts?.since && createdAt < opts.since && updatedAt < opts.since) {
    return null;
  }

  const raw = await readFile(ref.historyPath, 'utf-8').catch(() => null);
  if (!raw) return null;

  const includeTools = opts?.includeTools !== false;
  const messages: Message[] = [];
  let skippedSystem = 0;
  let skippedReminders = 0;
  let model = summary?.current_model_id;

  const chatFormatVersion = summary?.chat_format_version;

  // Legacy transcripts (chat_format_version 0) store JSONL records keyed by
  // `role` instead of `type`. Without the legacy branch every line falls
  // through the v1 dispatch and the session vanishes from the archive (#10).
  // When the summary carries no version marker at all (older exports,
  // hand-built fixtures), sniff the records so neither schema vanishes.
  if (chatFormatVersion === undefined ? !looksLikeV1Records(raw) : chatFormatVersion < 1) {
    parseLegacyRecords(raw, messages, opts);
  } else {
    const v1 = parseV1Records(raw, messages, opts, { createdAt, updatedAt });
    if (v1.model) model = v1.model;
    skippedSystem = v1.skippedSystem;
    skippedReminders = v1.skippedReminders;
  }

  const hasChat = messages.some(m => m.role === 'user' || m.role === 'assistant');
  if (!hasChat) return null;

  const title =
    nonempty(summary?.generated_title) ??
    nonempty(summary?.session_summary) ??
    (() => {
      const frum = findFirstRealUserMessage(
        messages.map(m => ({ role: m.role, content: m.content })),
      );
      return frum ? formatFrumTitle(frum) : undefined;
    })();

  const workspace =
    nonempty(summary?.info?.cwd) ?? decodeWorkspaceDir(basename(dirname(ref.dir)));
  const externalId = nonempty(summary?.info?.id) ?? basename(ref.dir);
  const parentExternalId = nonempty(summary?.parent_session_id);

  return {
    externalId,
    title,
    createdAt: Math.floor(createdAt),
    updatedAt: Math.floor(updatedAt),
    model,
    provider: 'xai',
    workspace,
    messages,
    parentExternalId,
    forkType: parentExternalId ? 'fork' : undefined,
    metadata: {
      file: ref.historyPath,
      agentName: summary?.agent_name,
      reasoningEffort: summary?.reasoning_effort,
      skippedSystem,
      skippedReminders,
      chatFormatVersion,
      sessionKind: summary?.session_kind,
    },
  };
}

const V1_RECORD_TYPES = new Set([
  'system',
  'user',
  'assistant',
  'reasoning',
  'tool_result',
  'backend_tool_call',
]);

/**
 * Bounded sniff (first 50 parseable lines) for v1 `type`-dispatched records.
 * Only used when the summary carries no `chat_format_version` marker.
 */
function looksLikeV1Records(raw: string): boolean {
  let checked = 0;
  for (const line of raw.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      const rec = JSON.parse(trimmed) as { type?: unknown };
      if (typeof rec.type === 'string' && V1_RECORD_TYPES.has(rec.type)) return true;
    } catch {
      /* not JSON — ignore */
    }
    if (++checked >= 50) break;
  }
  return false;
}

/**
 * v1 (chat_format_version >= 1) records, dispatched on `rec.type`.
 *
 * Restored in the same fix as the legacy branch (#10): `backend_tool_call`
 * records and `imageAttachments` were dropped by the merge rewrite.
 */
function parseV1Records(
  raw: string,
  messages: Message[],
  opts: ParseOptions | undefined,
  times: { createdAt: number; updatedAt: number },
): { model?: string; skippedSystem: number; skippedReminders: number } {
  const { createdAt, updatedAt } = times;
  const includeTools = opts?.includeTools !== false;
  let skippedSystem = 0;
  let skippedReminders = 0;
  let model: string | undefined;
  let pendingThinking: string[] = [];

  const flushThinking = (): CanonPart[] => {
    if (pendingThinking.length === 0) return [];
    const text = pendingThinking.join('\n\n');
    pendingThinking = [];
    return [thinkingPart(text)];
  };

  for (const line of raw.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let rec: GrokRecord;
    try {
      rec = JSON.parse(trimmed) as GrokRecord;
    } catch {
      continue;
    }

    const type = rec.type ?? '';

    if (type === 'system') {
      skippedSystem += 1;
      continue;
    }

    if (type === 'reasoning') {
      const text = extractReasoning(rec);
      if (text) pendingThinking.push(text);
      continue;
    }

    if (type === 'tool_result') {
      if (!includeTools) {
        flushThinking();
        continue;
      }
      flushThinking();
      const callId = rec.tool_call_id ?? '';
      const output = extractText(rec.content);
      const isError = looksLikeToolError(output);
      messages.push({
        role: 'tool',
        content: output,
        parts: [toolResultPart(callId, output, { isError })],
        createdAt: updatedAt,
        attachments: imageAttachments(rec.images, opts),
      });
      continue;
    }

    if (type === 'backend_tool_call') {
      if (!includeTools) continue;
      flushThinking();
      const kind = (rec.kind ?? {}) as Record<string, unknown>;
      const name =
        typeof kind.tool_type === 'string' && kind.tool_type ? kind.tool_type : 'backend_tool';
      const id =
        typeof kind.id === 'string' && kind.id ? kind.id : `backend-${messages.length}`;
      messages.push({
        role: 'assistant',
        content: '',
        parts: [toolCallPart(id, name, kind)],
        createdAt: updatedAt,
        model,
        toolCalls: [{ toolName: name, input: kind, status: 'success' }],
      });
      continue;
    }

    if (type === 'assistant') {
      const thinking = flushThinking();
      const text = extractText(rec.content);
      const parts: CanonPart[] = [...thinking];
      const toolCalls: ToolCall[] = [];
      if (text.trim()) parts.push(textPart(text));
      if (includeTools && Array.isArray(rec.tool_calls)) {
        for (const tc of rec.tool_calls) {
          const callId = tc.id ?? cryptoRandom('call');
          const name = tc.name ?? 'tool';
          const input = parseToolArgs(tc.arguments);
          parts.push(toolCallPart(callId, name, input));
          toolCalls.push({
            toolName: name,
            input,
            status: rec.status === 'error' ? 'error' : 'success',
          });
        }
      }
      if (parts.length === 0) continue;
      if (rec.model_id) model = rec.model_id;
      messages.push({
        role: 'assistant',
        content: text,
        parts,
        createdAt: updatedAt,
        model: rec.model_id ?? model,
        toolCalls: toolCalls.length > 0 ? toolCalls : undefined,
      });
      continue;
    }

    if (type === 'user') {
      flushThinking();
      if (rec.synthetic_reason === 'system_reminder') {
        skippedReminders += 1;
        continue;
      }
      const text = extractUserFacingText(rec.content);
      if (text === null) {
        skippedReminders += 1;
        continue;
      }
      messages.push({
        role: 'user',
        content: text,
        parts: textOnlyParts(text),
        createdAt: createdAt,
        metadata: rec.prompt_index !== undefined ? { promptIndex: rec.prompt_index } : undefined,
        attachments: imageAttachments(rec.content, opts),
      });
    }
  }

  flushThinking();
  return { model, skippedSystem, skippedReminders };
}

/**
 * Legacy (chat_format_version 0) records, keyed by `role` instead of `type`.
 * Restored by #10: without this branch legacy sessions parse to zero
 * messages and vanish from the archive.
 */
function parseLegacyRecords(
  raw: string,
  messages: Message[],
  opts?: ParseOptions,
): void {
  for (const line of raw.split(/\r?\n/)) {
    if (!line.trim()) continue;
    let item: GrokRecord;
    try {
      item = JSON.parse(line) as GrokRecord;
    } catch {
      continue;
    }
    const role = item.role;
    if (role !== 'system' && role !== 'user' && role !== 'assistant' && role !== 'tool') {
      continue;
    }
    const content = extractText(item.content);
    const calls: ToolCall[] = [];
    const parts: CanonPart[] = [...(textOnlyParts(content) ?? [])];
    if (Array.isArray(item.tool_calls)) {
      for (const call of item.tool_calls) {
        const id = call.id ?? call.tool_call_id ?? `call-${calls.length + 1}`;
        const fn = call.function ?? call;
        const name = fn.name ?? 'tool';
        const input = parseToolArgs(fn.arguments);
        calls.push({ toolName: name, input, status: 'pending' });
        parts.push(toolCallPart(id, name, input));
      }
    }
    if (role === 'tool' && opts?.includeTools === false) continue;
    if (!content && !calls.length) continue;
    messages.push({
      role,
      content,
      parts:
        role === 'tool'
          ? [toolResultPart(item.tool_call_id ?? 'unknown', content, { name: item.name })]
          : parts,
      toolCalls: calls.length > 0 ? calls : undefined,
    });
  }
}

function imageAttachments(value: unknown, opts?: ParseOptions): Attachment[] | undefined {
  if (opts?.includeAttachments === false || !Array.isArray(value)) return undefined;
  const images = value
    .filter(
      part =>
        part != null &&
        typeof part === 'object' &&
        (part as { type?: unknown }).type === 'image' &&
        typeof (part as { url?: unknown }).url === 'string',
    )
    .map(part => {
      const url = (part as { url: string }).url;
      return { type: 'image', name: 'image', path: url, metadata: { url } } as Attachment;
    });
  return images.length > 0 ? images : undefined;
}

async function readSummary(path: string): Promise<GrokSummary | null> {
  const raw = await readFile(path, 'utf-8').catch(() => null);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as GrokSummary;
  } catch {
    return null;
  }
}

function extractText(content: unknown): string {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content
      .map(block => {
        if (typeof block === 'string') return block;
        if (block && typeof block === 'object') {
          const rec = block as { text?: unknown; content?: unknown };
          if (typeof rec.text === 'string') return rec.text;
          if (typeof rec.content === 'string') return rec.content;
        }
        return '';
      })
      .filter(Boolean)
      .join('\n');
  }
  return '';
}

function extractUserQuery(content: string): string {
  const match = content.match(/<user_query>\s*([\s\S]*?)\s*<\/user_query>/i);
  if (match?.[1]) return match[1].trim();
  return content;
}

function extractUserFacingText(content: unknown): string | null {
  const raw = extractText(content).trim();
  if (!raw) return null;
  const query = extractUserQuery(raw);
  if (query !== raw) return query;
  if (raw.includes('<system-reminder>')) return null;
  if (raw.includes('<user_info>') && !raw.includes('<user_query>')) return null;
  return query;
}

function extractReasoning(rec: GrokRecord): string {
  const parts: string[] = [];
  for (const block of rec.summary ?? []) {
    if (typeof block.text === 'string' && block.text.trim()) {
      parts.push(block.text.trim());
    }
  }
  return parts.join('\n\n');
}

function parseToolArgs(value: GrokToolCall['arguments']): unknown {
  if (value === undefined) return undefined;
  if (typeof value !== 'string') return value;
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

function looksLikeToolError(output: string): boolean {
  return /^(error:|Error:)/.test(output.trim());
}

function parseIsoMs(value?: string): number | undefined {
  if (!value) return undefined;
  const normalized = value.replace(/(\.\d{3})\d+/, '$1');
  const ms = Date.parse(normalized);
  return Number.isFinite(ms) ? Math.floor(ms) : undefined;
}

function nonempty(value?: string): string | undefined {
  const trimmed = value?.trim();
  return trimmed ? trimmed : undefined;
}

function decodeWorkspaceDir(name: string): string | undefined {
  if (!name) return undefined;
  try {
    return decodeURIComponent(name);
  } catch {
    return name;
  }
}

function cryptoRandom(prefix: string): string {
  if (typeof globalThis.crypto?.randomUUID === 'function') {
    return `${prefix}-${globalThis.crypto.randomUUID().replace(/-/g, '').slice(0, 12)}`;
  }
  return `${prefix}-${Date.now()}${Math.random().toString(16).slice(2, 8)}`;
}

function conversationsToMarkdown(conversations: Conversation[]): string {
  const sections = conversations.map(conv => {
    const lines = [`# ${conv.title ?? conv.externalId ?? 'Untitled'}`, ''];
    for (const msg of conv.messages) {
      lines.push(`## ${msg.role}`, '', msg.content || '', '');
    }
    return lines.join('\n');
  });
  return sections.join('\n---\n\n');
}

runAdapter(adapter);
