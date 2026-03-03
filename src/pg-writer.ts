/**
 * Direct Postgres outbox writer for durable message delivery.
 *
 * Node writes incoming messages to the `message_outbox` table instead of
 * fire-and-forget HTTP POST to intercomd. Postgres NOTIFY fires automatically
 * via trigger, and the Rust outbox drain picks up rows within ~50ms.
 *
 * Uses pg.Pool (max 1 connection) for automatic reconnection and
 * concurrency safety — concurrent writeToOutbox calls are safely queued.
 */

import { Pool } from 'pg';

import { logger } from './logger.js';

let pgPool: Pool | null = null;

/**
 * Initialize the Postgres outbox writer.
 * Creates a pool with max 1 connection — handles reconnection automatically.
 */
export async function initPgWriter(dsn: string): Promise<void> {
  if (!dsn || dsn.trim() === '') {
    logger.warn('INTERCOM_POSTGRES_DSN not set — outbox writer disabled');
    return;
  }

  pgPool = new Pool({
    connectionString: dsn,
    max: 1,
    idleTimeoutMillis: 0, // keep connection alive
    connectionTimeoutMillis: 5000,
  });

  pgPool.on('error', (err) => {
    logger.warn({ err: err.message }, 'pg-writer pool error');
  });

  // Verify connectivity at startup
  try {
    const client = await pgPool.connect();
    client.release();
    logger.info('pg-writer connected to Postgres');
  } catch (err) {
    logger.warn(
      { err: (err as Error).message },
      'pg-writer initial connect failed — pool will retry automatically',
    );
  }
}

/**
 * Write a payload to the message outbox for durable delivery.
 * Returns true if the write succeeded, false otherwise.
 * Safe for concurrent calls — Pool handles queuing.
 */
export async function writeToOutbox(
  chatJid: string,
  payloadType: 'message' | 'chat_metadata',
  payload: unknown,
): Promise<boolean> {
  if (!pgPool) return false;

  try {
    await pgPool.query(
      'INSERT INTO message_outbox (chat_jid, payload_type, payload) VALUES ($1, $2, $3)',
      [chatJid, payloadType, payload],
    );
    return true;
  } catch (err) {
    logger.warn(
      { err: (err as Error).message, chatJid, payloadType },
      'outbox write failed',
    );
    return false;
  }
}

/**
 * Check if the Postgres writer is configured.
 */
export function isPgWriterConnected(): boolean {
  return pgPool !== null;
}
