//! WebSocket client to the CatSocks broker.
//!
//! Keeps a connection open to `wss://broker/r/<room>` with exponential
//! backoff (capped at 30 s) on every failure. Inbound text frames are
//! delivered to a caller-supplied handler after the stage attempts to
//! decrypt them with its single PSK (the stage name).
//!
//! Outbound encrypted frames flow through an mpsc sender returned by
//! [`spawn`] so the dispatch path can answer `GetState`, broadcast
//! `State` updates, etc.

use anyhow::Context;
use catcast_proto::{decrypt, encrypt, Key, Message, Plaintext, ProtoError};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Inbound, post-decrypt message addressed to this stage.
pub type InboundHandler = Arc<dyn Fn(Plaintext) + Send + Sync>;

/// Reason the inner `connect_and_pump` loop exited. Used by `run` to decide
/// whether to honour the backoff timer or reconnect immediately.
enum Disconnect {
    /// The outbound sender was dropped — propagate a clean shutdown.
    Shutdown,
    /// The operator (via catcast://about) asked us to drop and re-establish.
    /// Skip backoff and try again straight away.
    Forced,
    /// Real connection error. Apply backoff and retry.
    Error(anyhow::Error),
}

/// Spawn the socks client task.
///
/// - `url`: broker WebSocket URL.
/// - `name`: this stage's name (also the PSK).
/// - `key`: pre-derived AEAD key for the stage's PSK.
/// - `handler`: called once per inbound envelope that decrypts cleanly and
///   matches this stage's `target`. Failed decrypts, replays, wrong-target
///   messages are dropped silently per the threat model.
///
/// The returned sender accepts outbound [`Message`] values — the client
/// encrypts them with `key`, addressed to `name`, and pushes them on the
/// socket. While disconnected, messages queue in the bounded channel and
/// drain on reconnect.
pub fn spawn(
    url: String,
    name: String,
    key: Arc<Key>,
    handler: InboundHandler,
    abort: Arc<Notify>,
) -> mpsc::Sender<Message> {
    let (tx, rx) = mpsc::channel::<Message>(64);
    tokio::spawn(run(url, name, key, handler, rx, abort));
    tx
}

async fn run(
    url: String,
    name: String,
    key: Arc<Key>,
    handler: InboundHandler,
    mut rx: mpsc::Receiver<Message>,
    abort: Arc<Notify>,
) {
    let mut backoff = Duration::from_millis(500);
    loop {
        match connect_and_pump(&url, &name, &key, &handler, &mut rx, &abort).await {
            Disconnect::Shutdown => {
                tracing::info!("socks: channel closed, exiting");
                return;
            }
            Disconnect::Forced => {
                eprintln!("socks: force-reconnect requested; reconnecting now");
                backoff = Duration::from_millis(500);
            }
            Disconnect::Error(e) => {
                tracing::warn!("socks: connection error: {e:#}; reconnecting in {backoff:?}");
                eprintln!("socks: {e:#}; reconnecting in {backoff:?}");
                // Sleep with backoff, but cut short if a force-reconnect is
                // requested while we're waiting.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = abort.notified() => {
                        eprintln!("socks: force-reconnect during backoff; retrying immediately");
                        backoff = Duration::from_millis(500);
                        continue;
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn connect_and_pump(
    url: &str,
    name: &str,
    key: &Key,
    handler: &InboundHandler,
    rx: &mut mpsc::Receiver<Message>,
    abort: &Notify,
) -> Disconnect {
    let (ws, _resp) = match tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("connecting to {url}"))
    {
        Ok(c) => c,
        Err(e) => return Disconnect::Error(e),
    };
    tracing::info!("socks: connected to {url}");
    eprintln!("socks: connected to {url}");
    let (mut sink, mut stream) = ws.split();

    loop {
        tokio::select! {
            _ = abort.notified() => {
                let _ = sink.close().await;
                return Disconnect::Forced;
            }
            ws_msg = stream.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        match decrypt(key, text.as_str()) {
                            Ok(pt) if pt.target == name => handler(pt),
                            Ok(_) => { /* wrong target → drop silently */ }
                            Err(ProtoError::Decrypt) | Err(ProtoError::Replay { .. })
                            | Err(ProtoError::WrongTarget { .. }) => { /* drop silently */ }
                            Err(e) => {
                                tracing::debug!("socks: unparseable envelope dropped: {e}");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Binary(_))) => {
                        // Broker speaks text frames only — drop.
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        let _ = sink.send(WsMessage::Pong(p)).await;
                    }
                    Some(Ok(WsMessage::Pong(_))) | Some(Ok(WsMessage::Frame(_))) => {}
                    Some(Ok(WsMessage::Close(_))) | None => {
                        return Disconnect::Error(anyhow::anyhow!("socket closed by peer"));
                    }
                    Some(Err(e)) => {
                        return Disconnect::Error(anyhow::anyhow!("ws error: {e}"));
                    }
                }
            }
            outbound = rx.recv() => {
                let Some(msg) = outbound else {
                    let _ = sink.close().await;
                    return Disconnect::Shutdown;
                };
                let env = match encrypt(key, name, &msg)
                    .context("encrypting outbound message")
                {
                    Ok(e) => e,
                    Err(e) => return Disconnect::Error(e),
                };
                if let Err(e) = sink.send(WsMessage::Text(env.into())).await {
                    return Disconnect::Error(anyhow::anyhow!("send failed: {e}"));
                }
            }
        }
    }
}
