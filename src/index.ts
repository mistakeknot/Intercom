import fs from 'fs';
import path from 'path';

import {
  ASSISTANT_NAME,
  DATA_DIR,
  DEFAULT_MODEL,
  DEFAULT_RUNTIME,
  findModel,
  HOST_CALLBACK_PORT,
  IDLE_TIMEOUT,
  MAIN_GROUP_FOLDER,
  MODEL_CATALOG,
  POLL_INTERVAL,
  Runtime,
  runtimeForModel,
  TELEGRAM_BOT_TOKEN,
  TELEGRAM_ONLY,
  TRIGGER_PATTERN,
} from './config.js';
import { WhatsAppChannel } from './channels/whatsapp.js';
import { TelegramChannel } from './channels/telegram.js';
import {
  ContainerOutput,
  runContainerAgent,
  writeGroupsSnapshot,
  writeTasksSnapshot,
} from './container-runner.js';
import { cleanupOrphans, ensureContainerRuntimeRunning } from './container-runtime.js';
import {
  deleteSession,
  getAllChats,
  getAllRegisteredGroups,
  getAllSessions,
  getAllTasks,
  getMessagesSince,
  getNewMessages,
  getRecentConversation,
  getRouterState,
  initDatabase,
  setRegisteredGroup,
  setRouterState,
  setSession,
  storeChatMetadata,
  storeMessage,
  storeMessageDirect,
} from './db.js';
import { GroupQueue } from './group-queue.js';
import { resolveGroupFolderPath } from './group-folder.js';
import { startHostCallbackServer } from './host-callback.js';
import { processTaskIpc, startIpcWatcher } from './ipc.js';
import { findChannel, formatConversationHistory, formatMessages, formatOutbound } from './router.js';
import { StreamAccumulator } from './stream-accumulator.js';
import { startSchedulerLoop } from './task-scheduler.js';
import { Channel, CommandResult, NewMessage, RegisteredGroup } from './types.js';
import { logger } from './logger.js';
import { generateSummary, getCachedSummary, clearCachedSummary } from './summarizer.js';

// Re-export for backwards compatibility during refactor
export { escapeXml, formatMessages } from './router.js';

let lastTimestamp = '';
let sessions: Record<string, string> = {};
let registeredGroups: Record<string, RegisteredGroup> = {};
let lastAgentTimestamp: Record<string, string> = {};
let reportedModels: Record<string, string> = {}; // groupFolder → model name from container
let pendingModelSwitch: Record<string, string> = {}; // chatJid → previous model display name
let messageLoopRunning = false;

let whatsapp: WhatsAppChannel;
const channels: Channel[] = [];
const queue = new GroupQueue();

function loadState(): void {
  lastTimestamp = getRouterState('last_timestamp') || '';
  const agentTs = getRouterState('last_agent_timestamp');
  try {
    lastAgentTimestamp = agentTs ? JSON.parse(agentTs) : {};
  } catch {
    logger.warn('Corrupted last_agent_timestamp in DB, resetting');
    lastAgentTimestamp = {};
  }
  sessions = getAllSessions();
  registeredGroups = getAllRegisteredGroups();
  logger.info(
    { groupCount: Object.keys(registeredGroups).length },
    'State loaded',
  );
}

function saveState(): void {
  setRouterState('last_timestamp', lastTimestamp);
  setRouterState(
    'last_agent_timestamp',
    JSON.stringify(lastAgentTimestamp),
  );
}

function registerGroup(jid: string, group: RegisteredGroup): void {
  let groupDir: string;
  try {
    groupDir = resolveGroupFolderPath(group.folder);
  } catch (err) {
    logger.warn(
      { jid, folder: group.folder, err },
      'Rejecting group registration with invalid folder',
    );
    return;
  }

  registeredGroups[jid] = group;
  setRegisteredGroup(jid, group);

  // Create group folder
  fs.mkdirSync(path.join(groupDir, 'logs'), { recursive: true });

  logger.info(
    { jid, name: group.name, folder: group.folder },
    'Group registered',
  );
}

