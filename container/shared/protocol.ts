/**
 * Shared protocol types and IO helpers for Intercom container agents.
 * All runtimes (Claude, Gemini, Codex) speak this same protocol.
 *
 * Output channel: prefers Unix domain socket (length-prefixed frames)
 * when available at /workspace/ipc/output.sock, falls back to stdout
 * markers for backward compatibility with older host versions.
 */

import net from 'net';

export interface ContainerInput {
  prompt: string;
  sessionId?: string;
  groupFolder: string;
  chatJid: string;
  isMain: boolean;
  isScheduledTask?: boolean;
  assistantName?: string;
  model?: string;
  secrets?: Record<string, string>;
  previousContext?: string;
}

export interface StreamEvent {
  type: 'tool_start' | 'text_delta';
  toolName?: string;   // for tool_start: 'Bash', 'Read', etc.
  toolInput?: string;  // for tool_start: truncated input summary
  text?: string;       // for text_delta: text content
}

export interface ContainerOutput {
  status: 'success' | 'error';
  result: string | null;
  newSessionId?: string;
  error?: string;
  model?: string;
  event?: StreamEvent;
}

export const OUTPUT_START_MARKER = '---INTERCOM_OUTPUT_START---';
export const OUTPUT_END_MARKER = '---INTERCOM_OUTPUT_END---';

// --- UDS output channel ---

const UDS_SOCKET_PATH = '/workspace/ipc/output.sock';
const MAX_FRAME_SIZE = 4 * 1024 * 1024; // 4 MiB

let udsSocket: net.Socket | null = null;

/**
 * Try to connect to the host's UDS output socket.
 * Returns true if connected, false if unavailable (fall back to stdout).
 */
export async function initUdsOutput(): Promise<boolean> {
  return new Promise((resolve) => {
    const sock = new net.Socket();
    const timeout = setTimeout(() => {
      sock.destroy();
      resolve(false);
    }, 2000);

    sock.connect(UDS_SOCKET_PATH, () => {
      clearTimeout(timeout);
      udsSocket = sock;
      sock.on('error', (err) => {
        log(`UDS socket error: ${err.message}`);
        udsSocket = null;
      });
      sock.on('close', () => {
        udsSocket = null;
      });
      log('UDS output connected');
      resolve(true);
    });

    sock.on('error', () => {
      clearTimeout(timeout);
      log('UDS socket not available, using stdout fallback');
      resolve(false);
    });
  });
}

/**
 * Write a length-prefixed frame to the UDS socket.
 * Frame format: 4-byte big-endian length + UTF-8 JSON payload.
 * Returns true if written, false if socket unavailable.
 */
function writeUdsFrame(json: string): boolean {
  if (!udsSocket || udsSocket.destroyed) {
    return false;
  }

  const payload = Buffer.from(json, 'utf8');
  if (payload.length > MAX_FRAME_SIZE) {
    log(`UDS frame too large (${payload.length} bytes), falling back to stdout`);
    return false;
  }

  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length, 0);

  try {
    udsSocket.write(header);
    udsSocket.write(payload);
    return true;
  } catch {
    log('UDS write failed, falling back to stdout');
    udsSocket = null;
    return false;
  }
}

/**
 * Close the UDS output socket cleanly.
 */
export function closeUdsOutput(): void {
  if (udsSocket && !udsSocket.destroyed) {
    udsSocket.end();
    udsSocket = null;
  }
}

/**
 * Write output to the host. Tries UDS first, falls back to stdout markers.
 */
export function writeOutput(output: ContainerOutput): void {
  const json = JSON.stringify(output);

  if (writeUdsFrame(json)) {
    return;
  }

  // Fallback: stdout markers
  console.log(OUTPUT_START_MARKER);
  console.log(json);
  console.log(OUTPUT_END_MARKER);
}

export function log(message: string): void {
  console.error(`[agent-runner] ${message}`);
}

export async function readStdin(): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (chunk: string) => { data += chunk; });
    process.stdin.on('end', () => resolve(data));
    process.stdin.on('error', reject);
  });
}
