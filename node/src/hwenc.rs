//! Hardware H.264 encode via FFmpeg's vendor encoders — `h264_nvenc` (real
//! NVENC, the gold standard), `h264_amf` (AMD), `h264_qsv` (Intel QuickSync),
//! `h264_videotoolbox` (macOS), `h264_vaapi` (Linux). One thin backend
//! parameterised by the FFmpeg encoder name; the ladder in `video.rs` tries the
//! platform's candidates in priority order and steps down on a frame-send test
//! until one actually emits frames, falling to software openh264 as the floor.
//!
//! Feature-gated behind `hwenc`. Input is the same contiguous I420 buffer the
//! software path already produces (`allmystuff_pixels::scale_rgba_to_i420`), so
//! the encoder seam doesn't change: I420 in, Annex-B H.264 out.

use std::sync::Once;

use ffmpeg_next as ff;

use crate::video::EncodeOutcome;

static FF_INIT: Once = Once::new();

/// The hardware H.264 encoders to try, best first, for the current platform.
/// NVENC leads everywhere it exists (we ship mostly NVIDIA); each OS then lists
/// its remaining native options. The ladder frame-send-tests each in turn.
pub fn candidates() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["h264_nvenc", "h264_amf", "h264_qsv"]
    }
    #[cfg(target_os = "macos")]
    {
        &["h264_videotoolbox"]
    }
    #[cfg(target_os = "linux")]
    {
        &["h264_nvenc", "h264_vaapi", "h264_qsv"]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

/// One opened FFmpeg hardware H.264 encoder.
pub struct FfmpegH264 {
    encoder: ff::encoder::video::Encoder,
    name: &'static str,
    width: u32,
    height: u32,
    pts: i64,
}

/// Low-latency option dictionary for a given FFmpeg encoder — no B-frames, no
/// lookahead, headers in-band (so each forced IDR carries SPS/PPS), and the
/// vendor's "ultra low latency / realtime" knobs.
///
/// `bitrate` is the *average* target; we also hand the vendor rate controllers a
/// **peak (`maxrate`) and VBV (`bufsize`)**. Interactive routes keep a small,
/// cadence-relative reservoir so one scene change cannot become a packet wall;
/// Studio retains its explicit quality-first reservoir.
fn low_latency_opts(
    name: &str,
    bitrate: u32,
    fps: u32,
    game: bool,
    studio: bool,
) -> ff::Dictionary<'static> {
    let mut d = ff::Dictionary::new();
    match name {
        "h264_nvenc" => {
            d.set("preset", "p1"); // fastest
            d.set("tune", "ull"); // ultra-low-latency
            d.set("zerolatency", "1");
            d.set("delay", "0");
            d.set("forced-idr", "1"); // honour our pict_type=I requests
            d.set("rc", "vbr");
        }
        "h264_amf" => {
            d.set("usage", "ultralowlatency");
            d.set("quality", "speed");
            d.set("rc", "vbr_latency");
        }
        "h264_qsv" => {
            d.set("preset", "veryfast");
            d.set("low_power", "1");
            d.set("forced_idr", "1");
            // Low-latency screen-content posture: the remote-desktop
            // scenario hint, no frame queueing (default async_depth is 4 —
            // four frames of latency), no B-frame planning.
            d.set("scenario", "displayremoting");
            d.set("async_depth", "1");
            d.set("b_strategy", "0");
        }
        "h264_videotoolbox" => {
            d.set("realtime", "1");
            d.set("prio_speed", "1");
        }
        "h264_vaapi" => {
            d.set("low_power", "1");
        }
        _ => {}
    }
    // Peak/VBV headroom for every rate-controlled vendor (generic AVOptions →
    // rc_max_rate / rc_buffer_size; the hardware controllers honour them).
    // VideoToolbox ignores these. This follows Tune, not process-wide state:
    // Balanced, Game and Studio routes can encode concurrently.
    if name != "h264_videotoolbox" {
        let (maxrate, bufsize) = crate::video::burst_bounds(bitrate, fps, game, studio);
        d.set("maxrate", &maxrate.to_string());
        d.set("bufsize", &bufsize.to_string());
    }
    d
}

