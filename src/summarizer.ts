import { spawn } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';

import { getRouterState, setRouterState } from './db.js';
import { logger } from './logger.js';

const SUMMARY_MODEL = 'gpt-5.3-codex';  // Full model — we have the tokens

interface CachedSummary {
  summary: string;
  previousModel: string;
  messageCount: number;
  createdAt: string;
}

function summaryKey(chatJid: string): string {
  return `summary:${chatJid}`;
}

export function getCachedSummary(chatJid: string): CachedSummary | null {
  const raw = getRouterState(summaryKey(chatJid));
  if (!raw) return null;
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

export function clearCachedSummary(chatJid: string): void {
  setRouterState(summaryKey(chatJid), '');
}

/**
 * Generate a conversation summary via codex exec (fire-and-forget).
 * Writes result to router_state for later retrieval.
 */
export async function generateSummary(
  chatJid: string,
  messages: { sender_name: string; content: string; is_bot_message: boolean }[],
  previousModel: string,
  assistantName: string,
): Promise<void> {
  if (messages.length === 0) return;

  // Format conversation for the summarizer
  const transcript = messages.map((m) => {
    const role = m.is_bot_message ? assistantName : m.sender_name;
    return `${role}: ${m.content}`;
  }).join('\n');

  const prompt = [
    'Summarize this conversation concisely. Preserve:',
    '- Key topics discussed and decisions made',
    '- Any tasks, requests, or action items',
    '- Important context the user shared (names, preferences, data)',
    '- The current state of any ongoing work',
    '',
    'Keep it under 500 words. Use bullet points. Do not add commentary.',
    '',
    '---',
    transcript,
  ].join('\n');

  const outputFile = path.join(os.tmpdir(), `intercom-summary-${Date.now()}.txt`);

  try {
    const summary = await runCodexExec(prompt, outputFile);
    setRouterState(summaryKey(chatJid), JSON.stringify({
      summary,
      previousModel,
      messageCount: messages.length,
      createdAt: new Date().toISOString(),
    } satisfies CachedSummary));
    logger.info({ chatJid, messageCount: messages.length }, 'Conversation summary cached');
  } catch (err) {
    logger.warn({ chatJid, err }, 'Failed to generate conversation summary');
  } finally {
    try { fs.unlinkSync(outputFile); } catch { /* ignore */ }
  }
}

function runCodexExec(prompt: string, outputFile: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn('codex', [
      'exec',
      '--skip-git-repo-check',
      '--ephemeral',
      '--dangerously-bypass-approvals-and-sandbox',
      '-m', SUMMARY_MODEL,
      '-o', outputFile,
      '-',
    ], {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: { ...process.env },
      timeout: 30000,  // 30s hard timeout
    });

    let stderr = '';
    child.stderr?.on('data', (data: Buffer) => { stderr += data.toString(); });

    child.on('error', (err) => reject(new Error(`Failed to spawn codex: ${err.message}`)));
    child.on('close', (code) => {
      if (code !== 0) {
        reject(new Error(`codex exec exited ${code}: ${stderr.trim()}`));
        return;
      }
      try {
        resolve(fs.readFileSync(outputFile, 'utf-8').trim());
      } catch {
        reject(new Error(`No output from codex. stderr: ${stderr.trim()}`));
      }
    });

    child.stdin?.write(prompt);
    child.stdin?.end();
  });
}
