//! Mirror role: show the frames a paired relay pushes.
//!
//! Tiles are kept by hash in `tiles/` (capped at [`DEFAULT_CAP_BYTES`]
//! unless `catc relay cache` set another cap; least recently used go first), so pages that come back in the relay's
//! rotation cost only their list of hashes. Every received tile is decoded
//! and re-hashed before it is stored; a tile whose pixels do not match its
//! name is dropped. A frame is shown once every tile is present; missing
//! ones are requested with `FrameNeed` (at most [`MAX_ROUNDS`] times).
//!
//! The last frame survives a reboot (`frame.json` + the tile store), so a
//! restarted mirror shows the last known picture instead of a black screen.

use crate::socks::{FrameLink, FrameSlot, OutSlot, Outbound};
use crate::Shared;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use catcast_net::tiles::{self, Hash, Image};
use catcast_proto::{Key, Message, TileData};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::Emitter;
use tokio::sync::mpsc;

/// Default tile store budget. A 1080p screen of dashboard tiles is
/// ~0.3–1.5 MB, so this holds a rotation of several dozen pages.
pub const DEFAULT_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// Smallest cap accepted: below this even one 4K screen may not fit (the
/// frame on screen is kept regardless, so this only bounds churn).
pub const MIN_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// Persisted as `mirror-cache.json`; absent means [`DEFAULT_CAP_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCfg {
    pub cap_bytes: u64,
}

/// The cap in effect: the persisted one, else the default.
pub fn load_cap() -> u64 {
    crate::persist::mirror_cache_path()
        .and_then(|p| crate::persist::load_json::<CacheCfg>(&p))
        .unwrap_or_else(|e| {
            eprintln!("mirror: ignoring unreadable mirror-cache.json: {e:#}");
            None
        })
        .map(|c| c.cap_bytes.max(MIN_CAP_BYTES))
        .unwrap_or(DEFAULT_CAP_BYTES)
}
/// `FrameNeed` rounds per frame before giving up on it.
pub const MAX_ROUNDS: u32 = 3;
/// Event the mirror page listens for.
pub const FRAME_EVENT: &str = "catcast://frame";

/// Persisted as `mirror.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorCfg {
    pub secret_hex: String,
}

impl MirrorCfg {
    pub fn link(&self, own_name: &str) -> Result<FrameLink> {
        let secret = catcast_proto::parse_frame_secret(&self.secret_hex)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(FrameLink {
            key: Arc::new(Key::from_frame_secret(&secret)),
            target: own_name.to_owned(),
        })
    }
}

/// `frame.json`: the frame on screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameRecord {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub tile: u32,
    pub hashes: String,
    pub captured_ts: i64,
    /// Mirror-local unix-ms when the relay last confirmed this picture.
    pub confirmed_ts: i64,
    pub source_url: String,
}

/// What the mirror page reads.
#[derive(Debug, Clone, Serialize)]
pub struct ViewMeta {
    pub captured_ts: i64,
    pub confirmed_ts: i64,
    pub source_url: String,
    pub width: u32,
    pub height: u32,
}

/// Shared with the Tauri commands behind `ui/mirror.html`.
#[derive(Default)]
pub struct MirrorView {
    pub png: Mutex<Option<Arc<Vec<u8>>>>,
    pub meta: Mutex<Option<ViewMeta>>,
}

pub enum Cmd {
    Msg(Message),
    /// The broker link came up: ask the relay for its current frame.
    Connected,
    Configure(Option<MirrorCfg>),
    /// New cache cap; answers with the bytes in use after evicting.
    SetCap(u64, tokio::sync::oneshot::Sender<u64>),
}

struct Pending {
    rec: FrameRecord,
    hashes: Vec<Hash>,
    rounds: u32,
}

struct Mirror {
    app: tauri::AppHandle,
    shared: Arc<Mutex<Shared>>,
    out: OutSlot,
    frame: FrameSlot,
    name: String,
    view: Arc<MirrorView>,
    store: PathBuf,
    cap: u64,
    pending: Option<Pending>,
    shown: Option<Vec<Hash>>,
}

