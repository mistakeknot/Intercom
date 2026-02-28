import { Bot } from 'grammy';

import { ASSISTANT_NAME, TRIGGER_PATTERN } from '../config.js';
import {
  editTelegramViaIntercomd,
  routeTelegramCallback,
  routeTelegramIngress,
  sendTelegramViaIntercomd,
} from '../intercomd-client.js';
import { logger } from '../logger.js';
import {
  Channel,
  OnChatMetadata,
  OnCommand,
  OnInboundMessage,
  RegisteredGroup,
} from '../types.js';

export interface TelegramChannelOpts {
  onMessage: OnInboundMessage;
  onChatMetadata: OnChatMetadata;
  onCommand?: OnCommand;
  registeredGroups: () => Record<string, RegisteredGroup>;
}

export class TelegramChannel implements Channel {
  name = 'telegram';

  private bot: Bot | null = null;
  private opts: TelegramChannelOpts;
  private botToken: string;

  constructor(botToken: string, opts: TelegramChannelOpts) {
    this.botToken = botToken;
    this.opts = opts;
  }

  async connect(): Promise<void> {
    this.bot = new Bot(this.botToken);

    // Command to get chat ID (useful for registration)
    this.bot.command('chatid', (ctx) => {
      const chatId = ctx.chat.id;
      const chatType = ctx.chat.type;
      const chatName =
        chatType === 'private'
          ? ctx.from?.first_name || 'Private'
          : (ctx.chat as any).title || 'Unknown';

      ctx.reply(
        `Chat ID: \`tg:${chatId}\`\nName: ${chatName}\nType: ${chatType}`,
        { parse_mode: 'Markdown' },
      );
    });

    // Command to check bot status
    this.bot.command('ping', (ctx) => {
      ctx.reply(`${ASSISTANT_NAME} is online.`);
    });

    // Slash commands delegated to the orchestrator
    if (this.opts.onCommand) {
      const onCommand = this.opts.onCommand;

      for (const cmd of ['help', 'model', 'reset', 'status']) {
        this.bot.command(cmd, async (ctx) => {
          const chatJid = `tg:${ctx.chat.id}`;
          const args = (ctx.match as string) || '';
          try {
            const result = await onCommand(chatJid, cmd, args.trim());
            await ctx.reply(result.text, {
              parse_mode: result.parseMode || 'Markdown',
            });
          } catch (err) {
            logger.error({ cmd, chatJid, err }, 'Command handler error');
            await ctx.reply('Command failed. Check logs for details.');
          }
        });
      }
    }

    this.bot.on('message:text', async (ctx) => {
      // Skip commands
      if (ctx.message.text.startsWith('/')) return;

      const chatJid = `tg:${ctx.chat.id}`;
      let content = ctx.message.text;
      const timestamp = new Date(ctx.message.date * 1000).toISOString();
      const senderName =
        ctx.from?.first_name ||
        ctx.from?.username ||
        ctx.from?.id.toString() ||
        'Unknown';
      const sender = ctx.from?.id.toString() || '';
      const msgId = ctx.message.message_id.toString();
      const chatType = ctx.chat.type;

      // Determine chat name
      const chatName =
        chatType === 'private'
          ? senderName
          : (ctx.chat as any).title || chatJid;

      // Translate Telegram @bot_username mentions into TRIGGER_PATTERN format.
      // Telegram @mentions (e.g., @andy_ai_bot) won't match TRIGGER_PATTERN
      // (e.g., ^@Andy\b), so we prepend the trigger when the bot is @mentioned.
      const botUsername = ctx.me?.username?.toLowerCase();
      if (botUsername) {
        const entities = ctx.message.entities || [];
        const isBotMentioned = entities.some((entity) => {
          if (entity.type === 'mention') {
            const mentionText = content
              .substring(entity.offset, entity.offset + entity.length)
              .toLowerCase();
            return mentionText === `@${botUsername}`;
          }
          return false;
        });
        if (isBotMentioned && !TRIGGER_PATTERN.test(content)) {
          content = `@${ASSISTANT_NAME} ${content}`;
        }
      }

      let routedByRust = false;
      const routed = await routeTelegramIngress({
        chat_jid: chatJid,
        chat_name: chatName,
        chat_type: chatType,
        message_id: msgId,
        sender_id: sender,
        sender_name: senderName,
        content,
        timestamp,
        persist: false,
      });

      if (routed) {
        routedByRust = true;
        if (!routed.accepted) {
          logger.debug(
            { chatJid, reason: routed.reason ?? 'rejected' },
            'intercomd rejected Telegram ingress',
          );
          return;
        }
        if (routed.normalized_content) {
          content = routed.normalized_content;
        }
        if (routed.parity.runtime_fallback_used) {
          logger.warn(
            {
              chatJid,
              runtime: routed.runtime,
              model: routed.model,
            },
            'intercomd runtime profile fallback used for Telegram ingress',
          );
        }
      }

      // Store chat metadata for discovery
      this.opts.onChatMetadata(
        chatJid,
        timestamp,
        chatName,
        'telegram',
        chatType !== 'private',
      );

      // Only deliver full message for registered groups
      const group = this.opts.registeredGroups()[chatJid];
      if (!group) {
        if (routedByRust) {
          logger.warn(
            { chatJid },
            'Telegram ingress accepted by intercomd but group is missing in Node cache',
          );
        }
        logger.debug(
          { chatJid, chatName },
          'Message from unregistered Telegram chat',
        );
        return;
      }

      // Deliver message — startMessageLoop() will pick it up
      this.opts.onMessage(chatJid, {
        id: msgId,
        chat_jid: chatJid,
        sender,
        sender_name: senderName,
        content,
        timestamp,
        is_from_me: false,
      });

      logger.info(
        { chatJid, chatName, sender: senderName },
        'Telegram message stored',
      );
    });

    // Handle non-text messages with placeholders so the agent knows something was sent
    const storeNonText = async (ctx: any, placeholder: string) => {
      const chatJid = `tg:${ctx.chat.id}`;
      const group = this.opts.registeredGroups()[chatJid];
      if (!group) return;

      const timestamp = new Date(ctx.message.date * 1000).toISOString();
      const senderName =
        ctx.from?.first_name ||
        ctx.from?.username ||
        ctx.from?.id?.toString() ||
        'Unknown';
      const caption = ctx.message.caption ? ` ${ctx.message.caption}` : '';
      const content = `${placeholder}${caption}`;

      const routed = await routeTelegramIngress({
        chat_jid: chatJid,
        chat_name:
          ctx.chat.type === 'private'
            ? senderName
            : (ctx.chat as any).title || chatJid,
        chat_type: ctx.chat.type,
        message_id: ctx.message.message_id.toString(),
        sender_id: ctx.from?.id?.toString() || '',
        sender_name: senderName,
        content,
        timestamp,
        persist: false,
      });
      if (routed && !routed.accepted) return;

      this.opts.onChatMetadata(
        chatJid,
        timestamp,
        undefined,
        'telegram',
        ctx.chat.type !== 'private',
      );
      this.opts.onMessage(chatJid, {
        id: ctx.message.message_id.toString(),
        chat_jid: chatJid,
        sender: ctx.from?.id?.toString() || '',
        sender_name: senderName,
        content,
        timestamp,
        is_from_me: false,
      });
    };

    this.bot.on('message:photo', async (ctx) => await storeNonText(ctx, '[Photo]'));
    this.bot.on('message:video', async (ctx) => await storeNonText(ctx, '[Video]'));
    this.bot.on('message:voice', async (ctx) =>
      await storeNonText(ctx, '[Voice message]'),
    );
    this.bot.on('message:audio', async (ctx) => await storeNonText(ctx, '[Audio]'));
    this.bot.on('message:document', async (ctx) => {
      const name = ctx.message.document?.file_name || 'file';
      await storeNonText(ctx, `[Document: ${name}]`);
    });
    this.bot.on('message:sticker', async (ctx) => {
      const emoji = ctx.message.sticker?.emoji || '';
      await storeNonText(ctx, `[Sticker ${emoji}]`);
    });
    this.bot.on('message:location', async (ctx) => await storeNonText(ctx, '[Location]'));
    this.bot.on('message:contact', async (ctx) => await storeNonText(ctx, '[Contact]'));

    // Handle inline keyboard callback queries (gate approvals, budget actions)
    this.bot.on('callback_query:data', async (ctx) => {
      const chatJid = `tg:${ctx.chat?.id ?? ctx.callbackQuery.message?.chat.id}`;
      const messageId = ctx.callbackQuery.message?.message_id?.toString() ?? '';
      const data = ctx.callbackQuery.data;

      const result = await routeTelegramCallback({
        callback_query_id: ctx.callbackQuery.id,
        chat_jid: chatJid,
        message_id: messageId,
        sender_id: ctx.from?.id?.toString(),
        sender_name: ctx.from?.first_name || ctx.from?.username,
        data,
      });

      if (!result) {
        // intercomd unavailable — answer the callback to dismiss the spinner
        await ctx.answerCallbackQuery({ text: 'Service unavailable, try again later.' });
        return;
      }

      if (!result.ok) {
        logger.warn(
          { chatJid, data, error: result.error },
          'Callback query failed in intercomd',
        );
        await ctx.answerCallbackQuery({ text: result.error || 'Action failed' });
        return;
      }

      logger.info(
        { chatJid, action: result.action, target: result.target_id },
        'Callback query handled by intercomd',
      );
      // intercomd already answered the callback query and edited the message
    });

    // Handle errors gracefully
    this.bot.catch((err) => {
      logger.error({ err: err.message }, 'Telegram bot error');
    });

    // Register command menu in Telegram autocomplete
    await this.bot.api.setMyCommands([
      { command: 'help', description: 'Show available commands' },
      { command: 'model', description: 'Show or switch runtime (claude/gemini/codex)' },
      { command: 'reset', description: 'Clear session and stop container' },
      { command: 'status', description: 'Show runtime, session, and container status' },
      { command: 'ping', description: 'Check if bot is online' },
      { command: 'chatid', description: "Show this chat's registration ID" },
    ]);

    // Start polling — returns a Promise that resolves when started
    return new Promise<void>((resolve) => {
      this.bot!.start({
        onStart: (botInfo) => {
          logger.info(
            { username: botInfo.username, id: botInfo.id },
            'Telegram bot connected',
          );
          console.log(`\n  Telegram bot: @${botInfo.username}`);
          console.log(
            `  Send /chatid to the bot to get a chat's registration ID\n`,
          );
          resolve();
        },
      });
    });
  }

