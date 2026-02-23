/**
 * NanoClaw Gemini Agent Runner
 * Runs inside a container, receives config via stdin, outputs result to stdout.
 * Uses Google's Code Assist API (cloudcode-pa.googleapis.com) with OAuth.
 *
 * The Gemini CLI's OAuth token has cloud-platform scope which grants access
 * to the Code Assist proxy API, but NOT the standard generativelanguage API
 * or Vertex AI. So we talk directly to the Code Assist endpoint.
 *
 * Protocol is identical to the Claude agent-runner:
 *   Stdin:  ContainerInput JSON
 *   Stdout: ContainerOutput wrapped in OUTPUT markers
 *   IPC:    Follow-up messages via /workspace/ipc/input/
 */

import fs from 'fs';
import { getToolDeclarations, executeTool } from './tools.js';
import { getAccessToken } from './auth.js';
import { buildSystemPrompt } from '../../shared/system-prompt.js';
import {
  ContainerInput,
  writeOutput,
  readStdin,
  log,
} from '../../shared/protocol.js';
import type { IpcContext } from '../../shared/ipc-tools.js';
import {
  drainIpcInput,
  shouldClose,
  waitForIpcMessage,
  IPC_INPUT_DIR,
  IPC_POLL_MS,
} from '../../shared/ipc-input.js';
import { archiveConversation, type ParsedMessage } from '../../shared/session-base.js';

const CODE_ASSIST_ENDPOINT = 'https://cloudcode-pa.googleapis.com/v1internal';
const MODEL = 'gemini-3.1-pro-preview';
const MAX_TOOL_ROUNDS = 50;

// --- Code Assist API types ---

interface ContentPart {
  text?: string;
  thought?: boolean;
  thoughtSignature?: string;
  functionCall?: { name: string; args?: Record<string, unknown> };
  functionResponse?: { name: string; response: unknown };
}

interface Content {
  role: 'user' | 'model';
  parts: ContentPart[];
}

interface CodeAssistRequest {
  model: string;
  project: string;
  request: {
    contents: Content[];
    systemInstruction?: Content;
    tools?: Array<{ functionDeclarations: unknown[] }>;
    generationConfig?: {
      maxOutputTokens?: number;
    };
  };
}

interface CodeAssistCandidate {
  content?: {
    role: string;
    parts?: ContentPart[];
  };
  finishReason?: string;
}

interface CodeAssistResponse {
  response?: {
    candidates?: CodeAssistCandidate[];
    usageMetadata?: {
      promptTokenCount?: number;
      candidatesTokenCount?: number;
      totalTokenCount?: number;
      thoughtsTokenCount?: number;
    };
    modelVersion?: string;
  };
  traceId?: string;
}

interface SessionData {
  contents: Content[];
  systemInstruction: string;
  projectId: string;
}

// --- Session management ---

function loadSession(sessionId: string): SessionData | null {
  const sessionsDir = `/workspace/group/.sessions`;
  const sessionFile = `${sessionsDir}/${sessionId}.json`;
  if (!fs.existsSync(sessionFile)) return null;
  try {
    return JSON.parse(fs.readFileSync(sessionFile, 'utf-8'));
  } catch (err) {
    log(`Failed to load session ${sessionId}: ${err instanceof Error ? err.message : String(err)}`);
    return null;
  }
}

function saveSession(sessionId: string, data: SessionData): void {
  const sessionsDir = `/workspace/group/.sessions`;
  fs.mkdirSync(sessionsDir, { recursive: true });
  fs.writeFileSync(`${sessionsDir}/${sessionId}.json`, JSON.stringify(data));
}

