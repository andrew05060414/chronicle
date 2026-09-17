/**
 * Cursor adapter for hstry
 *
 * Full Composer session import (ported from am-history-importer):
 * - globalStorage/state.vscdb: composer.composerHeaders, composerData:{id}, bubbleId:{id}:*
 * - ItemTable + cursorDiskKV
 * - ~/.cursaves/snapshots/*.json.gz
 *
 * Legacy fallbacks: workbench chat tabs, aiService.prompts (workspace-only)
 */

import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
} from 'fs';
import { readdir, stat } from 'fs/promises';
import { basename, dirname, join } from 'path';
import { homedir, tmpdir } from 'os';
import { gunzipSync } from 'zlib';
import type {
  Adapter,
  AdapterInfo,
  Conversation,
  Message,
  ParseOptions,
  ToolCall,
} from '../types/index.ts';
import { runAdapter, textOnlyParts } from '../types/index.ts';

interface SqliteDb {
  prepare(sql: string): SqliteStatement;
  exec?(sql: string): void;
  run?(sql: string): void;
  close(): void;
}

interface SqliteStatement {
  all(...params: unknown[]): unknown[];
  get(...params: unknown[]): unknown;
}

let openDb: ((path: string, options?: { readonly?: boolean }) => SqliteDb) | null = null;

try {
  if (typeof Bun !== 'undefined') {
    // @ts-ignore - bun:sqlite is Bun-only
    const { Database: BunDb } = await import('bun:sqlite');
    openDb = (path: string, opts?: { readonly?: boolean }) => new BunDb(path, opts) as unknown as SqliteDb;
  } else {
    const mod = await import('better-sqlite3');
    const BetterSqlite = mod.default;
    openDb = (path: string, opts?: { readonly?: boolean }) => BetterSqlite(path, opts) as unknown as SqliteDb;
  }
} catch {
  // SQLite not available
}

declare const Bun: unknown;

const MAX_TEXT = 20_000;
const BUBBLE_USER = 1;
const BUBBLE_ASSISTANT = 2;

const CHAT_DATA_KEY = 'workbench.panel.aichat.view.aichat.chatdata';
const PROMPTS_KEY = 'aiService.prompts';
const GENERATIONS_KEY = 'aiService.generations';
const COMPOSER_HEADERS_KEY = 'composer.composerHeaders';

function cursorRoots(): string[] {
  const home = homedir();
  const appData = process.env.APPDATA || join(home, 'AppData', 'Roaming');
  const localAppData = process.env.LOCALAPPDATA || join(home, 'AppData', 'Local');
  const roots: string[] = [];

  if (process.platform === 'win32') {
    roots.push(
      join(appData, 'Cursor', 'User', 'globalStorage'),
      join(localAppData, 'Cursor', 'User', 'globalStorage'),
    );
  } else if (process.platform === 'darwin') {
    roots.push(join(home, 'Library', 'Application Support', 'Cursor', 'User', 'globalStorage'));
  } else {
    roots.push(join(home, '.config', 'Cursor', 'User', 'globalStorage'));
  }
  return [...new Set(roots)];
}

const DEFAULT_PATHS = cursorRoots();

interface BubbleHeader {
  bubbleId?: string;
  type?: number;
}

interface BubbleBody {
  type?: number;
  text?: string;
  richText?: string;
  createdAt?: string | number;
  toolResults?: unknown;
  toolFormerData?: unknown;
}

interface ComposerSessionData {
  name?: string;
  createdAt?: number | string;
  lastUpdatedAt?: number | string;
  fullConversationHeadersOnly?: BubbleHeader[];
  conversationMap?: Record<string, BubbleBody>;
}

interface ComposerHeaderEntry {
  composerId?: string;
  name?: string;
  subtitle?: string;
  createdAt?: number;
  lastUpdatedAt?: number;
  workspaceIdentifier?: {
    uri?: { fsPath?: string; path?: string };
  };
}

interface SnapshotFile {
  version?: number;
  composerId?: string;
  sourceProjectPath?: string;
  projectIdentifier?: string;
  composerData?: ComposerSessionData;
  bubbleEntries?: Record<string, BubbleBody>;
}

interface CursorPromptLegacy {
  prompt?: string;
  response?: string;
  createdAt?: number;
  conversationId?: string;
  model?: string;
}

interface CursorPromptModern {
  text?: string;
  commandType?: number;
}

type CursorPromptEntry = CursorPromptLegacy & CursorPromptModern;

