import { Channel, NewMessage } from './types.js';

export function escapeXml(s: string): string {
  if (!s) return '';
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

export function formatMessages(messages: NewMessage[]): string {
  const lines = messages.map((m) =>
    `<message sender="${escapeXml(m.sender_name)}" time="${m.timestamp}">${escapeXml(m.content)}</message>`,
  );
  return `<messages>\n${lines.join('\n')}\n</messages>`;
}

export function stripInternalTags(text: string): string {
  return text.replace(/<internal>[\s\S]*?<\/internal>/g, '').trim();
}

export function formatOutbound(rawText: string): string {
  const text = stripInternalTags(rawText);
  if (!text) return '';
  return text;
}

export function routeOutbound(
  channels: Channel[],
  jid: string,
  text: string,
): Promise<void> {
  const channel = channels.find((c) => c.ownsJid(jid) && c.isConnected());
  if (!channel) throw new Error(`No channel for JID: ${jid}`);
  return channel.sendMessage(jid, text);
}

/**
 * Format prior conversation messages as an XML preamble for model-switch context injection.
 * Returns empty string if no messages.
 */
export function formatConversationHistory(
  messages: { sender_name: string; content: string; timestamp: string; is_bot_message: boolean }[],
  previousModel: string,
  assistantName: string,
): string {
  if (messages.length === 0) return '';
  const lines = messages.map((m) => {
    const role = m.is_bot_message ? assistantName : m.sender_name;
    return `  <message role="${escapeXml(role)}" time="${m.timestamp}">${escapeXml(m.content)}</message>`;
  });
  return [
    `<conversation_history note="Prior conversation with ${escapeXml(previousModel)}. Continue naturally.">`,
    ...lines,
    '</conversation_history>',
  ].join('\n');
}

export function findChannel(
  channels: Channel[],
  jid: string,
): Channel | undefined {
  return channels.find((c) => c.ownsJid(jid));
}
