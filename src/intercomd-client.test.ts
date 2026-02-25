import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  editTelegramViaIntercomd,
  routeTelegramIngress,
  sendTelegramViaIntercomd,
} from './intercomd-client.js';

const originalFetch = globalThis.fetch;

describe('intercomd-client', () => {
  afterEach(() => {
    globalThis.fetch = originalFetch;
    vi.restoreAllMocks();
  });

  it('parses Telegram ingress responses from intercomd', async () => {
    globalThis.fetch = vi.fn(async () =>
      new Response(
        JSON.stringify({
          accepted: true,
          reason: null,
          normalized_content: '@Andy hello',
          group_name: 'Main',
          group_folder: 'main',
          runtime: 'claude',
          model: 'claude-opus-4-6',
          parity: {
            trigger_required: false,
            trigger_present: true,
            runtime_profile_found: true,
            runtime_fallback_used: false,
            model_fallback_used: true,
          },
        }),
        { status: 200, headers: { 'Content-Type': 'application/json' } },
      ),
    ) as typeof fetch;

    const response = await routeTelegramIngress({
      chat_jid: 'tg:1',
      message_id: '123',
      content: '@Andy hello',
      timestamp: '2026-02-25T00:00:00Z',
    });

    expect(response?.accepted).toBe(true);
    expect(response?.runtime).toBe('claude');
  });

  it('returns null on non-2xx responses for send endpoint', async () => {
    globalThis.fetch = vi.fn(async () => new Response('error', { status: 500 })) as typeof fetch;

    const response = await sendTelegramViaIntercomd({
      jid: 'tg:1',
      text: 'hello',
    });

    expect(response).toBeNull();
  });

  it('returns null when the edit endpoint throws', async () => {
    globalThis.fetch = vi.fn(async () => {
      throw new Error('network down');
    }) as typeof fetch;

    const response = await editTelegramViaIntercomd({
      jid: 'tg:1',
      message_id: '10',
      text: 'edit',
    });

    expect(response).toBeNull();
  });
});
