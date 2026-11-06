//! Relay role: push screenshots of what this stage shows to a mirror stage.
//!
//! After every finished page load (plus a settle delay) and then every
//! `every_secs`, the relay captures the screen, cuts it into tiles
//! ([`catcast_net::tiles`]) and sends a `Frame` with the tile hashes. Tiles
//! it has not sent before follow as `FrameTiles`, then `FrameEnd`. The
//! mirror asks for anything it still lacks with `FrameNeed`, so a tile the
//! relay wrongly believes delivered (mirror reinstalled, cache evicted,
//! message dropped) costs one round trip, never a wrong picture.
//!
//! An unchanged screen sends nothing, except one header every
//! [`HEARTBEAT`] so the mirror can show that it is still current.
//!
//! Skipped: bundled pages (the about page prints this stage's name, which
//! is its key) and a stage stuck on a proxy/SSO login page.

use crate::socks::{FrameLink, FrameSlot, OutSlot, Outbound};
use crate::Shared;
use anyhow::{Context, Result};
use base64::Engine as _;
use catcast_net::tiles::{self, Hash, Image};
use catcast_proto::{Key, Message, TileData, TILE};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// An unchanged screen re-sends its header this often.
pub const HEARTBEAT: Duration = Duration::from_secs(10 * 60);
/// Raw PNG bytes per `FrameTiles` message (before base64), matching the
/// streamed-update chunk size.
const BATCH_BYTES: usize = 48 * 1024;
/// Tile hashes remembered as sent (~ 60 distinct 1080p screens).
const SENT_CAP: usize = 8192;

/// Persisted as `relay.json`. Never part of `config.yaml`, so exporting a
/// relay's config does not hand out the pairing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayCfg {
    pub to: String,
    pub secret_hex: String,
    pub settle_secs: u64,
    pub every_secs: u64,
}

impl RelayCfg {
    pub fn link(&self) -> Result<FrameLink> {
        let secret = catcast_proto::parse_frame_secret(&self.secret_hex)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(FrameLink {
            key: Arc::new(Key::from_frame_secret(&secret)),
            target: self.to.clone(),
        })
    }
}

pub enum Cmd {
    /// A top-level page finished loading.
    PageLoaded,
    /// The mirror asks for tiles (or, with an empty list, the current frame).
    Need {
        seq: u64,
        tiles: Vec<String>,
    },
    Configure(Option<RelayCfg>),
}

/// Hash set with insertion-order eviction.
#[derive(Default)]
pub struct SentSet {
    order: VecDeque<Hash>,
    set: HashSet<Hash>,
}

impl SentSet {
    pub fn contains(&self, h: &Hash) -> bool {
        self.set.contains(h)
    }
    pub fn insert(&mut self, h: Hash) {
        if self.set.insert(h) {
            self.order.push_back(h);
            while self.order.len() > SENT_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }
    pub fn clear(&mut self) {
        self.order.clear();
        self.set.clear();
    }
}

/// The frame most recently sent, kept to answer `FrameNeed`.
struct Current {
    header: Message,
    seq: u64,
    hashes: Vec<Hash>,
    tiles: HashMap<Hash, Image>,
    sent_at: Instant,
}

struct Relay {
    app: tauri::AppHandle,
    shared: Arc<Mutex<Shared>>,
    out: OutSlot,
    frame: FrameSlot,
    cfg: Option<RelayCfg>,
    current: Option<Current>,
    sent: SentSet,
    seq: u64,
}

pub fn spawn(
    app: tauri::AppHandle,
    shared: Arc<Mutex<Shared>>,
    out: OutSlot,
    frame: FrameSlot,
    initial: Option<RelayCfg>,
) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel(64);
    let relay = Relay {
        app,
        shared,
        out,
        frame,
        cfg: None,
        current: None,
        sent: SentSet::default(),
        seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
    };
    tokio::spawn(relay.run(rx, initial));
    tx
}