  async sendMessage(jid: string, text: string): Promise<void> {
    const routed = await sendTelegramViaIntercomd({ jid, text });
    if (routed?.ok) {
      logger.info(
        { jid, length: text.length, chunks: routed.chunks_sent },
        'Telegram message sent via intercomd',
      );
      return;
    }
    if (routed && !routed.ok) {
      logger.warn(
        { jid, error: routed.error },
        'intercomd send failed, falling back to Node Telegram client',
      );
    }

    if (!this.bot) {
      logger.warn('Telegram bot not initialized');
      return;
    }

    try {
      const numericId = jid.replace(/^tg:/, '');

      // Telegram has a 4096 character limit per message — split if needed
      const MAX_LENGTH = 4096;
      if (text.length <= MAX_LENGTH) {
        await this.bot.api.sendMessage(numericId, text);
      } else {
        for (let i = 0; i < text.length; i += MAX_LENGTH) {
          await this.bot.api.sendMessage(
            numericId,
            text.slice(i, i + MAX_LENGTH),
          );
        }
      }
      logger.info({ jid, length: text.length }, 'Telegram message sent');
    } catch (err) {
      logger.error({ jid, err }, 'Failed to send Telegram message');
    }
  }

  isConnected(): boolean {
    return this.bot !== null;
  }

