/**
 * Shared tool executor for Intercom non-Claude runtimes.
 * Executes shell commands, file operations, grep, and glob.
 *
 * NOTE: These run inside an isolated Docker container with no network access
 * to the host. The shell commands are from the LLM agent, not user input.
 * execSync is intentional — the agent needs shell features (pipes, redirects).
 */

import { execSync } from 'child_process';
import fs from 'fs';
import path from 'path';
import { log } from './protocol.js';

const MAX_OUTPUT = 30000; // chars
const SHELL_TIMEOUT = 120_000; // 2 minutes

/** Secrets to strip from shell subprocess environments */
const SECRET_ENV_VARS = [
  'ANTHROPIC_API_KEY', 'CLAUDE_CODE_OAUTH_TOKEN',
  'GEMINI_REFRESH_TOKEN', 'GEMINI_OAUTH_CLIENT_ID', 'GEMINI_OAUTH_CLIENT_SECRET',
  'CODEX_OAUTH_ACCESS_TOKEN', 'CODEX_OAUTH_REFRESH_TOKEN',
];

function truncate(text: string, max = MAX_OUTPUT): string {
  if (text.length <= max) return text;
  return text.slice(0, max) + `\n... (truncated at ${max} chars)`;
}

export function runShellCommand(command: string, cwd?: string): string {
  const unsetPrefix = `unset ${SECRET_ENV_VARS.join(' ')} 2>/dev/null; `;
  try {
    const output = execSync(unsetPrefix + command, {
      cwd: cwd || '/workspace/group',
      timeout: SHELL_TIMEOUT,
      maxBuffer: 10 * 1024 * 1024,
      encoding: 'utf-8',
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return truncate(output);
  } catch (err: unknown) {
    const execErr = err as { stdout?: string; stderr?: string; status?: number };
    const stdout = execErr.stdout || '';
    const stderr = execErr.stderr || '';
    const code = execErr.status ?? 'unknown';
    return truncate(`Exit code ${code}\nstdout: ${stdout}\nstderr: ${stderr}`);
  }
}

export function readFile(filePath: string, offset?: number, limit?: number): string {
  try {
    const resolved = path.resolve('/workspace/group', filePath);
    const content = fs.readFileSync(resolved, 'utf-8');
    const lines = content.split('\n');

    const start = offset || 0;
    const end = limit ? start + limit : lines.length;
    const selected = lines.slice(start, end);

    return selected
      .map((line, i) => `${String(start + i + 1).padStart(6)} ${line}`)
      .join('\n');
  } catch (err) {
    return `Error reading file: ${err instanceof Error ? err.message : String(err)}`;
  }
}

export function writeFile(filePath: string, content: string): string {
  try {
    const resolved = path.resolve('/workspace/group', filePath);
    fs.mkdirSync(path.dirname(resolved), { recursive: true });
    fs.writeFileSync(resolved, content);
    return `File written: ${resolved}`;
  } catch (err) {
    return `Error writing file: ${err instanceof Error ? err.message : String(err)}`;
  }
}

export function editFile(filePath: string, oldString: string, newString: string): string {
  try {
    const resolved = path.resolve('/workspace/group', filePath);
    const content = fs.readFileSync(resolved, 'utf-8');

    const occurrences = content.split(oldString).length - 1;
    if (occurrences === 0) {
      return `Error: old_string not found in ${filePath}`;
    }
    if (occurrences > 1) {
      return `Error: old_string found ${occurrences} times in ${filePath}. Must be unique.`;
    }

    const newContent = content.replace(oldString, newString);
    fs.writeFileSync(resolved, newContent);
    return `File edited: ${resolved}`;
  } catch (err) {
    return `Error editing file: ${err instanceof Error ? err.message : String(err)}`;
  }
}

export function grepSearch(pattern: string, searchPath?: string, includeGlob?: string): string {
  const args = ['-rn', '--color=never'];
  if (includeGlob) args.push('--include=' + includeGlob);
  args.push('--', pattern);
  args.push(searchPath || '/workspace/group');

  const cmd = ['grep'].concat(args).join(' ');
  try {
    const output = execSync(cmd, {
      cwd: '/workspace/group',
      timeout: 30_000,
      maxBuffer: 5 * 1024 * 1024,
      encoding: 'utf-8',
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return truncate(output);
  } catch (err: unknown) {
    const execErr = err as { stdout?: string; status?: number };
    if (execErr.status === 1) return 'No matches found.';
    return truncate(execErr.stdout || 'grep error');
  }
}

export function globFiles(pattern: string, searchPath?: string): string {
  const basePath = searchPath || '/workspace/group';
  try {
    const output = execSync(
      `find ${JSON.stringify(basePath)} -name ${JSON.stringify(pattern)} -type f 2>/dev/null | head -200`,
      {
        timeout: 10_000,
        maxBuffer: 1024 * 1024,
        encoding: 'utf-8',
      },
    );
    return output.trim() || 'No files found.';
  } catch {
    return 'No files found.';
  }
}

export function listDirectory(dirPath?: string): string {
  const resolved = path.resolve('/workspace/group', dirPath || '.');
  try {
    const entries = fs.readdirSync(resolved, { withFileTypes: true });
    return entries
      .map(e => `${e.isDirectory() ? 'd' : '-'} ${e.name}`)
      .join('\n') || '(empty directory)';
  } catch (err) {
    return `Error listing directory: ${err instanceof Error ? err.message : String(err)}`;
  }
}
