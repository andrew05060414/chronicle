import { syncGemini } from '../providers/gemini.js';
import { syncGrok } from '../providers/grok.js';
import { syncPerplexity } from '../providers/perplexity.js';

let failures = 0;
function check(label, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  console.log(`${ok ? 'PASS' : 'FAIL'} ${label}`);
  if (!ok) { console.log(`  got ${JSON.stringify(actual)}, want ${JSON.stringify(expected)}`); failures++; }
}

function rpc(rpcId, data) {
  return `)]}'\n${JSON.stringify([["wrb.fr", rpcId, JSON.stringify(data), null]])}\n`;
}

const nowSec = Math.floor(Date.now() / 1000) - 3600;
const geminiHtml = '<script data-id="_gd">{"SNlM0e":"csrf-token","FdrFJe":"sid-token","cfb2h":"build-label"}</script>';
let geminiListArg = null;
globalThis.fetch = async (url, init = {}) => {
  const parsed = new URL(url);
  if (parsed.pathname === '/app') return new Response(geminiHtml, { status: 200 });
  if (parsed.searchParams.get('rpcids') === 'MaZiqc') {
    const request = JSON.parse(init.body.get('f.req'));
    geminiListArg = JSON.parse(request[0][0][1]);
    const chats = JSON.stringify(geminiListArg) === JSON.stringify([20])
      ? [[`c_gemini-fixture`, 'Gemini fixture chat', null, null, null, [nowSec, 0]]]
      : [];
    return new Response(rpc('MaZiqc', [null, null, chats]));
  }
  if (parsed.searchParams.get('rpcids') === 'hNvQHb') {
    const turn = [null, null, [['hello gemini']], [[[null, ['gemini response']]]]];
    return new Response(rpc('hNvQHb', [[turn], null]));
  }
  throw new Error(`unexpected Gemini request: ${url} ${init.method ?? 'GET'}`);
};

const geminiPushes = [];
const geminiProgress = [];
const geminiRegistrations = [];
const gemini = await syncGemini({
  state: {},
  push: async (source, adapter, conversations) => {
    geminiPushes.push({ source, adapter, conversations });
    return conversations.length;
  },
  report: async progress => geminiProgress.push(progress),
  register: async (source, adapter) => geminiRegistrations.push([source, adapter]),
  log: () => {},
});
check('gemini list uses current unfiltered payload', geminiListArg, [20]);
check('gemini sync count', gemini.conversations, 1);
check('gemini source and adapter', [geminiPushes[0]?.source, geminiPushes[0]?.adapter], ['gemini-web', 'gemini']);
check('gemini messages', geminiPushes[0]?.conversations[0]?.messages.map(message => message.content), ['hello gemini', 'gemini response']);
check('gemini reports detected sessions', geminiProgress.find(progress => progress.detected === 1)?.phase, 'importing');
check('gemini registers source', geminiRegistrations, [['gemini-web', 'gemini']]);

globalThis.fetch = async (url, init = {}) => {
  const parsed = new URL(url);
  if (parsed.pathname.endsWith('/list_ask_threads')) {
    return new Response(JSON.stringify([{
      slug: 'perplexity-fixture-slug',
      context_uuid: 'perplexity-fixture',
      title: 'Perplexity fixture chat',
      last_query_datetime: new Date(nowSec * 1000).toISOString(),
      display_model: 'sonar',
    }]), { headers: { 'Content-Type': 'application/json' } });
  }
  if (parsed.pathname.includes('/rest/thread/perplexity-fixture-slug')) {
    return new Response(JSON.stringify({ entries: [{
      query_str: 'hello perplexity',
      updated_datetime: new Date(nowSec * 1000).toISOString(),
      display_model: 'sonar',
      blocks: [{ markdown_block: { answer: 'perplexity response' } }],
    }] }), { headers: { 'Content-Type': 'application/json' } });
  }
  throw new Error(`unexpected Perplexity request: ${url} ${init.method ?? 'GET'}`);
};