class SqliteKv {
  private db: SqliteDb;
  private tmpDir: string | null = null;

  constructor(dbPath: string) {
    if (!openDb) throw new Error('SQLite not available');
    try {
      this.db = openDb(dbPath, { readonly: true });
      this.db.prepare('SELECT 1').get();
    } catch {
      const dir = join(tmpdir(), `hstry-cursor-${Date.now()}-${Math.random().toString(16).slice(2)}`);
      mkdirSync(dir, { recursive: true });
      this.tmpDir = dir;
      const tmpDb = join(dir, 'state.vscdb');
      copyFileSync(dbPath, tmpDb);
      for (const suffix of ['-wal', '-shm']) {
        const side = dbPath + suffix;
        if (existsSync(side)) copyFileSync(side, tmpDb + suffix);
      }
      this.db = openDb(tmpDb, { readonly: false });
      try {
        if (typeof this.db.exec === 'function') {
          this.db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
        } else if (typeof this.db.run === 'function') {
          this.db.run('PRAGMA wal_checkpoint(TRUNCATE)');
        }
      } catch {
        /* ignore */
      }
    }
  }

  getItem(key: string, table = 'ItemTable'): string | null {
    try {
      const row = this.db
        .prepare(`SELECT value FROM ${table} WHERE key = ?`)
        .get(key) as { value: string | Buffer | null } | undefined;
      if (!row || row.value == null) return null;
      if (typeof row.value === 'string') return row.value;
      return Buffer.from(row.value).toString('utf8');
    } catch {
      return null;
    }
  }

  getJson<T>(key: string, table = 'ItemTable'): T | null {
    const raw = this.getItem(key, table) ?? this.getItem(key, 'cursorDiskKV');
    if (!raw) return null;
    try {
      return JSON.parse(raw) as T;
    } catch {
      return null;
    }
  }

  listKeys(prefix: string, table = 'cursorDiskKV'): string[] {
    try {
      const rows = this.db
        .prepare(`SELECT key FROM ${table} WHERE key LIKE ?`)
        .all(`${prefix}%`) as Array<{ key: string }>;
      return rows.map((r) => r.key);
    } catch {
      return [];
    }
  }

  hasComposerData(): boolean {
    if (this.getJson(COMPOSER_HEADERS_KEY)) return true;
    const prefixes = [
      ...this.listKeys('composerData:', 'cursorDiskKV'),
      ...this.listKeys('composerData:', 'ItemTable'),
      ...this.listKeys('bubbleId:', 'cursorDiskKV'),
      ...this.listKeys('bubbleId:', 'ItemTable'),
    ];
    return prefixes.length > 0;
  }

  close(): void {
    try {
      this.db.close();
    } catch {
      /* ignore */
    }
    if (this.tmpDir) {
      try {
        rmSync(this.tmpDir, { recursive: true, force: true });
      } catch {
        /* ignore */
      }
    }
  }
}

function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

function toMs(value: unknown): number | undefined {
  if (typeof value === 'number' && Number.isFinite(value) && value > 0) {
    return Math.floor(value < 1e12 ? value * 1000 : value);
  }
  if (typeof value === 'string') {
    if (value.includes('T')) {
      const parsed = Date.parse(value);
      if (Number.isFinite(parsed)) return Math.floor(parsed);
    }
    const n = Number(value);
    if (Number.isFinite(n) && n > 0) return Math.floor(n < 1e12 ? n * 1000 : n);
  }
  return undefined;
}

function bubbleText(b: BubbleBody | undefined): string {
  if (!b) return '';
  if (typeof b.text === 'string' && b.text.trim()) return b.text.trim();
  if (typeof b.richText === 'string' && b.richText.trim()) return b.richText.trim();
  return '';
}

function extractToolCalls(body: BubbleBody | undefined): ToolCall[] {
  if (!body?.toolResults) return [];
  const tools = Array.isArray(body.toolResults) ? body.toolResults : [body.toolResults];
  const out: ToolCall[] = [];
  for (const tool of tools) {
    if (!tool || typeof tool !== 'object') continue;
    const t = tool as Record<string, unknown>;
    const name =
      (typeof t.name === 'string' && t.name) ||
      (typeof t.toolName === 'string' && t.toolName) ||
      'tool';
    const output =
      typeof t.result === 'string'
        ? t.result
        : JSON.stringify(t.result ?? t.output ?? '');
    out.push({
      toolName: name,
      input: t.params ?? t.input,
      output: truncate(output, 8000),
      status: 'success',
    });
  }
  return out;
}

