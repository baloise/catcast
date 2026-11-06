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
//!
//! A relay or mirror stage also holds a second key, the frame key (see
//! [`FrameLink`]). Messages under it are tagged [`Via::Frame`] so dispatch
//! only accepts relay/mirror traffic that way, never commands: whoever
//! holds the frame secret (say, a thief of the mirror box) cannot steer
//! the relay.

use anyhow::Context;
use catcast_proto::{decrypt, encrypt, Key, Message, Plaintext, ProtoError};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Which key an inbound message decrypted under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The stage's own name-derived key: operator commands.
    Own,
    /// The relay/mirror frame key: frames and tile requests only.
    Frame,
}

/// Inbound, post-decrypt message addressed to this stage.
pub type InboundHandler = Arc<dyn Fn(Plaintext, Via) + Send + Sync>;
pub type ConnectionHandler = Arc<dyn Fn(bool) + Send + Sync>;

/// What the stage sends: its own messages under its own key, or relay /
/// mirror traffic under the frame key.
#[derive(Debug, Clone)]
pub enum Outbound {
    Own(Message),
    Frame(Message),
}

/// The frame key and the name frames are addressed to (always the mirror's
/// name: the relay sends to it, the mirror answers under it).
pub struct FrameLink {
    pub key: Arc<Key>,
    pub target: String,
}

/// Set by `SetRelay` / `SetMirror` at runtime, read per message.
pub type FrameSlot = Arc<std::sync::RwLock<Option<FrameLink>>>;

/// The outbound sender, `None` until the socks task is up and always in
/// offline modes.
pub type OutSlot = Arc<std::sync::Mutex<Option<mpsc::Sender<Outbound>>>>;

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
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    url: String,
    name: String,
    key: Arc<Key>,
    frame: FrameSlot,
    handler: InboundHandler,
    abort: Arc<Notify>,
    on_connection: ConnectionHandler,
) -> mpsc::Sender<Outbound> {
    let (tx, rx) = mpsc::channel::<Outbound>(64);
    let link = Link {
        name,
        key,
        frame,
        handler,
    };
    tokio::spawn(run(url, link, rx, abort, on_connection));
    tx
}

/// Everything the pump needs to decrypt and encrypt.
struct Link {
    name: String,
    key: Arc<Key>,
    frame: FrameSlot,
    handler: InboundHandler,
}

impl Link {
    fn inbound(&self, text: &str) {
        match decrypt(&self.key, text) {
            Ok(pt) if pt.target == self.name => return (self.handler)(pt, Via::Own),
            Ok(_) => return, // wrong target → drop silently
            Err(ProtoError::Decrypt) => {}
            Err(ProtoError::Replay { .. }) | Err(ProtoError::WrongTarget { .. }) => return,
            Err(e) => {
                tracing::debug!("socks: unparseable envelope dropped: {e}");
                return;
            }
        }
        let guard = self.frame.read().expect("frame slot poisoned");
        let Some(link) = guard.as_ref() else { return };
        if let Ok(pt) = decrypt(&link.key, text) {
            if pt.target == link.target {
                drop(guard);
                (self.handler)(pt, Via::Frame);
            }
        }
    }

    /// Encrypt one outbound message; `None` for frame traffic while no
    /// frame key is set (dropped).
    fn seal(&self, out: &Outbound) -> Option<anyhow::Result<String>> {
        match out {
            Outbound::Own(msg) => Some(encrypt(&self.key, &self.name, msg).map_err(Into::into)),
            Outbound::Frame(msg) => {
                let guard = self.frame.read().expect("frame slot poisoned");
                let link = guard.as_ref()?;
                Some(encrypt(&link.key, &link.target, msg).map_err(Into::into))
            }
        }
    }
}

async fn run(
    url: String,
    link: Link,
    mut rx: mpsc::Receiver<Outbound>,
    abort: Arc<Notify>,
    on_connection: ConnectionHandler,
) {
    let mut backoff = Duration::from_millis(500);
    on_connection(false);
    loop {
        match connect_and_pump(&url, &link, &mut rx, &abort, &on_connection).await {
            Disconnect::Shutdown => {
                on_connection(false);
                tracing::info!("socks: channel closed, exiting");
                return;
            }
            Disconnect::Forced => {
                on_connection(false);
                eprintln!("socks: force-reconnect requested; reconnecting now");
                backoff = Duration::from_millis(500);
            }
            Disconnect::Error(e) => {
                on_connection(false);
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
    link: &Link,
    rx: &mut mpsc::Receiver<Outbound>,
    abort: &Notify,
    on_connection: &ConnectionHandler,
) -> Disconnect {
    let (ws, _resp) = match connect_ws(url)
        .await
        .with_context(|| format!("connecting to {url}"))
    {
        Ok(c) => c,
        Err(e) => return Disconnect::Error(e),
    };
    tracing::info!("socks: connected to {url}");
    eprintln!("socks: connected to {url}");
    on_connection(true);
    // `run` updates the UI status; this function only owns the socket pump.
    let (mut sink, mut stream) = ws.split();

    loop {
        tokio::select! {
            _ = abort.notified() => {
                let _ = sink.close().await;
                return Disconnect::Forced;
            }
            ws_msg = stream.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => link.inbound(text.as_str()),
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
                let env = match link.seal(&msg) {
                    None => continue,
                    Some(Ok(e)) => e,
                    Some(Err(e)) => {
                        return Disconnect::Error(e.context("encrypting outbound message"))
                    }
                };
                if let Err(e) = sink.send(WsMessage::Text(env.into())).await {
                    return Disconnect::Error(anyhow::anyhow!("send failed: {e}"));
                }
            }
        }
    }
}

async fn connect_ws(
    url: &str,
) -> anyhow::Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    catcast_net::connect(url).await
}
