//! Hardware H.264 encode on **macOS via VideoToolbox** — the Apple Silicon /
//! Intel Quick Sync media engine behind `VTCompressionSession`. The ladder in
//! `video.rs` opens it and frame-send-tests it; a Mac that can't produce
//! frames (unlikely, but the ladder trusts nothing) steps down to software
//! openh264 exactly as on every other platform.
//!
//! Why VideoToolbox and not FFmpeg here: the same reason Windows got Media
//! Foundation — FFmpeg's vendor encoders need the libav* toolchain at build
//! time, which end users and our own CI don't carry. VideoToolbox ships
//! inside macOS and the node already links Apple frameworks for capture, so
//! this path adds **no new build dependency** (the FFI below is hand-declared
//! against the system frameworks; CF plumbing rides the `core-foundation`
//! crate that's already a macOS dependency).
//!
//! This closes the gap that made a Mac host the slow one in the fleet:
//! software openh264 at a Retina 2816×1762 @ 60 fps was the encoder the
//! viewer experienced as a 0–6 fps slideshow.
//!
//! Input is the same contiguous I420 the software path produces; it's copied
//! into a planar `y420` CVPixelBuffer (three planes, per-plane stride).
//! Output arrives on VideoToolbox's own callback queue as AVCC samples
//! (length-prefixed NAL units + out-of-band parameter sets); they're
//! converted to the Annex-B byte stream every other backend speaks — SPS/PPS
//! prepended on keyframes — and handed back on the encode thread. Same seam
//! as openh264/MF/FFmpeg: I420 in, Annex-B H.264 out.

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use crate::video::EncodeOutcome;

use core_foundation::base::{Boolean, CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryGetValue, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFStringRef;

// ---- hand-declared FFI (VideoToolbox / CoreMedia / CoreVideo) -----------

type OSStatus = i32;
type CVReturn = i32;
type VTCompressionSessionRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;
type CFArrayRef = *const c_void;

/// `CMTime` by value, as CoreMedia lays it out. `flags = 1` is
/// `kCMTimeFlags_Valid`; all-zero is `kCMTimeInvalid`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

impl CMTime {
    fn valid(value: i64, timescale: i32) -> Self {
        CMTime {
            value,
            timescale,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        }
    }
    fn invalid() -> Self {
        CMTime {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        }
    }
}

/// `'avc1'` — `kCMVideoCodecType_H264`.
const CM_VIDEO_CODEC_TYPE_H264: u32 = u32::from_be_bytes(*b"avc1");
/// `'y420'` — `kCVPixelFormatType_420YpCbCr8Planar`: contiguous planar I420,
/// exactly the layout `scale_rgba_to_i420` hands every backend.
const CV_PIXEL_FORMAT_I420: u32 = u32::from_be_bytes(*b"y420");

type VTCompressionOutputCallback = extern "C" fn(
    output_refcon: *mut c_void,
    source_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample: CMSampleBufferRef,
);

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTCompressionSessionCreate(
        allocator: CFTypeRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFTypeRef,
        output_callback: VTCompressionOutputCallback,
        output_refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);

    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxH264SliceBytes: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetFormatDescription(sample: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample: CMSampleBufferRef,
        create_if_necessary: Boolean,
    ) -> CFArrayRef;
    fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        buffer: CMBlockBufferRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        out_ptr: *mut *const u8,
        out_size: *mut usize,
        out_count: *mut usize,
        out_nal_header_len: *mut i32,
    ) -> OSStatus;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;

    static kCMSampleAttachmentKey_NotSync: CFStringRef;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferCreate(
        allocator: CFTypeRef,
        width: usize,
        height: usize,
        pixel_format: u32,
        attributes: CFDictionaryRef,
        out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> CVReturn;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> CVReturn;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
}

// ---- the encoder ---------------------------------------------------------

/// One encoded access unit as the output callback parsed it.
struct EncodedUnit {
    annexb: Vec<u8>,
    key: bool,
}

/// The output callback's channel end, boxed as the session's refcon. Parsing
/// happens *inside* the callback (the sample buffer is only guaranteed alive
/// for its duration); only owned bytes cross the channel.
struct CallbackSink {
    tx: mpsc::Sender<Result<EncodedUnit, String>>,
}