function sessionFromComposer(opts: {
  composerId: string;
  composerData: ComposerSessionData;
  bubbles: Record<string, BubbleBody>;
  projectPath?: string;
  sourcePath: string;
  opts?: ParseOptions;
}): Conversation | null {
  const headers = opts.composerData.fullConversationHeadersOnly || [];
  const messages: Message[] = [];
  let firstTime: number | undefined;
  let lastTime: number | undefined;

  const pushBubble = (header: BubbleHeader, body: BubbleBody | undefined) => {
    const text = truncate(bubbleText(body), MAX_TEXT);
    if (!text) return;
    const createdAt = toMs(body?.createdAt) ?? toMs(opts.composerData.createdAt);
    if (createdAt) {
      if (!firstTime || createdAt < firstTime) firstTime = createdAt;
      if (!lastTime || createdAt > lastTime) lastTime = createdAt;
    }
    const type = header.type ?? body?.type;
    if (type === BUBBLE_USER) {
      messages.push({
        role: 'user',
        content: text,
        parts: textOnlyParts(text),
        createdAt,
      });
    } else if (type === BUBBLE_ASSISTANT) {
      const toolCalls = extractToolCalls(body);
      messages.push({
        role: 'assistant',
        content: text,
        parts: textOnlyParts(text),
        createdAt,
        toolCalls: toolCalls.length > 0 ? toolCalls : undefined,
      });
    }
  };

  if (headers.length > 0) {
    for (const header of headers) {
      const id = header.bubbleId;
      if (!id) continue;
      const body = opts.bubbles[id] || opts.composerData.conversationMap?.[id];
      pushBubble(header, body);
    }
  } else {
    const map = opts.composerData.conversationMap || opts.bubbles;
    for (const [id, body] of Object.entries(map)) {
      pushBubble({ bubbleId: id, type: body.type }, body);
    }
  }

  if (messages.length === 0) return null;

  const createdAt = firstTime ?? toMs(opts.composerData.createdAt) ?? Date.now();
  const updatedAt = lastTime ?? toMs(opts.composerData.lastUpdatedAt) ?? createdAt;

  if (opts.opts?.since) {
    const lastModified = updatedAt ?? createdAt;
    if (createdAt < opts.opts.since && lastModified < opts.opts.since) return null;
  }

  const workspace = opts.projectPath || basename(opts.sourcePath);
  return {
    externalId: opts.composerId,
    title: opts.composerData.name || `cursor:${opts.composerId.slice(0, 8)}`,
    createdAt,
    updatedAt,
    workspace,
    provider: 'cursor',
    messages,
    metadata: {
      source: 'cursor-composer',
      composerId: opts.composerId,
      sourcePath: opts.sourcePath,
    },
  };
}

function loadFromGlobalDb(dbPath: string, opts?: ParseOptions): Conversation[] {
  const db = new SqliteKv(dbPath);
  const conversations: Conversation[] = [];
  try {
    const headers =
      db.getJson<{ allComposers?: ComposerHeaderEntry[] }>(COMPOSER_HEADERS_KEY)?.allComposers ||
      [];

    const composerIds = new Set<string>();
    for (const h of headers) {
      if (typeof h.composerId === 'string') composerIds.add(h.composerId);
    }

    for (const key of [
      ...db.listKeys('composerData:', 'cursorDiskKV'),
      ...db.listKeys('composerData:', 'ItemTable'),
    ]) {
      const id = key.slice('composerData:'.length);
      if (id) composerIds.add(id);
    }

    for (const composerId of composerIds) {
      const composerData =
        db.getJson<ComposerSessionData>(`composerData:${composerId}`, 'cursorDiskKV') ||
        db.getJson<ComposerSessionData>(`composerData:${composerId}`, 'ItemTable');
      if (!composerData) continue;

      const bubbles: Record<string, BubbleBody> = { ...(composerData.conversationMap || {}) };
      for (const key of [
        ...db.listKeys(`bubbleId:${composerId}:`, 'cursorDiskKV'),
        ...db.listKeys(`bubbleId:${composerId}:`, 'ItemTable'),
      ]) {
        const bubbleId = key.slice(`bubbleId:${composerId}:`.length);
        const body =
          db.getJson<BubbleBody>(key, 'cursorDiskKV') || db.getJson<BubbleBody>(key, 'ItemTable');
        if (body) bubbles[bubbleId] = body;
      }

      const headerEntry = headers.find((h) => h.composerId === composerId);
      const projectPath =
        headerEntry?.workspaceIdentifier?.uri?.fsPath ||
        headerEntry?.workspaceIdentifier?.uri?.path;

      const session = sessionFromComposer({
        composerId,
        composerData: {
          ...composerData,
          name:
            composerData.name ||
            (typeof headerEntry?.name === 'string' ? headerEntry.name : undefined),
        },
        bubbles,
        projectPath,
        sourcePath: `${dbPath}#${composerId}`,
        opts,
      });
      if (session) conversations.push(session);
    }
  } finally {
    db.close();
  }
  return conversations;
}

