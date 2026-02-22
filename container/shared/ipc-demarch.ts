/**
 * Demarch IPC query bridge — container side.
 * Shared query/response logic used by all runtimes (Claude MCP, Gemini tools, Codex shell).
 * Writes query files to /workspace/ipc/queries/, polls /workspace/ipc/responses/ for results.
 */

import crypto from 'crypto';
import fs from 'fs';
import path from 'path';
import { log } from './protocol.js';

const IPC_DIR = '/workspace/ipc';
const QUERIES_DIR = path.join(IPC_DIR, 'queries');
const RESPONSES_DIR = path.join(IPC_DIR, 'responses');

const DEFAULT_TIMEOUT_MS = 30_000;
const POLL_INTERVAL_MS = 200;

/**
 * Send a query to the Demarch kernel via the IPC bridge and wait for the response.
 * Returns the result string on success, or an error message string on failure.
 */
export async function queryKernel(
  type: string,
  params: Record<string, unknown> = {},
  timeoutMs: number = DEFAULT_TIMEOUT_MS,
): Promise<string> {
  const uuid = crypto.randomUUID();

  const query = {
    uuid,
    type,
    params,
    timestamp: new Date().toISOString(),
  };

  // Atomic write: tmp then rename
  fs.mkdirSync(QUERIES_DIR, { recursive: true });
  const queryPath = path.join(QUERIES_DIR, `${uuid}.json`);
  const tempPath = `${queryPath}.tmp`;
  fs.writeFileSync(tempPath, JSON.stringify(query, null, 2));
  fs.renameSync(tempPath, queryPath);

  log(`Demarch query: ${type} (${uuid})`);

  // Poll for response
  const responsePath = path.join(RESPONSES_DIR, `${uuid}.json`);
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    if (fs.existsSync(responsePath)) {
      try {
        const raw = fs.readFileSync(responsePath, 'utf-8');
        const response = JSON.parse(raw);
        // Clean up response file
        try { fs.unlinkSync(responsePath); } catch { /* ignore */ }
        if (response.status === 'error') {
          return `Error: ${response.result || 'Unknown error'}`;
        }
        return response.result || '';
      } catch (err) {
        return `Error parsing response: ${err instanceof Error ? err.message : String(err)}`;
      }
    }
    await sleep(POLL_INTERVAL_MS);
  }

  // Timeout — clean up the query file if it's still there
  try { fs.unlinkSync(queryPath); } catch { /* ignore */ }
  return 'Error: Query timed out — Demarch kernel may not be available.';
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