pub struct VideoToolboxH264 {
    session: VTCompressionSessionRef,
    /// Raw refcon handed to the session; reboxed and dropped after
    /// `VTCompressionSessionInvalidate` guarantees no further callbacks.
    sink: *mut CallbackSink,
    rx: mpsc::Receiver<Result<EncodedUnit, String>>,
    frame_index: i64,
    fps: i32,
    width: usize,
    height: usize,
}

// SAFETY: the session is created and driven from exactly one thread at a time
// — the route's capture/encode thread (`H264Codec` demands `Send` so the
// stream can *move* there, never `Sync`). VideoToolbox itself is documented
// thread-safe for session calls; the raw pointers are owned by this struct.
unsafe impl Send for VideoToolboxH264 {}

impl VideoToolboxH264 {
    /// Open a hardware H.264 compression session for exactly `w`×`h` — the
    /// ladder rebuilds on resize, same contract as every other backend.
    /// Requires the *hardware* encoder: if the box can't provide one, this
    /// fails and the ladder steps down to openh264 (falling back to Apple's
    /// software H.264 would just be a slower openh264 with extra steps).
    pub fn open(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let sink = Box::into_raw(Box::new(CallbackSink { tx }));
        let mut session: VTCompressionSessionRef = std::ptr::null_mut();

        // Encoder specification: hardware or nothing (see above), plus the
        // low-latency rate controller where the system has it (macOS
        // 11.3+): strict one-in-one-out, no reordering, and faster rate
        // adaptation to link changes — the productized form of the realtime
        // posture the properties below ask for piecemeal. The key is built
        // by *name* rather than the linked symbol so binaries still load on
        // older systems (an unknown spec key there fails the create, which
        // is why the create retries without it — hardware-required stays
        // non-negotiable on both attempts).
        let spec_for = |low_latency: bool| unsafe {
            let mut pairs = vec![(
                core_foundation::string::CFString::wrap_under_get_rule(
                    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                )
                .as_CFType(),
                CFBoolean::true_value().as_CFType(),
            )];
            if low_latency {
                pairs.push((
                    core_foundation::string::CFString::new("EnableLowLatencyRateControl")
                        .as_CFType(),
                    CFBoolean::true_value().as_CFType(),
                ));
            }
            CFDictionary::from_CFType_pairs(&pairs)
        };

        let mut status = 0;
        for low_latency in [true, false] {
            let spec = spec_for(low_latency);
            status = unsafe {
                VTCompressionSessionCreate(
                    std::ptr::null(),
                    w as i32,
                    h as i32,
                    CM_VIDEO_CODEC_TYPE_H264,
                    spec.as_concrete_TypeRef(),
                    std::ptr::null(),
                    std::ptr::null(),
                    output_callback,
                    sink.cast(),
                    &mut session,
                )
            };
            if status == 0 && !session.is_null() {
                if !low_latency {
                    tracing::info!(
                        "VideoToolbox opened without low-latency rate control (pre-11.3 macOS?)"
                    );
                }
                break;
            }
            session = std::ptr::null_mut();
        }
        if status != 0 || session.is_null() {
            // Re-own the sink so the failed open leaks nothing.
            unsafe { drop(Box::from_raw(sink)) };
            return Err(format!("VTCompressionSessionCreate: OSStatus {status}"));
        }

        let enc = VideoToolboxH264 {
            session,
            sink,
            rx,
            frame_index: 0,
            fps: fps.max(1) as i32,
            width: w as usize,
            height: h as usize,
        };

        // Streaming posture: realtime, no B-frames (reordering adds latency
        // and complicates the Annex-B chain), Main profile, the ladder's
        // bitrate budget, and an effectively-disabled internal keyframe clock
        // — the stream owns its IDR cadence and forces them per frame.
        unsafe {
            enc.set_prop(kVTCompressionPropertyKey_RealTime, CFBoolean::true_value())?;
            enc.set_prop(
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::false_value(),
            )?;
            let _ = VTSessionSetProperty(
                enc.session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Main_AutoLevel.cast(),
            );
            enc.set_prop(
                kVTCompressionPropertyKey_AverageBitRate,
                CFNumber::from(bitrate as i64),
            )?;
            // Best-effort dials: an encoder that ignores them still streams.
            let _ = VTSessionSetProperty(
                enc.session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                CFNumber::from(f64::from(fps)).as_CFTypeRef(),
            );
            let _ = VTSessionSetProperty(
                enc.session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                CFNumber::from(i64::from(i32::MAX)).as_CFTypeRef(),
            );
            if crate::video::paced_slices_enabled() {
                // Give the app-side pacer real cut points on macOS. This is a
                // slice-shape dial, not a bitrate/VBV clamp: VideoToolbox keeps
                // its quality headroom while no individual NAL can become an
                // unshapable keyframe-sized wall.
                let _ = VTSessionSetProperty(
                    enc.session,
                    kVTCompressionPropertyKey_MaxH264SliceBytes,
                    CFNumber::from(crate::video::PACE_SLICE_BYTES as i64).as_CFTypeRef(),
                );
            }
            let status = VTCompressionSessionPrepareToEncodeFrames(enc.session);
            if status != 0 {
                return Err(format!("VTCompressionSessionPrepare: OSStatus {status}"));
            }
        }
        Ok(enc)
    }