/**
 * Get available groups list for the agent.
 * Returns groups ordered by most recent activity.
 */
export function getAvailableGroups(): import('./container-runner.js').AvailableGroup[] {
  const chats = getAllChats();
  const registeredJids = new Set(Object.keys(registeredGroups));

  return chats
    .filter((c) => c.jid !== '__group_sync__' && c.is_group)
    .map((c) => ({
      jid: c.jid,
      name: c.name,
      lastActivity: c.last_message_time,
      isRegistered: registeredJids.has(c.jid),
    }));
}

/** @internal - exported for testing */
export function _setRegisteredGroups(groups: Record<string, RegisteredGroup>): void {
  registeredGroups = groups;
}

/**
 * Process all pending messages for a group.
 * Called by the GroupQueue when it's this group's turn.
 */
async function processGroupMessages(chatJid: string): Promise<boolean> {
  const group = registeredGroups[chatJid];
  if (!group) return true;

  const channel = findChannel(channels, chatJid);
  if (!channel) {
    console.log(`Warning: no channel owns JID ${chatJid}, skipping messages`);
    return true;
  }

  const isMainGroup = group.folder === MAIN_GROUP_FOLDER;

  const sinceTimestamp = lastAgentTimestamp[chatJid] || '';
  const missedMessages = getMessagesSince(chatJid, sinceTimestamp, ASSISTANT_NAME);

  if (missedMessages.length === 0) return true;

  // For non-main groups, check if trigger is required and present
  if (!isMainGroup && group.requiresTrigger !== false) {
    const hasTrigger = missedMessages.some((m) =>
      TRIGGER_PATTERN.test(m.content.trim()),
    );
    if (!hasTrigger) return true;
  }

  const prompt = formatMessages(missedMessages);

  // Model switch context carryover: inject summary + recent messages (or fallback to raw history)
  let finalPrompt = prompt;
  if (pendingModelSwitch[chatJid] !== undefined) {
    const prevModel = pendingModelSwitch[chatJid];
    delete pendingModelSwitch[chatJid];

    const cached = getCachedSummary(chatJid);
    if (cached?.summary) {
      // Summary available — use summary + last 5 raw messages for recency
      const recentRaw = getRecentConversation(chatJid, 5);
      const rawBlock = formatConversationHistory(recentRaw, prevModel, ASSISTANT_NAME);
      finalPrompt = [
        `<conversation_summary note="Prior conversation with ${prevModel}, summarized.">`,
        cached.summary,
        '</conversation_summary>',
        '',
        rawBlock,
        '',
        prompt,
      ].filter(Boolean).join('\n');
      logger.info({ group: group.name, summaryLen: cached.summary.length, recentMessages: recentRaw.length }, 'Injecting summary + recent history after model switch');
    } else {
      // Fallback — summary not ready, use raw messages (phase 1 behavior)
      const history = getRecentConversation(chatJid, 20);
      const historyBlock = formatConversationHistory(history, prevModel, ASSISTANT_NAME);
      if (historyBlock) {
        finalPrompt = historyBlock + '\n\n' + prompt;
        logger.info({ group: group.name, historyMessages: history.length }, 'Injecting raw history (summary not available)');
      }
    }

    clearCachedSummary(chatJid);
  }

  // Advance cursor so the piping path in startMessageLoop won't re-fetch
  // these messages. Save the old cursor so we can roll back on error.
  const previousCursor = lastAgentTimestamp[chatJid] || '';
  lastAgentTimestamp[chatJid] =
    missedMessages[missedMessages.length - 1].timestamp;
  saveState();

  logger.info(
    { group: group.name, messageCount: missedMessages.length },
    'Processing messages',
  );

  // Track idle timer for closing stdin when agent is idle
  let idleTimer: ReturnType<typeof setTimeout> | null = null;

  const resetIdleTimer = () => {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = setTimeout(() => {
      logger.debug({ group: group.name }, 'Idle timeout, closing container stdin');
      queue.closeStdin(chatJid);
    }, IDLE_TIMEOUT);
  };

  await channel.setTyping?.(chatJid, true);
  let hadError = false;
  let outputSentToUser = false;

  const accumulator = new StreamAccumulator(channel, chatJid);
  const useStreaming = accumulator.supportsStreaming;

  const output = await runAgent(group, finalPrompt, chatJid, async (result) => {
    // Route streaming events to accumulator
    if (result.event && useStreaming) {
      if (result.event.type === 'tool_start') {
        accumulator.addToolStart(result.event.toolName || 'Unknown', result.event.toolInput || '');
      } else if (result.event.type === 'text_delta' && result.event.text) {
        accumulator.addTextDelta(result.event.text);
      }
      resetIdleTimer();
      return;
    }

    // Final result — finalize accumulator or send directly
    if (result.result) {
      const raw = typeof result.result === 'string' ? result.result : JSON.stringify(result.result);
      // Strip <internal>...</internal> blocks — agent uses these for internal reasoning
      const text = raw.replace(/<internal>[\s\S]*?<\/internal>/g, '').trim();
      logger.info({ group: group.name }, `Agent output: ${raw.slice(0, 200)}`);
      if (text) {
        if (useStreaming) {
          await accumulator.finalize(raw);
        } else {
          await channel.sendMessage(chatJid, text);
        }
        // Store bot response so conversation history survives model switches
        storeMessageDirect({
          id: `bot-${Date.now()}`,
          chat_jid: chatJid,
          sender: 'bot',
          sender_name: ASSISTANT_NAME,
          content: text,
          timestamp: new Date().toISOString(),
          is_from_me: true,
          is_bot_message: true,
        });
        outputSentToUser = true;
      }
      // Only reset idle timer on actual results, not session-update markers (result: null)
      resetIdleTimer();
    }

    if (result.status === 'success') {
      queue.notifyIdle(chatJid);
    }

    if (result.status === 'error') {
      hadError = true;
    }
  });

  accumulator.dispose();
  await channel.setTyping?.(chatJid, false);
  if (idleTimer) clearTimeout(idleTimer);

  if (output === 'error' || hadError) {
    // If we already sent output to the user, don't roll back the cursor —
    // the user got their response and re-processing would send duplicates.
    if (outputSentToUser) {
      logger.warn({ group: group.name }, 'Agent error after output was sent, skipping cursor rollback to prevent duplicates');
      return true;
    }
    // Roll back cursor so retries can re-process these messages
    lastAgentTimestamp[chatJid] = previousCursor;
    saveState();
    logger.warn({ group: group.name }, 'Agent error, rolled back message cursor for retry');
    return false;
  }

  return true;
}

