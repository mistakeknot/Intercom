import { INTERCOMD_URL } from './config.js';
import { logger } from './logger.js';

const REQUEST_TIMEOUT_MS = 5000;

export interface TelegramIngressRequest {
  chat_jid: string;
  chat_name?: string;
  chat_type?: string;
  message_id: string;
  sender_id?: string;
  sender_name?: string;
  content: string;
  timestamp: string;
  persist?: boolean;
}

export interface TelegramIngressResponse {
  accepted: boolean;
  reason?: string | null;
  normalized_content: string;
  group_name?: string | null;
  group_folder?: string | null;
  runtime?: string | null;
  model?: string | null;
  parity: {
    trigger_required: boolean;
    trigger_present: boolean;
    runtime_profile_found: boolean;
    runtime_fallback_used: boolean;
    model_fallback_used: boolean;
  };
}

export interface TelegramSendRequest {
  jid: string;
  text: string;
}

export interface TelegramSendResponse {
  ok: boolean;
  error?: string | null;
  message_ids: string[];
  chunks_planned: number;
  chunks_sent: number;
  chunk_lengths: number[];
  parity: {
    max_chars_per_chunk: number;
    all_chunks_within_limit: boolean;
  };
}

export interface TelegramEditRequest {
  jid: string;
  message_id: string;
  text: string;
}

export interface TelegramEditResponse {
  ok: boolean;
  error?: string | null;
  truncated: boolean;
  parity_max_chars: number;
}

async function postJson<T>(endpoint: string, payload: unknown): Promise<T | null> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

  try {
    const response = await fetch(`${INTERCOMD_URL}${endpoint}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
      signal: controller.signal,
    });

    if (!response.ok) {
      const body = await response.text().catch(() => '');
      logger.warn(
        { endpoint, status: response.status, body: body.slice(0, 200) },
        'intercomd request failed',
      );
      return null;
    }

    return (await response.json()) as T;
  } catch (err) {
    logger.warn({ endpoint, err }, 'Failed to call intercomd');
    return null;
  } finally {
    clearTimeout(timeout);
  }
}

export function routeTelegramIngress(
  request: TelegramIngressRequest,
): Promise<TelegramIngressResponse | null> {
  return postJson<TelegramIngressResponse>('/v1/telegram/ingress', request);
}

export function sendTelegramViaIntercomd(
  request: TelegramSendRequest,
): Promise<TelegramSendResponse | null> {
  return postJson<TelegramSendResponse>('/v1/telegram/send', request);
}

export function editTelegramViaIntercomd(
  request: TelegramEditRequest,
): Promise<TelegramEditResponse | null> {
  return postJson<TelegramEditResponse>('/v1/telegram/edit', request);
}

export interface TelegramCallbackRequest {
  callback_query_id: string;
  chat_jid: string;
  message_id: string;
  sender_id?: string;
  sender_name?: string;
  data: string;
}

export interface TelegramCallbackResponse {
  ok: boolean;
  action: string;
  target_id: string;
  result?: string | null;
  error?: string | null;
}

export function routeTelegramCallback(
  request: TelegramCallbackRequest,
): Promise<TelegramCallbackResponse | null> {
  return postJson<TelegramCallbackResponse>('/v1/telegram/callback', request);
}
