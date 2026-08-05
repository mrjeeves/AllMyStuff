//! In-house DXGI Output Duplication — the Windows screen-capture session.
//!
//! Why not xcap's recorder, which we use everywhere else? Its DXGI
//! implementation has two defects that together cost most of the
//! framerate in the field: walking to the target monitor it calls
//! `DuplicateOutput` on **every output it passes** (DXGI returns
//! `E_INVALIDARG` when an output is already duplicated, killing the whole
//! creation even though the *target* is free), and its capture thread
//! never exits — `stop()` only sleeps a waker, so the thread's clone of
//! the duplication handle leaks for the process lifetime. Net effect: the
//! first session in a process works, and every later one — any monitor —
//! fails with `0x80070057` and drops to per-frame GDI screenshots.
//!
//! This module duplicates exactly the requested output, swizzles BGRA →
//! RGBA while copying out of the staging texture (fused into the copy we
//! must do anyway, so downstream stays the one RGBA pipeline), exits its
//! thread on stop, and re-acquires after `DXGI_ERROR_ACCESS_LOST` (mode
//! change, fullscreen transition, UAC desktop) instead of dying.
//!
//! Frames are damage-driven: `AcquireNextFrame` returns only when the
//! desktop actually changed, so an idle screen costs polling, not copies.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

/// One captured desktop frame, already RGBA.
pub struct RawFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Authoritative clockwise rotation of the scan-out, normalized to
    /// {0,90,180,270}. From `DXGI_OUTDUPL_DESC.Rotation`, read once per
    /// duplication. The raw buffer is rotated by THIS to become upright.
    pub rotation_deg: u32,
}

