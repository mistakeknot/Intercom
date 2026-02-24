/**
 * Shared Intercom IPC tool definitions for non-Claude runtimes.
 * These tools allow the agent to send messages, schedule tasks, etc.
 * Same functionality as ipc-mcp-stdio.ts but as plain function calls.
 */

import fs from 'fs';
import path from 'path';
import { log } from './protocol.js';

const IPC_DIR = '/workspace/ipc';
const MESSAGES_DIR = path.join(IPC_DIR, 'messages');
const TASKS_DIR = path.join(IPC_DIR, 'tasks');

function writeIpcFile(dir: string, data: object): string {
  fs.mkdirSync(dir, { recursive: true });
  const filename = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}.json`;
  const filepath = path.join(dir, filename);
  const tempPath = `${filepath}.tmp`;
  fs.writeFileSync(tempPath, JSON.stringify(data, null, 2));
  fs.renameSync(tempPath, filepath);
  return filename;
}

export interface IpcContext {
  chatJid: string;
  groupFolder: string;
  isMain: boolean;
}

export function sendMessage(ctx: IpcContext, text: string, sender?: string): string {
  const data: Record<string, string | undefined> = {
    type: 'message',
    chatJid: ctx.chatJid,
    text,
    sender: sender || undefined,
    groupFolder: ctx.groupFolder,
    timestamp: new Date().toISOString(),
  };
  const filename = writeIpcFile(MESSAGES_DIR, data);
  log(`IPC: sent message (${filename})`);
  return 'Message sent.';
}

export function scheduleTask(
  ctx: IpcContext,
  prompt: string,
  scheduleType: 'cron' | 'interval' | 'once',
  scheduleValue: string,
  contextMode: 'group' | 'isolated' = 'group',
  targetGroupJid?: string,
): string {
  // Validate schedule_value
  if (scheduleType === 'interval') {
    const ms = parseInt(scheduleValue, 10);
    if (isNaN(ms) || ms <= 0) {
      return `Invalid interval: "${scheduleValue}". Must be positive milliseconds.`;
    }
  } else if (scheduleType === 'once') {
    const date = new Date(scheduleValue);
    if (isNaN(date.getTime())) {
      return `Invalid timestamp: "${scheduleValue}". Use ISO 8601 format.`;
    }
  }

  const targetJid = ctx.isMain && targetGroupJid ? targetGroupJid : ctx.chatJid;

  const data = {
    type: 'schedule_task',
    prompt,
    schedule_type: scheduleType,
    schedule_value: scheduleValue,
    context_mode: contextMode,
    targetJid,
    createdBy: ctx.groupFolder,
    timestamp: new Date().toISOString(),
  };

  const filename = writeIpcFile(TASKS_DIR, data);
  return `Task scheduled (${filename}): ${scheduleType} - ${scheduleValue}`;
}

export function listTasks(ctx: IpcContext): string {
  const tasksFile = path.join(IPC_DIR, 'current_tasks.json');
  try {
    if (!fs.existsSync(tasksFile)) {
      return 'No scheduled tasks found.';
    }
    const allTasks = JSON.parse(fs.readFileSync(tasksFile, 'utf-8'));
    const tasks = ctx.isMain
      ? allTasks
      : allTasks.filter((t: { groupFolder: string }) => t.groupFolder === ctx.groupFolder);

    if (tasks.length === 0) return 'No scheduled tasks found.';

    return 'Scheduled tasks:\n' + tasks
      .map((t: { id: string; prompt: string; schedule_type: string; schedule_value: string; status: string; next_run: string }) =>
        `- [${t.id}] ${t.prompt.slice(0, 50)}... (${t.schedule_type}: ${t.schedule_value}) - ${t.status}, next: ${t.next_run || 'N/A'}`)
      .join('\n');
  } catch (err) {
    return `Error reading tasks: ${err instanceof Error ? err.message : String(err)}`;
  }
}

export function pauseTask(ctx: IpcContext, taskId: string): string {
  writeIpcFile(TASKS_DIR, {
    type: 'pause_task',
    taskId,
    groupFolder: ctx.groupFolder,
    isMain: ctx.isMain,
    timestamp: new Date().toISOString(),
  });
  return `Task ${taskId} pause requested.`;
}

export function resumeTask(ctx: IpcContext, taskId: string): string {
  writeIpcFile(TASKS_DIR, {
    type: 'resume_task',
    taskId,
    groupFolder: ctx.groupFolder,
    isMain: ctx.isMain,
    timestamp: new Date().toISOString(),
  });
  return `Task ${taskId} resume requested.`;
}

export function cancelTask(ctx: IpcContext, taskId: string): string {
  writeIpcFile(TASKS_DIR, {
    type: 'cancel_task',
    taskId,
    groupFolder: ctx.groupFolder,
    isMain: ctx.isMain,
    timestamp: new Date().toISOString(),
  });
  return `Task ${taskId} cancellation requested.`;
}

export function registerGroup(
  ctx: IpcContext,
  jid: string,
  name: string,
  folder: string,
  trigger: string,
): string {
  if (!ctx.isMain) {
    return 'Only the main group can register new groups.';
  }

  writeIpcFile(TASKS_DIR, {
    type: 'register_group',
    jid,
    name,
    folder,
    trigger,
    timestamp: new Date().toISOString(),
  });

  return `Group "${name}" registered. It will start receiving messages immediately.`;
}
