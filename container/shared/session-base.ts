/**
 * Shared session archival helpers for NanoClaw container agents.
 */

import fs from 'fs';
import path from 'path';
import { log } from './protocol.js';

export function sanitizeFilename(summary: string): string {
  return summary
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 50);
}

export function generateFallbackName(): string {
  const time = new Date();
  return `conversation-${time.getHours().toString().padStart(2, '0')}${time.getMinutes().toString().padStart(2, '0')}`;
}

export interface ParsedMessage {
  role: 'user' | 'assistant';
  content: string;
}

export function formatTranscriptMarkdown(messages: ParsedMessage[], title?: string | null, assistantName = 'Andy'): string {
  const now = new Date();
  const formatDateTime = (d: Date) => d.toLocaleString('en-US', {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
    hour12: true,
  });

  const lines: string[] = [];
  lines.push(`# ${title || 'Conversation'}`);
  lines.push('');
  lines.push(`Archived: ${formatDateTime(now)}`);
  lines.push('');
  lines.push('---');
  lines.push('');

  for (const msg of messages) {
    const sender = msg.role === 'user' ? 'User' : assistantName;
    const content = msg.content.length > 2000
      ? msg.content.slice(0, 2000) + '...'
      : msg.content;
    lines.push(`**${sender}**: ${content}`);
    lines.push('');
  }

  return lines.join('\n');
}

/**
 * Save conversation history to a markdown file in conversations/.
 */
export function archiveConversation(messages: ParsedMessage[], title?: string, assistantName?: string): void {
  if (messages.length === 0) return;

  const name = title ? sanitizeFilename(title) : generateFallbackName();
  const conversationsDir = '/workspace/group/conversations';
  fs.mkdirSync(conversationsDir, { recursive: true });

  const date = new Date().toISOString().split('T')[0];
  const filename = `${date}-${name}.md`;
  const filePath = path.join(conversationsDir, filename);

  const markdown = formatTranscriptMarkdown(messages, title, assistantName);
  fs.writeFileSync(filePath, markdown);
  log(`Archived conversation to ${filePath}`);
}