    /// Set a required session property; a refusal fails the open (the ladder
    /// steps down) rather than shipping an encoder in an unknown posture.
    unsafe fn set_prop<T: TCFType>(&self, key: CFStringRef, value: T) -> Result<(), String> {
        let status = VTSessionSetProperty(self.session, key, value.as_CFTypeRef());
        if status != 0 {
            return Err(format!("VTSessionSetProperty: OSStatus {status}"));
        }
        Ok(())
    }

    pub fn label(&self) -> &str {
        "VideoToolbox (hardware)"
    }

    /// Encode one contiguous I420 frame (the trait seam: `w*h` Y then
    /// quarter-size U then V), returning every Annex-B access unit the
    /// session had ready, oldest first — see [`EncodeOutcome`]. (The old
    /// shape returned one queued unit *instead of* encoding the input frame,
    /// silently skipping that frame's content whenever the callback ran a
    /// unit ahead.) `pub(crate)`: the outcome type is the video module's
    /// internal seam.
    pub(crate) fn encode_i420(
        &mut self,
        i420: &[u8],
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        // Everything already delivered goes out first, in order.
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Ok(unit)) => units.push((unit.annexb, unit.key)),
                Ok(Err(e)) => return Err(format!("VideoToolbox encode callback: {e}")),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("VideoToolbox session went away".into())
                }
            }
        }

        let pixel_buffer = self.fill_pixel_buffer(i420)?;
        let frame_props = force_idr.then(|| unsafe {
            CFDictionary::from_CFType_pairs(&[(
                core_foundation::string::CFString::wrap_under_get_rule(
                    kVTEncodeFrameOptionKey_ForceKeyFrame,
                )
                .as_CFType(),
                CFBoolean::true_value().as_CFType(),
            )])
        });

        let pts = CMTime::valid(self.frame_index, self.fps);
        self.frame_index += 1;
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                pts,
                CMTime::invalid(),
                frame_props
                    .as_ref()
                    .map_or(std::ptr::null(), |d| d.as_concrete_TypeRef()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        unsafe { CFRelease(pixel_buffer.cast()) };
        if status != 0 {
            return Err(format!(
                "VTCompressionSessionEncodeFrame: OSStatus {status}"
            ));
        }

        // Realtime + no reordering emits ~1:1 within a frame time; when the
        // drain above found nothing, wait a bounded beat for this frame's own
        // unit — keeps the ladder's frame-send test and the common case 1:1.
        // A media engine running behind delivers into a later call's drain
        // instead of parking the capture thread; an empty return reads as
        // buffering (`consumed` is true either way — EncodeFrame accepted it).
        if units.is_empty() {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(unit)) => units.push((unit.annexb, unit.key)),
                Ok(Err(e)) => return Err(format!("VideoToolbox encode callback: {e}")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("VideoToolbox session went away".into())
                }
            }
            // Surface errors that raced the bounded wait. `while let
            // Ok(Ok(..))` silently stopped on an `Err` callback and left the
            // live session looking healthy until some unrelated later call.
            loop {
                match self.rx.try_recv() {
                    Ok(Ok(extra)) => units.push((extra.annexb, extra.key)),
                    Ok(Err(e)) => return Err(format!("VideoToolbox encode callback: {e}")),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err("VideoToolbox session went away".into())
                    }
                }
            }
        }
        Ok(EncodeOutcome {
            units,
            consumed: true,
            // No ref-invalidation on this rung (like MF): 0 = "no timestamp
            // to invalidate against", per `EncodeOutcome::input_ts`.
            input_ts: 0,
        })
    }

    /// Copy the contiguous I420 planes into a fresh planar `y420`
    /// CVPixelBuffer, honouring each plane's stride.
    fn fill_pixel_buffer(&self, i420: &[u8]) -> Result<CVPixelBufferRef, String> {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w / 2, h / 2);
        let need = w * h + 2 * (cw * ch);
        if i420.len() < need {
            return Err(format!("I420 buffer too small: {} < {need}", i420.len()));
        }
        let mut pb: CVPixelBufferRef = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                std::ptr::null(),
                w,
                h,
                CV_PIXEL_FORMAT_I420,
                std::ptr::null(),
                &mut pb,
            )
        };
        if status != 0 || pb.is_null() {
            return Err(format!("CVPixelBufferCreate: CVReturn {status}"));
        }
        unsafe {
            if CVPixelBufferLockBaseAddress(pb, 0) != 0 {
                CFRelease(pb.cast());
                return Err("CVPixelBufferLockBaseAddress failed".into());
            }
            let planes: [(&[u8], usize, usize); 3] = [
                (&i420[..w * h], w, h),
                (&i420[w * h..w * h + cw * ch], cw, ch),
                (&i420[w * h + cw * ch..need], cw, ch),
            ];
            for (idx, (src, src_stride, rows)) in planes.iter().enumerate() {
                let dst = CVPixelBufferGetBaseAddressOfPlane(pb, idx);
                let dst_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, idx);
                let dst_rows = CVPixelBufferGetHeightOfPlane(pb, idx);
                if dst.is_null() || dst_rows < *rows {
                    CVPixelBufferUnlockBaseAddress(pb, 0);
                    CFRelease(pb.cast());
                    return Err("CVPixelBuffer plane layout mismatch".into());
                }
                for row in 0..*rows {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(row * src_stride),
                        dst.add(row * dst_stride),
                        *src_stride,
                    );
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, 0);
        }
        Ok(pb)
    }
}

