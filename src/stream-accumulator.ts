/**
 * StreamAccumulator — manages a single Telegram message that updates in real-time
 * as the agent works. Tool calls appear as they happen, then the final response
 * replaces/follows the tool log.
 */
import { Channel } from './types.js';
import { stripInternalTags } from './router.js';
import { logger } from './logger.js';

const DEBOUNCE_MS = 500;
const MAX_TOOL_LINES = 20;
const MAX_MESSAGE_LENGTH = 4096;

/**
 * Format tool input into a compact, human-readable summary.
 */
function formatToolInput(toolName: string, rawInput: string): string {
  try {
    const input = JSON.parse(rawInput);
    switch (toolName) {
      case 'Bash':
        return input.command ? `\`${input.command}\`` : rawInput;
      case 'Read':
      case 'Write':
      case 'Edit':
        return input.file_path || rawInput;
      case 'Grep':
      case 'Glob':
        return input.pattern || rawInput;
      case 'WebSearch':
        return input.query || rawInput;
      case 'WebFetch':
        return input.url || rawInput;
      case 'Task':
        return input.description || rawInput;
      default:
        // Truncated raw for unknown tools
        return rawInput.length > 80 ? rawInput.slice(0, 80) + '...' : rawInput;
    }
  } catch {
    return rawInput.length > 80 ? rawInput.slice(0, 80) + '...' : rawInput;
  }
}

export class StreamAccumulator {
  private channel: Channel;
  private chatJid: string;
  private messageId: string | null = null;
  private toolLines: string[] = [];
  private textParts: string[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private flushChain = Promise.resolve();

  /** True if the channel supports message editing (send + edit). */
  readonly supportsStreaming: boolean;

  constructor(channel: Channel, chatJid: string) {
    this.channel = channel;
    this.chatJid = chatJid;
    this.supportsStreaming = !!(channel.sendMessageWithId && channel.editMessage);
  }

  addToolStart(name: string, input: string): void {
    const summary = formatToolInput(name, input);
    const line = `>> ${name}: ${summary}`;

    if (this.toolLines.length < MAX_TOOL_LINES) {
      this.toolLines.push(line);
    } else if (this.toolLines.length === MAX_TOOL_LINES) {
      this.toolLines.push('... (more tools)');
    }
    // Beyond MAX_TOOL_LINES + 1, silently drop

    this.scheduleFlush();
  }

  addTextDelta(text: string): void {
    this.textParts.push(text);
    this.scheduleFlush();
  }

  /**
   * Finalize the streaming message with the clean result text.
   * If we have an active message, edits it. Otherwise falls back to sendMessage.
   */
  async finalize(rawResult: string): Promise<void> {
    // Cancel any pending flush — we're doing the final one now
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }

    const cleanText = stripInternalTags(rawResult);
    if (!cleanText) return;

    // Wait for any in-flight flush to complete
    await this.flushChain;

    // Build final message: tool log (collapsed) + result text
    const toolLog = this.toolLines.length > 0
      ? this.toolLines.join('\n') + '\n\n'
      : '';
    const finalContent = toolLog + cleanText;

    // If final content fits in one message and we have an existing message, edit it
    if (this.messageId && finalContent.length <= MAX_MESSAGE_LENGTH) {
      const ok = await this.channel.editMessage!(this.chatJid, this.messageId, finalContent);
      if (ok) return;
    }

    // Fallback: send as a new message via the standard sendMessage (which handles splitting)
    await this.channel.sendMessage(this.chatJid, cleanText);
  }

  /**
   * Clean up timers. Call when the agent invocation is done.
   */
  dispose(): void {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
  }

  private scheduleFlush(): void {
    if (this.flushTimer) return; // Already scheduled
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      this.flushChain = this.flushChain.then(() => this.flush());
    }, DEBOUNCE_MS);
  }

  private async flush(): Promise<void> {
    const content = this.buildContent();
    if (!content) return;

    if (!this.messageId) {
      // First flush — create the message
      this.messageId = await this.channel.sendMessageWithId!(this.chatJid, content);
      if (!this.messageId) {
        logger.warn({ jid: this.chatJid }, 'Failed to create streaming message');
      }
    } else {
      // Subsequent flush — edit the existing message
      await this.channel.editMessage!(this.chatJid, this.messageId, content);
    }
  }

  private buildContent(): string {
    const parts: string[] = [];

    if (this.toolLines.length > 0) {
      parts.push(this.toolLines.join('\n'));
    }

    const text = this.textParts.join('');
    if (text) {
      parts.push(text);
    }

    const content = parts.join('\n\n');
    // Truncate to Telegram's limit
    return content.length > MAX_MESSAGE_LENGTH
      ? content.slice(0, MAX_MESSAGE_LENGTH)
      : content;
  }
}