impl Relay {
    async fn run(mut self, mut rx: mpsc::Receiver<Cmd>, initial: Option<RelayCfg>) {
        let mut next: Option<Instant> = None;
        if initial.is_some() {
            self.configure(initial);
            next = self.settle_deadline();
        }
        loop {
            let sleep = async {
                match next {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    None => return,
                    Some(Cmd::PageLoaded) => next = self.settle_deadline(),
                    Some(Cmd::Configure(c)) => {
                        self.configure(c);
                        next = self.settle_deadline();
                    }
                    Some(Cmd::Need { seq, tiles }) => {
                        if let Err(e) = self.answer_need(seq, &tiles).await {
                            eprintln!("relay: answering a tile request failed: {e:#}");
                        }
                    }
                },
                _ = sleep => {
                    if let Err(e) = self.capture_and_send().await {
                        eprintln!("relay: {e:#}");
                    }
                    next = self.cfg.as_ref().map(|c| Instant::now() + Duration::from_secs(c.every_secs.max(5)));
                }
            }
        }
    }

    fn settle_deadline(&self) -> Option<Instant> {
        self.cfg
            .as_ref()
            .map(|c| Instant::now() + Duration::from_secs(c.settle_secs))
    }

    fn configure(&mut self, cfg: Option<RelayCfg>) {
        let link = match cfg.as_ref().map(RelayCfg::link) {
            Some(Ok(l)) => Some(l),
            Some(Err(e)) => {
                eprintln!("relay: ignoring pairing with a bad secret: {e:#}");
                None
            }
            None => None,
        };
        let cfg = if link.is_some() { cfg } else { None };
        // A new pairing means a mirror that has none of our tiles.
        if cfg != self.cfg {
            self.sent.clear();
            self.current = None;
        }
        *self.frame.write().expect("frame slot poisoned") = link;
        {
            let mut sh = self.shared.lock().unwrap();
            sh.state.relay_to = cfg.as_ref().map(|c| c.to.clone());
        }
        match &cfg {
            Some(c) => eprintln!(
                "relay: pushing to {} (settle {}s, every {}s)",
                c.to, c.settle_secs, c.every_secs
            ),
            None => eprintln!("relay: off"),
        }
        self.cfg = cfg;
    }

    /// Why this screen must not leave the building, if it must not.
    fn skip_reason(&self) -> Option<String> {
        let sh = self.shared.lock().unwrap();
        match sh.state.actual_url.as_deref() {
            None => Some("nothing loaded yet".into()),
            Some(u) if catcast_core::state::is_about_url(u) => Some("on a bundled page".into()),
            Some(u) if sh.state.stuck() => Some(format!("stuck on {u}")),
            Some(_) => None,
        }
    }

    async fn capture_and_send(&mut self) -> Result<()> {
        if self.cfg.is_none() {
            return Ok(());
        }
        if let Some(why) = self.skip_reason() {
            eprintln!("relay: not capturing ({why})");
            return Ok(());
        }
        let source_url = self
            .shared
            .lock()
            .unwrap()
            .state
            .actual_url
            .clone()
            .unwrap_or_default();
        let img = crate::capture::capture(&self.app)
            .await
            .context("capture")?;
        let (width, height) = (img.width, img.height);
        let cut = tokio::task::spawn_blocking(move || tiles::split(&img, TILE))
            .await
            .context("tile split task")?;
        let hashes: Vec<Hash> = cut.iter().map(|t| t.hash).collect();

        if let Some(cur) = &self.current {
            if cur.hashes == hashes && cur.sent_at.elapsed() < HEARTBEAT {
                return Ok(()); // unchanged screen: nothing to send
            }
        }

        self.seq += 1;
        let seq = self.seq;
        let header = Message::Frame {
            seq,
            width,
            height,
            tile: TILE,
            hashes: tiles::pack_hashes(&hashes),
            captured_ts: chrono::Utc::now().timestamp_millis(),
            source_url,
        };
        let tile_map: HashMap<Hash, Image> = cut.into_iter().map(|t| (t.hash, t.image)).collect();
        let fresh: Vec<Hash> = unique(&hashes)
            .into_iter()
            .filter(|h| !self.sent.contains(h))
            .collect();

        self.send(header.clone()).await?;
        let bytes = self.send_tiles(seq, &fresh, &tile_map).await?;
        self.send(Message::FrameEnd { seq }).await?;
        for h in &fresh {
            self.sent.insert(*h);
        }
        eprintln!(
            "relay: frame {seq}: {} tiles, {} sent ({} KB)",
            hashes.len(),
            fresh.len(),
            bytes / 1024
        );
        self.current = Some(Current {
            header,
            seq,
            hashes,
            tiles: tile_map,
            sent_at: Instant::now(),
        });
        self.shared.lock().unwrap().state.last_frame_ts =
            Some(chrono::Utc::now().timestamp_millis());
        Ok(())
    }

