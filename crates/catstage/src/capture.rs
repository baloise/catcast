//! Screenshot of the kiosk webview, for the relay role.
//!
//! - Windows: WebView2 `CapturePreview` (PNG of the visible viewport).
//! - Linux: WebKitGTK `get_snapshot` of the visible region (dev loop only).
//!
//! Both paths need a *visible*, painted window: WebView2 stops rendering a
//! minimised or hidden window and returns a stale or blank image. A relay
//! is therefore a real kiosk screen (or a second monitor), never a hidden
//! helper.

use anyhow::{anyhow, Context, Result};
use catcast_net::tiles::Image;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tokio::sync::oneshot;

/// Upper bound for one capture. CapturePreview usually answers in well
/// under a second.
const TIMEOUT: Duration = Duration::from_secs(15);

/// What a platform hands back before the (off-main-thread) conversion.
enum Raw {
    #[cfg_attr(not(windows), allow(dead_code))]
    Png(Vec<u8>),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Rgba(Image),
}

type Reply = Arc<Mutex<Option<oneshot::Sender<Result<Raw>>>>>;

// Unused where capture is not implemented (macOS).
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn answer(reply: &Reply, r: Result<Raw>) {
    if let Some(tx) = reply.lock().expect("capture reply poisoned").take() {
        let _ = tx.send(r);
    }
}

/// Capture what the kiosk window shows right now.
pub async fn capture(app: &tauri::AppHandle) -> Result<Image> {
    let window = app
        .get_webview_window(crate::WINDOW_LABEL)
        .context("no kiosk window")?;
    let (tx, rx) = oneshot::channel();
    let reply: Reply = Arc::new(Mutex::new(Some(tx)));
    start(&window, Arc::clone(&reply))?;
    let raw = tokio::time::timeout(TIMEOUT, rx)
        .await
        .map_err(|_| anyhow!("capture timed out after {TIMEOUT:?}"))?
        .map_err(|_| anyhow!("capture callback dropped"))??;
    match raw {
        Raw::Rgba(img) => Ok(img),
        Raw::Png(png) => tokio::task::spawn_blocking(move || catcast_net::tiles::decode_png(&png))
            .await
            .context("PNG decode task")?,
    }
}

#[cfg(windows)]
fn start(window: &tauri::WebviewWindow, reply: Reply) -> Result<()> {
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::UI::Shell::SHCreateMemStream;

    let outer = Arc::clone(&reply);
    window
        .with_webview(move |pw| {
            let res = (|| -> Result<()> {
                // SAFETY: plain COM calls on the UI thread that owns the
                // controller; `with_webview` runs this closure there.
                unsafe {
                    let core = pw
                        .controller()
                        .CoreWebView2()
                        .context("ICoreWebView2Controller::CoreWebView2")?;
                    let stream = SHCreateMemStream(None).context("SHCreateMemStream failed")?;
                    let done_stream = stream.clone();
                    let done_reply = Arc::clone(&reply);
                    let handler = CapturePreviewCompletedHandler::create(Box::new(move |hr| {
                        let r = hr
                            .map_err(|e| anyhow!("CapturePreview: {e}"))
                            .and_then(|()| read_stream(&done_stream))
                            .map(Raw::Png);
                        answer(&done_reply, r);
                        Ok(())
                    }));
                    core.CapturePreview(
                        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                        &stream,
                        &handler,
                    )
                    .context("ICoreWebView2::CapturePreview")?;
                }
                Ok(())
            })();
            if let Err(e) = res {
                answer(&reply, Err(e));
            }
        })
        .map_err(|e| {
            answer(&outer, Err(anyhow!("with_webview: {e}")));
            anyhow!("with_webview: {e}")
        })
}

/// Read a whole in-memory COM stream.
#[cfg(windows)]
unsafe fn read_stream(stream: &windows::Win32::System::Com::IStream) -> Result<Vec<u8>> {
    use windows::Win32::System::Com::{STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};

    let mut stat = STATSTG::default();
    unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.context("IStream::Stat")?;
    let len = usize::try_from(stat.cbSize).context("capture larger than memory")?;
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }.context("IStream::Seek")?;
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < len {
        let mut read = 0u32;
        let want = u32::try_from(len - filled).unwrap_or(u32::MAX);
        unsafe {
            stream
                .Read(buf[filled..].as_mut_ptr().cast(), want, Some(&mut read))
                .ok()
        }
        .context("IStream::Read")?;
        if read == 0 {
            break;
        }
        filled += read as usize;
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(target_os = "linux")]
fn start(window: &tauri::WebviewWindow, reply: Reply) -> Result<()> {
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

    let outer = Arc::clone(&reply);
    window
        .with_webview(move |pw| {
            let done_reply = Arc::clone(&reply);
            pw.inner().snapshot(
                SnapshotRegion::Visible,
                SnapshotOptions::NONE,
                None::<&webkit2gtk::gio::Cancellable>,
                move |res| {
                    let r = res
                        .map_err(|e| anyhow!("webkit snapshot: {e}"))
                        .and_then(surface_to_rgba)
                        .map(Raw::Rgba);
                    answer(&done_reply, r);
                },
            );
        })
        .map_err(|e| {
            answer(&outer, Err(anyhow!("with_webview: {e}")));
            anyhow!("with_webview: {e}")
        })
}

/// Cairo ARGB32 / RGB24 (native-endian, premultiplied) to straight RGBA.
/// (`chunks_exact`, not `as_chunks`: MSRV 1.82.)
#[cfg(target_os = "linux")]
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
fn surface_to_rgba(surface: cairo::Surface) -> Result<Image> {
    let img = cairo::ImageSurface::try_from(surface)
        .map_err(|_| anyhow!("snapshot is not an image surface"))?;
    let (w, h, stride) = (
        img.width() as usize,
        img.height() as usize,
        img.stride() as usize,
    );
    let opaque = img.format() == cairo::Format::Rgb24;
    let mut rgba = Vec::with_capacity(w * h * 4);
    img.with_data(|data| {
        for y in 0..h {
            let row = &data[y * stride..y * stride + w * 4];
            for px in row.chunks_exact(4) {
                let v = u32::from_ne_bytes([px[0], px[1], px[2], px[3]]);
                let a = if opaque { 255 } else { (v >> 24) as u8 };
                let un = |c: u32| -> u8 {
                    let c = (c & 0xff) as u8;
                    match a {
                        255 => c,
                        0 => 0,
                        _ => ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8,
                    }
                };
                rgba.extend_from_slice(&[un(v >> 16), un(v >> 8), un(v), a]);
            }
        }
    })
    .map_err(|e| anyhow!("read snapshot pixels: {e}"))?;
    Image::new(w as u32, h as u32, rgba)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn start(_window: &tauri::WebviewWindow, _reply: Reply) -> Result<()> {
    anyhow::bail!("screen capture is not implemented on this platform")
}
