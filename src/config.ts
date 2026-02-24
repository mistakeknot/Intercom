import os from 'os';
import path from 'path';

import { readEnvFile } from './env.js';

// Read config values from .env (falls back to process.env).
// Secrets are NOT read here — they stay on disk and are loaded only
// where needed (container-runner.ts) to avoid leaking to child processes.
const envConfig = readEnvFile([
  'ASSISTANT_NAME',
  'ASSISTANT_HAS_OWN_NUMBER',
  'NANOCLAW_RUNTIME',
  'TELEGRAM_BOT_TOKEN',
  'TELEGRAM_ONLY',
]);

export const ASSISTANT_NAME =
  process.env.ASSISTANT_NAME || envConfig.ASSISTANT_NAME || 'Andy';
export const ASSISTANT_HAS_OWN_NUMBER =
  (process.env.ASSISTANT_HAS_OWN_NUMBER || envConfig.ASSISTANT_HAS_OWN_NUMBER) === 'true';
export const POLL_INTERVAL = 2000;
export const SCHEDULER_POLL_INTERVAL = 60000;

// Absolute paths needed for container mounts
const PROJECT_ROOT = process.cwd();
const HOME_DIR = process.env.HOME || os.homedir();

// Mount security: allowlist stored OUTSIDE project root, never mounted into containers
export const MOUNT_ALLOWLIST_PATH = path.join(
  HOME_DIR,
  '.config',
  'intercom',
  'mount-allowlist.json',
);
export const STORE_DIR = path.resolve(PROJECT_ROOT, 'store');
export const GROUPS_DIR = path.resolve(PROJECT_ROOT, 'groups');
export const DATA_DIR = path.resolve(PROJECT_ROOT, 'data');
export const MAIN_GROUP_FOLDER = 'main';

// --- Multi-runtime support ---
export type Runtime = 'claude' | 'gemini' | 'codex';

export const DEFAULT_RUNTIME: Runtime =
  (process.env.NANOCLAW_RUNTIME as Runtime) || (envConfig.NANOCLAW_RUNTIME as Runtime) || 'claude';

// --- Model catalog ---
export interface ModelEntry {
  id: string;           // e.g. 'claude-opus-4-6'
  runtime: Runtime;     // which container image to use
  displayName: string;  // e.g. 'Claude Opus 4.6'
}

export const MODEL_CATALOG: ModelEntry[] = [
  { id: 'claude-opus-4-6', runtime: 'claude', displayName: 'Claude Opus 4.6' },
  { id: 'claude-sonnet-4-6', runtime: 'claude', displayName: 'Claude Sonnet 4.6' },
  { id: 'gemini-3.1-pro', runtime: 'gemini', displayName: 'Gemini 3.1 Pro' },
  { id: 'gemini-2.5-flash', runtime: 'gemini', displayName: 'Gemini 2.5 Flash' },
  { id: 'gpt-5.1-codex', runtime: 'codex', displayName: 'GPT-5.1 Codex' },
];

export const DEFAULT_MODEL = 'claude-opus-4-6';

export function findModel(id: string): ModelEntry | undefined {
  return MODEL_CATALOG.find(m => m.id === id);
}

/**
 * Infer runtime from model ID. Checks the catalog first, then falls back
 * to prefix-based inference so arbitrary model IDs (e.g. gpt-5.3-codex,
 * claude-haiku-4-5, gemini-2.5-pro) work without catalog updates.
 */
export function runtimeForModel(modelId: string): Runtime {
  const catalogEntry = findModel(modelId);
  if (catalogEntry) return catalogEntry.runtime;

  const id = modelId.toLowerCase();
  if (id.startsWith('claude-')) return 'claude';
  if (id.startsWith('gemini-')) return 'gemini';
  if (id.startsWith('gpt-') || id.startsWith('codex-') || id.startsWith('o1-') || id.startsWith('o3-') || id.startsWith('o4-')) return 'codex';

  return DEFAULT_RUNTIME;
}

export const CONTAINER_IMAGES: Record<Runtime, string> = {
  claude: process.env.CONTAINER_IMAGE || 'nanoclaw-agent:latest',
  gemini: process.env.CONTAINER_IMAGE_GEMINI || 'nanoclaw-agent-gemini:latest',
  codex: process.env.CONTAINER_IMAGE_CODEX || 'nanoclaw-agent-codex:latest',
};

// Legacy single image reference (for backward compat)
export const CONTAINER_IMAGE = CONTAINER_IMAGES[DEFAULT_RUNTIME];
export const CONTAINER_TIMEOUT = parseInt(
  process.env.CONTAINER_TIMEOUT || '1800000',
  10,
);
export const CONTAINER_MAX_OUTPUT_SIZE = parseInt(
  process.env.CONTAINER_MAX_OUTPUT_SIZE || '10485760',
  10,
); // 10MB default
export const IPC_POLL_INTERVAL = 1000;
export const IDLE_TIMEOUT = parseInt(
  process.env.IDLE_TIMEOUT || '1800000',
  10,
); // 30min default — how long to keep container alive after last result
export const MAX_CONCURRENT_CONTAINERS = Math.max(
  1,
  parseInt(process.env.MAX_CONCURRENT_CONTAINERS || '5', 10) || 5,
);

function escapeRegex(str: string): string {
  return str.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

export const TRIGGER_PATTERN = new RegExp(
  `^@${escapeRegex(ASSISTANT_NAME)}\\b`,
  'i',
);

// Timezone for scheduled tasks (cron expressions, etc.)
// Uses system timezone by default
export const TIMEZONE =
  process.env.TZ || Intl.DateTimeFormat().resolvedOptions().timeZone;

// --- Telegram channel ---
export const TELEGRAM_BOT_TOKEN =
  process.env.TELEGRAM_BOT_TOKEN || envConfig.TELEGRAM_BOT_TOKEN || '';
export const TELEGRAM_ONLY =
  (process.env.TELEGRAM_ONLY || envConfig.TELEGRAM_ONLY) === 'true';
