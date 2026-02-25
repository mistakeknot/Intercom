/**
 * Intercom Codex Agent Runner
 * Runs inside a container, receives config via stdin, outputs result to stdout.
 * Wraps `codex exec` CLI for non-interactive agent execution.
 *
 * Protocol is identical to the Claude agent-runner:
 *   Stdin:  ContainerInput JSON
 *   Stdout: ContainerOutput wrapped in OUTPUT markers
 *   IPC:    Follow-up messages via /workspace/ipc/input/
 */

import fs from 'fs';
import { spawn } from 'child_process';
import { buildSystemPrompt } from '../../shared/system-prompt.js';
import {
  ContainerInput,
  writeOutput,
  readStdin,
  log,
} from '../../shared/protocol.js';
import {
  drainIpcInput,
  shouldClose,
  waitForIpcMessage,
  IPC_INPUT_DIR,
} from '../../shared/ipc-input.js';
import { archiveConversation, type ParsedMessage } from '../../shared/session-base.js';

let MODEL = 'gpt-5.3-codex';

function generateSessionId(): string {
  return `codex-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

/**
 * Run a single query via `codex exec`.
 * Returns the agent's text response.
 */
async function runCodexExec(
  prompt: string,
  workDir: string,
): Promise<string> {

  const outputFile = '/tmp/codex-output.txt';
  try { fs.unlinkSync(outputFile); } catch { /* ignore */ }

  return new Promise<string>((resolve, reject) => {
    const args = [
      'exec',
      '--skip-git-repo-check',
      '--ephemeral',
      '--dangerously-bypass-approvals-and-sandbox',
      '-C', workDir,
      '-m', MODEL,
      '-o', outputFile,
      '-', // read prompt from stdin
    ];

    log(`Running: codex ${args.join(' ')}`);

    const child = spawn('codex', args, {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: {
        ...process.env,
        // Codex picks up auth from ~/.codex/auth.json (mounted into container)
        HOME: '/home/node',
      },
    });

    let stderr = '';

    child.stdout?.on('data', (data: Buffer) => {
      // codex exec with -o writes to file, stdout may have progress
      log(`codex stdout: ${data.toString().trim()}`);
    });

    child.stderr?.on('data', (data: Buffer) => {
      stderr += data.toString();
    });

    child.on('error', (err) => {
      reject(new Error(`Failed to spawn codex: ${err.message}`));
    });

    child.on('close', (code) => {
      if (code !== 0) {
        reject(new Error(`codex exec exited with code ${code}: ${stderr.trim()}`));
        return;
      }

      try {
        const output = fs.readFileSync(outputFile, 'utf-8').trim();
        resolve(output);
      } catch {
        // If output file doesn't exist, try stderr for error info
        reject(new Error(`No output from codex exec. stderr: ${stderr.trim()}`));
      }
    });

    // Send prompt via stdin
    child.stdin?.write(prompt);
    child.stdin?.end();
  });
}

async function main(): Promise<void> {
  let containerInput: ContainerInput;

  try {
    const stdinData = await readStdin();
    containerInput = JSON.parse(stdinData);
    try { fs.unlinkSync('/tmp/input.json'); } catch { /* may not exist */ }
    log(`Received input for group: ${containerInput.groupFolder}`);
    if (containerInput.model) {
      MODEL = containerInput.model;
      log(`Using model from host: ${MODEL}`);
    }
  } catch (err) {
    writeOutput({
      status: 'error',
      result: null,
      error: `Failed to parse input: ${err instanceof Error ? err.message : String(err)}`,
    });
    process.exit(1);
  }

  // Set up Codex auth from secrets
  const secrets = containerInput.secrets || {};
  const refreshToken = secrets.CODEX_OAUTH_REFRESH_TOKEN;
  const accessToken = secrets.CODEX_OAUTH_ACCESS_TOKEN;

  if (!refreshToken && !accessToken) {
    writeOutput({
      status: 'error',
      result: null,
      error: 'Missing Codex OAuth credentials. Need CODEX_OAUTH_REFRESH_TOKEN or CODEX_OAUTH_ACCESS_TOKEN in .env',
    });
    process.exit(1);
  }

  // Write Codex auth file so the CLI can find it
  const codexHome = '/home/node/.codex';
  fs.mkdirSync(codexHome, { recursive: true });
  const authData = {
    OPENAI_API_KEY: null,
    tokens: {
      id_token: secrets.CODEX_OAUTH_ID_TOKEN || '',
      access_token: accessToken || '',
      refresh_token: refreshToken || '',
      account_id: secrets.CODEX_OAUTH_ACCOUNT_ID || '',
    },
    last_refresh: new Date().toISOString(),
  };
  fs.writeFileSync(`${codexHome}/auth.json`, JSON.stringify(authData));

  const systemPrompt = buildSystemPrompt(
    containerInput.groupFolder,
    containerInput.isMain,
    undefined,
    'codex',
  );

  const sessionId = containerInput.sessionId || generateSessionId();
  const conversationHistory: ParsedMessage[] = [];
  const workDir = '/workspace/group';

  // Write system prompt as AGENTS.md so codex exec picks it up automatically
  fs.writeFileSync(`${workDir}/AGENTS.md`, systemPrompt);

  fs.mkdirSync(IPC_INPUT_DIR, { recursive: true });
  try { fs.unlinkSync('/workspace/ipc/input/_close'); } catch { /* ignore */ }

  // Build initial prompt
  let prompt = containerInput.prompt;
  if (containerInput.isScheduledTask) {
    prompt = `[SCHEDULED TASK - The following message was sent automatically and is not coming directly from the user or group.]\n\n${prompt}`;
  }
  const pending = drainIpcInput();
  if (pending.length > 0) {
    log(`Draining ${pending.length} pending IPC messages into initial prompt`);
    prompt += '\n' + pending.join('\n');
  }

  // Announce model to host
  writeOutput({ status: 'success', result: null, model: MODEL });

  // Query loop
  try {
    while (true) {
      log(`Starting query (session: ${sessionId})...`);

      conversationHistory.push({ role: 'user', content: prompt });

      const result = await runCodexExec(prompt, workDir);

      conversationHistory.push({ role: 'assistant', content: result });

      writeOutput({
        status: 'success',
        result,
        newSessionId: sessionId,
      });

      if (shouldClose()) {
        log('Close sentinel detected, exiting');
        break;
      }

      writeOutput({ status: 'success', result: null, newSessionId: sessionId });

      log('Query ended, waiting for next IPC message...');
      const nextMessage = await waitForIpcMessage();
      if (nextMessage === null) {
        log('Close sentinel received, exiting');
        break;
      }

      log(`Got new message (${nextMessage.length} chars), starting new query`);
      prompt = nextMessage;
    }
  } catch (err) {
    const errorMessage = err instanceof Error ? err.message : String(err);
    log(`Agent error: ${errorMessage}`);
    writeOutput({
      status: 'error',
      result: null,
      newSessionId: sessionId,
      error: errorMessage,
    });
    process.exit(1);
  }

  // Archive conversation
  try {
    archiveConversation(conversationHistory.filter(m => m.content.length > 0));
  } catch (err) {
    log(`Failed to archive: ${err instanceof Error ? err.message : String(err)}`);
  }
}

main();