  ownsJid(jid: string): boolean {
    return jid.startsWith('tg:');
  }

  async disconnect(): Promise<void> {
    if (this.bot) {
      this.bot.stop();
      this.bot = null;
      logger.info('Telegram bot stopped');
    }
  }

  async setTyping(jid: string, isTyping: boolean): Promise<void> {
    if (!this.bot || !isTyping) return;
    try {
      const numericId = jid.replace(/^tg:/, '');
      await this.bot.api.sendChatAction(numericId, 'typing');
    } catch (err) {
      logger.debug({ jid, err }, 'Failed to send Telegram typing indicator');
    }
  }

  async sendMessageWithId(jid: string, text: string): Promise<string | null> {
    const routed = await sendTelegramViaIntercomd({ jid, text });
    if (routed?.ok) {
      return routed.message_ids[0] || null;
    }
    if (routed && !routed.ok) {
      logger.warn(
        { jid, error: routed.error },
        'intercomd send-with-id failed, falling back to Node Telegram client',
      );
    }

    if (!this.bot) return null;
    try {
      const numericId = jid.replace(/^tg:/, '');
      const truncated = text.length > 4096 ? text.slice(0, 4096) : text;
      const msg = await this.bot.api.sendMessage(numericId, truncated);
      return msg.message_id.toString();
    } catch (err) {
      logger.error({ jid, err }, 'Failed to send Telegram message with ID');
      return null;
    }
  }

  async editMessage(jid: string, messageId: string, text: string): Promise<boolean> {
    const routed = await editTelegramViaIntercomd({
      jid,
      message_id: messageId,
      text,
    });
    if (routed?.ok) {
      return true;
    }
    if (routed && !routed.ok) {
      logger.warn(
        { jid, messageId, error: routed.error },
        'intercomd edit failed, falling back to Node Telegram client',
      );
    }

    if (!this.bot) return false;
    try {
      const numericId = jid.replace(/^tg:/, '');
      const truncated = text.length > 4096 ? text.slice(0, 4096) : text;
      await this.bot.api.editMessageText(numericId, parseInt(messageId, 10), truncated);
      return true;
    } catch (err: any) {
      // Telegram returns 400 "message is not modified" if content is identical — silence it
      if (err?.error_code === 400 && err?.description?.includes('not modified')) {
        return true;
      }
      logger.error({ jid, messageId, err }, 'Failed to edit Telegram message');
      return false;
    }
  }
}
