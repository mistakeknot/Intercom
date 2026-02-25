/**
 * Lightweight HTTP callback server for intercomd → Node host communication.
 *
 * intercomd handles IPC polling and Demarch queries natively in Rust.
 * For actions that still need Node (sending messages via Grammy/Baileys,
 * task management via SQLite), intercomd calls back to this server.
 *
 * Endpoints:
 *   POST /v1/ipc/send-message   — send a message to a chat JID
 *   POST /v1/ipc/forward-task   — forward a task command for processing
 *   GET  /healthz               — health check
 */

import http from 'http';

import { logger } from './logger.js';

export interface HostCallbackDeps {
  sendMessage: (jid: string, text: string, sender?: string) => Promise<void>;
  forwardTask: (
    task: Record<string, unknown>,
    groupFolder: string,
    isMain: boolean,
  ) => Promise<void>;
  getRegisteredGroups: () => Record<
    string,
    { name: string; folder: string; trigger: string }
  >;
}

const MAX_BODY_BYTES = 1_048_576; // 1 MiB

function readBody(req: http.IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    let body = '';
    let bytes = 0;
    req.on('data', (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > MAX_BODY_BYTES) {
        req.destroy();
        reject(new Error('Request body too large'));
        return;
      }
      body += chunk.toString();
    });
    req.on('end', () => resolve(body));
    req.on('error', reject);
  });
}

function jsonResponse(
  res: http.ServerResponse,
  statusCode: number,
  data: unknown,
): void {
  const body = JSON.stringify(data);
  res.writeHead(statusCode, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(body),
  });
  res.end(body);
}

export function startHostCallbackServer(
  port: number,
  deps: HostCallbackDeps,
  host = '127.0.0.1',
): http.Server {
  const server = http.createServer(async (req, res) => {
    const url = req.url ?? '';
    const method = req.method ?? '';

    // Health check
    if (method === 'GET' && url === '/healthz') {
      jsonResponse(res, 200, { status: 'ok', service: 'intercom-host' });
      return;
    }

    // Registered groups (GET) — returns jid → {name, folder, trigger} map
    if (method === 'GET' && url === '/v1/ipc/registered-groups') {
      jsonResponse(res, 200, deps.getRegisteredGroups());
      return;
    }

    // All other routes are POST with JSON body
    if (method !== 'POST') {
      jsonResponse(res, 405, { error: 'Method not allowed' });
      return;
    }

    let body: string;
    try {
      body = await readBody(req);
    } catch (err) {
      jsonResponse(res, 413, {
        error: err instanceof Error ? err.message : 'Body read error',
      });
      return;
    }

    let data: Record<string, unknown>;
    try {
      data = JSON.parse(body);
    } catch {
      jsonResponse(res, 400, { error: 'Invalid JSON' });
      return;
    }

    try {
      if (url === '/v1/ipc/send-message') {
        const jid = data.chat_jid as string;
        const text = data.text as string;
        const sender = data.sender as string | undefined;
        if (!jid || !text) {
          jsonResponse(res, 400, { error: 'Missing chat_jid or text' });
          return;
        }
        await deps.sendMessage(jid, text, sender);
        jsonResponse(res, 200, { status: 'ok' });
        return;
      }

      if (url === '/v1/ipc/forward-task') {
        const task = data.task as Record<string, unknown>;
        const groupFolder = data.group_folder as string;
        const isMain = data.is_main as boolean;
        if (!task || !groupFolder) {
          jsonResponse(res, 400, {
            error: 'Missing task or group_folder',
          });
          return;
        }
        await deps.forwardTask(task, groupFolder, isMain ?? false);
        jsonResponse(res, 200, { status: 'ok' });
        return;
      }

      jsonResponse(res, 404, { error: `Unknown route: ${url}` });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      logger.error({ url, err: msg }, 'Host callback handler error');
      jsonResponse(res, 500, { error: msg });
    }
  });

  server.listen(port, host, () => {
    logger.info({ port, host }, 'Host callback server listening');
  });

  return server;
}