impl FfmpegH264 {
    /// Open `name` for `width`×`height` at `fps`/`bitrate`. Returns `Err` if the
    /// encoder isn't built into FFmpeg or won't open (no hardware, bad params);
    /// the caller steps down to the next candidate.
    pub fn open(
        name: &'static str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        game: bool,
        studio: bool,
    ) -> Result<Self, String> {
        FF_INIT.call_once(|| {
            let _ = ff::init();
        });
        let codec = ff::encoder::find_by_name(name)
            .ok_or_else(|| format!("{name}: not built into this FFmpeg"))?;
        let context = ff::codec::context::Context::new();
        let mut video = context
            .encoder()
            .video()
            .map_err(|e| format!("{name}: encoder().video(): {e}"))?;
        video.set_width(width);
        video.set_height(height);
        video.set_format(ff::format::Pixel::YUV420P);
        // 1/fps time base; we drive pts ourselves, one tick per frame.
        video.set_time_base(ff::Rational::new(1, fps.max(1) as i32));
        video.set_frame_rate(Some(ff::Rational::new(fps.max(1) as i32, 1)));
        video.set_bit_rate(bitrate as usize);
        video.set_max_b_frames(0); // no reordering — latency
                                   // GOP ≈ 4 s: we force IDRs ourselves on the adaptive cadence (2–8 s),
                                   // so this is a backstop, not the primary keyframe source — short
                                   // enough that a late joiner / lost-reference recovers without waiting
                                   // the full relaxed interval, long enough not to spam keyframes.
        video.set_gop(fps.saturating_mul(4).max(1));
        let encoder = video
            .open_as_with(codec, low_latency_opts(name, bitrate, fps, game, studio))
            .map_err(|e| format!("{name}: open: {e}"))?;
        Ok(Self {
            encoder,
            name,
            width,
            height,
            pts: 0,
        })
    }

    pub fn label(&self) -> &'static str {
        self.name
    }

    /// Encode one contiguous I420 frame (`width*height` Y, then quarter-size U,
    /// then V). Returns every Annex-B access unit the encoder had ready, oldest
    /// first (with B-frames off and zero latency that's normally exactly one;
    /// a backend that buffers hands its backlog to a later call) — see
    /// [`EncodeOutcome`]. `send_frame` accepting the frame is what `consumed`
    /// reports. `pub(crate)`: the outcome type is the video module's internal
    /// seam.
    pub(crate) fn encode_i420(
        &mut self,
        i420: &[u8],
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        let (w, h) = (self.width as usize, self.height as usize);
        let ysize = w * h;
        let csize = (w / 2) * (h / 2);
        if i420.len() < ysize + 2 * csize {
            return Err(format!(
                "{}: short I420 ({} < {})",
                self.name,
                i420.len(),
                ysize + 2 * csize
            ));
        }
        let mut frame = ff::frame::Video::new(ff::format::Pixel::YUV420P, self.width, self.height);
        // Stride read before the mutable plane borrow (can't hold both at once).
        let (s0, s1, s2) = (frame.stride(0), frame.stride(1), frame.stride(2));
        copy_plane(frame.data_mut(0), s0, &i420[..ysize], w, h);
        copy_plane(
            frame.data_mut(1),
            s1,
            &i420[ysize..ysize + csize],
            w / 2,
            h / 2,
        );
        copy_plane(
            frame.data_mut(2),
            s2,
            &i420[ysize + csize..ysize + 2 * csize],
            w / 2,
            h / 2,
        );
        let input_ts = self.pts;
        frame.set_pts(Some(input_ts));
        self.pts += 1;
        if force_idr {
            // Request an I-frame; `forced-idr`/`forced_idr` in the open opts make
            // the vendor encoders honour it as a real IDR.
            frame.set_kind(ff::picture::Type::I);
        }
        self.encoder
            .send_frame(&frame)
            .map_err(|e| format!("{}: send_frame: {e}", self.name))?;
        Ok(EncodeOutcome {
            units: self.drain()?,
            consumed: true,
            input_ts: u64::try_from(input_ts).unwrap_or(0),
        })
    }

    /// Pull every packet the encoder has ready — each one is its own access
    /// unit with its own keyframe flag, in encode order (concatenating them
    /// would smear a whole drained backlog into one oversized "unit" with one
    /// timestamp).
    fn drain(&mut self) -> Result<Vec<(Vec<u8>, bool)>, String> {
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut packet = ff::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        if !data.is_empty() {
                            units.push((data.to_vec(), packet.is_key()));
                        }
                    }
                }
                // EAGAIN (needs more input) / EOF — nothing more right now.
                Err(ff::Error::Other {
                    errno: ff::util::error::EAGAIN,
                })
                | Err(ff::Error::Eof) => break,
                Err(e) => return Err(format!("{}: receive_packet: {e}", self.name)),
            }
        }
        Ok(units)
    }
}

/// Row-by-row copy of a tightly packed plane into an FFmpeg frame plane that may
/// have a larger (aligned) stride.
fn copy_plane(dst: &mut [u8], stride: usize, src: &[u8], width: usize, height: usize) {
    for y in 0..height {
        let s = &src[y * width..y * width + width];
        let d = &mut dst[y * stride..y * stride + width];
        d.copy_from_slice(s);
    }
}