pub fn spawn(
    app: tauri::AppHandle,
    shared: Arc<Mutex<Shared>>,
    out: OutSlot,
    frame: FrameSlot,
    name: String,
    view: Arc<MirrorView>,
    initial: Option<MirrorCfg>,
) -> Result<mpsc::Sender<Cmd>> {
    let store = crate::persist::tiles_dir()?;
    let (tx, rx) = mpsc::channel(256);
    let m = Mirror {
        app,
        shared,
        out,
        frame,
        name,
        view,
        store,
        cap: load_cap(),
        pending: None,
        shown: None,
    };
    tokio::spawn(m.run(rx, initial));
    Ok(tx)
}

impl Mirror {
    async fn run(mut self, mut rx: mpsc::Receiver<Cmd>, initial: Option<MirrorCfg>) {
        self.configure(initial, false).await;
        if let Err(e) = self.restore().await {
            eprintln!("mirror: no previous frame to show: {e:#}");
        }
        while let Some(cmd) = rx.recv().await {
            let res = match cmd {
                Cmd::Msg(m) => self.on_message(m).await,
                Cmd::Connected => self.pull().await,
                Cmd::Configure(c) => {
                    self.configure(c, true).await;
                    Ok(())
                }
                Cmd::SetCap(cap, reply) => {
                    self.cap = cap;
                    let store = self.store.clone();
                    let keep: HashSet<Hash> = self.shown.iter().flatten().copied().collect();
                    let used = tokio::task::spawn_blocking(move || evict(&store, cap, &keep))
                        .await
                        .unwrap_or(0);
                    eprintln!(
                        "mirror: tile cache cap {} MB, {} MB used",
                        cap >> 20,
                        used >> 20
                    );
                    let _ = reply.send(used);
                    Ok(())
                }
            };
            if let Err(e) = res {
                eprintln!("mirror: {e:#}");
            }
        }
    }

    async fn configure(&mut self, cfg: Option<MirrorCfg>, pull: bool) {
        let link = match cfg.as_ref().map(|c| c.link(&self.name)) {
            Some(Ok(l)) => Some(l),
            Some(Err(e)) => {
                eprintln!("mirror: ignoring pairing with a bad secret: {e:#}");
                None
            }
            None => None,
        };
        let paired = link.is_some();
        *self.frame.write().expect("frame slot poisoned") = link;
        eprintln!("mirror: {}", if paired { "paired" } else { "not paired" });
        if paired && pull {
            let _ = self.pull().await;
        }
    }

    /// Ask the relay to repeat its current frame.
    async fn pull(&self) -> Result<()> {
        if self.frame.read().expect("frame slot poisoned").is_none() {
            return Ok(());
        }
        self.send(Message::FrameNeed {
            seq: 0,
            tiles: vec![],
        })
        .await
    }