impl Drop for VideoToolboxH264 {
    fn drop(&mut self) {
        unsafe {
            // Invalidate synchronously guarantees no further callbacks touch
            // the refcon; only then may the sink box die.
            VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast());
            drop(Box::from_raw(self.sink));
        }
    }
}

/// VideoToolbox's output callback — runs on the session's own queue. Parses
/// the sample to owned bytes (the buffer is only alive for the call) and
/// ships it to the encode thread. Never panics: any malformed sample turns
/// into an `Err` the encode call surfaces.
extern "C" fn output_callback(
    output_refcon: *mut c_void,
    _source_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    sample: CMSampleBufferRef,
) {
    let sink = unsafe { &*output_refcon.cast::<CallbackSink>() };
    if status != 0 {
        let _ = sink.tx.send(Err(format!("OSStatus {status}")));
        return;
    }
    if sample.is_null() {
        return; // a dropped frame (rate control) — nothing to ship
    }
    let _ = sink.tx.send(parse_sample(sample));
}

/// One AVCC sample → an owned Annex-B access unit (+ keyframe flag).
fn parse_sample(sample: CMSampleBufferRef) -> Result<EncodedUnit, String> {
    unsafe {
        // Keyframe: the attachments say "NotSync" for delta frames; a missing
        // attachments array means every sample is sync.
        let key = {
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
            if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
                true
            } else {
                let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
                let not_sync =
                    CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync.cast::<c_void>());
                not_sync.is_null()
                    || not_sync == CFBoolean::false_value().as_CFTypeRef().cast::<c_void>()
            }
        };

        // Parameter sets (SPS/PPS) + the AVCC length-prefix width.
        let desc = CMSampleBufferGetFormatDescription(sample);
        if desc.is_null() {
            return Err("sample without format description".into());
        }
        let mut nal_len: i32 = 4;
        let mut count: usize = 0;
        let st = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            desc,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            &mut nal_len,
        );
        if st != 0 {
            return Err(format!("H264 parameter sets unavailable: OSStatus {st}"));
        }
        let mut params: Vec<Vec<u8>> = Vec::with_capacity(count);
        for i in 0..count {
            let mut ptr: *const u8 = std::ptr::null();
            let mut size: usize = 0;
            let st = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                desc,
                i,
                &mut ptr,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if st != 0 || ptr.is_null() {
                return Err(format!("H264 parameter set {i}: OSStatus {st}"));
            }
            params.push(std::slice::from_raw_parts(ptr, size).to_vec());
        }

        // The sample's NAL units, AVCC length-prefixed.
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return Err("sample without data buffer".into());
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        let st = CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast());
        if st != 0 {
            return Err(format!("CMBlockBufferCopyDataBytes: OSStatus {st}"));
        }

        let annexb = avcc_to_annexb(&avcc, nal_len as usize, &params, key)?;
        Ok(EncodedUnit { annexb, key })
    }
}

