//! Screenshot tiling for the relay/mirror feature.
//!
//! A relay cuts each screenshot into a fixed grid of square tiles
//! ([`catcast_proto::TILE`] pixels; the last column and row are narrower
//! when the screen size is not a multiple) and hashes every tile's pixels.
//! A mirror keeps tiles by hash, so a page that comes back in the next
//! rotation costs only the list of hashes, and a page whose clock ticks
//! costs the few tiles under the clock.
//!
//! Hashing pixels instead of the PNG bytes matters: PNG is a compressed
//! stream, so one changed pixel changes every byte after it.

// `chunks_exact` with a constant size, not `as_chunks`: the workspace MSRV
// is 1.82 and `as_chunks` is 1.88. `unknown_lints` keeps older clippy quiet
// about a lint it does not know yet.
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use catcast_proto::TILE_HASH_LEN;
use sha2::{Digest, Sha256};

/// Largest screen edge a frame may claim. Bounds what a mirror allocates
/// for a frame header it has not verified yet.
pub const MAX_EDGE: u32 = 8192;

pub type Hash = [u8; TILE_HASH_LEN];

/// An 8-bit RGBA image, row-major, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self> {
        ensure!(
            rgba.len() == width as usize * height as usize * 4,
            "rgba buffer is {} bytes, {width}x{height} needs {}",
            rgba.len(),
            width as usize * height as usize * 4
        );
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

/// One cut-out tile with the hash of its pixels.
#[derive(Debug, Clone)]
pub struct Tile {
    pub hash: Hash,
    pub image: Image,
}

/// Hash of a tile: SHA-256 over its size and pixels, truncated.
pub fn hash_pixels(img: &Image) -> Hash {
    let mut h = Sha256::new();
    h.update(img.width.to_le_bytes());
    h.update(img.height.to_le_bytes());
    h.update(&img.rgba);
    let full = h.finalize();
    let mut out = [0u8; TILE_HASH_LEN];
    out.copy_from_slice(&full[..TILE_HASH_LEN]);
    out
}

/// Columns and rows of the grid for a `width` x `height` screen.
pub fn grid(width: u32, height: u32, tile: u32) -> (u32, u32) {
    (width.div_ceil(tile), height.div_ceil(tile))
}

/// Cut `img` into tiles, row-major.
pub fn split(img: &Image, tile: u32) -> Vec<Tile> {
    let (cols, rows) = grid(img.width, img.height, tile);
    let mut out = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let x0 = c * tile;
            let y0 = r * tile;
            let w = tile.min(img.width - x0);
            let h = tile.min(img.height - y0);
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for y in y0..y0 + h {
                let start = ((y * img.width + x0) * 4) as usize;
                rgba.extend_from_slice(&img.rgba[start..start + (w * 4) as usize]);
            }
            let image = Image {
                width: w,
                height: h,
                rgba,
            };
            out.push(Tile {
                hash: hash_pixels(&image),
                image,
            });
        }
    }
    out
}

/// Reassemble a `width` x `height` image from row-major tile hashes; `get`
/// supplies each tile's pixels. Fails if a tile is missing or has the
/// wrong size for its grid cell.
pub fn compose(
    width: u32,
    height: u32,
    tile: u32,
    hashes: &[Hash],
    mut get: impl FnMut(&Hash) -> Option<Image>,
) -> Result<Image> {
    check_geometry(width, height, tile, hashes.len())?;
    let (cols, _) = grid(width, height, tile);
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for (i, hash) in hashes.iter().enumerate() {
        let c = i as u32 % cols;
        let r = i as u32 / cols;
        let x0 = c * tile;
        let y0 = r * tile;
        let w = tile.min(width - x0);
        let h = tile.min(height - y0);
        let t = get(hash).with_context(|| format!("tile {} missing", to_hex(hash)))?;
        ensure!(
            t.width == w && t.height == h,
            "tile {} is {}x{}, cell needs {w}x{h}",
            to_hex(hash),
            t.width,
            t.height
        );
        for y in 0..h {
            let src = (y * w * 4) as usize;
            let dst = (((y0 + y) * width + x0) * 4) as usize;
            rgba[dst..dst + (w * 4) as usize].copy_from_slice(&t.rgba[src..src + (w * 4) as usize]);
        }
    }
    Image::new(width, height, rgba)
}

/// Reject frame headers whose size or tile count does not add up.
pub fn check_geometry(width: u32, height: u32, tile: u32, count: usize) -> Result<()> {
    ensure!(tile > 0, "tile size is 0");
    ensure!(
        (1..=MAX_EDGE).contains(&width) && (1..=MAX_EDGE).contains(&height),
        "frame {width}x{height} outside 1..={MAX_EDGE}"
    );
    let (cols, rows) = grid(width, height, tile);
    ensure!(
        count == (cols * rows) as usize,
        "{count} tile hashes for a {cols}x{rows} grid"
    );
    Ok(())
}

/// Decode a PNG of any colour type into 8-bit RGBA.
pub fn decode_png(bytes: &[u8]) -> Result<Image> {
    let mut dec = png::Decoder::new(bytes);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().context("read PNG header")?;
    let (w, h) = (reader.info().width, reader.info().height);
    ensure!(
        w <= MAX_EDGE && h <= MAX_EDGE,
        "PNG {w}x{h} larger than {MAX_EDGE}"
    );
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("decode PNG")?;
    buf.truncate(info.buffer_size());
    let px = w as usize * h as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("indexed PNG was not expanded"),
    };
    ensure!(rgba.len() == px * 4, "PNG decoded to an unexpected size");
    Image::new(w, h, rgba)
}