    async fn answer_need(&mut self, seq: u64, wanted: &[String]) -> Result<()> {
        let Some(cur) = &self.current else {
            return Ok(()); // nothing captured yet; the next capture follows
        };
        let (cur_seq, header) = (cur.seq, cur.header.clone());
        if wanted.is_empty() || seq != cur_seq {
            // A (re)connected mirror, or a request for a superseded frame:
            // repeat the current one and let the mirror ask again.
            self.send(header).await?;
            return self.send(Message::FrameEnd { seq: cur_seq }).await;
        }
        let want: Vec<Hash> = wanted
            .iter()
            .filter_map(|h| tiles::parse_hex(h).ok())
            .collect();
        let tile_map = std::mem::take(&mut self.current.as_mut().expect("checked").tiles);
        let res = self.send_tiles(seq, &want, &tile_map).await;
        self.current.as_mut().expect("checked").tiles = tile_map;
        let bytes = res?;
        for h in &want {
            self.sent.insert(*h);
        }
        eprintln!(
            "relay: frame {seq}: mirror asked for {} tiles ({} KB)",
            want.len(),
            bytes / 1024
        );
        self.send(Message::FrameEnd { seq }).await
    }

    /// Encode and send `which` tiles in batches; returns raw PNG bytes sent.
    async fn send_tiles(
        &self,
        seq: u64,
        which: &[Hash],
        tile_map: &HashMap<Hash, Image>,
    ) -> Result<usize> {
        let mut batch: Vec<TileData> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut total = 0usize;
        for h in which {
            let Some(img) = tile_map.get(h) else { continue };
            let png = tiles::encode_png(img)?;
            total += png.len();
            batch_bytes += png.len();
            batch.push(TileData {
                hash: tiles::to_hex(h),
                png: base64::engine::general_purpose::STANDARD.encode(&png),
            });
            if batch_bytes >= BATCH_BYTES {
                let tiles = std::mem::take(&mut batch);
                batch_bytes = 0;
                self.send(Message::FrameTiles { seq, tiles }).await?;
            }
        }
        if !batch.is_empty() {
            self.send(Message::FrameTiles { seq, tiles: batch }).await?;
        }
        Ok(total)
    }

    /// Queue one frame-key message, waiting for room (unlike `broadcast`,
    /// which drops when the queue is full).
    async fn send(&self, msg: Message) -> Result<()> {
        let tx = self.out.lock().unwrap().clone();
        match tx {
            Some(tx) => tx
                .send(Outbound::Frame(msg))
                .await
                .map_err(|_| anyhow::anyhow!("broker connection task is gone")),
            None => Ok(()),
        }
    }
}

/// Hashes in first-seen order, without duplicates.
fn unique(hashes: &[Hash]) -> Vec<Hash> {
    let mut seen = HashSet::new();
    hashes.iter().copied().filter(|h| seen.insert(*h)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_set_evicts_oldest_first() {
        let mut s = SentSet::default();
        for i in 0..(SENT_CAP + 3) {
            let mut h = [0u8; 16];
            h[..8].copy_from_slice(&(i as u64).to_le_bytes());
            s.insert(h);
        }
        let first = [0u8; 16];
        assert!(!s.contains(&first));
        let mut last = [0u8; 16];
        last[..8].copy_from_slice(&((SENT_CAP + 2) as u64).to_le_bytes());
        assert!(s.contains(&last));
        assert_eq!(s.order.len(), SENT_CAP);
    }

    #[test]
    fn unique_keeps_order() {
        let a = [1u8; 16];
        let b = [2u8; 16];
        assert_eq!(unique(&[a, b, a, b, a]), vec![a, b]);
    }

    #[test]
    fn relay_cfg_rejects_bad_secret() {
        let cfg = RelayCfg {
            to: "m".into(),
            secret_hex: "nope".into(),
            settle_secs: 3,
            every_secs: 60,
        };
        assert!(cfg.link().is_err());
        let ok = RelayCfg {
            secret_hex: "ab".repeat(32),
            ..cfg
        };
        assert_eq!(ok.link().unwrap().target, "m");
    }
}
