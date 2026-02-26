import fs from 'fs';
import path from 'path';

import {
  ASSISTANT_NAME,
  DATA_DIR,
  DEFAULT_MODEL,
  DEFAULT_RUNTIME,
  findModel,
  HOST_CALLBACK_PORT,
  MAIN_GROUP_FOLDER,
  MODEL_CATALOG,
  Runtime,
  runtimeForModel,
  TELEGRAM_BOT_TOKEN,
  TELEGRAM_ONLY,
} from './config.js';
import { WhatsAppChannel } from './channels/whatsapp.js';
import { TelegramChannel } from './channels/telegram.js';
import { writeGroupsSnapshot } from './container-runner.js';
import {
  cleanupOrphans,
  ensureContainerRuntimeRunning,
} from './container-runtime.js';
import {
  deleteSession,
  getAllChats,
  getAllRegisteredGroups,
  getAllSessions,
  initDatabase,
  setRegisteredGroup,
  setSession,
  storeChatMetadata,
  storeMessage,
} from './db.js';
import { GroupQueue } from './group-queue.js';
import { resolveGroupFolderPath } from './group-folder.js';
import { startHostCallbackServer } from './host-callback.js';
import { processTaskIpc, startIpcWatcher } from './ipc.js';
import { findChannel, formatOutbound } from './router.js';
import {
  Channel,
  CommandResult,
  NewMessage,
  RegisteredGroup,
} from './types.js';
import { logger } from './logger.js';

let sessions: Record<string, string> = {};
let registeredGroups: Record<string, RegisteredGroup> = {};
let reportedModels: Record<string, string> = {}; // groupFolder → model name from container

let whatsapp: WhatsAppChannel;
const channels: Channel[] = [];
const queue = new GroupQueue();

function loadState(): void {
  sessions = getAllSessions();
  registeredGroups = getAllRegisteredGroups();
  logger.info(
    { groupCount: Object.keys(registeredGroups).length },
    'State loaded',
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
export function _setRegisteredGroups(
  groups: Record<string, RegisteredGroup>,
): void {
  registeredGroups = groups;
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
  const sessionsDir = path.join(
    DATA_DIR,
    '..',
    'groups',
    groupFolder,
    '.sessions',
  );
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
      "/chatid — Show this chat's registration ID",
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
  const uptime =
    uptimeHr > 0 ? `${uptimeHr}h ${uptimeMin % 60}m` : `${uptimeMin}m`;

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
    newModel = MODEL_CATALOG.find(
      (m) =>
        m.id.includes(lower) || m.displayName.toLowerCase().includes(lower),
    );
  }

  // Accept arbitrary model IDs — infer runtime from prefix pattern
  if (!newModel) {
    const runtime = runtimeForModel(args.toLowerCase());
    newModel = { id: args.toLowerCase(), runtime, displayName: args };
  }

  if (newModel.id === currentModelId) {
    return {
      text: `Already using \`${newModel.displayName}\`.`,
      parseMode: 'Markdown',
    };
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

  return {
    text: `Switched from ${prevDisplay} to *${newModel.displayName}*.`,
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
    onChatMetadata: (
      chatJid: string,
      timestamp: string,
      name?: string,
      channel?: string,
      isGroup?: boolean,
    ) => storeChatMetadata(chatJid, timestamp, name, channel, isGroup),
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

  // Rust orchestrator handles message loop, scheduler, and container dispatch.
  // Node is now the channel layer + command handler + host callback server.
  logger.info('Orchestration delegated to intercomd (Rust daemon)');

  startIpcWatcher({
    sendMessage: (jid, text) => {
      const channel = findChannel(channels, jid);
      if (!channel) throw new Error(`No channel for JID: ${jid}`);
      return channel.sendMessage(jid, text);
    },
    registeredGroups: () => registeredGroups,
    registerGroup,
    syncGroupMetadata: (force) =>
      whatsapp?.syncGroupMetadata(force) ?? Promise.resolve(),
    getAvailableGroups,
    writeGroupsSnapshot: (gf, im, ag, rj) =>
      writeGroupsSnapshot(gf, im, ag, rj),
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
          syncGroupMetadata: (force) =>
            whatsapp?.syncGroupMetadata(force) ?? Promise.resolve(),
          getAvailableGroups,
          writeGroupsSnapshot: (gf, im, ag, rj) =>
            writeGroupsSnapshot(gf, im, ag, rj),
        },
      );
    },
  });
}

// Guard: only run when executed directly, not when imported by tests
const isDirectRun =
  process.argv[1] &&
  new URL(import.meta.url).pathname ===
    new URL(`file://${process.argv[1]}`).pathname;

if (isDirectRun) {
  main().catch((err) => {
    logger.error({ err }, 'Failed to start Intercom');
    process.exit(1);
  });
}