/// A running duplication session. Dropping it stops the thread — really
/// stops it: the thread exits and the duplication handle is released, so
/// the output is immediately re-duplicable.
pub struct Session {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Begin duplicating the monitor with this id (xcap's monitor id — the
/// raw `HMONITOR` value, which is also what `DXGI_OUTPUT_DESC` carries).
/// Fails fast on the caller's thread when the output can't be duplicated
/// right now (someone else holds it, RDP session), so the caller can fall
/// back without waiting on a channel.
/// A live duplication: the session handle, its frame stream, and the
/// reclaim lane spent frame buffers ride back on.
pub type StartedDuplication = (Session, mpsc::Receiver<RawFrame>, mpsc::SyncSender<Vec<u8>>);

pub fn start(monitor_id: u32) -> Result<StartedDuplication, String> {
    start_named(monitor_id, None)
}

/// Start on a route-lifetime display identity when one is available. The
/// numeric handle is used only for the initial legacy/default-primary bind;
/// exact monitor routes pass `\\.\DISPLAYn` so every rebuild is immune to
/// `HMONITOR` recycling.
pub fn start_named(
    monitor_id: u32,
    stable_name: Option<&str>,
) -> Result<StartedDuplication, String> {
    let mut dup = match stable_name {
        Some(name) => Duplication::new_named(name)?,
        None => Duplication::new(monitor_id)?,
    };
    // A shallow channel: the consumer drains to the freshest frame each
    // tick; anything it hasn't taken by the time two more arrive is stale.
    let (tx, rx) = mpsc::sync_channel::<RawFrame>(2);
    // The reclaim lane: the consumer hands spent frame buffers back so the
    // per-frame copy-out lands in already-touched pages instead of a fresh
    // multi-megabyte OS allocation (whose demand-zeroing costs more than
    // the copy). Best-effort — an unused lane just means fresh allocations,
    // exactly the old behaviour.
    let (reclaim_tx, reclaim_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    dup.reclaim = Some(reclaim_rx);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = std::thread::spawn(move || pump(dup, &stop_thread, &tx));
    Ok((
        Session {
            stop,
            thread: Some(thread),
        },
        rx,
        reclaim_tx,
    ))
}

fn pump(mut dup: Duplication, stop: &AtomicBool, tx: &mpsc::SyncSender<RawFrame>) {
    // The duplication readback competes with whatever loaded the GPU/CPU —
    // the exact condition the stream exists for.
    crate::os_perf::boost_media_thread();
    // Follows desktop switches so a UAC prompt is *captured* rather than
    // freezing the stream — see the ACCESS_LOST arm below.
    let mut desktop = crate::win_privilege::DesktopFollower::new();
    while !stop.load(Ordering::SeqCst) {
        match dup.next_frame(100) {
            Ok(Some(frame)) => {
                // try_send: a full channel means the consumer is behind;
                // dropping this frame just means the next one is fresher.
                let _ = tx.try_send(frame);
            }
            Ok(None) => {} // timeout (idle screen) or a mouse-only update
            Err(NextError::AccessLost) => {
                // Mode change / fullscreen toggle / secure desktop: the
                // duplication is dead but the output identity remains. A
                // wake/topology change can replace HMONITOR, so rebind by
                // `\\.\DISPLAYn`, never by the dead numeric handle.
                let old_id = dup.monitor_id;
                let device_name = dup.device_name.clone();
                tracing::debug!(
                    "duplication of {device_name} ({old_id:#x}) lost — re-acquiring by name"
                );
                // The secure-desktop case, and the reason this loop used to
                // look like a freeze: a duplication belongs to the desktop its
                // thread is attached to, so once Windows switches to `Winlogon`
                // for a UAC prompt, re-acquiring from a thread still standing on
                // `Default` rebuilds a duplication of a desktop nobody is
                // looking at. Every re-acquire "succeeds" and every frame is the
                // stale desktop, for as long as the prompt is up.
                //
                // Attaching to the input desktop first is what fixes it.
                // Attaching needs SYSTEM (the `Winlogon` desktop admits nobody
                // else), so on an unprivileged agent `follow` simply returns
                // false and the retry loop below rides the prompt out as it
                // always did. This is only reached on `ACCESS_LOST` — a desktop
                // switch — never per frame.
                if desktop.follow() {
                    tracing::info!(
                        "capture: following desktop switch to {:?}{}",
                        desktop.desktop_name(),
                        if desktop.on_secure_desktop() {
                            " (secure desktop — UAC prompt)"
                        } else {
                            ""
                        }
                    );
                }
                // DXGI requires the lost duplication interface to be
                // released before DuplicateOutput is attempted again.
                drop(dup.dup.take());
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match Duplication::new_named(&device_name) {
                        Ok(mut d) => {
                            // The reclaim lane outlives the duplication —
                            // carry it onto the rebuilt session.
                            d.reclaim = dup.reclaim.take();
                            tracing::info!(
                                "duplication rebound {device_name}: {old_id:#x} -> {:#x}",
                                d.monitor_id
                            );
                            dup = d;
                            break;
                        }
                        Err(e) if Instant::now() < deadline => {
                            tracing::debug!("re-acquire not ready ({e}); retrying");
                            std::thread::sleep(Duration::from_millis(250));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "duplication of {device_name} ({old_id:#x}) not recoverable: {e}"
                            );
                            return;
                        }
                    }
                }
            }
            Err(NextError::Fatal(e)) => {
                tracing::warn!(
                    "duplication of {} ({:#x}) ended: {e}",
                    dup.device_name,
                    dup.monitor_id
                );
                return;
            }
        }
    }
}

enum NextError {
    /// The duplication handle died (mode change etc.) — recreate it.
    AccessLost,
    /// Anything else — end the session; the route falls back.
    Fatal(String),
}

/// One DXGI output binding. `monitor_id` is the volatile `HMONITOR`; the
/// `device_name` (`\\.\DISPLAYn`) is the route-lifetime identity used to
/// find the same output again after Windows invalidates and replaces the
/// handle during wake/topology changes.
struct BoundOutput {
    dup: IDXGIOutputDuplication,
    rotation_deg: u32,
    monitor_id: u32,
    device_name: String,
}

/// Find and duplicate one output on `device`'s adapter. An
/// already-duplicated sibling output must not abort the search. The
/// duplicated output's own rotation is authoritative for raw scan-out.
unsafe fn duplicate_matching_output(
    device: &ID3D11Device,
    wanted: &str,
    mut matches: impl FnMut(u32, &str) -> bool,
) -> Result<BoundOutput, String> {
    let dxgi: IDXGIDevice = device.cast().map_err(|e| e.to_string())?;
    let adapter: IDXGIAdapter = dxgi.GetAdapter().map_err(|e| e.to_string())?;
    let mut index = 0u32;
    loop {
        let Ok(output) = adapter.EnumOutputs(index) else {
            return Err(format!("monitor {wanted} not found on the capture adapter"));
        };
        index += 1;
        let desc = output.GetDesc().map_err(|e| e.to_string())?;
        let monitor_id = desc.Monitor.0 as usize as u32;
        let name_end = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
        let device_name = String::from_utf16_lossy(&desc.DeviceName[..name_end]);
        if !matches(monitor_id, &device_name) {
            continue;
        }
        let output1: IDXGIOutput1 = output.cast().map_err(|e| e.to_string())?;
        let dup = output1
            .DuplicateOutput(device)
            .map_err(|e| format!("DuplicateOutput: {e}"))?;
        // GetDesc is infallible and by-value.
        let rotation_deg = match dup.GetDesc().Rotation {
            DXGI_MODE_ROTATION_ROTATE90 => 90,
            DXGI_MODE_ROTATION_ROTATE180 => 180,
            DXGI_MODE_ROTATION_ROTATE270 => 270,
            _ => 0, // IDENTITY / UNSPECIFIED / anything else: upright.
        };
        // The datastream↔monitor link uses the same `\\.\DISPLAYn` key as
        // telemetry, so one grep correlates the route with its panel.
        tracing::info!(
            "capture bound to {} (monitor id {monitor_id} · rotation {rotation_deg}°)",
            device_name
        );
        return Ok(BoundOutput {
            dup,
            rotation_deg,
            monitor_id,
            device_name,
        });
    }
}

/// Initial bind by the advertised raw monitor handle.
unsafe fn duplicate_output(device: &ID3D11Device, monitor_id: u32) -> Result<BoundOutput, String> {
    duplicate_matching_output(device, &format!("{monitor_id:#x}"), |candidate, _| {
        candidate == monitor_id
    })
}

/// Rebind by the stable route-lifetime display name after `HMONITOR` churn.
unsafe fn duplicate_output_named(
    device: &ID3D11Device,
    device_name: &str,
) -> Result<BoundOutput, String> {
    duplicate_matching_output(device, device_name, |_, candidate| {
        candidate.eq_ignore_ascii_case(device_name)
    })
}

/// Fetch the current pointer bitmap from a held frame (held-frame only).
/// `None` on failure — the caller keeps its previous shape.
unsafe fn fetch_cursor_shape(dup: &IDXGIOutputDuplication, size: u32) -> Option<CursorShape> {
    let mut buf = vec![0u8; size as usize];
    let mut required = 0u32;
    let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    dup.GetFramePointerShape(
        size,
        buf.as_mut_ptr() as *mut core::ffi::c_void,
        &mut required,
        &mut info,
    )
    .ok()?;
    Some(CursorShape {
        kind: info.Type,
        width: info.Width,
        height: info.Height,
        pitch: info.Pitch,
        buf,
    })
}

/// The mouse cursor as DXGI hands it over — the real OS pointer shape (arrow,
/// I-beam, hand, resize, a custom app cursor). Desktop Duplication never draws
/// it into the frame (that's a hardware overlay), so on Windows we composite it
/// ourselves; macOS/Linux capture bakes their own cursor in already. Cached
/// because the *shape* only changes when the pointer image does (a new
/// `PointerShapeBufferSize`), while the *position* updates every mouse move.
struct CursorShape {
    /// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`: 1 monochrome, 2 color (BGRA), 4
    /// masked-color. Stored raw so no extra windows-crate import is needed.
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    buf: Vec<u8>,
}

struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: Option<IDXGIOutputDuplication>,
    monitor_id: u32,
    device_name: String,
    /// CPU-readable copy target, reused across frames of the same size.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Clockwise degrees from `DXGI_OUTDUPL_DESC.Rotation`, read at
    /// construction; fixed for the duplication's lifetime. A mode change
    /// kills the duplication with ACCESS_LOST and pump rebuilds it, so the
    /// rebuilt one re-reads the (possibly new) orientation for free.
    rotation_deg: u32,
    /// Latest cursor bitmap (see [`CursorShape`]); `None` until the first
    /// shape arrives.
    cursor: Option<CursorShape>,
    /// Cursor top-left on the desktop surface, and whether it's showing —
    /// from `DXGI_OUTDUPL_FRAME_INFO.PointerPosition`, updated on every mouse
    /// move (`LastMouseUpdateTime != 0`).
    ptr_x: i32,
    ptr_y: i32,
    ptr_visible: bool,
    /// The persistent cursor-free desktop image, refreshed in place by each
    /// damage frame's readback (the swizzle writes straight into it — no
    /// per-frame allocation, no zeroing) and re-emitted with the cursor at
    /// its new spot on pointer-only moves. Every outgoing frame is one
    /// copy-out of this buffer plus a small cursor-rect composite; the old
    /// shape paid a zeroed allocation, a swizzle, AND a full-frame clone per
    /// damage frame. Empty until the first readback.
    clean: Vec<u8>,
    clean_dims: (u32, u32),
    /// Rate limit for cursor-only re-emits so a fast mouse can't spin the
    /// pump faster than the capture cadence.
    last_cursor_emit: Instant,
    /// Spent frame buffers handed back by the consumer (see [`start`]);
    /// `None` until the session wires it.
    reclaim: Option<mpsc::Receiver<Vec<u8>>>,
}