    async fn on_message(&mut self, msg: Message) -> Result<()> {
        match msg {
            Message::Frame {
                seq,
                width,
                height,
                tile,
                hashes,
                captured_ts,
                source_url,
            } => {
                let list = tiles::unpack_hashes(&hashes)?;
                tiles::check_geometry(width, height, tile, list.len())?;
                let rounds = match &self.pending {
                    Some(p) if p.rec.seq == seq => p.rounds,
                    _ => 0,
                };
                self.pending = Some(Pending {
                    rec: FrameRecord {
                        seq,
                        width,
                        height,
                        tile,
                        hashes,
                        captured_ts,
                        confirmed_ts: 0,
                        source_url,
                    },
                    hashes: list,
                    rounds,
                });
                Ok(())
            }
            Message::FrameTiles { tiles: batch, .. } => {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store_batch(&store, &batch))
                    .await
                    .context("tile store task")?
            }
            Message::FrameEnd { seq } => self.finish(seq).await,
            _ => Ok(()),
        }
    }

    async fn finish(&mut self, seq: u64) -> Result<()> {
        let Some(p) = self.pending.as_mut().filter(|p| p.rec.seq == seq) else {
            return Ok(()); // end of a frame whose header we never saw
        };
        let missing: Vec<Hash> = unique(&p.hashes)
            .into_iter()
            .filter(|h| !tile_path(&self.store, h).exists())
            .collect();
        if !missing.is_empty() {
            p.rounds += 1;
            if p.rounds > MAX_ROUNDS {
                eprintln!(
                    "mirror: frame {seq} still lacks {} tiles after {MAX_ROUNDS} requests, dropped",
                    missing.len()
                );
                self.pending = None;
                return Ok(());
            }
            let tiles = missing.iter().map(tiles::to_hex).collect();
            return self.send(Message::FrameNeed { seq, tiles }).await;
        }

        let Pending {
            mut rec, hashes, ..
        } = self.pending.take().expect("checked above");
        rec.confirmed_ts = chrono::Utc::now().timestamp_millis();
        if self.shown.as_ref() != Some(&hashes) {
            let store = self.store.clone();
            let r = rec.clone();
            let hs = hashes.clone();
            let png = tokio::task::spawn_blocking(move || render(&store, &r, &hs))
                .await
                .context("compose task")??;
            *self.view.png.lock().unwrap() = Some(Arc::new(png));
            self.shown = Some(hashes.clone());
            eprintln!("mirror: showing frame {seq} of {}", rec.source_url);
        }
        *self.view.meta.lock().unwrap() = Some(meta_of(&rec));
        if let Err(e) =
            crate::persist::frame_path().and_then(|p| crate::persist::save_json(&p, &rec))
        {
            eprintln!("mirror: could not persist frame.json: {e:#}");
        }
        let store = self.store.clone();
        let keep: HashSet<Hash> = hashes.into_iter().collect();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || evict(&store, cap, &keep));

        let snap = {
            let mut sh = self.shared.lock().unwrap();
            sh.state.last_frame_ts = Some(rec.confirmed_ts);
            sh.state.clone()
        };
        crate::broadcast(&self.out, Message::State(snap));
        let _ = self
            .app
            .emit_to(crate::WINDOW_LABEL, FRAME_EVENT, rec.confirmed_ts);
        Ok(())
    }

    /// Show the frame from before the restart, if its tiles are all there.
    async fn restore(&mut self) -> Result<()> {
        let path = crate::persist::frame_path()?;
        let Some(rec) = crate::persist::load_json::<FrameRecord>(&path)? else {
            return Ok(());
        };
        let hashes = tiles::unpack_hashes(&rec.hashes)?;
        let store = self.store.clone();
        let (r, hs) = (rec.clone(), hashes.clone());
        let png = tokio::task::spawn_blocking(move || render(&store, &r, &hs))
            .await
            .context("compose task")??;
        *self.view.png.lock().unwrap() = Some(Arc::new(png));
        *self.view.meta.lock().unwrap() = Some(meta_of(&rec));
        self.shown = Some(hashes);
        self.shared.lock().unwrap().state.last_frame_ts = Some(rec.confirmed_ts);
        eprintln!("mirror: restored the last frame of {}", rec.source_url);
        Ok(())
    }

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

fn meta_of(rec: &FrameRecord) -> ViewMeta {
    ViewMeta {
        captured_ts: rec.captured_ts,
        confirmed_ts: rec.confirmed_ts,
        source_url: rec.source_url.clone(),
        width: rec.width,
        height: rec.height,
    }
}

fn unique(hashes: &[Hash]) -> Vec<Hash> {
    let mut seen = HashSet::new();
    hashes.iter().copied().filter(|h| seen.insert(*h)).collect()
}

pub fn tile_path(store: &Path, h: &Hash) -> PathBuf {
    store.join(format!("{}.png", tiles::to_hex(h)))
}

