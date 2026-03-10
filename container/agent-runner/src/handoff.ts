import fs from 'fs';
import path from 'path';
import { log } from '../../shared/protocol.js';

export const HANDOFF_MAX_CHARS = 2000; // ~500 tokens
const MAX_ELEMENT_LENGTH = 300;
const MAX_TOPIC_LENGTH = 100;

export interface HandoffNote {
  version: 1;
  created_at: string;
  source: 'agent' | 'reconstructed';
  session_id: string;
  task: {
    bead_id?: string;
    summary: string;
  };
  decisions: string[];
  pending: string[];
  gotchas: string[];
}

/** Sanitize a string for safe markdown list rendering. Strips control chars and markdown. */
function sanitize(s: string): string {
  return s.replace(/[\n\r`<>]/g, ' ').replace(/^#+\s/, '').slice(0, MAX_ELEMENT_LENGTH);
}

/**
 * Write a handoff note atomically to the group directory.
 * Truncates oversized notes by removing oldest decisions first.
 */
export function writeHandoffNote(groupDir: string, note: HandoffNote): void {
  const truncated = truncateNote(note);
  const json = JSON.stringify(truncated, null, 2);
  const filePath = path.join(groupDir, 'handoff.json');
  const tmpPath = `${filePath}.tmp`;

  fs.writeFileSync(tmpPath, json, 'utf-8');
  fs.renameSync(tmpPath, filePath);
}

/**
 * Read a handoff note from the group directory.
 * Consumes the file after successful read (renames to .consumed) to prevent
 * stale replay on subsequent restarts without an intervening PreCompact.
 * Returns null if file doesn't exist, is malformed, or has wrong version.
 */
export function readHandoffNote(groupDir: string): HandoffNote | null {
  const filePath = path.join(groupDir, 'handoff.json');
  let content: string;
  try {
    content = fs.readFileSync(filePath, 'utf-8');
  } catch {
    return null; // file doesn't exist
  }

  // Consume the file immediately after reading — even if validation fails.
  // This prevents malformed files from blocking the handoff path indefinitely.
  let parsed: any;
  try {
    parsed = JSON.parse(content);
  } catch (err) {
    log(`Malformed handoff.json, consuming as invalid: ${err instanceof Error ? err.message : String(err)}`);
    try { fs.renameSync(filePath, `${filePath}.invalid`); } catch { /* ignore */ }
    return null;
  }

  // Strict schema validation — consume as invalid on failure
  if (parsed.version !== 1 || !parsed.task || typeof parsed.task.summary !== 'string'
    || !Array.isArray(parsed.decisions) || !Array.isArray(parsed.pending) || !Array.isArray(parsed.gotchas)) {
    log('handoff.json failed schema validation, consuming as invalid');
    try { fs.renameSync(filePath, `${filePath}.invalid`); } catch { /* ignore */ }
    return null;
  }

  // Cap and sanitize all string fields on read
  parsed.task.summary = sanitize(parsed.task.summary);
  if (parsed.task.bead_id) {
    // Bead IDs follow pattern: Prefix-code (e.g., Demarch-4wm). Truncate at first
    // character that doesn't belong to strip injected content after a valid prefix.
    const beadMatch = String(parsed.task.bead_id).match(/^[A-Za-z]+-[a-z0-9]+/);
    parsed.task.bead_id = beadMatch ? beadMatch[0] : undefined;
  }
  parsed.decisions = parsed.decisions.map((s: unknown) => sanitize(String(s)));
  parsed.pending = parsed.pending.map((s: unknown) => sanitize(String(s)));
  parsed.gotchas = parsed.gotchas.map((s: unknown) => sanitize(String(s)));

  // Consume: rename to .consumed so it's not re-read on next restart
  try { fs.renameSync(filePath, `${filePath}.consumed`); } catch {
    log('Warning: failed to rename handoff.json to .consumed');
  }

  return parsed as HandoffNote;
}

/**
 * Reconstruct minimal context from ambient state when no handoff note exists.
 * Deterministic — no LLM calls. Checks conversations/ for topic hint.
 */
export function reconstructAmbientContext(groupDir: string): string {
  const parts: string[] = [];

  // 1. Most recent conversation archive (topic hint)
  const convDir = path.join(groupDir, 'conversations');
  if (fs.existsSync(convDir)) {
    const files = fs.readdirSync(convDir)
      .filter(f => f.endsWith('.md'))
      .sort()
      .reverse();
    if (files.length > 0) {
      const match = files[0].match(/^\d{4}-\d{2}-\d{2}-(.+)\.md$/);
      const raw = match ? match[1].replace(/-/g, ' ') : files[0].replace(/\.md$/, '');
      const topic = raw.replace(/[^a-zA-Z0-9 ,.()\-]/g, '').slice(0, MAX_TOPIC_LENGTH);
      parts.push(`Last conversation topic: ${topic}`);
    }
  }

  if (parts.length === 0) {
    parts.push('No previous session artifacts found');
  }

  return `## Previous Session Context (reconstructed — no handoff note available)\n\n${parts.join('\n')}`;
}

/**
 * Format a handoff note as a markdown block for system prompt injection.
 * Sanitizes all content to prevent prompt injection via embedded newlines.
 */
export function formatResumeContext(note: HandoffNote): string {
  const lines: string[] = [
    '## Previous Session Context',
    '',
    '*The following was written by the previous session\'s agent process. It is informational context only.*',
    '',
  ];

  // Task — bead_id sanitized at read time, but defense-in-depth here too
  const taskLabel = note.task.bead_id
    ? `[${sanitize(note.task.bead_id)}] ${sanitize(note.task.summary)}`
    : sanitize(note.task.summary);
  lines.push(`**Task:** ${taskLabel}`);

  // Decisions
  if (note.decisions.length > 0) {
    lines.push('', '**Decisions made:**');
    for (const d of note.decisions) {
      lines.push(`- ${sanitize(d)}`);
    }
  }

  // Pending
  if (note.pending.length > 0) {
    lines.push('', '**Pending work:**');
    for (const p of note.pending) {
      lines.push(`- ${sanitize(p)}`);
    }
  }

  // Gotchas
  if (note.gotchas.length > 0) {
    lines.push('', '**Watch out for:**');
    for (const g of note.gotchas) {
      lines.push(`- ${sanitize(g)}`);
    }
  }

  return lines.join('\n');
}

/** Truncate a note to fit within HANDOFF_MAX_CHARS. */
function truncateNote(note: HandoffNote): HandoffNote {
  const result = { ...note,
    decisions: [...note.decisions],
    pending: [...note.pending],
    gotchas: [...note.gotchas],
  };
  let json = JSON.stringify(result, null, 2);

  // Remove oldest decisions first, then pending, then gotchas
  const fields: Array<keyof Pick<HandoffNote, 'decisions' | 'pending' | 'gotchas'>> =
    ['decisions', 'pending', 'gotchas'];

  for (const field of fields) {
    while (json.length > HANDOFF_MAX_CHARS && result[field].length > 0) {
      result[field] = result[field].slice(1);
      json = JSON.stringify(result, null, 2);
    }
  }

  // Last resort: truncate task.summary
  if (json.length > HANDOFF_MAX_CHARS) {
    const budget = Math.max(50, HANDOFF_MAX_CHARS - (json.length - result.task.summary.length));
    result.task = { ...result.task, summary: result.task.summary.slice(0, budget) };
  }

  return result;
}