impl Duplication {
    fn new(monitor_id: u32) -> Result<Self, String> {
        Self::open(|device| unsafe { duplicate_output(device, monitor_id) })
    }

    fn new_named(device_name: &str) -> Result<Self, String> {
        Self::open(|device| unsafe { duplicate_output_named(device, device_name) })
    }

    fn open(
        bind: impl FnOnce(&ID3D11Device) -> Result<BoundOutput, String>,
    ) -> Result<Self, String> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
            let device = device.ok_or("D3D11CreateDevice returned no device")?;
            let context = context.ok_or("D3D11CreateDevice returned no context")?;
            let bound = bind(&device)?;
            Ok(Duplication {
                device,
                context,
                dup: Some(bound.dup),
                monitor_id: bound.monitor_id,
                device_name: bound.device_name,
                staging: None,
                rotation_deg: bound.rotation_deg,
                cursor: None,
                ptr_x: 0,
                ptr_y: 0,
                ptr_visible: false,
                clean: Vec::new(),
                clean_dims: (0, 0),
                last_cursor_emit: Instant::now(),
                reclaim: None,
            })
        }
    }

    /// Wait up to `timeout_ms` for the desktop to change; `Ok(None)` on
    /// timeout or a cursor-only update (no new pixels).
    fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<RawFrame>, NextError> {
        unsafe {
            let dup = self
                .dup
                .as_ref()
                .cloned()
                .ok_or_else(|| NextError::Fatal("duplication is not bound".into()))?;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(e) = dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                return match e.code() {
                    c if c == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                    c if c == DXGI_ERROR_ACCESS_LOST => Err(NextError::AccessLost),
                    _ => Err(NextError::Fatal(e.to_string())),
                };
            }
            // From here the frame is held. Pointer metadata must be read
            // while it's held; so must the GPU copy be *queued*. Everything
            // else happens after release.
            if info.PointerShapeBufferSize > 0 {
                // A new pointer bitmap is available — cache it. Best-effort:
                // a failed fetch keeps the previous shape, never breaks capture.
                self.update_cursor_shape(&dup, info.PointerShapeBufferSize);
            }
            if info.LastMouseUpdateTime != 0 {
                let p = info.PointerPosition;
                self.ptr_x = p.Position.x;
                self.ptr_y = p.Position.y;
                self.ptr_visible = p.Visible.as_bool();
            }
            if info.LastPresentTime == 0 {
                // No new desktop pixels: release at once. If only the pointer
                // moved, re-emit the retained clean frame with the cursor at
                // its new spot so it doesn't freeze on a static screen.
                let _ = dup.ReleaseFrame();
                return Ok(self.cursor_only_frame());
            }
            // New desktop pixels: queue the GPU copy into our reusable
            // staging texture, then release the frame IMMEDIATELY — before
            // the CPU map/swizzle. Holding it through the readback throttled
            // the duplication itself (the compositor can't hand over the
            // next frame until release); copy-then-release-then-map is the
            // documented fast path, and `Map` still waits for the queued
            // copy to complete.
            let queued = self.queue_copy(resource);
            let _ = dup.ReleaseFrame();
            match queued.map_err(NextError::Fatal)? {
                Some((staging, w, h)) => self.read_back(&staging, w, h).map_err(NextError::Fatal),
                None => Ok(None),
            }
        }
    }

    /// Fetch and cache the current pointer bitmap (held-frame only). Silent on
    /// failure — the cursor just keeps its last shape.
    unsafe fn update_cursor_shape(&mut self, dup: &IDXGIOutputDuplication, size: u32) {
        if let Some(shape) = fetch_cursor_shape(dup, size) {
            self.cursor = Some(shape);
        }
    }

    /// A pointer-only update (no desktop change): re-emit the retained clean
    /// frame with the cursor moved to its new spot, rate-limited to the
    /// capture cadence. `None` when there's nothing to show or the limiter
    /// says wait.
    fn cursor_only_frame(&mut self) -> Option<RawFrame> {
        if !self.ptr_visible || self.cursor.is_none() || self.clean.is_empty() {
            return None;
        }
        if self.last_cursor_emit.elapsed() < Duration::from_millis(15) {
            return None;
        }
        let (w, h) = self.clean_dims;
        Some(self.assemble_frame(w, h))
    }

    /// Queue the GPU copy of the held frame into the reusable staging
    /// texture. Runs while the frame is held; the caller releases the frame
    /// right after, before any CPU readback. `Ok(None)` = degenerate frame.
    unsafe fn queue_copy(
        &mut self,
        resource: Option<IDXGIResource>,
    ) -> Result<Option<(ID3D11Texture2D, u32, u32)>, String> {
        let resource = resource.ok_or("AcquireNextFrame returned no resource")?;
        let texture: ID3D11Texture2D = resource.cast().map_err(|e| e.to_string())?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let (w, h) = (desc.Width, desc.Height);
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let staging = self.staging_for(w, h)?;
        self.context.CopyResource(&staging, &texture);
        Ok(Some((staging, w, h)))
    }

    /// Map the staging copy and swizzle it straight into the persistent
    /// cursor-free desktop buffer (`Map` waits for the queued copy), then
    /// assemble the outgoing frame.
    unsafe fn read_back(
        &mut self,
        staging: &ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Result<Option<RawFrame>, String> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map staging: {e}"))?;
        let pitch = mapped.RowPitch as usize;
        let (wu, hu) = (w as usize, h as usize);
        let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * hu);
        // BGRA → RGBA fused into the copy we must do anyway; alpha forced
        // opaque (duplication leaves it undefined). Lives in the pixels
        // crate so it compiles at opt-level 3 in every profile.
        allmystuff_pixels::bgra_to_rgba_into(src, pitch, wu, hu, &mut self.clean);
        self.context.Unmap(staging, 0);
        self.clean_dims = (w, h);
        Ok(Some(self.assemble_frame(w, h)))
    }

    /// The outgoing frame: one copy-out of the clean desktop plus the real
    /// OS pointer composited over the copy's small rect — Desktop
    /// Duplication delivers the desktop WITHOUT the cursor (it's a hardware
    /// overlay), so we draw it in ourselves, matching what macOS/Linux
    /// capture bakes in. `clean` itself stays cursor-free so a pointer-only
    /// move re-emits without re-capturing; compositing runs on the
    /// pre-rotation buffer, so the cursor rotates with the frame downstream.
    fn assemble_frame(&mut self, w: u32, h: u32) -> RawFrame {
        // Copy out into a reclaimed buffer when the consumer has returned
        // one — warm pages instead of a fresh demand-zeroed allocation.
        let mut rgba = self
            .reclaim
            .as_ref()
            .and_then(|r| r.try_recv().ok())
            .unwrap_or_default();
        rgba.clear();
        rgba.extend_from_slice(&self.clean);
        if self.ptr_visible {
            if let Some(cur) = &self.cursor {
                composite_cursor(&mut rgba, w, h, cur, self.ptr_x, self.ptr_y);
            }
        }
        self.last_cursor_emit = Instant::now();
        RawFrame {
            rgba,
            width: w,
            height: h,
            rotation_deg: self.rotation_deg,
        }
    }

    /// The reusable CPU-readable texture, rebuilt when the desktop size
    /// changes (resolution switch mid-session).
    unsafe fn staging_for(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
        if let Some((tex, sw, sh)) = &self.staging {
            if (*sw, *sh) == (w, h) {
                return Ok(tex.clone());
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| format!("CreateTexture2D (staging): {e}"))?;
        let tex = tex.ok_or("CreateTexture2D returned no texture")?;
        self.staging = Some((tex.clone(), w, h));
        Ok(tex)
    }
}