const perplexityPushes = [];
const perplexityProgress = [];
const perplexityRegistrations = [];
const perplexity = await syncPerplexity({
  state: {},
  push: async (source, adapter, conversations) => {
    perplexityPushes.push({ source, adapter, conversations });
    return conversations.length;
  },
  report: async progress => perplexityProgress.push(progress),
  register: async (source, adapter) => perplexityRegistrations.push([source, adapter]),
  log: () => {},
});
check('perplexity sync count', perplexity.conversations, 1);
check('perplexity source and adapter', [perplexityPushes[0]?.source, perplexityPushes[0]?.adapter], ['perplexity-web', 'perplexity']);
check('perplexity messages', perplexityPushes[0]?.conversations[0]?.messages.map(message => message.content), ['hello perplexity', 'perplexity response']);
check('perplexity reports processed sessions', perplexityProgress.at(-1), { phase: 'complete', processed: 1 });
check('perplexity registers source', perplexityRegistrations, [['perplexity-web', 'perplexity']]);

// Grok Provider Verification
globalThis.fetch = async (url, init = {}) => {
  const parsed = new URL(url);
  if (parsed.pathname === '/rest/app-chat/conversations') {
    return new Response(JSON.stringify({
      conversations: [{
        conversationId: 'grok-fixture-conv',
        title: 'Grok fixture chat',
        createTime: new Date(nowSec * 1000).toISOString(),
        modifyTime: new Date(nowSec * 1000).toISOString(),
      }],
    }), { headers: { 'Content-Type': 'application/json' } });
  }
  if (parsed.pathname === '/rest/app-chat/conversations/grok-fixture-conv/responses') {
    return new Response(JSON.stringify({
      responses: [
        {
          responseId: 'resp-user-1',
          sender: 'human',
          message: 'hello grok',
          createTime: new Date(nowSec * 1000).toISOString(),
        },
        {
          responseId: 'resp-asst-1',
          sender: 'assistant',
          message: '', // empty to test load-responses hydration
          createTime: new Date(nowSec * 1000 + 1000).toISOString(),
          model: 'grok-3',
          parentResponseId: 'resp-user-1',
        },
      ],
    }), { headers: { 'Content-Type': 'application/json' } });
  }
  if (parsed.pathname === '/rest/app-chat/conversations/grok-fixture-conv/load-responses') {
    return new Response(JSON.stringify({
      responses: [
        {
          responseId: 'resp-asst-1',
          sender: 'assistant',
          message: 'grok response',
          thinkingTrace: 'grok thinking',
          createTime: new Date(nowSec * 1000 + 1000).toISOString(),
          model: 'grok-3',
          parentResponseId: 'resp-user-1',
        },
      ],
    }), { headers: { 'Content-Type': 'application/json' } });
  }
  throw new Error(`unexpected Grok request: ${url} ${init.method ?? 'GET'}`);
};

const grokPushes = [];
const grokProgress = [];
const grokRegistrations = [];
const grok = await syncGrok({
  state: {},
  push: async (source, adapter, conversations) => {
    grokPushes.push({ source, adapter, conversations });
    return conversations.length;
  },
  report: async progress => grokProgress.push(progress),
  register: async (source, adapter) => grokRegistrations.push([source, adapter]),
  log: () => {},
});
check('grok sync count', grok.conversations, 1);
check('grok source and adapter', [grokPushes[0]?.source, grokPushes[0]?.adapter], ['grok-web', 'grok']);
check('grok messages', grokPushes[0]?.conversations[0]?.messages.map(message => message.content), ['hello grok', 'grok response']);
check('grok thinking part', grokPushes[0]?.conversations[0]?.messages[1]?.parts.find(p => p.type === 'thinking')?.text, 'grok thinking');
check('grok reports processed sessions', grokProgress.at(-1), { phase: 'complete', detected: 1, processed: 1 });
check('grok registers source', grokRegistrations, [['grok-web', 'grok']]);

process.exit(failures ? 1 : 0);
