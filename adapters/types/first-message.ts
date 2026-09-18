/**
 * Shared first-real-user-message (FRUM) extraction for adapters.
 *
 * Different agent harnesses (Claude Code, Codex, Pi, etc.) all face the same
 * problem: the first user message in a session is often a system bootstrap
 * (AGENTS.md instructions, system prompt, MCP tool list). Using that as a
 * conversation title produces noise. This module gives every adapter the same
 * filter so derived titles describe what the user actually asked.
 *
 * Mirrors `is_system_context` in crates/hstry-cli/src/main.rs — keep them
 * aligned when adding new markers.
 */

/**
 * Markers for whole-message bootstrap dumps (system prompts, AGENTS.md
 * injections, skill lists). These only ever constitute the *entire* user turn
 * when they are genuine bootstrap, so `includes()` matching is safe here —
 * and it keeps these markers aligned with `is_system_context` in
 * crates/hstry-cli/src/main.rs, which also uses `contains()`.
 */
const SYSTEM_CONTEXT_MARKERS = [
  '# AGENTS.md',
  '# Agent Configuration',
  '<available_skills>',
  'Guidance for coding agents',
  '<SYSTEM_PROMPT>',
  '</SYSTEM_PROMPT>',
];

/**
 * Markers for blocks that harnesses (Claude Code, Grok, ...) routinely
 * *append* to an otherwise genuine user turn. Matching these with
 * `includes()` misclassifies the whole turn as system context and the title
 * silently falls through to a later message (or `undefined`). They only count
 * when the message starts with them — mirroring how Rust's
 * `is_continuation_fragment` uses `starts_with` for the compaction notice.
 */
const APPENDED_BLOCK_MARKERS = [
  'The conversation history before this point was compacted',
  '<system-reminder>',
  '<user_info>',
];

/** Returns true if `content` looks like a system bootstrap, not a real user request. */
export function isSystemContext(content: string): boolean {
  if (!content) return false;
  for (const marker of SYSTEM_CONTEXT_MARKERS) {
    if (content.includes(marker)) return true;
  }
  const trimmed = content.trimStart();
  for (const marker of APPENDED_BLOCK_MARKERS) {
    if (trimmed.startsWith(marker)) return true;
  }
  if (content.includes('AGENTS.md') && content.includes('instructions')) return true;
  return false;
}

interface FrumCandidate {
  role?: string;
  content?: string;
}

/**
 * Find the first user message that isn't system context.
 * Returns trimmed content, or undefined if none found.
 */
export function findFirstRealUserMessage(messages: FrumCandidate[]): string | undefined {
  for (const msg of messages) {
    const role = msg.role?.toLowerCase();
    if (role !== 'user' && role !== 'human') continue;
    const content = msg.content?.trim();
    if (!content) continue;
    if (isSystemContext(content)) continue;
    return content;
  }
  return undefined;
}

/**
 * Format a FRUM as a one-line title preview. Collapses whitespace and truncates.
 */
export function formatFrumTitle(message: string, maxLen = 80): string {
  const collapsed = message.replace(/\s+/g, ' ').trim();
  if (collapsed.length <= maxLen) return collapsed;
  return `${collapsed.slice(0, maxLen - 3)}...`;
}