/// Draw the DXGI cursor bitmap into an RGBA desktop frame with its top-left at
/// `(px, py)`, clipped to the frame. Covers the three
/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`s (color / masked-color / monochrome); an
/// unknown type is skipped. Fully bounds-checked, so a malformed shape can only
/// draw less, never out of bounds.
fn composite_cursor(dst: &mut [u8], dw: u32, dh: u32, cur: &CursorShape, px: i32, py: i32) {
    composite_cursor_impl::<false>(dst, dw, dh, cur, px, py)
}

/// [`composite_cursor`] generic over the destination's channel order —
/// `BGRA_DST = true` blends into BGRA pixels (the GPU lane's save-under
/// patch, which never leaves the GPU's native order), `false` into RGBA
/// (the CPU lane's frame buffers). The cursor source is BGRA either way;
/// only the write order differs, and the const parameter folds the swap
/// away.
fn composite_cursor_impl<const BGRA_DST: bool>(
    dst: &mut [u8],
    dw: u32,
    dh: u32,
    cur: &CursorShape,
    px: i32,
    py: i32,
) {
    let dw = dw as i32;
    let dh = dh as i32;
    let pitch = cur.pitch as usize;
    match cur.kind {
        // Color: 32bpp BGRA, straight alpha — the usual modern cursor.
        2 => {
            let (cw, ch) = (cur.width as i32, cur.height as i32);
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= dh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= dw {
                        continue;
                    }
                    let s = cy as usize * pitch + cx as usize * 4;
                    if s + 3 >= cur.buf.len() {
                        continue;
                    }
                    let a = cur.buf[s + 3] as u32;
                    if a == 0 {
                        continue;
                    }
                    let (b, g, r) = (
                        cur.buf[s] as u32,
                        cur.buf[s + 1] as u32,
                        cur.buf[s + 2] as u32,
                    );
                    let (c0, c2) = if BGRA_DST { (b, r) } else { (r, b) };
                    let d = (dy as usize * dw as usize + dx as usize) * 4;
                    dst[d] = ((c0 * a + dst[d] as u32 * (255 - a)) / 255) as u8;
                    dst[d + 1] = ((g * a + dst[d + 1] as u32 * (255 - a)) / 255) as u8;
                    dst[d + 2] = ((c2 * a + dst[d + 2] as u32 * (255 - a)) / 255) as u8;
                }
            }
        }
        // Masked color: 32bpp BGRA where the alpha byte is a 1-bit mask —
        // 0 = paint the RGB opaque, 0xFF = XOR the RGB onto the screen.
        4 => {
            let (cw, ch) = (cur.width as i32, cur.height as i32);
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= dh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= dw {
                        continue;
                    }
                    let s = cy as usize * pitch + cx as usize * 4;
                    if s + 3 >= cur.buf.len() {
                        continue;
                    }
                    let (b, g, r, a) = (cur.buf[s], cur.buf[s + 1], cur.buf[s + 2], cur.buf[s + 3]);
                    let (c0, c2) = if BGRA_DST { (b, r) } else { (r, b) };
                    let d = (dy as usize * dw as usize + dx as usize) * 4;
                    if a == 0 {
                        dst[d] = c0;
                        dst[d + 1] = g;
                        dst[d + 2] = c2;
                    } else {
                        dst[d] ^= c0;
                        dst[d + 1] ^= g;
                        dst[d + 2] ^= c2;
                    }
                }
            }
        }
        // Monochrome: two stacked 1bpp masks (AND over XOR); the real height is
        // half `Height`. AND=0 -> opaque (XOR selects black/white); AND=1,XOR=1
        // -> invert the screen; AND=1,XOR=0 -> transparent.
        1 => {
            let cw = cur.width as i32;
            let ch = (cur.height / 2) as i32;
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= dh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= dw {
                        continue;
                    }
                    let byte = cx as usize / 8;
                    let bit = 7 - (cx as usize % 8);
                    let and_at = cy as usize * pitch + byte;
                    let xor_at = (cy + ch) as usize * pitch + byte;
                    if and_at >= cur.buf.len() || xor_at >= cur.buf.len() {
                        continue;
                    }
                    let and_bit = (cur.buf[and_at] >> bit) & 1;
                    let xor_bit = (cur.buf[xor_at] >> bit) & 1;
                    let d = (dy as usize * dw as usize + dx as usize) * 4;
                    if and_bit == 0 {
                        let v = if xor_bit == 1 { 255 } else { 0 };
                        dst[d] = v;
                        dst[d + 1] = v;
                        dst[d + 2] = v;
                    } else if xor_bit == 1 {
                        dst[d] = 255 - dst[d];
                        dst[d + 1] = 255 - dst[d + 1];
                        dst[d + 2] = 255 - dst[d + 2];
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(feature = "host")]
pub use gpu_lane::{start_gpu, start_gpu_named, GpuFrame, GpuLane};

/// The GPU zero-copy capture lane: duplication, cursor composite, and
/// BGRA→NV12 conversion all on **one** D3D11 device, frames leaving as
/// NV12 *textures* for a device-manager-fed encoder MFT
/// (`video::run_gpu_lane` is the consuming half). Per frame the CPU
/// touches at most a cursor-sized save-under patch — the pointer is the
/// one thing Desktop Duplication never draws, so its rect round-trips
/// through a tiny staging texture while everything else stays GPU work:
/// acquired frame → `CopyResource` into the persistent clean texture →
/// VideoProcessor blt (fused colour convert + scale) → encoder reads the
/// ring texture in place.
#[cfg(feature = "host")]
mod gpu_lane {
    use super::*;
    use crate::gpu_pipeline::{create_video_device, GpuConvert, NV12_RING};
    use windows::Win32::Foundation::LUID;
    use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_USAGE_DEFAULT};
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager;

    /// Map a QPC stamp (DXGI's `LastPresentTime`) onto the monotonic
    /// clock: age = (now_qpc − stamp)/frequency, subtracted from a
    /// paired `Instant::now()`. Windows' `Instant` IS QPC underneath, so
    /// the pairing error is the nanoseconds between the two reads.
    /// Degenerate stamps (future/zero frequency) clamp to "now".
    fn qpc_to_instant(stamp: i64) -> Instant {
        use windows_sys::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };
        let now = Instant::now();
        let (mut qpc_now, mut freq) = (0i64, 0i64);
        unsafe {
            QueryPerformanceCounter(&mut qpc_now);
            QueryPerformanceFrequency(&mut freq);
        }
        if freq <= 0 || qpc_now <= stamp {
            return now;
        }
        let age_ns = (qpc_now - stamp) as u128 * 1_000_000_000 / freq as u128;
        // `checked_sub`, not `-`: a pathological present-stamp (e.g. a
        // negative `LastPresentTime` slipping past the `qpc_now <= stamp`
        // guard) yields an `age` beyond the monotonic epoch, and
        // `Instant - Duration` panics (→ abort) on underflow.
        now.checked_sub(Duration::from_nanos(age_ns.min(u64::MAX as u128) as u64))
            .unwrap_or(now)
    }

    /// One GPU-lane frame: a checked-out NV12 ring texture the encoder
    /// reads in place. `slot` rides back on [`GpuLane::release`] once
    /// nothing downstream can still read the texture.
    pub struct GpuFrame {
        pub slot: usize,
        pub tex: ID3D11Texture2D,
        /// The texture's (fitted) size.
        pub out_w: u32,
        pub out_h: u32,
        /// CPU time the capture side spent producing this frame (copy
        /// queue + cursor patch + blt queue) — the lane's analog of the
        /// CPU lane's convert column.
        pub spent: Duration,
        /// When the compositor PRESENTED the desktop pixels this frame
        /// carries (`LastPresentTime`, mapped onto the monotonic clock at
        /// acquire) — the anchor of the M1 capture-age span: encode
        /// start minus this is how stale the pixels already were before
        /// the encoder ever saw them (queue wait + freshest-wins
        /// displacement included). `None` only for cursor-only re-emits
        /// before the first clean desktop.
        pub presented: Option<Instant>,
    }

    // SAFETY: the texture lives on a multithread-protected device (see
    // `gpu_pipeline::create_video_device`); a frame is produced by the
    // capture thread, sent, and from then on touched only by the route
    // thread — never concurrently.
    unsafe impl Send for GpuFrame {}

    /// A running GPU capture lane, as handed to `video::run_gpu_lane`.
    /// Dropping it (the `session`) stops the capture thread and releases
    /// the duplication.
    pub struct GpuLane {
        pub session: Session,
        pub frames: mpsc::Receiver<GpuFrame>,
        /// Spent ring slots ride back to the capture side on this.
        pub release: mpsc::SyncSender<usize>,
        /// The lane's D3D11 device — the direct-NVENC rung opens its
        /// session on it (input textures must be device-local).
        pub device: ID3D11Device,
        /// The device manager the encoder MFT must be opened with.
        pub manager: IMFDXGIDeviceManager,
        /// The duplication device's adapter — scopes encoder enumeration
        /// to the GPU that actually holds the textures.
        pub adapter_luid: LUID,
        /// Fitted output size (what the NV12 textures measure).
        pub out_size: (u32, u32),
        /// The desktop's size (pre-fit) — for re-fitting when the edge
        /// cap changes mid-route.
        pub src_size: (u32, u32),
    }

    /// Start the GPU lane on `monitor_id`, fitting output to `out_edge`.
    /// Every failure is soft — the caller falls back to the CPU lane.
    /// Rotated outputs are declined here: the VideoProcessor could rotate,
    /// but they're rare and the CPU lane's rotation path is proven.
    pub fn start_gpu(monitor_id: u32, out_edge: u32) -> Result<GpuLane, String> {
        start_gpu_named(monitor_id, None, out_edge)
    }

    /// Start the experimental shared-texture lane on a stable display name
    /// when the route has one. Repeated lane rebuilds therefore cannot follow
    /// a recycled numeric monitor handle onto a different screen.
    pub fn start_gpu_named(
        monitor_id: u32,
        stable_name: Option<&str>,
        out_edge: u32,
    ) -> Result<GpuLane, String> {
        let (device, context) = create_video_device()?;
        let bound = unsafe {
            match stable_name {
                Some(name) => duplicate_output_named(&device, name)?,
                None => duplicate_output(&device, monitor_id)?,
            }
        };
        if bound.rotation_deg != 0 {
            return Err(format!(
                "output is rotated ({}°) — the CPU lane handles rotation",
                bound.rotation_deg
            ));
        }
        let desc = unsafe { bound.dup.GetDesc() };
        let (sw, sh) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        if sw == 0 || sh == 0 {
            return Err("duplication reports a degenerate mode".into());
        }
        let (dw, dh) = crate::video::fit_within_even(sw, sh, out_edge);
        let gpu = GpuConvert::on_device(device.clone(), context.clone(), sw, sh, dw, dh)?;
        let manager = gpu.manager();
        let adapter_luid = unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(|e| e.to_string())?;
            let adapter: IDXGIAdapter = dxgi.GetAdapter().map_err(|e| e.to_string())?;
            let desc = adapter.GetDesc().map_err(|e| e.to_string())?;
            let name = String::from_utf16_lossy(&desc.Description);
            // The field-log identification line: which physical GPU this
            // lane lives on, for which monitor, at what geometry.
            tracing::info!(
                "GPU lane device for monitor {monitor_id:#x}: {} (LUID {:08x}-{:08x}) · \
                 desktop {sw}×{sh} → fitted {dw}×{dh}",
                name.trim_end_matches('\0').trim(),
                desc.AdapterLuid.HighPart,
                desc.AdapterLuid.LowPart,
            );
            desc.AdapterLuid
        };
        let clean = bgra_default_texture(&device, sw, sh)?;
        let (tx, rx) = mpsc::sync_channel::<GpuFrame>(2);
        let (release_tx, release_rx) = mpsc::sync_channel::<usize>(NV12_RING);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let mut d = GpuDup {
            device,
            context,
            dup: Some(bound.dup),
            monitor_id: bound.monitor_id,
            device_name: bound.device_name,
            gpu,
            clean,
            cursor_staging: None,
            patch: Vec::new(),
            src: (sw, sh),
            cursor: None,
            ptr_x: 0,
            ptr_y: 0,
            ptr_visible: false,
            have_clean: false,
            presented: None,
            last_emit: Instant::now(),
            release: release_rx,
        };
        let device = d.device.clone();
        let thread = std::thread::spawn(move || pump_gpu(&mut d, &stop_thread, &tx));
        Ok(GpuLane {
            session: Session {
                stop,
                thread: Some(thread),
            },
            frames: rx,
            release: release_tx,
            device,
            manager,
            adapter_luid,
            out_size: (dw, dh),
            src_size: (sw, sh),
        })
    }

    /// A GPU-resident BGRA texture with no bind flags — copy target, blt
    /// input.
    fn bgra_default_texture(
        device: &ID3D11Device,
        w: u32,
        h: u32,
    ) -> Result<ID3D11Texture2D, String> {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .map_err(|e| format!("CreateTexture2D (clean): {e}"))?;
            tex.ok_or_else(|| "CreateTexture2D returned no texture".to_string())
        }
    }

    /// The GPU lane's duplication state, owned by its capture thread.
    struct GpuDup {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        dup: Option<IDXGIOutputDuplication>,
        monitor_id: u32,
        device_name: String,
        gpu: GpuConvert,
        /// The persistent cursor-free desktop image — GPU-resident,
        /// refreshed by each damage frame's `CopyResource`. The cursor
        /// patch is drawn on and restored around each blt, so between
        /// frames it is always clean (pointer-only moves re-blt it).
        clean: ID3D11Texture2D,
        /// Save-under staging for the cursor rect: pristine pixels out
        /// (`Map`), patched pixels back (`UpdateSubresource`), pristine
        /// restored after the blt. Sized to the current shape; rebuilt
        /// when a bigger one arrives.
        cursor_staging: Option<(ID3D11Texture2D, u32, u32)>,
        /// CPU-side patch buffer, reused across frames.
        patch: Vec<u8>,
        src: (u32, u32),
        cursor: Option<CursorShape>,
        ptr_x: i32,
        ptr_y: i32,
        ptr_visible: bool,
        have_clean: bool,
        /// When the clean desktop's pixels were PRESENTED by the
        /// compositor (QPC `LastPresentTime` mapped to the monotonic
        /// clock at acquire) — carried onto every frame composed from
        /// them, cursor-only re-emits included (the desktop really is
        /// that old).
        presented: Option<Instant>,
        /// Rate limit for pointer-only re-emits (see [`Duplication`]).
        last_emit: Instant,
        release: mpsc::Receiver<usize>,
    }

    // SAFETY: all COM state lives on a multithread-protected device and is
    // driven only by the capture thread this value moves onto at spawn.
    unsafe impl Send for GpuDup {}

    fn pump_gpu(d: &mut GpuDup, stop: &AtomicBool, tx: &mpsc::SyncSender<GpuFrame>) {
        crate::os_perf::boost_media_thread();
        while !stop.load(Ordering::SeqCst) {
            // Slots the consumer has finished with go back into rotation
            // before we look for a frame to put in one.
            while let Ok(slot) = d.release.try_recv() {
                d.gpu.release(slot);
            }
            match d.next_gpu_frame(100, tx) {
                Ok(()) => {}
                Err(NextError::AccessLost) => {
                    // Mode change / fullscreen toggle / secure desktop.
                    // Re-acquire the same named output on the same device;
                    // Windows may have replaced its raw HMONITOR. If the mode came
                    // back *different* (size or rotation), every fitted
                    // size in the lane is stale: end the thread and let
                    // the consumer restart the lane fresh.
                    let old_id = d.monitor_id;
                    let device_name = d.device_name.clone();
                    tracing::debug!(
                        "GPU-lane duplication of {device_name} ({old_id:#x}) lost — re-acquiring by name"
                    );
                    // A lost duplication must be released before DXGI will
                    // permit DuplicateOutput on the replacement interface.
                    drop(d.dup.take());
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        match unsafe { duplicate_output_named(&d.device, &device_name) } {
                            Ok(bound) => {
                                let desc = unsafe { bound.dup.GetDesc() };
                                if bound.rotation_deg != 0
                                    || (desc.ModeDesc.Width, desc.ModeDesc.Height) != d.src
                                {
                                    tracing::debug!(
                                        "GPU-lane duplication came back with a different mode — \
                                         lane restart"
                                    );
                                    return;
                                }
                                tracing::info!(
                                    "GPU-lane duplication rebound {device_name}: {old_id:#x} -> {:#x}",
                                    bound.monitor_id
                                );
                                d.dup = Some(bound.dup);
                                d.monitor_id = bound.monitor_id;
                                d.device_name = bound.device_name;
                                break;
                            }
                            Err(e) if Instant::now() < deadline => {
                                tracing::debug!("GPU-lane re-acquire not ready ({e}); retrying");
                                std::thread::sleep(Duration::from_millis(250));
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "GPU-lane duplication of {device_name} ({old_id:#x}) \
                                     not recoverable: {e}"
                                );
                                return;
                            }
                        }
                    }
                }
                Err(NextError::Fatal(e)) => {
                    tracing::warn!(
                        "GPU-lane capture for {} ({:#x}) ended: {e}",
                        d.device_name,
                        d.monitor_id
                    );
                    return;
                }
            }
        }
    }

    impl GpuDup {
        /// One acquire tick: refresh cursor metadata, refresh the clean
        /// desktop texture on damage, and send a composed NV12 frame when
        /// there's anything new to show.
        fn next_gpu_frame(
            &mut self,
            timeout_ms: u32,
            tx: &mpsc::SyncSender<GpuFrame>,
        ) -> Result<(), NextError> {
            unsafe {
                let dup = self
                    .dup
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| NextError::Fatal("GPU duplication is not bound".into()))?;
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                if let Err(e) = dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                    return match e.code() {
                        c if c == DXGI_ERROR_WAIT_TIMEOUT => Ok(()),
                        c if c == DXGI_ERROR_ACCESS_LOST => Err(NextError::AccessLost),
                        _ => Err(NextError::Fatal(e.to_string())),
                    };
                }
                if info.PointerShapeBufferSize > 0 {
                    if let Some(shape) = fetch_cursor_shape(&dup, info.PointerShapeBufferSize) {
                        self.cursor = Some(shape);
                    }
                }
                if info.LastMouseUpdateTime != 0 {
                    let p = info.PointerPosition;
                    self.ptr_x = p.Position.x;
                    self.ptr_y = p.Position.y;
                    self.ptr_visible = p.Visible.as_bool();
                }
                if info.LastPresentTime != 0 {
                    // New desktop pixels: queue the GPU copy into the
                    // persistent clean texture, then release immediately —
                    // same discipline as the CPU lane (holding the frame
                    // throttles the compositor).
                    let refreshed = self.refresh_clean(resource);
                    let _ = dup.ReleaseFrame();
                    if !refreshed {
                        return Ok(());
                    }
                    self.have_clean = true;
                    // M1 capture-age anchor: map the compositor's QPC
                    // present stamp onto the monotonic clock, so the
                    // encode side can say how stale these pixels were by
                    // the time it saw them.
                    self.presented = Some(qpc_to_instant(info.LastPresentTime));
                } else {
                    let _ = dup.ReleaseFrame();
                    // Pointer-only update: re-emit the retained clean
                    // desktop with the cursor at its new spot, rate-limited
                    // so a fast mouse can't spin the pump.
                    if !self.have_clean || !self.ptr_visible || self.cursor.is_none() {
                        return Ok(());
                    }
                    if self.last_emit.elapsed() < Duration::from_millis(15) {
                        return Ok(());
                    }
                }
                self.compose_and_send(tx)
            }
        }

        /// Queue the copy of a held frame's texture into `clean`. `false`
        /// when the resource is missing or degenerate — skip the frame.
        unsafe fn refresh_clean(&mut self, resource: Option<IDXGIResource>) -> bool {
            let Some(resource) = resource else {
                return false;
            };
            let Ok(texture) = resource.cast::<ID3D11Texture2D>() else {
                return false;
            };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            if (desc.Width, desc.Height) != self.src {
                // A size change without ACCESS_LOST shouldn't happen; skip
                // rather than corrupt (the mode-change path restarts the
                // lane).
                return false;
            }
            self.context.CopyResource(&self.clean, &texture);
            true
        }

        /// Blt the clean desktop — with the cursor patched over its rect —
        /// to a free NV12 ring slot and hand it to the consumer. All GPU
        /// work except the cursor-sized patch blend.
        fn compose_and_send(&mut self, tx: &mpsc::SyncSender<GpuFrame>) -> Result<(), NextError> {
            let t0 = Instant::now();
            let patched = if self.ptr_visible && self.cursor.is_some() {
                self.patch_cursor_on_clean()
            } else {
                None
            };
            let converted = self.gpu.convert(&self.clean);
            // Restore is queued after the blt: the outgoing frame keeps
            // the cursor while `clean` returns to cursor-free.
            if let Some(rect) = patched {
                self.restore_clean(rect);
            }
            let (slot, tex) = match converted {
                Ok(Some(s)) => s,
                // Every slot still checked out — the consumer is far
                // behind, and this frame is stale by definition.
                Ok(None) => return Ok(()),
                Err(e) => return Err(NextError::Fatal(format!("GPU convert: {e}"))),
            };
            self.last_emit = Instant::now();
            let (out_w, out_h) = self.gpu.out_size();
            let frame = GpuFrame {
                slot,
                tex,
                out_w,
                out_h,
                spent: t0.elapsed(),
                presented: self.presented,
            };
            if tx.try_send(frame).is_err() {
                // Full channel = consumer behind; reclaim the slot at once.
                self.gpu.release(slot);
            }
            Ok(())
        }

        /// Draw the cursor over `clean`'s rect via the save-under staging:
        /// pristine rect out (`Map`), blend on the CPU, patched rect back.
        /// Returns the affected rect for [`Self::restore_clean`], or
        /// `None` when nothing was drawn (off-screen cursor, staging
        /// trouble) — the frame then goes out cursor-less, like a failed
        /// shape fetch on the CPU lane.
        fn patch_cursor_on_clean(&mut self) -> Option<(u32, u32, u32, u32)> {
            let (cw, ch) = {
                let cur = self.cursor.as_ref()?;
                let eff_h = cur.height / if cur.kind == 1 { 2 } else { 1 };
                (cur.width, eff_h)
            };
            if cw == 0 || ch == 0 {
                return None;
            }
            let (sw, sh) = self.src;
            let rx = self.ptr_x.max(0);
            let ry = self.ptr_y.max(0);
            let right = (self.ptr_x + cw as i32).min(sw as i32);
            let bottom = (self.ptr_y + ch as i32).min(sh as i32);
            if rx >= right || ry >= bottom {
                return None;
            }
            let (rw, rh) = ((right - rx) as u32, (bottom - ry) as u32);
            let (rx, ry) = (rx as u32, ry as u32);
            unsafe {
                let staging = self.staging_for_cursor(cw, ch)?;
                let src_box = D3D11_BOX {
                    left: rx,
                    top: ry,
                    front: 0,
                    right: rx + rw,
                    bottom: ry + rh,
                    back: 1,
                };
                self.context.CopySubresourceRegion(
                    &staging,
                    0,
                    0,
                    0,
                    0,
                    &self.clean,
                    0,
                    Some(&src_box),
                );
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                if self
                    .context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .is_err()
                {
                    return None;
                }
                let pitch = mapped.RowPitch as usize;
                let (rwu, rhu) = (rw as usize, rh as usize);
                let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rhu);
                self.patch.clear();
                self.patch.reserve(rwu * rhu * 4);
                for row in 0..rhu {
                    self.patch.extend_from_slice(&src[row * pitch..][..rwu * 4]);
                }
                self.context.Unmap(&staging, 0);
                let cur = self.cursor.as_ref()?;
                composite_cursor_impl::<true>(
                    &mut self.patch,
                    rw,
                    rh,
                    cur,
                    // Cursor position relative to the patch origin (≤0 when
                    // the shape is clipped at the left/top edge).
                    self.ptr_x - rx as i32,
                    self.ptr_y - ry as i32,
                );
                let dst_box = D3D11_BOX {
                    left: rx,
                    top: ry,
                    front: 0,
                    right: rx + rw,
                    bottom: ry + rh,
                    back: 1,
                };
                self.context.UpdateSubresource(
                    &self.clean,
                    0,
                    Some(&dst_box),
                    self.patch.as_ptr() as *const core::ffi::c_void,
                    rw * 4,
                    0,
                );
                Some((rx, ry, rw, rh))
            }
        }

        /// Undo the cursor patch: copy the pristine save-under back onto
        /// `clean`'s rect. Queued after the blt, so ordering on the
        /// immediate context guarantees the outgoing frame saw the cursor.
        fn restore_clean(&mut self, (rx, ry, rw, rh): (u32, u32, u32, u32)) {
            let Some((staging, _, _)) = &self.cursor_staging else {
                return;
            };
            unsafe {
                let src_box = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: rw,
                    bottom: rh,
                    back: 1,
                };
                self.context.CopySubresourceRegion(
                    &self.clean,
                    0,
                    rx,
                    ry,
                    0,
                    staging,
                    0,
                    Some(&src_box),
                );
            }
        }

        /// The save-under staging texture, at least the current shape's
        /// size (recreated only when a bigger shape arrives).
        unsafe fn staging_for_cursor(&mut self, w: u32, h: u32) -> Option<ID3D11Texture2D> {
            if let Some((tex, tw, th)) = &self.cursor_staging {
                if *tw >= w && *th >= h {
                    return Some(tex.clone());
                }
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            if self
                .device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .is_err()
            {
                return None;
            }
            let tex = tex?;
            self.cursor_staging = Some((tex.clone(), w, h));
            Some(tex)
        }
    }
}