/// Encode RGBA as PNG (fast compression; tiles are small).
pub fn encode_png(img: &Image) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().context("write PNG header")?;
        w.write_image_data(&img.rgba).context("write PNG data")?;
    }
    Ok(out)
}

/// Base64 of the concatenated hashes, as carried by `Message::Frame`.
pub fn pack_hashes(hashes: &[Hash]) -> String {
    let flat: Vec<u8> = hashes.iter().flatten().copied().collect();
    base64::engine::general_purpose::STANDARD.encode(flat)
}

pub fn unpack_hashes(packed: &str) -> Result<Vec<Hash>> {
    let flat = base64::engine::general_purpose::STANDARD
        .decode(packed.trim())
        .context("tile hashes are not base64")?;
    ensure!(
        flat.len() % TILE_HASH_LEN == 0,
        "tile hash list is not a multiple of {TILE_HASH_LEN} bytes"
    );
    Ok(flat
        .chunks_exact(TILE_HASH_LEN)
        .map(|c| c.try_into().expect("exact chunk"))
        .collect())
}

pub fn to_hex(hash: &Hash) -> String {
    catcast_proto::to_hex(hash)
}

pub fn parse_hex(s: &str) -> Result<Hash> {
    let s = s.trim();
    ensure!(
        s.len() == TILE_HASH_LEN * 2,
        "tile hash {s:?} has the wrong length"
    );
    let mut out = [0u8; TILE_HASH_LEN];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .with_context(|| format!("tile hash {s:?} is not hex"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A 1080p-ish "dashboard": flat background with a few blocks.
    fn screen(w: u32, h: u32) -> Image {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = ((x / 50 + y / 70) % 3) as u8 * 60;
                rgba.extend_from_slice(&[v, 255 - v, 30, 255]);
            }
        }
        Image::new(w, h, rgba).unwrap()
    }

    fn paint(img: &mut Image, x0: u32, y0: u32, w: u32, h: u32, rgb: [u8; 3]) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let i = ((y * img.width + x) * 4) as usize;
                img.rgba[i..i + 3].copy_from_slice(&rgb);
            }
        }
    }

    #[test]
    fn grid_covers_partial_edges() {
        assert_eq!(grid(1920, 1080, 128), (15, 9));
        assert_eq!(grid(128, 128, 128), (1, 1));
        assert_eq!(grid(129, 1, 128), (2, 1));
    }

    #[test]
    fn split_then_compose_is_lossless() {
        let img = screen(1000, 700); // not a multiple of 128 either way
        let tiles = split(&img, 128);
        assert_eq!(tiles.len(), 8 * 6);
        let store: HashMap<Hash, Image> = tiles.iter().map(|t| (t.hash, t.image.clone())).collect();
        let hashes: Vec<Hash> = tiles.iter().map(|t| t.hash).collect();
        let back = compose(1000, 700, 128, &hashes, |h| store.get(h).cloned()).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn a_ticking_clock_changes_only_the_tiles_under_it() {
        let before = screen(1920, 1080);
        let mut after = before.clone();
        // A clock in the bottom-right corner, inside one tile.
        paint(&mut after, 1800, 1000, 60, 20, [255, 255, 255]);
        let a = split(&before, 128);
        let b = split(&after, 128);
        let changed: Vec<usize> = (0..a.len()).filter(|&i| a[i].hash != b[i].hash).collect();
        // Column 1800/128 = 14, row 1000/128 = 7, 15 columns per row.
        assert_eq!(changed, vec![7 * 15 + 14]);
    }

    #[test]
    fn identical_frames_hash_identically_and_repeated_tiles_dedup() {
        let mut img = Image::new(256, 128, vec![9u8; 256 * 128 * 4]).unwrap();
        let t = split(&img, 128);
        assert_eq!(t[0].hash, t[1].hash, "two identical cells share a hash");
        paint(&mut img, 0, 0, 1, 1, [1, 2, 3]);
        let t2 = split(&img, 128);
        assert_ne!(t2[0].hash, t[0].hash);
        assert_eq!(t2[1].hash, t[1].hash);
    }

    #[test]
    fn compose_reports_missing_and_misfit_tiles() {
        let img = screen(300, 200);
        let tiles = split(&img, 128);
        let hashes: Vec<Hash> = tiles.iter().map(|t| t.hash).collect();
        assert!(compose(300, 200, 128, &hashes, |_| None).is_err());
        let wrong = Image::new(1, 1, vec![0; 4]).unwrap();
        assert!(compose(300, 200, 128, &hashes, |_| Some(wrong.clone())).is_err());
        assert!(compose(300, 200, 128, &hashes[1..], |_| None).is_err());
        assert!(check_geometry(MAX_EDGE + 1, 10, 128, 0).is_err());
    }

    #[test]
    fn png_round_trip_and_rgb_input() {
        let img = screen(130, 70);
        let png = encode_png(&img).unwrap();
        assert_eq!(decode_png(&png).unwrap(), img);

        // An RGB PNG (what some capture paths emit) decodes to opaque RGBA.
        let mut rgb_png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut rgb_png, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        let d = decode_png(&rgb_png).unwrap();
        assert_eq!(d.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn hashes_pack_and_hex_round_trip() {
        let hs: Vec<Hash> = vec![[1u8; TILE_HASH_LEN], [0xab; TILE_HASH_LEN]];
        assert_eq!(unpack_hashes(&pack_hashes(&hs)).unwrap(), hs);
        assert!(unpack_hashes("AAA=").is_err());
        assert_eq!(parse_hex(&to_hex(&hs[1])).unwrap(), hs[1]);
        assert!(parse_hex("xy").is_err());
    }
}