async function runAgent(
  group: RegisteredGroup,
  prompt: string,
  chatJid: string,
  onOutput?: (output: ContainerOutput) => Promise<void>,
): Promise<'success' | 'error'> {
  const isMain = group.folder === MAIN_GROUP_FOLDER;
  const sessionId = sessions[group.folder];

  // Update tasks snapshot for container to read (filtered by group)
  const tasks = getAllTasks();
  writeTasksSnapshot(
    group.folder,
    isMain,
    tasks.map((t) => ({
      id: t.id,
      groupFolder: t.group_folder,
      prompt: t.prompt,
      schedule_type: t.schedule_type,
      schedule_value: t.schedule_value,
      status: t.status,
      next_run: t.next_run,
    })),
  );

  // Update available groups snapshot (main group only can see all groups)
  const availableGroups = getAvailableGroups();
  writeGroupsSnapshot(
    group.folder,
    isMain,
    availableGroups,
    new Set(Object.keys(registeredGroups)),
  );

  // Wrap onOutput to track session ID and model from streamed results
  const wrappedOnOutput = onOutput
    ? async (output: ContainerOutput) => {
        if (output.newSessionId) {
          sessions[group.folder] = output.newSessionId;
          setSession(group.folder, output.newSessionId);
        }
        if (output.model) {
          reportedModels[group.folder] = output.model;
        }
        await onOutput(output);
      }
    : undefined;

  try {
    const output = await runContainerAgent(
      group,
      {
        prompt,
        sessionId,
        groupFolder: group.folder,
        chatJid,
        isMain,
        assistantName: ASSISTANT_NAME,
      },
      (proc, containerName) => queue.registerProcess(chatJid, proc, containerName, group.folder),
      wrappedOnOutput,
    );

    if (output.newSessionId) {
      sessions[group.folder] = output.newSessionId;
      setSession(group.folder, output.newSessionId);
    }

    if (output.status === 'error') {
      logger.error(
        { group: group.name, error: output.error },
        'Container agent error',
      );
      return 'error';
    }

    return 'success';
  } catch (err) {
    logger.error({ group: group.name, err }, 'Agent error');
    return 'error';
  }
}