function generateSessionId(): string {
  return `gemini-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

// --- Code Assist API client ---

async function loadCodeAssistProject(accessToken: string): Promise<string> {
  const response = await fetch(`${CODE_ASSIST_ENDPOINT}:loadCodeAssist`, {
    method: 'POST',
    headers: {
      'Authorization': `Bearer ${accessToken}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      metadata: {
        ideType: 'IDE_UNSPECIFIED',
        platform: 'PLATFORM_UNSPECIFIED',
        pluginType: 'GEMINI',
      },
    }),
  });

  if (!response.ok) {
    const text = await response.text();
    throw new Error(`loadCodeAssist failed (${response.status}): ${text}`);
  }

  const data = await response.json() as {
    cloudaicompanionProject?: string;
    currentTier?: { id: string; name: string };
    paidTier?: { id: string; name: string };
  };

  const projectId = data.cloudaicompanionProject;
  if (!projectId) {
    throw new Error('No project ID returned from Code Assist setup');
  }

  const tier = data.paidTier?.name || data.currentTier?.name || 'unknown';
  log(`Code Assist project: ${projectId} (tier: ${tier})`);
  return projectId;
}

async function generateContent(
  accessToken: string,
  projectId: string,
  contents: Content[],
  systemInstruction: string,
  toolDeclarations: unknown[],
): Promise<CodeAssistResponse> {
  const request: CodeAssistRequest = {
    model: MODEL,
    project: projectId,
    request: {
      contents,
      systemInstruction: {
        role: 'user',
        parts: [{ text: systemInstruction }],
      },
      tools: [{ functionDeclarations: toolDeclarations }],
      generationConfig: {
        maxOutputTokens: 16384,
      },
    },
  };

  const response = await fetch(`${CODE_ASSIST_ENDPOINT}:generateContent`, {
    method: 'POST',
    headers: {
      'Authorization': `Bearer ${accessToken}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(request),
  });

  if (!response.ok) {
    const text = await response.text();
    throw new Error(`generateContent failed (${response.status}): ${text}`);
  }

  return await response.json() as CodeAssistResponse;
}

// --- Agent query loop ---

async function runQuery(
  refreshToken: string,
  clientId: string,
  clientSecret: string,
  projectId: string,
  prompt: string,
  contents: Content[],
  systemInstruction: string,
  ipcCtx: IpcContext,
  toolDeclarations: unknown[],
): Promise<{ result: string | null; closedDuringQuery: boolean }> {
  contents.push({ role: 'user', parts: [{ text: prompt }] });

  let closedDuringQuery = false;
  let ipcPolling = true;

  const pollIpc = () => {
    if (!ipcPolling) return;
    if (shouldClose()) {
      log('Close sentinel detected during query');
      closedDuringQuery = true;
      ipcPolling = false;
      return;
    }
    const messages = drainIpcInput();
    if (messages.length > 0) {
      log(`IPC messages during query: ${messages.length} (will process after current query)`);
    }
    setTimeout(pollIpc, IPC_POLL_MS);
  };
  setTimeout(pollIpc, IPC_POLL_MS);

  let result: string | null = null;

  for (let round = 0; round < MAX_TOOL_ROUNDS; round++) {
    if (closedDuringQuery) break;

    log(`Gemini generate round ${round + 1}...`);

    // Get fresh access token (cached if still valid)
    const accessToken = await getAccessToken(refreshToken, clientId, clientSecret);

    const response = await generateContent(
      accessToken,
      projectId,
      contents,
      systemInstruction,
      toolDeclarations,
    );

    const candidate = response.response?.candidates?.[0];
    const parts = candidate?.content?.parts || [];

    // Extract function calls (filter out thought parts)
    const functionCalls = parts.filter(p => p.functionCall);
    const textParts = parts.filter(p => p.text && !p.thought);

    if (functionCalls.length > 0) {
      // Model wants to call tools — add FULL model response to history
      // (preserves thoughtSignature, thought parts, etc. required by Gemini 3+)
      contents.push({ role: 'model', parts: [...parts] });

      // Execute all function calls
      const responseParts: ContentPart[] = await Promise.all(functionCalls.map(async (p) => {
        const name = p.functionCall?.name || 'unknown';
        const args = (p.functionCall?.args || {}) as Record<string, unknown>;
        const toolResult = await executeTool(name, args, ipcCtx);
        return {
          functionResponse: {
            name,
            response: { result: toolResult },
          },
        };
      }));
      contents.push({ role: 'user', parts: responseParts });

      continue;
    }

    // No function calls — extract text response
    result = textParts.map(p => p.text).join('') || null;
    if (result) {
      contents.push({ role: 'model', parts: [{ text: result }] });
    }

    if (response.response?.usageMetadata) {
      const usage = response.response.usageMetadata;
      log(`Tokens — prompt: ${usage.promptTokenCount}, output: ${usage.candidatesTokenCount}, thinking: ${usage.thoughtsTokenCount || 0}, total: ${usage.totalTokenCount}`);
    }
    break;
  }

  ipcPolling = false;
  return { result, closedDuringQuery };
}

// --- Main ---

async function main(): Promise<void> {
  let containerInput: ContainerInput;

  try {
    const stdinData = await readStdin();
    containerInput = JSON.parse(stdinData);
    try { fs.unlinkSync('/tmp/input.json'); } catch { /* may not exist */ }
    log(`Received input for group: ${containerInput.groupFolder}`);
  } catch (err) {
    writeOutput({
      status: 'error',
      result: null,
      error: `Failed to parse input: ${err instanceof Error ? err.message : String(err)}`,
    });
    process.exit(1);
  }

  const secrets = containerInput.secrets || {};

  // Validate required secrets
  const refreshToken = secrets.GEMINI_REFRESH_TOKEN;
  const clientId = secrets.GEMINI_OAUTH_CLIENT_ID;
  const clientSecret = secrets.GEMINI_OAUTH_CLIENT_SECRET;

  if (!refreshToken || !clientId || !clientSecret) {
    writeOutput({
      status: 'error',
      result: null,
      error: 'Missing Gemini OAuth credentials. Need GEMINI_REFRESH_TOKEN, GEMINI_OAUTH_CLIENT_ID, GEMINI_OAUTH_CLIENT_SECRET in .env',
    });
    process.exit(1);
  }

  // Get access token and discover Code Assist project ID
  let accessToken: string;
  let projectId: string;

  try {
    accessToken = await getAccessToken(refreshToken, clientId, clientSecret);
    projectId = await loadCodeAssistProject(accessToken);
  } catch (err) {
    writeOutput({
      status: 'error',
      result: null,
      error: `Auth/setup failed: ${err instanceof Error ? err.message : String(err)}`,
    });
    process.exit(1);
  }

  const ipcCtx: IpcContext = {
    chatJid: containerInput.chatJid,
    groupFolder: containerInput.groupFolder,
    isMain: containerInput.isMain,
  };

  const toolDeclarations = getToolDeclarations(containerInput.isMain);
  const systemInstruction = buildSystemPrompt(
    containerInput.groupFolder,
    containerInput.isMain,
    undefined,
    'gemini',
  );

  // Load or create session
  const sessionId = containerInput.sessionId || generateSessionId();
  const sessionData = containerInput.sessionId
    ? loadSession(containerInput.sessionId)
    : null;

  const contents: Content[] = sessionData?.contents || [];

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

      const queryResult = await runQuery(
        refreshToken,
        clientId,
        clientSecret,
        projectId,
        prompt,
        contents,
        systemInstruction,
        ipcCtx,
        toolDeclarations,
      );

      // Save session after each query
      saveSession(sessionId, { contents, systemInstruction, projectId });

      // Emit result
      writeOutput({
        status: 'success',
        result: queryResult.result,
        newSessionId: sessionId,
      });

      if (queryResult.closedDuringQuery) {
        log('Close sentinel consumed during query, exiting');
        break;
      }

      // Emit session update
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
    const messages: ParsedMessage[] = contents
      .filter(c => c.role === 'user' || c.role === 'model')
      .map(c => ({
        role: (c.role === 'model' ? 'assistant' : 'user') as 'user' | 'assistant',
        content: c.parts
          ?.filter((p): p is ContentPart & { text: string } => typeof p.text === 'string' && !p.thought)
          .map(p => p.text)
          .join('') || '',
      }))
      .filter(m => m.content.length > 0);

    archiveConversation(messages);
  } catch (err) {
    log(`Failed to archive: ${err instanceof Error ? err.message : String(err)}`);
  }
}

main();
