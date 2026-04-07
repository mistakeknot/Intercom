/**
 * Volume mount construction and container argument building for Intercom.
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

import {
  CONTAINER_IMAGES,
  DATA_DIR,
  GROUPS_DIR,
  TIMEZONE,
  type Runtime,
} from './config.js';
import { readEnvFile } from './env.js';
import { resolveGroupFolderPath, resolveGroupIpcPath } from './group-folder.js';
import { logger } from './logger.js';
import { CONTAINER_RUNTIME_BIN, readonlyMountArgs } from './container-runtime.js';
import { validateAdditionalMounts } from './mount-security.js';
import { RegisteredGroup } from './types.js';

export interface VolumeMount {
  hostPath: string;
  containerPath: string;
  readonly: boolean;
  exclude?: string[]; // Subdirectory names to hide via tmpfs overlay
}

export function buildVolumeMounts(
  group: RegisteredGroup,
  isMain: boolean,
  runtime: Runtime = 'claude',
): VolumeMount[] {
  const mounts: VolumeMount[] = [];
  const projectRoot = process.cwd();
  const groupDir = resolveGroupFolderPath(group.folder);

  if (isMain) {
    // Main gets the project root read-only. Writable paths the agent needs
    // (group folder, IPC, .claude/) are mounted separately below.
    // Read-only prevents the agent from modifying host application code
    // (src/, dist/, package.json, etc.) which would bypass the sandbox
    // entirely on next restart.
    mounts.push({
      hostPath: projectRoot,
      containerPath: '/workspace/project',
      readonly: true,
    });

    // Main also gets its group folder as the working directory
    mounts.push({
      hostPath: groupDir,
      containerPath: '/workspace/group',
      readonly: false,
    });
  } else {
    // Other groups only get their own folder
    mounts.push({
      hostPath: groupDir,
      containerPath: '/workspace/group',
      readonly: false,
    });

    // Global memory directory (read-only for non-main)
    // Only directory mounts are supported, not file mounts
    const globalDir = path.join(GROUPS_DIR, 'global');
    if (fs.existsSync(globalDir)) {
      mounts.push({
        hostPath: globalDir,
        containerPath: '/workspace/global',
        readonly: true,
      });
    }
  }

  // Claude runtime: per-group .claude/ sessions directory with settings and skills
  // Non-Claude runtimes skip this — they manage sessions internally as JSON files
  if (runtime === 'claude') {
    const groupSessionsDir = path.join(
      DATA_DIR,
      'sessions',
      group.folder,
      '.claude',
    );
    fs.mkdirSync(groupSessionsDir, { recursive: true });
    const settingsFile = path.join(groupSessionsDir, 'settings.json');
    if (!fs.existsSync(settingsFile)) {
      fs.writeFileSync(settingsFile, JSON.stringify({
        env: {
          CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS: '1',
          CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD: '1',
          CLAUDE_CODE_DISABLE_AUTO_MEMORY: '0',
        },
      }, null, 2) + '\n');
    }

    // Sync skills from container/skills/ into each group's .claude/skills/
    const skillsSrc = path.join(process.cwd(), 'container', 'skills');
    const skillsDst = path.join(groupSessionsDir, 'skills');
    if (fs.existsSync(skillsSrc)) {
      for (const skillDir of fs.readdirSync(skillsSrc)) {
        const srcDir = path.join(skillsSrc, skillDir);
        if (!fs.statSync(srcDir).isDirectory()) continue;
        const dstDir = path.join(skillsDst, skillDir);
        fs.cpSync(srcDir, dstDir, { recursive: true });
      }
    }
    mounts.push({
      hostPath: groupSessionsDir,
      containerPath: '/home/node/.claude',
      readonly: false,
    });
  }

  // Per-group IPC namespace: each group gets its own IPC directory
  // This prevents cross-group privilege escalation via IPC
  const groupIpcDir = resolveGroupIpcPath(group.folder);
  fs.mkdirSync(path.join(groupIpcDir, 'messages'), { recursive: true });
  fs.mkdirSync(path.join(groupIpcDir, 'tasks'), { recursive: true });
  fs.mkdirSync(path.join(groupIpcDir, 'input'), { recursive: true });
  fs.mkdirSync(path.join(groupIpcDir, 'queries'), { recursive: true });
  fs.mkdirSync(path.join(groupIpcDir, 'responses'), { recursive: true });
  mounts.push({
    hostPath: groupIpcDir,
    containerPath: '/workspace/ipc',
    readonly: false,
  });

  // Mount agent-runner source from host — recompiled on container startup.
  // Bypasses sticky build cache for code changes.
  // Each runtime has its own runner source directory and container layout.
  const runnerDirMap: Record<Runtime, string> = {
    claude: 'agent-runner',
    gemini: 'gemini-runner',
    codex: 'codex-runner',
  };
  const runnerSrc = path.join(projectRoot, 'container', runnerDirMap[runtime], 'src');
  if (fs.existsSync(runnerSrc)) {
    // Claude: runner lives at /app/src (flat layout)
    // Gemini/Codex: runner lives at /app/{runner}/src (nested to preserve ../../shared imports)
    const containerRunnerPath = runtime === 'claude'
      ? '/app/src'
      : `/app/${runnerDirMap[runtime]}/src`;
    mounts.push({
      hostPath: runnerSrc,
      containerPath: containerRunnerPath,
      readonly: true,
    });
  }

  // Non-Claude runtimes also need the shared code mounted
  if (runtime !== 'claude') {
    const sharedSrc = path.join(projectRoot, 'container', 'shared');
    if (fs.existsSync(sharedSrc)) {
      mounts.push({
        hostPath: sharedSrc,
        containerPath: '/app/shared',
        readonly: true,
      });
    }
  }

  // Additional mounts validated against external allowlist (tamper-proof from containers)
  if (group.containerConfig?.additionalMounts) {
    const validatedMounts = validateAdditionalMounts(
      group.containerConfig.additionalMounts,
      group.name,
      isMain,
    );
    mounts.push(...validatedMounts);
  }

  return mounts;
}

export function buildContainerArgs(mounts: VolumeMount[], containerName: string, runtime: Runtime): string[] {
  const args: string[] = ['run', '-i', '--rm', '--name', containerName];

  // Pass host timezone so container's local time matches the user's
  args.push('-e', `TZ=${TIMEZONE}`);

  // Run as host user so bind-mounted files are accessible.
  // Skip when running as root (uid 0), as the container's node user (uid 1000),
  // or when getuid is unavailable (native Windows without WSL).
  const hostUid = process.getuid?.();
  const hostGid = process.getgid?.();
  if (hostUid != null && hostUid !== 0 && hostUid !== 1000) {
    args.push('--user', `${hostUid}:${hostGid}`);
    args.push('-e', 'HOME=/home/node');
  }

  for (const mount of mounts) {
    if (mount.readonly) {
      args.push(...readonlyMountArgs(mount.hostPath, mount.containerPath));
    } else {
      args.push('-v', `${mount.hostPath}:${mount.containerPath}`);
    }

    // Overlay excluded subdirectories with empty tmpfs so they're invisible to the agent
    if (mount.exclude) {
      for (const subdir of mount.exclude) {
        if (!subdir || subdir.includes('..') || subdir.includes('/') || subdir.includes('\\') || subdir.includes(',')) {
          logger.warn({ subdir, hostPath: mount.hostPath }, 'Skipping invalid exclude value');
          continue;
        }
        args.push('--mount', `type=tmpfs,destination=${mount.containerPath}/${subdir},tmpfs-size=0`);
      }
    }
  }

  args.push(CONTAINER_IMAGES[runtime]);

  return args;
}

/**
 * Read the Claude OAuth token fresh from ~/.claude/.credentials.json.
 * Claude Code auto-refreshes this file, so we always get a valid token.
 * Returns the accessToken or undefined if not available.
 */