async function startMessageLoop(): Promise<void> {
  if (messageLoopRunning) {
    logger.debug('Message loop already running, skipping duplicate start');
    return;
  }
  messageLoopRunning = true;

  logger.info(`Intercom running (trigger: @${ASSISTANT_NAME})`);

  while (true) {
    try {
      const jids = Object.keys(registeredGroups);
      const { messages, newTimestamp } = getNewMessages(jids, lastTimestamp, ASSISTANT_NAME);

      if (messages.length > 0) {
        logger.info({ count: messages.length }, 'New messages');

        // Advance the "seen" cursor for all messages immediately
        lastTimestamp = newTimestamp;
        saveState();

        // Deduplicate by group
        const messagesByGroup = new Map<string, NewMessage[]>();
        for (const msg of messages) {
          const existing = messagesByGroup.get(msg.chat_jid);
          if (existing) {
            existing.push(msg);
          } else {
            messagesByGroup.set(msg.chat_jid, [msg]);
          }
        }

        for (const [chatJid, groupMessages] of messagesByGroup) {
          const group = registeredGroups[chatJid];
          if (!group) continue;

          const channel = findChannel(channels, chatJid);
          if (!channel) {
            console.log(`Warning: no channel owns JID ${chatJid}, skipping messages`);
            continue;
          }

          const isMainGroup = group.folder === MAIN_GROUP_FOLDER;
          const needsTrigger = !isMainGroup && group.requiresTrigger !== false;

          // For non-main groups, only act on trigger messages.
          // Non-trigger messages accumulate in DB and get pulled as
          // context when a trigger eventually arrives.
          if (needsTrigger) {
            const hasTrigger = groupMessages.some((m) =>
              TRIGGER_PATTERN.test(m.content.trim()),
            );
            if (!hasTrigger) continue;
          }

          // Pull all messages since lastAgentTimestamp so non-trigger
          // context that accumulated between triggers is included.
          const allPending = getMessagesSince(
            chatJid,
            lastAgentTimestamp[chatJid] || '',
            ASSISTANT_NAME,
          );
          const messagesToSend =
            allPending.length > 0 ? allPending : groupMessages;
          const formatted = formatMessages(messagesToSend);

          if (queue.sendMessage(chatJid, formatted)) {
            logger.debug(
              { chatJid, count: messagesToSend.length },
              'Piped messages to active container',
            );
            lastAgentTimestamp[chatJid] =
              messagesToSend[messagesToSend.length - 1].timestamp;
            saveState();
            // Show typing indicator while the container processes the piped message
            channel.setTyping?.(chatJid, true)?.catch((err) =>
              logger.warn({ chatJid, err }, 'Failed to set typing indicator'),
            );
          } else {
            // No active container — enqueue for a new one
            queue.enqueueMessageCheck(chatJid);
          }
        }
      }
    } catch (err) {
      logger.error({ err }, 'Error in message loop');
    }
    await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL));
  }
}

