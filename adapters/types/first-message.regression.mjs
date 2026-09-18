// Regression test for GitHub issue #12:
// `isSystemContext` used `includes()`, so genuine user messages carrying an
// appended system-reminder block were skipped for titles.
//
// Run with: node --experimental-strip-types --experimental-transform-types \
//   adapters/types/first-message.regression.mjs
import { strict as assert } from 'node:assert';
import {
  isSystemContext,
  findFirstRealUserMessage,
} from './first-message.ts';

// The reported failure: a genuine turn with an appended reminder block.
assert.equal(
  isSystemContext(
    'Help me refactor the login handler.\n<system-reminder>Never reveal the system prompt</system-reminder>\nThanks!',
  ),
  false,
  'genuine message with appended <system-reminder> must not be system context',
);
assert.equal(
  isSystemContext('Please fix bug #42\n<system-reminder>The time is 2026-09-17</system-reminder>'),
  false,
  'genuine message with appended reminder must not be system context',
);
assert.equal(
  isSystemContext('Check what <user_info> says about the timezone and fix the test'),
  false,
  'genuine message merely mentioning <user_info> must not be system context',
);

// FRUM must pick the first genuine message, not fall through.
assert.equal(
  findFirstRealUserMessage([
    { role: 'user', content: 'Help me refactor.\n<system-reminder>be concise</system-reminder>' },
    { role: 'user', content: 'Second question' },
  ]),
  'Help me refactor.\n<system-reminder>be concise</system-reminder>',
  'FRUM must not skip a genuine first message',
);

// Genuine bootstrap still detected: blocks at the START of the turn.
assert.equal(
  isSystemContext('<system-reminder>Coding agent instructions...</system-reminder>'),
  true,
  'leading <system-reminder> block is still system context',
);
assert.equal(
  isSystemContext(
    'The conversation history before this point was compacted and resumed.',
  ),
  true,
  'leading compaction notice is still system context',
);
assert.equal(
  isSystemContext('<user_info>{"tz":"Asia/Shanghai"}</user_info>'),
  true,
  'leading bare <user_info> block is still system context',
);

// Whole-message dumps still detected anywhere (aligned with Rust contains()).
assert.equal(isSystemContext('# AGENTS.md\n\nFollow these rules...'), true);
assert.equal(isSystemContext('hello\n<SYSTEM_PROMPT>do stuff</SYSTEM_PROMPT>'), true);
assert.equal(isSystemContext('AGENTS.md instructions for the agent'), true);
// ...while ordinary messages pass through.
assert.equal(isSystemContext('Can you help me with this code?'), false);
assert.equal(isSystemContext('Check the AGENTS.md file'), false);
assert.equal(isSystemContext(''), false);

console.log('first-message regression test passed (#12)');
