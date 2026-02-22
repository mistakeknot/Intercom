/**
 * Gemini tool declarations and executor dispatch.
 * Declares tools as plain objects for the Code Assist API.
 */

import * as executor from '../../shared/executor.js';
import * as ipcTools from '../../shared/ipc-tools.js';
import type { IpcContext } from '../../shared/ipc-tools.js';
import { log } from '../../shared/protocol.js';

interface ToolDeclaration {
  name: string;
  description: string;
  parameters?: {
    type: string;
    properties: Record<string, unknown>;
    required?: string[];
  };
}

/**
 * Tool declarations for the Code Assist API.
 * Format matches Gemini's FunctionDeclaration schema.
 */
export function getToolDeclarations(isMain: boolean): ToolDeclaration[] {
  const tools: ToolDeclaration[] = [
    {
      name: 'run_shell_command',
      description: 'Execute a shell command in the workspace. Use for git, npm, system operations. Working directory is /workspace/group.',
      parameters: {
        type: 'object',
        properties: {
          command: { type: 'string', description: 'The shell command to execute' },
          cwd: { type: 'string', description: 'Working directory (optional, defaults to /workspace/group)' },
        },
        required: ['command'],
      },
    },
    {
      name: 'read_file',
      description: 'Read file contents with line numbers. Supports offset/limit for large files.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'File path (absolute or relative to /workspace/group)' },
          offset: { type: 'number', description: 'Starting line number (0-indexed, optional)' },
          limit: { type: 'number', description: 'Maximum number of lines to read (optional)' },
        },
        required: ['path'],
      },
    },
    {
      name: 'write_file',
      description: 'Write content to a file. Creates parent directories automatically. Overwrites existing files.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'File path (absolute or relative to /workspace/group)' },
          content: { type: 'string', description: 'The content to write' },
        },
        required: ['path', 'content'],
      },
    },
    {
      name: 'edit_file',
      description: 'Replace a unique string in a file with new content. The old_string must appear exactly once.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'File path' },
          old_string: { type: 'string', description: 'The exact string to find (must be unique in file)' },
          new_string: { type: 'string', description: 'The replacement string' },
        },
        required: ['path', 'old_string', 'new_string'],
      },
    },
    {
      name: 'grep_search',
      description: 'Search file contents with regex patterns. Returns matching lines with file paths and line numbers.',
      parameters: {
        type: 'object',
        properties: {
          pattern: { type: 'string', description: 'Regex pattern to search for' },
          path: { type: 'string', description: 'Directory or file to search in (optional, defaults to /workspace/group)' },
          include: { type: 'string', description: 'File glob pattern to filter (e.g., "*.ts", optional)' },
        },
        required: ['pattern'],
      },
    },
    {
      name: 'glob_files',
      description: 'Find files matching a glob pattern.',
      parameters: {
        type: 'object',
        properties: {
          pattern: { type: 'string', description: 'Glob pattern (e.g., "*.ts", "package.json")' },
          path: { type: 'string', description: 'Directory to search in (optional)' },
        },
        required: ['pattern'],
      },
    },
    {
      name: 'list_directory',
      description: 'List contents of a directory.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'Directory path (optional, defaults to current directory)' },
        },
      },
    },
    {
      name: 'send_message',
      description: 'Send a message to the user or group immediately while you\'re still running. Use for progress updates or to send multiple messages.',
      parameters: {
        type: 'object',
        properties: {
          text: { type: 'string', description: 'The message text to send' },
          sender: { type: 'string', description: 'Your role/identity name (optional)' },
        },
        required: ['text'],
      },
    },
    {
      name: 'schedule_task',
      description: 'Schedule a recurring or one-time task.',
      parameters: {
        type: 'object',
        properties: {
          prompt: { type: 'string', description: 'What the agent should do when the task runs' },
          schedule_type: { type: 'string', description: 'cron, interval, or once' },
          schedule_value: { type: 'string', description: 'cron expression, milliseconds, or ISO timestamp' },
          context_mode: { type: 'string', description: 'group or isolated (default: group)' },
          target_group_jid: { type: 'string', description: 'Target group JID (main only, optional)' },
        },
        required: ['prompt', 'schedule_type', 'schedule_value'],
      },
    },
    {
      name: 'list_tasks',
      description: 'List all scheduled tasks.',
      parameters: { type: 'object', properties: {} },
    },
    {
      name: 'pause_task',
      description: 'Pause a scheduled task.',
      parameters: {
        type: 'object',
        properties: {
          task_id: { type: 'string', description: 'The task ID to pause' },
        },
        required: ['task_id'],
      },
    },
    {
      name: 'resume_task',
      description: 'Resume a paused task.',
      parameters: {
        type: 'object',
        properties: {
          task_id: { type: 'string', description: 'The task ID to resume' },
        },
        required: ['task_id'],
      },
    },
    {
      name: 'cancel_task',
      description: 'Cancel and delete a scheduled task.',
      parameters: {
        type: 'object',
        properties: {
          task_id: { type: 'string', description: 'The task ID to cancel' },
        },
        required: ['task_id'],
      },
    },
  ];

  if (isMain) {
    tools.push({
      name: 'register_group',
      description: 'Register a new messaging group (main only).',
      parameters: {
        type: 'object',
        properties: {
          jid: { type: 'string', description: 'The messaging JID' },
          name: { type: 'string', description: 'Display name for the group' },
          folder: { type: 'string', description: 'Folder name (lowercase, hyphens)' },
          trigger: { type: 'string', description: 'Trigger word (e.g., "@Andy")' },
        },
        required: ['jid', 'name', 'folder', 'trigger'],
      },
    });
  }

  return tools;
}