/**
 * Startup recovery: check for unprocessed messages in registered groups.
 * Handles crash between advancing lastTimestamp and processing messages.
 */
function recoverPendingMessages(): void {
  for (const [chatJid, group] of Object.entries(registeredGroups)) {
    const sinceTimestamp = lastAgentTimestamp[chatJid] || '';
    const pending = getMessagesSince(chatJid, sinceTimestamp, ASSISTANT_NAME);
    if (pending.length > 0) {
      logger.info(
        { group: group.name, pendingCount: pending.length },
        'Recovery: found unprocessed messages',
      );
      queue.enqueueMessageCheck(chatJid);
    }
  }
}

// --- Slash command handlers ---

const startedAt = Date.now();
function getModelName(groupFolder: string, modelId?: string): string {
  if (reportedModels[groupFolder]) return reportedModels[groupFolder];
  if (modelId) {
    const entry = findModel(modelId);
    if (entry) return entry.displayName;
  }
  return findModel(DEFAULT_MODEL)?.displayName || DEFAULT_MODEL;
}

function clearGroupSession(groupFolder: string): void {
  // Remove from SQLite
  deleteSession(groupFolder);
  // Remove from in-memory cache
  delete sessions[groupFolder];
  // Remove .sessions/*.json files (Agent SDK session state)
  const sessionsDir = path.join(DATA_DIR, '..', 'groups', groupFolder, '.sessions');
  try {
    const files = fs.readdirSync(sessionsDir);
    for (const f of files) {
      if (f.endsWith('.json')) {
        fs.unlinkSync(path.join(sessionsDir, f));
      }
    }
  } catch {
    // Directory may not exist — that's fine
  }
}

function handleHelp(): CommandResult {
  return {
    text: [
      `*${ASSISTANT_NAME} Commands*`,
      '',
      '/help — Show this command list',
      '/status — Show runtime, session, and container status',
      '/model — Show available models',
      '/model <#> — Switch model by number',
      '/model <name> — Switch model by name',
      '/reset — Clear session and stop running container',
      '/new — Start a fresh chat (alias for /reset)',
      '/ping — Check if bot is online',
      '/chatid — Show this chat\'s registration ID',
    ].join('\n'),
    parseMode: 'Markdown',
  };
}

