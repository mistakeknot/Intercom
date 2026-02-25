/**
 * Step: environment — Detect OS, Node, container runtimes, existing config.
 * Replaces 01-check-environment.sh
 */
import fs from 'fs';
import path from 'path';

import Database from 'better-sqlite3';

import { STORE_DIR } from '../src/config.js';
import { logger } from '../src/logger.js';
import { commandExists, getPlatform, isHeadless, isWSL } from './platform.js';
import { emitStatus } from './status.js';

export async function run(_args: string[]): Promise<void> {
  const projectRoot = process.cwd();

  logger.info('Starting environment check');

  const platform = getPlatform();
  const wsl = isWSL();
  const headless = isHeadless();

  // Check Apple Container
  let appleContainer: 'installed' | 'not_found' = 'not_found';
  if (commandExists('container')) {
    appleContainer = 'installed';
  }

  // Check Docker
  let docker: 'running' | 'installed_not_running' | 'not_found' = 'not_found';
  if (commandExists('docker')) {
    try {
      const { execSync } = await import('child_process');
      execSync('docker info', { stdio: 'ignore' });
      docker = 'running';
    } catch {
      docker = 'installed_not_running';
    }
  }

  // Check Rust toolchain (optional in this migration phase)
  const rustc: 'installed' | 'not_found' = commandExists('rustc')
    ? 'installed'
    : 'not_found';
  const cargo: 'installed' | 'not_found' = commandExists('cargo')
    ? 'installed'
    : 'not_found';

  // Check existing config
  const hasEnv = fs.existsSync(path.join(projectRoot, '.env'));

  const authDir = path.join(projectRoot, 'store', 'auth');
  const hasAuth =
    fs.existsSync(authDir) &&
    fs.readdirSync(authDir).length > 0;

  let hasRegisteredGroups = false;
  // Check JSON file first (pre-migration)
  if (fs.existsSync(path.join(projectRoot, 'data', 'registered_groups.json'))) {
    hasRegisteredGroups = true;
  } else {
    // Check SQLite directly using better-sqlite3 (no sqlite3 CLI needed)
    const dbPath = path.join(STORE_DIR, 'messages.db');
    if (fs.existsSync(dbPath)) {
      try {
        const db = new Database(dbPath, { readonly: true });
        const row = db.prepare(
          'SELECT COUNT(*) as count FROM registered_groups',
        ).get() as { count: number };
        if (row.count > 0) hasRegisteredGroups = true;
        db.close();
      } catch {
        // Table might not exist yet
      }
    }
  }

  if (rustc === 'not_found' || cargo === 'not_found') {
    logger.warn(
      { rustc, cargo },
      'Rust toolchain not fully available — Rust migration path will be skipped during setup',
    );
  }

  logger.info({ platform, wsl, appleContainer, docker, rustc, cargo, hasEnv, hasAuth, hasRegisteredGroups },
    'Environment check complete');

  emitStatus('CHECK_ENVIRONMENT', {
    PLATFORM: platform,
    IS_WSL: wsl,
    IS_HEADLESS: headless,
    APPLE_CONTAINER: appleContainer,
    DOCKER: docker,
    RUSTC: rustc,
    CARGO: cargo,
    HAS_ENV: hasEnv,
    HAS_AUTH: hasAuth,
    HAS_REGISTERED_GROUPS: hasRegisteredGroups,
    STATUS: 'success',
    LOG: 'logs/setup.log',
  });
}