/**
 * Execute a tool call and return the result string.
 */
export function executeTool(
  name: string,
  args: Record<string, unknown>,
  ipcCtx: IpcContext,
): string {
  log(`Executing tool: ${name}`);

  switch (name) {
    case 'run_shell_command':
      return executor.runShellCommand(
        args.command as string,
        args.cwd as string | undefined,
      );

    case 'read_file':
      return executor.readFile(
        args.path as string,
        args.offset as number | undefined,
        args.limit as number | undefined,
      );

    case 'write_file':
      return executor.writeFile(args.path as string, args.content as string);

    case 'edit_file':
      return executor.editFile(
        args.path as string,
        args.old_string as string,
        args.new_string as string,
      );

    case 'grep_search':
      return executor.grepSearch(
        args.pattern as string,
        args.path as string | undefined,
        args.include as string | undefined,
      );

    case 'glob_files':
      return executor.globFiles(
        args.pattern as string,
        args.path as string | undefined,
      );

    case 'list_directory':
      return executor.listDirectory(args.path as string | undefined);

    case 'send_message':
      return ipcTools.sendMessage(ipcCtx, args.text as string, args.sender as string | undefined);

    case 'schedule_task':
      return ipcTools.scheduleTask(
        ipcCtx,
        args.prompt as string,
        args.schedule_type as 'cron' | 'interval' | 'once',
        args.schedule_value as string,
        (args.context_mode as 'group' | 'isolated') || 'group',
        args.target_group_jid as string | undefined,
      );

    case 'list_tasks':
      return ipcTools.listTasks(ipcCtx);

    case 'pause_task':
      return ipcTools.pauseTask(ipcCtx, args.task_id as string);

    case 'resume_task':
      return ipcTools.resumeTask(ipcCtx, args.task_id as string);

    case 'cancel_task':
      return ipcTools.cancelTask(ipcCtx, args.task_id as string);

    case 'register_group':
      return ipcTools.registerGroup(
        ipcCtx,
        args.jid as string,
        args.name as string,
        args.folder as string,
        args.trigger as string,
      );

    default:
      return `Unknown tool: ${name}`;
  }
}