function handleStatus(chatJid: string): CommandResult {
  const group = registeredGroups[chatJid];
  if (!group) {
    return { text: 'This chat is not registered.' };
  }

  const modelId = group.model || DEFAULT_MODEL;
  const sessionId = sessions[group.folder];
  const active = queue.isActive(chatJid);
  const uptimeMs = Date.now() - startedAt;
  const uptimeMin = Math.floor(uptimeMs / 60000);
  const uptimeHr = Math.floor(uptimeMin / 60);
  const uptime = uptimeHr > 0
    ? `${uptimeHr}h ${uptimeMin % 60}m`
    : `${uptimeMin}m`;

  const lines = [
    `*Status for ${group.name}*`,
    '',
    `Model: \`${getModelName(group.folder, modelId)}\``,
    `Session: ${sessionId ? `\`${sessionId.slice(0, 12)}...\`` : '_none_'}`,
    `Container: ${active ? 'active' : 'idle'}`,
    `Assistant: ${ASSISTANT_NAME}`,
    `Uptime: ${uptime}`,
  ];

  return { text: lines.join('\n'), parseMode: 'Markdown' };
}

function handleModel(chatJid: string, args: string): CommandResult {
  const group = registeredGroups[chatJid];
  if (!group) {
    return { text: 'This chat is not registered.' };
  }

  const currentModelId = group.model || DEFAULT_MODEL;

  // No args — show numbered catalog
  if (!args) {
    const currentDisplay = getModelName(group.folder, currentModelId);
    const catalogList = MODEL_CATALOG.map((m, i) => {
      const active = m.id === currentModelId;
      return ` ${i + 1}. \`${m.id}\` — ${m.displayName}${active ? ' (active)' : ''}`;
    }).join('\n');

    return {
      text: [
        `*Current model:* ${currentDisplay}`,
        '',
        catalogList,
        '',
        'Switch: `/model <name>` or `/model <#>`',
      ].join('\n'),
      parseMode: 'Markdown',
    };
  }

  // Resolve by number or name
  let newModel = findModel(args.toLowerCase());
  if (!newModel) {
    const num = parseInt(args, 10);
    if (num >= 1 && num <= MODEL_CATALOG.length) {
      newModel = MODEL_CATALOG[num - 1];
    }
  }
  // Try substring match
  if (!newModel) {
    const lower = args.toLowerCase();
    newModel = MODEL_CATALOG.find(m =>
      m.id.includes(lower) || m.displayName.toLowerCase().includes(lower),
    );
  }

  // Accept arbitrary model IDs — infer runtime from prefix pattern
  if (!newModel) {
    const runtime = runtimeForModel(args.toLowerCase());
    newModel = { id: args.toLowerCase(), runtime, displayName: args };
  }

  if (newModel.id === currentModelId) {
    return { text: `Already using \`${newModel.displayName}\`.`, parseMode: 'Markdown' };
  }

  const prevDisplay = getModelName(group.folder, currentModelId);

  // Kill running container, clear session, update model
  queue.killGroup(chatJid);
  clearGroupSession(group.folder);
  group.model = newModel.id;
  group.runtime = newModel.runtime;
  registeredGroups[chatJid] = group;
  setRegisteredGroup(chatJid, group);

  // Clear stale reported model name — next container run will report the new one
  delete reportedModels[group.folder];

  // Flag for conversation history injection on next message
  pendingModelSwitch[chatJid] = prevDisplay;

  // Fire-and-forget: pre-generate summary for richer context carryover
  const history = getRecentConversation(chatJid, 50);
  if (history.length > 0) {
    generateSummary(chatJid, history, prevDisplay, ASSISTANT_NAME).catch(() => {});
  }

  return {
    text: `Switched from ${prevDisplay} to *${newModel.displayName}*.\nConversation context will carry over.`,
    parseMode: 'Markdown',
  };
}

function handleReset(chatJid: string): CommandResult {
  const group = registeredGroups[chatJid];
  if (!group) {
    return { text: 'This chat is not registered.' };
  }

  const wasActive = queue.isActive(chatJid);
  queue.killGroup(chatJid);
  clearGroupSession(group.folder);
  delete pendingModelSwitch[chatJid];
  clearCachedSummary(chatJid);

  const parts = ['Session cleared.'];
  if (wasActive) parts.push('Running container stopped.');
  parts.push('Next message will start a fresh session.');

  return { text: parts.join(' ') };
}

async function handleCommand(
  chatJid: string,
  command: string,
  args: string,
): Promise<CommandResult> {
  switch (command) {
    case 'help':
      return handleHelp();
    case 'status':
      return handleStatus(chatJid);
    case 'model':
      return handleModel(chatJid, args);
    case 'reset':
    case 'new':
      return handleReset(chatJid);
    default:
      return { text: `Unknown command: /${command}` };
  }
}

function ensureContainerSystemRunning(): void {
  ensureContainerRuntimeRunning();
  cleanupOrphans();
}