function readClaudeOAuthToken(): string | undefined {
  const credPath = path.join(os.homedir(), '.claude', '.credentials.json');
  try {
    const data = JSON.parse(fs.readFileSync(credPath, 'utf-8'));
    const token = data?.claudeAiOauth?.accessToken;
    if (token) {
      logger.debug('Read Claude OAuth token from credentials file');
      return token;
    }
  } catch {
    // File doesn't exist or is malformed — fall through
  }
  return undefined;
}

/**
 * Read allowed secrets from .env for passing to the container via stdin.
 * Secrets are never written to disk or mounted as files.
 * All runtime secrets are read — the container uses what it needs.
 *
 * For Claude: if neither CLAUDE_CODE_OAUTH_TOKEN nor ANTHROPIC_API_KEY
 * is set in .env, reads the OAuth token from ~/.claude/.credentials.json
 * (auto-refreshed by Claude Code).
 */
export function readSecrets(): Record<string, string> {
  const secrets = readEnvFile([
    // Claude
    'CLAUDE_CODE_OAUTH_TOKEN', 'ANTHROPIC_API_KEY',
    // Gemini (Code Assist API)
    'GEMINI_REFRESH_TOKEN', 'GEMINI_OAUTH_CLIENT_ID', 'GEMINI_OAUTH_CLIENT_SECRET',
    // Codex/OpenAI
    'CODEX_OAUTH_ACCESS_TOKEN', 'CODEX_OAUTH_REFRESH_TOKEN',
    'CODEX_OAUTH_ID_TOKEN', 'CODEX_OAUTH_ACCOUNT_ID',
  ]);

  // Auto-refresh: read Claude OAuth from credentials file if not in .env
  if (!secrets['CLAUDE_CODE_OAUTH_TOKEN'] && !secrets['ANTHROPIC_API_KEY']) {
    const token = readClaudeOAuthToken();
    if (token) {
      secrets['CLAUDE_CODE_OAUTH_TOKEN'] = token;
    }
  }

  return secrets;
}
