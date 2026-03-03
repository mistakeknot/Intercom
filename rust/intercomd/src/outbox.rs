//! Outbox drain loop + LISTEN/NOTIFY integration.
//!
//! The outbox is a durable write path: Node writes messages to `message_outbox`,
//! Postgres fires NOTIFY, Rust drains rows and dispatches to the GroupQueue.
//!
//! Design: the drain is a pure write path. It claims outbox rows, stores them
//! in the destination table, then hands off to `queue.enqueue_message_check()`.
//! All dispatch logic (trigger checking, cursor advancement, context accumulation)
//! stays in `process_group_messages()`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use intercom_core::{NewMessage, PgPool};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_postgres::NoTls;
use tracing::{debug, error, info, warn};

use crate::queue::GroupQueue;

/// Typed payload for chat_metadata outbox rows.
#[derive(Debug, Deserialize)]
struct ChatMetadataPayload {
    jid: String,
    timestamp: String,
    name: Option<String>,
    channel: Option<String>,
    is_group: Option<bool>,
}

/// Run the outbox drain loop. Exits when shutdown signal fires.
pub async fn run_outbox_drain(
    pool: PgPool,
    queue: Arc<GroupQueue>,
    mut drain_signal: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Recover any stale 'processing' rows from a prior crash
    match pool.recover_stale_outbox_rows().await {
        Ok(count) if count > 0 => info!(count, "recovered stale outbox rows"),
        Ok(_) => {}
        Err(e) => warn!(err = %e, "failed to recover stale outbox rows"),
    }

    let fallback_interval = Duration::from_secs(30);

    loop {
        // Wait for drain signal or fallback timeout
        tokio::select! {
            _ = drain_signal.recv() => {
                debug!("outbox drain signaled by LISTEN");
            }
            _ = tokio::time::sleep(fallback_interval) => {
                debug!("outbox drain fallback poll");
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("outbox drain shutting down");
                    return;
                }
            }
        }

        // Drain loop: keep claiming until no rows remain
        loop {
            let rows = match pool.claim_outbox_rows(10).await {
                Ok(rows) => rows,
                Err(e) => {
                    error!(err = %e, "failed to claim outbox rows");
                    break;
                }
            };

            if rows.is_empty() {
                break;
            }

            info!(count = rows.len(), "claimed outbox rows");

            for row in &rows {
                match row.payload_type.as_str() {
                    "message" => {
                        match serde_json::from_value::<NewMessage>(row.payload.clone()) {
                            Ok(msg) => {
                                let chat_jid = msg.chat_jid.clone();
                                match pool.store_message(&msg).await {
                                    Ok(()) => {
                                        if let Err(e) = pool.mark_outbox_delivered(row.id).await {
                                            error!(id = row.id, err = %e, "failed to mark outbox delivered");
                                        }
                                        queue.enqueue_message_check(&chat_jid).await;
                                    }
                                    Err(e) => {
                                        warn!(id = row.id, err = %e, "transient error storing message");
                                        if let Err(e2) = pool.mark_outbox_retry(row.id, &e.to_string()).await {
                                            error!(id = row.id, err = %e2, "failed to mark outbox retry");
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!(id = row.id, err = %e, "permanent: failed to deserialize message payload");
                                if let Err(e2) = pool.mark_outbox_failed(row.id, &e.to_string()).await {
                                    error!(id = row.id, err = %e2, "failed to mark outbox failed");
                                }
                            }
                        }
                    }
                    "chat_metadata" => {
                        let meta = match serde_json::from_value::<ChatMetadataPayload>(row.payload.clone()) {
                            Ok(m) => m,
                            Err(e) => {
                                error!(id = row.id, err = %e, "permanent: failed to deserialize chat_metadata payload");
                                let _ = pool.mark_outbox_failed(row.id, &e.to_string()).await;
                                continue;
                            }
                        };

                        match pool.store_chat_metadata(&meta.jid, &meta.timestamp, meta.name.as_deref(), meta.channel.as_deref(), meta.is_group).await {
                            Ok(()) => {
                                if let Err(e) = pool.mark_outbox_delivered(row.id).await {
                                    error!(id = row.id, err = %e, "failed to mark outbox delivered");
                                }
                            }
                            Err(e) => {
                                warn!(id = row.id, err = %e, "transient error storing chat metadata");
                                if let Err(e2) = pool.mark_outbox_retry(row.id, &e.to_string()).await {
                                    error!(id = row.id, err = %e2, "failed to mark outbox retry");
                                }
                            }
                        }
                    }
                    other => {
                        error!(id = row.id, payload_type = other, "permanent: unknown payload_type");
                        let _ = pool.mark_outbox_failed(row.id, &format!("unknown payload_type: {other}")).await;
                    }
                }
            }
        }
    }
}

/// Redact password from DSN for safe logging.
fn redact_dsn(dsn: &str) -> String {
    // postgres://user:password@host/db → postgres://user:***@host/db
    let Some(scheme_end) = dsn.find("://") else {
        return dsn.to_string();
    };
    let Some(at_pos) = dsn.find('@') else {
        return dsn.to_string();
    };
    // Only search for the password colon in the userinfo segment (after :// and before @)
    let userinfo = &dsn[scheme_end + 3..at_pos];
    if let Some(colon_in_userinfo) = userinfo.find(':') {
        let abs_colon = scheme_end + 3 + colon_in_userinfo;
        let mut redacted = dsn[..abs_colon + 1].to_string();
        redacted.push_str("***");
        redacted.push_str(&dsn[at_pos..]);
        return redacted;
    }
    // No password in userinfo (e.g. postgres://user@host:5432/db) — nothing to redact
    dsn.to_string()
}

/// Run the LISTEN loop. Maintains a dedicated Postgres connection for LISTEN
/// on the `intercom_outbox` channel. On notification, signals the drain loop.
pub async fn run_listen_loop(
    dsn: String,
    drain_tx: mpsc::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        // Check shutdown before connecting
        if *shutdown.borrow() {
            info!("LISTEN loop shutting down");
            return;
        }

        info!(dsn = %redact_dsn(&dsn), "LISTEN loop connecting");

        let (client, mut connection) = match tokio_postgres::connect(&dsn, NoTls).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(err = %e, backoff_secs = backoff.as_secs(), "LISTEN connect failed, retrying");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return; }
                    }
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        // Reset backoff on successful connect
        backoff = Duration::from_secs(1);

        // Convert the Connection into a Stream of AsyncMessage via poll_message.
        // This drives the protocol and yields notices/notifications.
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
        let conn_stream = futures::stream::poll_fn(move |cx| {
            connection.poll_message(cx)
        });
        let conn_handle = tokio::spawn(async move {
            tokio::pin!(conn_stream);
            while let Some(item) = conn_stream.next().await {
                if msg_tx.send(item).is_err() {
                    break; // receiver dropped
                }
            }
        });

        // Issue LISTEN
        if let Err(e) = client.execute("LISTEN intercom_outbox", &[]).await {
            warn!(err = %e, "LISTEN command failed");
            conn_handle.abort();
            continue;
        }

        info!("LISTEN active on intercom_outbox");

        // Signal an initial drain in case rows accumulated before LISTEN was established
        let _ = drain_tx.try_send(());

        // Poll for notifications
        loop {
            tokio::select! {
                msg = msg_rx.recv() => {
                    match msg {
                        Some(Ok(msg)) => {
                            if let tokio_postgres::AsyncMessage::Notification(_) = msg {
                                let _ = drain_tx.try_send(());
                            }
                        }
                        Some(Err(e)) => {
                            warn!(err = %e, "LISTEN connection error");
                            break;
                        }
                        None => {
                            warn!("LISTEN connection closed");
                            break;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("LISTEN loop shutting down");
                        conn_handle.abort();
                        return;
                    }
                }
            }
        }

        // Abort the connection driver task before reconnecting
        conn_handle.abort();

        // Connection dropped — retry with backoff
        warn!(backoff_secs = backoff.as_secs(), "LISTEN reconnecting");
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Run the outbox cleanup loop. Deletes old delivered rows periodically.
pub async fn run_outbox_cleanup(pool: PgPool, mut shutdown: watch::Receiver<bool>) {
    let interval = Duration::from_secs(3600); // 1 hour

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                match pool.cleanup_outbox(7).await {
                    Ok(count) if count > 0 => info!(deleted = count, "outbox cleanup"),
                    Ok(_) => {}
                    Err(e) => warn!(err = %e, "outbox cleanup failed"),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("outbox cleanup shutting down");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_dsn_with_password() {
        assert_eq!(
            redact_dsn("postgres://user:secret@localhost:5432/db"),
            "postgres://user:***@localhost:5432/db"
        );
    }

    #[test]
    fn redact_dsn_without_password() {
        // No password segment — should return unchanged
        assert_eq!(
            redact_dsn("postgres://localhost:5432/db"),
            "postgres://localhost:5432/db"
        );
    }

    #[test]
    fn redact_dsn_empty() {
        assert_eq!(redact_dsn(""), "");
    }

    #[test]
    fn redact_dsn_user_no_password_with_port() {
        // user@host:port — port colon must NOT be treated as password separator
        assert_eq!(
            redact_dsn("postgres://user@localhost:5432/db"),
            "postgres://user@localhost:5432/db"
        );
    }
}