async function main(): Promise<void> {
  ensureContainerSystemRunning();
  initDatabase();
  logger.info('Database initialized');
  loadState();

  // Graceful shutdown handlers
  const shutdown = async (signal: string) => {
    logger.info({ signal }, 'Shutdown signal received');
    await queue.shutdown(10000);
    for (const ch of channels) await ch.disconnect();
    process.exit(0);
  };
  process.on('SIGTERM', () => shutdown('SIGTERM'));
  process.on('SIGINT', () => shutdown('SIGINT'));

  // Channel callbacks (shared by all channels)
  const channelOpts = {
    onMessage: (_chatJid: string, msg: NewMessage) => storeMessage(msg),
    onChatMetadata: (chatJid: string, timestamp: string, name?: string, channel?: string, isGroup?: boolean) =>
      storeChatMetadata(chatJid, timestamp, name, channel, isGroup),
    onCommand: handleCommand,
    registeredGroups: () => registeredGroups,
  };

  // Create and connect channels
  if (!TELEGRAM_ONLY) {
    whatsapp = new WhatsAppChannel(channelOpts);
    channels.push(whatsapp);
    await whatsapp.connect();
  }

  if (TELEGRAM_BOT_TOKEN) {
    const telegram = new TelegramChannel(TELEGRAM_BOT_TOKEN, channelOpts);
    channels.push(telegram);
    await telegram.connect();
  }

  // Start subsystems (independently of connection handler)
  const rustOrchestrator = process.env.RUST_ORCHESTRATOR === 'true';
  if (rustOrchestrator) {
    logger.info('RUST_ORCHESTRATOR=true — Node scheduler/message loop disabled, Rust orchestrator handles dispatch');
  } else {
    startSchedulerLoop({
      registeredGroups: () => registeredGroups,
      getSessions: () => sessions,
      queue,
      onProcess: (groupJid, proc, containerName, groupFolder) => queue.registerProcess(groupJid, proc, containerName, groupFolder),
      sendMessage: async (jid, rawText) => {
        const channel = findChannel(channels, jid);
        if (!channel) {
          console.log(`Warning: no channel owns JID ${jid}, cannot send message`);
          return;
        }
        const text = formatOutbound(rawText);
        if (text) await channel.sendMessage(jid, text);
      },
    });
  }
  startIpcWatcher({
    sendMessage: (jid, text) => {
      const channel = findChannel(channels, jid);
      if (!channel) throw new Error(`No channel for JID: ${jid}`);
      return channel.sendMessage(jid, text);
    },
    registeredGroups: () => registeredGroups,
    registerGroup,
    syncGroupMetadata: (force) => whatsapp?.syncGroupMetadata(force) ?? Promise.resolve(),
    getAvailableGroups,
    writeGroupsSnapshot: (gf, im, ag, rj) => writeGroupsSnapshot(gf, im, ag, rj),
  });
  // Host callback server — intercomd calls back here for message sends + task forwarding
  startHostCallbackServer(HOST_CALLBACK_PORT, {
    sendMessage: async (jid, text) => {
      const channel = findChannel(channels, jid);
      if (!channel) throw new Error(`No channel for JID: ${jid}`);
      await channel.sendMessage(jid, text);
    },
    getRegisteredGroups: () => registeredGroups,
    forwardTask: async (task, groupFolder, isMain) => {
      await processTaskIpc(
        task as Parameters<typeof processTaskIpc>[0],
        groupFolder,
        isMain,
        {
          sendMessage: async (jid, rawText) => {
            const channel = findChannel(channels, jid);
            if (!channel) return;
            const text = formatOutbound(rawText);
            if (text) await channel.sendMessage(jid, text);
          },
          registeredGroups: () => registeredGroups,
          registerGroup,
          syncGroupMetadata: (force) => whatsapp?.syncGroupMetadata(force) ?? Promise.resolve(),
          getAvailableGroups,
          writeGroupsSnapshot: (gf, im, ag, rj) => writeGroupsSnapshot(gf, im, ag, rj),
        },
      );
    },
  });
  if (!rustOrchestrator) {
    queue.setProcessMessagesFn(processGroupMessages);
    recoverPendingMessages();
    startMessageLoop().catch((err) => {
      logger.fatal({ err }, 'Message loop crashed unexpectedly');
      process.exit(1);
    });
  }
}

// Guard: only run when executed directly, not when imported by tests
const isDirectRun =
  process.argv[1] &&
  new URL(import.meta.url).pathname === new URL(`file://${process.argv[1]}`).pathname;

if (isDirectRun) {
  main().catch((err) => {
    logger.error({ err }, 'Failed to start Intercom');
    process.exit(1);
  });
}
