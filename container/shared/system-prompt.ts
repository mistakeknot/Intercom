/**
 * Shared system prompt builder for NanoClaw non-Claude runtimes.
 * Loads CLAUDE.md files and constructs the system instruction.
 */

import fs from 'fs';
import path from 'path';
import { log } from './protocol.js';

/**
 * Build the system prompt from CLAUDE.md files and tool instructions.
 */
export function buildSystemPrompt(
  groupFolder: string,
  isMain: boolean,
  assistantName = 'Andy',
  runtime: string = 'gemini',
): string {
  const parts: string[] = [];

  parts.push(`You are ${assistantName}, a helpful personal AI assistant running in a containerized environment.`);
  parts.push(`You are running on the ${runtime} runtime.`);
  parts.push('');

  // Load group CLAUDE.md
  const groupClaudeMd = '/workspace/group/CLAUDE.md';
  if (fs.existsSync(groupClaudeMd)) {
    parts.push('# Group Instructions');
    parts.push(fs.readFileSync(groupClaudeMd, 'utf-8'));
    parts.push('');
  }

  // Load global CLAUDE.md (non-main groups only)
  if (!isMain) {
    const globalClaudeMd = '/workspace/global/CLAUDE.md';
    if (fs.existsSync(globalClaudeMd)) {
      parts.push('# Global Instructions');
      parts.push(fs.readFileSync(globalClaudeMd, 'utf-8'));
      parts.push('');
    }
  }

  // Load extra directory CLAUDE.md files
  const extraBase = '/workspace/extra';
  if (fs.existsSync(extraBase)) {
    for (const entry of fs.readdirSync(extraBase)) {
      const claudeMdPath = path.join(extraBase, entry, 'CLAUDE.md');
      if (fs.existsSync(claudeMdPath)) {
        parts.push(`# ${entry} Context`);
        parts.push(fs.readFileSync(claudeMdPath, 'utf-8'));
        parts.push('');
      }
    }
  }

  // Tool usage instructions
  parts.push('# Available Tools');
  parts.push('');
  parts.push('You have access to the following tools. Use them to accomplish tasks:');
  parts.push('');
  parts.push('- **run_shell_command**: Execute shell commands. Use for git, npm, system operations.');
  parts.push('- **read_file**: Read file contents with line numbers. Supports offset/limit for large files.');
  parts.push('- **write_file**: Write or overwrite a file. Creates parent directories automatically.');
  parts.push('- **edit_file**: Replace a unique string in a file with new content.');
  parts.push('- **grep_search**: Search file contents with regex patterns.');
  parts.push('- **glob_files**: Find files matching a glob pattern.');
  parts.push('- **list_directory**: List directory contents.');
  parts.push('- **send_message**: Send a message to the user/group immediately.');
  parts.push('- **schedule_task**: Schedule a recurring or one-time task.');
  parts.push('- **list_tasks**: List all scheduled tasks.');
  parts.push('- **pause_task**: Pause a scheduled task.');
  parts.push('- **resume_task**: Resume a paused task.');
  parts.push('- **cancel_task**: Cancel a scheduled task.');
  if (isMain) {
    parts.push('- **register_group**: Register a new messaging group (main only).');
  }
  parts.push('');
  parts.push('# Guidelines');
  parts.push('');
  parts.push('- Read files before editing them.');
  parts.push('- Use edit_file for targeted changes, write_file for new files or complete rewrites.');
  parts.push('- Keep responses concise and focused.');
  parts.push('- When executing shell commands, be mindful of the working directory (/workspace/group).');

  return parts.join('\n');
}