function parseSnapshotFile(path: string, opts?: ParseOptions): Conversation | null {
  const raw = path.endsWith('.gz') ? gunzipSync(readFileSync(path)) : readFileSync(path);
  const data = JSON.parse(raw.toString('utf8')) as SnapshotFile;
  const composerId =
    data.composerId ||
    basename(path)
      .replace(/\.json(\.gz)?$/i, '')
      .replace(/\.\d+$/, '');
  return sessionFromComposer({
    composerId,
    composerData: data.composerData || {},
    bubbles: data.bubbleEntries || {},
    projectPath: data.sourceProjectPath,
    sourcePath: path,
    opts,
  });
}

function walkFiles(
  root: string,
  pred: (name: string, full: string) => boolean,
  out: string[] = [],
): string[] {
  if (!existsSync(root)) return out;
  let st;
  try {
    st = statSync(root);
  } catch {
    return out;
  }
  if (st.isFile()) {
    if (pred(basename(root), root)) out.push(root);
    return out;
  }
  let entries;
  try {
    entries = readdirSync(root, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const full = join(root, entry.name);
    if (entry.isDirectory()) walkFiles(full, pred, out);
    else if (pred(entry.name, full)) out.push(full);
  }
  return out;
}

interface ChatTabBubble {
  type?: string | number;
  text?: string;
  rawText?: string;
  modelType?: string;
  createdAt?: number | string;
}

interface ChatTabData {
  tabs?: Array<{
    id?: string;
    tabId?: string;
    title?: string;
    createdAt?: number | string;
    lastUpdatedAt?: number | string;
    bubbles?: ChatTabBubble[];
  }>;
}

function parseWorkbenchChatData(
  rawJson: string,
  workspaceId: string,
  sourcePath: string,
  opts?: ParseOptions,
): Conversation[] {
  try {
    const data = JSON.parse(rawJson) as ChatTabData;
    if (!data?.tabs || !Array.isArray(data.tabs)) return [];

    const conversations: Conversation[] = [];
    for (const tab of data.tabs) {
      const tabId = tab.id || tab.tabId;
      if (!tabId || !tab.bubbles || !Array.isArray(tab.bubbles)) continue;

      const messages: Message[] = [];
      let firstTime: number | undefined;
      let lastTime: number | undefined;

      for (const b of tab.bubbles) {
        const text = truncate(b.text || b.rawText || '', MAX_TEXT);
        if (!text) continue;

        const createdAt = toMs(b.createdAt) ?? toMs(tab.createdAt);
        if (createdAt) {
          if (!firstTime || createdAt < firstTime) firstTime = createdAt;
          if (!lastTime || createdAt > lastTime) lastTime = createdAt;
        }

        const isUser = b.type === 'user' || b.type === 1 || b.type === 'human';
        if (isUser) {
          messages.push({
            role: 'user',
            content: text,
            parts: textOnlyParts(text),
            createdAt,
          });
        } else {
          messages.push({
            role: 'assistant',
            content: text,
            parts: textOnlyParts(text),
            createdAt,
            model: b.modelType,
          });
        }
      }

      if (messages.length === 0) continue;
      const createdAt = firstTime ?? toMs(tab.createdAt) ?? Date.now();
      const updatedAt = lastTime ?? toMs(tab.lastUpdatedAt) ?? createdAt;

      if (opts?.since && createdAt < opts.since && updatedAt < opts.since) continue;

      conversations.push({
        externalId: tabId,
        title: tab.title || messages[0]?.content?.slice(0, 80) || 'Cursor Chat',
        createdAt,
        updatedAt,
        workspace: workspaceId,
        provider: 'cursor',
        messages,
        metadata: {
          source: 'cursor-chat-tabs',
          tabId,
          sourcePath,
        },
      });
    }
    return conversations;
  } catch {
    return [];
  }
}

function parseModernPromptsOnly(
  value: string,
  generations: Array<{ unixMs?: number; textDescription?: string; type?: string }> | undefined,
  workspaceId: string,
  opts?: ParseOptions,
): Conversation[] {
  try {
    const prompts = JSON.parse(value) as CursorPromptEntry[];
    if (!Array.isArray(prompts) || prompts.length === 0) return [];

    const messages: Message[] = [];
    let firstTime: number | undefined;
    let lastTime: number | undefined;

    for (let i = 0; i < prompts.length; i++) {
      const text = prompts[i].text?.trim() || prompts[i].prompt?.trim();
      if (!text) continue;
      const gen = generations?.[i];
      const createdAt = toMs(gen?.unixMs) ?? toMs(prompts[i].createdAt);
      messages.push({ role: 'user', content: text, parts: textOnlyParts(text), createdAt });
      if (prompts[i].response) {
        messages.push({
          role: 'assistant',
          content: prompts[i].response!,
          parts: textOnlyParts(prompts[i].response!),
          createdAt,
          model: prompts[i].model,
        });
      }
      if (createdAt) {
        if (!firstTime || createdAt < firstTime) firstTime = createdAt;
        if (!lastTime || createdAt > lastTime) lastTime = createdAt;
      }
    }

    if (messages.length === 0) return [];
    const createdAt = firstTime ?? Date.now();
    const updatedAt = lastTime ?? createdAt;
    if (opts?.since && createdAt < opts.since && updatedAt < opts.since) return [];

    return [
      {
        externalId: `cursor-prompts-${workspaceId}`,
        title: messages[0]?.content?.slice(0, 80),
        createdAt,
        updatedAt,
        workspace: workspaceId,
        provider: 'cursor',
        messages,
        metadata: { source: 'cursor-prompts-fallback' },
      },
    ];
  } catch {
    return [];
  }
}

function loadFromDb(dbPath: string, opts?: ParseOptions): Conversation[] {
  const conversations: Conversation[] = [];
  const workspaceId = basename(dirname(dbPath));

  // 1. Try loading Composer sessions
  const composerConvs = loadFromGlobalDb(dbPath, opts);
  if (composerConvs.length > 0) {
    conversations.push(...composerConvs);
  }

  // 2. Try loading workbench chat tabs and prompts via SqliteKv
  let kv: SqliteKv | null = null;
  try {
    kv = new SqliteKv(dbPath);
    const chatDataRaw = kv.getItem(CHAT_DATA_KEY);
    if (chatDataRaw) {
      const chatConvs = parseWorkbenchChatData(chatDataRaw, workspaceId, dbPath, opts);
      conversations.push(...chatConvs);
    }

    // 3. If still nothing found, try legacy prompts
    if (conversations.length === 0) {
      const promptsRaw = kv.getItem(PROMPTS_KEY);
      if (promptsRaw) {
        const genRaw = kv.getItem(GENERATIONS_KEY);
        let generations: Array<{ unixMs?: number; textDescription?: string; type?: string }> | undefined;
        if (genRaw) {
          try {
            generations = JSON.parse(genRaw);
          } catch {
            /* ignore */
          }
        }
        const promptConvs = parseModernPromptsOnly(promptsRaw, generations, workspaceId, opts);
        conversations.push(...promptConvs);
      }
    }
  } catch {
    /* ignore */
  } finally {
    kv?.close();
  }

  return conversations;
}

function expandScanRoots(inputPath?: string): string[] {
  if (!inputPath || inputPath === '.' || inputPath === '') {
    return DEFAULT_PATHS;
  }
  const expanded = inputPath.replace(/^~(?=$|\/|\\)/, homedir());
  return [expanded];
}

function isSnapshotFile(name: string, fullPath: string): boolean {
  if (name.endsWith('.meta.json')) return false;
  if (name.endsWith('.json.gz')) return true;
  if (name.endsWith('.json')) {
    const p = fullPath.toLowerCase();
    if (p.includes('snapshots') || p.includes('cursaves') || p.includes('snapshot')) return true;
    try {
      const head = readFileSync(fullPath, { encoding: 'utf8', flag: 'r' }).slice(0, 500);
      return head.includes('composerId') || head.includes('bubbleEntries') || head.includes('composerData');
    } catch {
      return false;
    }
  }
  return false;
}

function loadAllCursorSessions(inputPath: string, opts?: ParseOptions): Conversation[] {
  const conversations: Conversation[] = [];
  const seen = new Set<string>();
  const limit = opts?.limit && opts.limit > 0 ? opts.limit : 0;

  const add = (conv: Conversation | null): boolean => {
    if (!conv) return false;
    const id = conv.externalId ?? `${conv.createdAt}-${conv.title}`;
    if (seen.has(id)) return false;
    seen.add(id);
    conversations.push(conv);
    return true;
  };

  // NOTE: collect the FULL candidate set before sorting + slicing (#15).
  // Short-circuiting collection at `limit` returns whichever sessions the
  // filesystem walk happens to find first, not the newest ones.
  const roots = expandScanRoots(inputPath);

  for (const root of roots) {
    if (!existsSync(root)) continue;

    // 1. Primary: load live sessions from SQLite state.vscdb
    if (root.endsWith('state.vscdb') || basename(root) === 'state.vscdb') {
      for (const conv of loadFromDb(root, opts)) {
        add(conv);
      }
    } else {
      const dbFiles = walkFiles(root, (name) => name === 'state.vscdb');
      dbFiles.sort((a, b) => {
        const aGlobal = a.includes('globalStorage') ? 0 : 1;
        const bGlobal = b.includes('globalStorage') ? 0 : 1;
        return aGlobal - bGlobal;
      });

      for (const dbPath of dbFiles) {
        for (const conv of loadFromDb(dbPath, opts)) {
          add(conv);
        }
      }
    }

    // 2. Secondary: fill in any historical sessions only present in offline snapshots (.json / .json.gz)
    const snapshots = walkFiles(root, isSnapshotFile).sort((a, b) => {
      try {
        return statSync(b).mtimeMs - statSync(a).mtimeMs;
      } catch {
        return 0;
      }
    });

    for (const snap of snapshots) {
      try {
        add(parseSnapshotFile(snap, opts));
      } catch {
        /* skip */
      }
    }
  }

  conversations.sort((a, b) => b.createdAt - a.createdAt);
  return limit > 0 ? conversations.slice(0, limit) : conversations;
}

function conversationsToMarkdown(conversations: Conversation[]): string {
  const blocks: string[] = [];
  for (const conv of conversations) {
    blocks.push(`# ${conv.title ?? 'Conversation'}`);
    blocks.push('');
    for (const msg of conv.messages) {
      blocks.push(`## ${msg.role}`);
      blocks.push('');
      blocks.push(msg.content || '');
      blocks.push('');
    }
  }
  return blocks.join('\n').trim() + '\n';
}

const adapter: Adapter = {
  info(): AdapterInfo {
    return {
      name: 'cursor',
      displayName: 'Cursor',
      version: '2.0.0',
      defaultPaths: DEFAULT_PATHS,
    };
  },

  async detect(path: string): Promise<number | null> {
    if (!openDb) return null;

    const roots = expandScanRoots(path);
    for (const root of roots) {
      if (!existsSync(root)) continue;

      const snapshots = walkFiles(root, isSnapshotFile);
      if (snapshots.length > 0) return 0.95;

      const dbs: string[] = [];
      if (root.endsWith('state.vscdb') || basename(root) === 'state.vscdb') {
        dbs.push(root);
      } else {
        dbs.push(...walkFiles(root, (name) => name === 'state.vscdb'));
      }

      for (const dbPath of dbs.slice(0, 5)) {
        let db: SqliteKv | null = null;
        try {
          db = new SqliteKv(dbPath);
          const ok = db.hasComposerData() || db.getItem(CHAT_DATA_KEY) || db.getItem(PROMPTS_KEY);
          if (ok) return 0.95;
        } catch {
          /* continue */
        } finally {
          db?.close();
        }
      }
    }

    return null;
  },

  async parse(path: string, opts?: ParseOptions): Promise<Conversation[]> {
    if (!openDb) return [];
    return loadAllCursorSessions(path, opts);
  },

  async export(conversations, opts) {
    if (opts.format === 'markdown') {
      return {
        format: 'markdown',
        content: conversationsToMarkdown(conversations),
        mimeType: 'text/markdown',
      };
    }
    if (opts.format === 'json' || opts.format === 'cursor') {
      return {
        format: opts.format === 'cursor' ? 'cursor' : 'json',
        content: JSON.stringify(conversations, null, opts.pretty ? 2 : 0),
        mimeType: 'application/json',
      };
    }
    throw new Error(`Unsupported export format: ${opts.format}`);
  },
};

runAdapter(adapter);
