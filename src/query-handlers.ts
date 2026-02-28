/**
 * Demarch query handlers — host side.
 * Routes IPC queries from containers to ic/bd CLIs and returns structured responses.
 * All handlers gracefully degrade when CLIs are unavailable.
 *
 * Security: uses execFileSync (no shell) to prevent command injection from
 * container-supplied params. Arguments are passed as arrays, never interpolated.
 */

import { execFileSync } from 'child_process';
import fs from 'fs';
import path from 'path';

import { logger } from './logger.js';

export interface QueryResponse {
  status: 'ok' | 'error';
  result: string;
}

/**
 * Execute a CLI command safely (no shell). Returns stdout or null on failure.
 */
function execCli(bin: string, args: string[]): string | null {
  try {
    return execFileSync(bin, args, {
      encoding: 'utf-8',
      timeout: 15_000,
      env: { ...process.env, PATH: `${process.env.PATH}:/usr/local/bin` },
    }).trim();
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    logger.debug({ bin, args, error: msg }, 'CLI command failed');
    return null;
  }
}

/**
 * Check if a CLI tool is available on the host.
 */
function isCliAvailable(bin: string): boolean {
  try {
    execFileSync('which', [bin], { encoding: 'utf-8', timeout: 5000 });
    return true;
  } catch {
    return false;
  }
}

const STANDALONE_MSG =
  'Demarch kernel not available — Intercom is running in standalone mode.';

function handleRunStatus(params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('ic')) return { status: 'error', result: STANDALONE_MSG };

  const runId = params.runId as string | undefined;
  const args = runId
    ? ['run', 'status', runId, '--json']
    : ['run', 'current', '--json'];
  const result = execCli('ic', args);
  if (result === null) {
    return { status: 'error', result: 'No active run found or ic command failed.' };
  }
  return { status: 'ok', result };
}

function handleSprintPhase(_params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('ic')) return { status: 'error', result: STANDALONE_MSG };

  const result = execCli('ic', ['run', 'phase', '--json']);
  if (result === null) {
    return { status: 'error', result: 'No active sprint phase or ic command failed.' };
  }
  return { status: 'ok', result };
}

function handleSearchBeads(params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('bd')) return { status: 'error', result: STANDALONE_MSG };

  const id = params.id as string | undefined;
  if (id) {
    const result = execCli('bd', ['show', id, '--json']);
    if (result === null) {
      return { status: 'error', result: `Bead ${id} not found.` };
    }
    return { status: 'ok', result };
  }

  const status = params.status as string | undefined;
  const query = params.query as string | undefined;

  const args = ['list', '--json'];
  if (status) args.push(`--status=${status}`);
  if (query) args.push(`--search=${query}`);

  const result = execCli('bd', args);
  if (result === null) {
    return { status: 'error', result: 'Failed to search beads.' };
  }
  return { status: 'ok', result };
}

function handleSpecLookup(params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('ic')) return { status: 'error', result: STANDALONE_MSG };

  const artifactId = params.artifactId as string | undefined;
  if (artifactId) {
    const result = execCli('ic', ['run', 'artifact', 'get', artifactId, '--json']);
    if (result === null) {
      return { status: 'error', result: `Artifact ${artifactId} not found.` };
    }
    return { status: 'ok', result };
  }

  const result = execCli('ic', ['run', 'artifact', 'list', '--json']);
  if (result === null) {
    return { status: 'error', result: 'No artifacts found or ic command failed.' };
  }
  return { status: 'ok', result };
}

function handleReviewSummary(_params: Record<string, unknown>): QueryResponse {
  // Read latest verdict files from flux-drive research output
  const searchDirs = [
    'docs/research/flux-drive',
    'docs/research',
  ];

  for (const dir of searchDirs) {
    const fullDir = path.resolve(dir);
    if (!fs.existsSync(fullDir)) continue;

    try {
      const files = fs.readdirSync(fullDir)
        .filter((f) => f.endsWith('.json') && f.includes('verdict'))
        .sort()
        .reverse()
        .slice(0, 3);

      if (files.length === 0) continue;

      const verdicts = files.map((f) => {
        try {
          return fs.readFileSync(path.join(fullDir, f), 'utf-8');
        } catch { return null; }
      }).filter(Boolean);

      if (verdicts.length > 0) {
        return { status: 'ok', result: `[${verdicts.join(',')}]` };
      }
    } catch { /* continue to next dir */ }
  }

  return { status: 'error', result: 'No review verdicts found.' };
}

function handleNextWork(_params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('bd')) return { status: 'error', result: STANDALONE_MSG };

  const result = execCli('bd', ['ready', '--json']);
  if (result === null) {
    return { status: 'error', result: 'No ready work items found.' };
  }
  return { status: 'ok', result };
}

function handleResearch(params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('ic')) return { status: 'error', result: STANDALONE_MSG };

  const query = params.query as string | undefined;
  if (!query) {
    return { status: 'error', result: 'research requires a query parameter' };
  }

  const args = ['discovery', 'search', '--json', query];
  const result = execCli('ic', args);
  if (result === null) {
    return {
      status: 'error',
      result: 'Research tool not available — ic discovery subcommand may not exist yet.',
    };
  }
  return { status: 'ok', result };
}

function handleRunEvents(params: Record<string, unknown>): QueryResponse {
  if (!isCliAvailable('ic')) return { status: 'error', result: STANDALONE_MSG };

  const limit = String((params.limit as number) || 20);
  const since = params.since as string | undefined;

  const args = ['events', 'tail', '--consumer=intercom', '--json', `--limit=${limit}`];
  if (since) args.push(`--since=${since}`);

  const result = execCli('ic', args);
  if (result === null) {
    return { status: 'error', result: 'No events found or ic command failed.' };
  }
  return { status: 'ok', result };
}

/**
 * Route a query to the appropriate handler.
 */
export function handleQuery(
  type: string,
  params: Record<string, unknown>,
  sourceGroup: string,
  isMain: boolean,
): QueryResponse {
  logger.debug({ type, sourceGroup, isMain }, 'Processing Demarch query');

  switch (type) {
    case 'run_status':
      return handleRunStatus(params);
    case 'sprint_phase':
      return handleSprintPhase(params);
    case 'search_beads':
      return handleSearchBeads(params);
    case 'spec_lookup':
      return handleSpecLookup(params);
    case 'review_summary':
      return handleReviewSummary(params);
    case 'next_work':
      return handleNextWork(params);
    case 'run_events':
      return handleRunEvents(params);
    case 'research':
      return handleResearch(params);
    default:
      return { status: 'error', result: `Unknown query type: ${type}` };
  }
}