/// Convert one AVCC access unit (NALs behind `nal_len`-byte big-endian length
/// prefixes) to the Annex-B byte stream, prepending the parameter sets on
/// keyframes so every IDR is a self-contained decode entry — the in-band
/// SPS/PPS discipline openh264 follows and the viewers rely on. Pure, so the
/// byte-walk is unit-testable.
fn avcc_to_annexb(
    avcc: &[u8],
    nal_len: usize,
    params: &[Vec<u8>],
    key: bool,
) -> Result<Vec<u8>, String> {
    if !(1..=4).contains(&nal_len) {
        return Err(format!("unsupported NAL length prefix: {nal_len}"));
    }
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(avcc.len() + 64);
    if key {
        for p in params {
            out.extend_from_slice(&START);
            out.extend_from_slice(p);
        }
    }
    let mut at = 0usize;
    while at + nal_len <= avcc.len() {
        let mut size = 0usize;
        for &b in &avcc[at..at + nal_len] {
            size = (size << 8) | usize::from(b);
        }
        at += nal_len;
        let Some(end) = at.checked_add(size).filter(|&e| e <= avcc.len()) else {
            return Err("truncated AVCC NAL unit".into());
        };
        out.extend_from_slice(&START);
        out.extend_from_slice(&avcc[at..end]);
        at = end;
    }
    if at != avcc.len() {
        return Err("trailing bytes after the last AVCC NAL unit".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_converts_to_annexb_with_params_on_keyframes() {
        // Two NALs behind 4-byte prefixes; SPS/PPS prepended only on keys.
        let avcc = [0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC];
        let params = vec![vec![0x67, 1], vec![0x68, 2]];
        let key = avcc_to_annexb(&avcc, 4, &params, true).unwrap();
        assert_eq!(
            key,
            [
                0, 0, 0, 1, 0x67, 1, // SPS
                0, 0, 0, 1, 0x68, 2, // PPS
                0, 0, 0, 1, 0xAA, 0xBB, // NAL 1
                0, 0, 0, 1, 0xCC, // NAL 2
            ]
        );
        let delta = avcc_to_annexb(&avcc, 4, &params, false).unwrap();
        assert_eq!(delta, [0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
        // 2-byte prefixes walk too.
        let two = avcc_to_annexb(&[0, 1, 0xDD], 2, &[], false).unwrap();
        assert_eq!(two, [0, 0, 0, 1, 0xDD]);
    }

    #[test]
    fn avcc_walk_rejects_malformed_units() {
        // A length running past the buffer must error, never panic or emit
        // garbage into the byte stream.
        assert!(avcc_to_annexb(&[0, 0, 0, 9, 0xAA], 4, &[], false).is_err());
        // Trailing bytes that aren't a whole prefix+unit are malformed too.
        assert!(avcc_to_annexb(&[0, 0, 0, 1, 0xAA, 0x00], 4, &[], false).is_err());
        assert!(avcc_to_annexb(&[], 5, &[], false).is_err());
    }
}
