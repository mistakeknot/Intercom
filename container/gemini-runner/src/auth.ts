/**
 * Gemini OAuth authentication.
 * Uses refresh_token + client_id/client_secret to get fresh access tokens.
 * Same approach as the Gemini CLI uses internally.
 */

import { log } from '../../shared/protocol.js';

const TOKEN_ENDPOINT = 'https://oauth2.googleapis.com/token';

interface TokenResponse {
  access_token: string;
  expires_in: number;
  token_type: string;
}

let cachedToken: { accessToken: string; expiresAt: number } | null = null;

/**
 * Get a fresh access token using the OAuth refresh token.
 */
export async function getAccessToken(
  refreshToken: string,
  clientId: string,
  clientSecret: string,
): Promise<string> {
  // Return cached token if still valid (with 60s buffer)
  if (cachedToken && Date.now() < cachedToken.expiresAt - 60_000) {
    return cachedToken.accessToken;
  }

  log('Refreshing Gemini OAuth access token...');

  const body = new URLSearchParams({
    grant_type: 'refresh_token',
    refresh_token: refreshToken,
    client_id: clientId,
    client_secret: clientSecret,
  });

  const response = await fetch(TOKEN_ENDPOINT, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body.toString(),
  });

  if (!response.ok) {
    const text = await response.text();
    throw new Error(`OAuth token refresh failed (${response.status}): ${text}`);
  }

  const data: TokenResponse = await response.json() as TokenResponse;

  cachedToken = {
    accessToken: data.access_token,
    expiresAt: Date.now() + data.expires_in * 1000,
  };

  log('Access token refreshed successfully');
  return cachedToken.accessToken;
}