/// Verify and store one batch. A tile whose pixels do not hash to its
/// name is refused.
pub fn store_batch(store: &Path, batch: &[TileData]) -> Result<()> {
    for t in batch {
        let hash = tiles::parse_hex(&t.hash)?;
        let path = tile_path(store, &hash);
        if path.exists() {
            continue;
        }
        let png = base64::engine::general_purpose::STANDARD
            .decode(t.png.as_bytes())
            .context("tile is not base64")?;
        let img = tiles::decode_png(&png)?;
        if tiles::hash_pixels(&img) != hash {
            bail!("tile {} does not match its pixels", t.hash);
        }
        let tmp = path.with_extension(format!("png.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &png).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("storing {}", path.display()))?;
    }
    Ok(())
}

/// Compose the frame from the store and encode it as one PNG.
pub fn render(store: &Path, rec: &FrameRecord, hashes: &[Hash]) -> Result<Vec<u8>> {
    let mut cache: HashMap<Hash, Image> = HashMap::new();
    for h in unique(hashes) {
        let path = tile_path(store, &h);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        cache.insert(h, tiles::decode_png(&bytes)?);
        // Recently used tiles survive eviction.
        if let Ok(f) = std::fs::File::options().append(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }
    let img = tiles::compose(rec.width, rec.height, rec.tile, hashes, |h| {
        cache.get(h).cloned()
    })?;
    tiles::encode_png(&img)
}

/// Delete least recently used tiles until the store fits `cap` bytes.
/// Tiles in `keep` (the frame on screen) are never deleted. Returns the
/// bytes still in use.
pub fn evict(store: &Path, cap: u64, keep: &HashSet<Hash>) -> u64 {
    let Ok(rd) = std::fs::read_dir(store) else {
        return 0;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            Some((md.modified().ok()?, md.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= cap {
        return total;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= cap {
            break;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if tiles::parse_hex(stem).is_ok_and(|h| keep.contains(&h)) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("catstage-mirror-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn data_of(t: &tiles::Tile) -> TileData {
        TileData {
            hash: tiles::to_hex(&t.hash),
            png: base64::engine::general_purpose::STANDARD
                .encode(tiles::encode_png(&t.image).unwrap()),
        }
    }

    fn screen() -> Image {
        let (w, h) = (300u32, 200u32);
        let rgba = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, (i / 300) as u8, 7, 255])
            .collect();
        Image::new(w, h, rgba).unwrap()
    }

    #[test]
    fn store_then_render_reproduces_the_screen() {
        let store = tmp("render");
        let img = screen();
        let cut = tiles::split(&img, 128);
        let batch: Vec<TileData> = cut.iter().map(data_of).collect();
        store_batch(&store, &batch).unwrap();
        let hashes: Vec<Hash> = cut.iter().map(|t| t.hash).collect();
        let rec = FrameRecord {
            seq: 1,
            width: 300,
            height: 200,
            tile: 128,
            hashes: tiles::pack_hashes(&hashes),
            captured_ts: 0,
            confirmed_ts: 0,
            source_url: String::new(),
        };
        let png = render(&store, &rec, &hashes).unwrap();
        assert_eq!(tiles::decode_png(&png).unwrap(), img);
        std::fs::remove_dir_all(&store).unwrap();
    }

    #[test]
    fn a_tile_that_lies_about_its_hash_is_refused() {
        let store = tmp("lie");
        let cut = tiles::split(&screen(), 128);
        let mut bad = data_of(&cut[0]);
        bad.hash = tiles::to_hex(&cut[1].hash);
        assert!(store_batch(&store, &[bad]).is_err());
        assert!(!tile_path(&store, &cut[1].hash).exists());
        std::fs::remove_dir_all(&store).unwrap();
    }

    #[test]
    fn eviction_spares_the_frame_on_screen() {
        let store = tmp("evict");
        let cut = tiles::split(&screen(), 128);
        store_batch(&store, &cut.iter().map(data_of).collect::<Vec<_>>()).unwrap();
        let keep: HashSet<Hash> = [cut[0].hash].into_iter().collect();
        let kept = std::fs::metadata(tile_path(&store, &cut[0].hash))
            .unwrap()
            .len();
        assert!(
            evict(&store, u64::MAX, &keep) > kept,
            "nothing evicted under a big cap"
        );
        assert_eq!(evict(&store, 0, &keep), kept);
        assert!(tile_path(&store, &cut[0].hash).exists());
        for t in &cut[1..] {
            assert!(!tile_path(&store, &t.hash).exists());
        }
        std::fs::remove_dir_all(&store).unwrap();
    }
}
