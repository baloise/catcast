//! WebSocket plumbing for the CLI.
//!
//! The CLI opens one connection per command, blasts the encrypted command(s),
//! optionally waits a few seconds for replies (e.g. `stage list --probe`),
//! then closes. There is no long-lived state on the broker side.

use anyhow::{anyhow, Context, Result};
use catcast_proto::{decrypt, encrypt, Key, Message, Plaintext};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub struct Broker {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Broker {
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _resp) = connect_ws(url)
            .await
            .with_context(|| format!("connect to broker {url}"))?;
        Ok(Self { ws })
    }

    pub async fn send(&mut self, key: &Key, target: &str, msg: &Message) -> Result<()> {
        let payload = encrypt(key, target, msg)?;
        self.ws.send(WsMessage::Text(payload.into())).await?;
        Ok(())
    }

    /// Listen for `dur`, returning every plaintext that decrypts under any of
    /// `keys`, keyed by the matching stage name.
    pub async fn collect(
        &mut self,
        keys: &HashMap<String, Key>,
        dur: Duration,
    ) -> Vec<(String, Plaintext)> {
        let mut out = Vec::new();
        self.collect_until(keys, dur, |name, pt| {
            out.push((name.to_string(), pt));
            false
        })
        .await;
        out
    }

    /// Like [`collect`](Self::collect), but hands each plaintext to `on_msg`
    /// as it arrives and stops early once `on_msg` returns `true`. Returns
    /// whether it stopped because `on_msg` said so (as opposed to the
    /// deadline or a closed socket).
    pub async fn collect_until<F>(
        &mut self,
        keys: &HashMap<String, Key>,
        dur: Duration,
        mut on_msg: F,
    ) -> bool
    where
        F: FnMut(&str, Plaintext) -> bool,
    {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let text = match timeout(remaining, self.ws.next()).await {
                Err(_) | Ok(None) | Ok(Some(Err(_))) => return false,
                Ok(Some(Ok(WsMessage::Text(t)))) => t,
                Ok(Some(Ok(_))) => continue, // ignore binary/ping/etc.
            };
            for (name, key) in keys {
                if let Ok(pt) = decrypt(key, &text) {
                    if &pt.target == name {
                        if on_msg(name, pt) {
                            return true;
                        }
                        break;
                    }
                }
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

async fn connect_ws(
    url: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    catcast_net::connect(url).await
}

pub fn keys_for(names: &[String]) -> Result<HashMap<String, Key>> {
    let mut out = HashMap::with_capacity(names.len());
    for n in names {
        out.insert(n.clone(), Key::from_psk(n).map_err(|e| anyhow!("{e}"))?);
    }
    Ok(out)
}
