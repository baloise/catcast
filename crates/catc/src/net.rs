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
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

pub struct Broker {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Broker {
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _resp) = connect_async(url)
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
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match timeout(remaining, self.ws.next()).await {
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(Err(_))) => break,
                Ok(Some(Ok(WsMessage::Text(t)))) => {
                    for (name, key) in keys {
                        if let Ok(pt) = decrypt(key, &t) {
                            if &pt.target == name {
                                out.push((name.clone(), pt));
                                break;
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue, // ignore binary/ping/etc.
            }
        }
        out
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

pub fn keys_for(names: &[String]) -> Result<HashMap<String, Key>> {
    let mut out = HashMap::with_capacity(names.len());
    for n in names {
        out.insert(n.clone(), Key::from_psk(n).map_err(|e| anyhow!("{e}"))?);
    }
    Ok(out)
}
