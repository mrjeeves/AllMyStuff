//! The video decode path on a phone.
//!
//! Two transports can carry a screen/camera stream, and the phone consumes
//! both:
//!
//! * **MJPEG over the media channel** — standalone baseline JPEGs, possibly
//!   chunked across several ~64 KiB messages sharing a `seq`. [`VideoSink`]
//!   demuxes these off the channel and reassembles whole frames with
//!   [`VideoAssembler`], handing the platform a ready-to-decode [`JpegFrame`].
//!
//! * **H.264 over an RTP track lane** — Annex-B access units delivered by the
//!   mesh's `video_inbound` lane. The phone feeds these straight to a hardware
//!   decoder (VideoToolbox on iOS, MediaCodec on Android) as an [`H264Au`].
//!
//! Both converge on one seam — [`VideoDecoder`] — which turns a
//! [`CompressedFrame`] into an [`RgbaFrame`] the UI paints. This crate defines
//! the seam and the MJPEG reassembly (pure, testable); the actual JPEG/H.264
//! decoding is platform code that implements [`VideoDecoder`]. We decode
//! *on the phone* from the compressed bytes that already crossed the mesh —
//! never pull raw RGBA over the network.

use allmystuff_session::{MediaPayload, VideoAssembler, VideoFrame, VideoStatusFrame};

/// A frame of pixels ready to paint: tightly-packed RGBA8888, row-major,
/// `width * height * 4` bytes. `source_*` is the capture's full resolution,
/// which normalized input coordinates map back onto (see
/// [`crate::media::InputEncoder`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaFrame {
    pub route: String,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub rgba: Vec<u8>,
}

/// One complete JPEG, reassembled off the media channel — ready for the
/// platform's JPEG decoder. Distilled from a [`VideoFrame`] once all its
/// chunks have arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpegFrame {
    pub route: String,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub jpeg: Vec<u8>,
}

impl From<VideoFrame> for JpegFrame {
    fn from(f: VideoFrame) -> Self {
        JpegFrame {
            route: f.route,
            seq: f.seq,
            width: f.width,
            height: f.height,
            source_width: f.source_width,
            source_height: f.source_height,
            jpeg: f.jpeg,
        }
    }
}

/// One H.264 access unit off the RTP lane, ready for a hardware decoder.
/// `key` marks an IDR (a clean decode entry point); `pts_micros` is the
/// presentation timestamp in microseconds (the lane's `rtp_timestamp`
/// rescaled from the 90 kHz clock: `rtp_ts * 1000 / 90`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Au {
    pub route: String,
    /// Annex-B byte stream (start-code-delimited NAL units).
    pub data: Vec<u8>,
    pub key: bool,
    pub pts_micros: i64,
}

/// What a [`VideoDecoder`] is handed — whichever transport the stream took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressedFrame {
    /// A reassembled MJPEG frame from the media channel.
    Jpeg(JpegFrame),
    /// An H.264 access unit from the RTP lane.
    H264(H264Au),
}

impl CompressedFrame {
    pub fn route(&self) -> &str {
        match self {
            CompressedFrame::Jpeg(f) => &f.route,
            CompressedFrame::H264(f) => &f.route,
        }
    }
}

/// The platform decode seam. iOS implements this over VideoToolbox, Android
/// over MediaCodec; tests can implement a trivial one. A decoder is per-route
/// (it holds the H.264 reference state), so the app keeps one per live stream.
pub trait VideoDecoder {
    /// Decode one compressed frame to RGBA, or `None` if this frame only
    /// advanced decoder state and produced no displayable picture yet (e.g. a
    /// non-key H.264 AU before the first IDR).
    fn decode(&mut self, frame: &CompressedFrame) -> Option<RgbaFrame>;

    /// The decoder lost its place (a decode error, a rebuilt pipeline). The
    /// app should follow this with a [`RouteControl::Refresh`] to the streamer
    /// so the next access unit is an IDR rather than waiting out the periodic
    /// interval.
    ///
    /// [`RouteControl::Refresh`]: allmystuff_protocol::RouteControl::Refresh
    fn refresh(&mut self) {}
}

/// What demuxing one media-channel payload through a [`VideoSink`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoUpdate {
    /// A complete MJPEG frame is ready to decode + paint.
    Frame(JpegFrame),
    /// The host reported why the stream is (not) producing pixels — render
    /// this instead of a silent black screen.
    Status(VideoStatusFrame),
}

/// Demuxes a display/camera route's frames off the shared media channel and
/// reassembles chunked MJPEG. One sink per phone is enough — it keys partial
/// frames by route internally (via [`VideoAssembler`]).
#[derive(Debug, Default)]
pub struct VideoSink {
    assembler: VideoAssembler,
}

impl VideoSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one decoded media-channel payload. Returns a [`VideoUpdate`] when
    /// a whole frame (or a status change) is ready; `None` while a chunked
    /// frame is still arriving, or for a payload that isn't this plane's.
    pub fn accept(&mut self, payload: MediaPayload) -> Option<VideoUpdate> {
        match payload {
            MediaPayload::Video(frame) => self
                .assembler
                .push(frame)
                .map(|full| VideoUpdate::Frame(full.into())),
            MediaPayload::VideoStatus(status) => Some(VideoUpdate::Status(status)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// Build a `VideoFrame` the way the wire does — through JSON, with the
    /// jpeg bytes base64'd — since the `MediaTagVideo` tag type is private to
    /// allmystuff-session.
    fn chunk(route: &str, seq: u64, idx: u16, of: u16, jpeg: Vec<u8>) -> VideoFrame {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
        serde_json::from_value(serde_json::json!({
            "t": "video",
            "route": route,
            "seq": seq,
            "width": 1920,
            "height": 1080,
            "source_width": 3840,
            "source_height": 2160,
            "chunk": idx,
            "chunks": of,
            "jpeg": b64,
        }))
        .expect("valid VideoFrame json")
    }

    #[test]
    fn whole_frame_passes_straight_through() {
        let mut sink = VideoSink::new();
        let out = sink.accept(MediaPayload::Video(chunk(
            "r",
            1,
            0,
            1,
            vec![0xFF, 0xD8, 0xFF],
        )));
        match out {
            Some(VideoUpdate::Frame(f)) => {
                assert_eq!(f.seq, 1);
                assert_eq!(f.jpeg, vec![0xFF, 0xD8, 0xFF]);
                assert_eq!((f.source_width, f.source_height), (3840, 2160));
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn chunked_frame_only_emits_once_every_piece_arrives() {
        let mut sink = VideoSink::new();
        // First of two chunks → nothing yet.
        assert!(sink
            .accept(MediaPayload::Video(chunk("r", 5, 0, 2, vec![1, 2, 3])))
            .is_none());
        // Second chunk completes it → the reassembled, concatenated frame.
        let out = sink.accept(MediaPayload::Video(chunk("r", 5, 1, 2, vec![4, 5])));
        match out {
            Some(VideoUpdate::Frame(f)) => assert_eq!(f.jpeg, vec![1, 2, 3, 4, 5]),
            other => panic!("expected the reassembled frame, got {other:?}"),
        }
    }

    #[test]
    fn non_video_payloads_are_ignored_by_the_video_sink() {
        let mut sink = VideoSink::new();
        let audio = MediaPayload::Audio(allmystuff_session::AudioFrame::new(
            "r",
            0,
            48000,
            1,
            vec![0],
        ));
        assert!(sink.accept(audio).is_none());
    }

    // A stand-in decoder: "decodes" a JPEG by echoing its bytes as fake RGBA,
    // and only produces a picture from an H.264 key frame — enough to exercise
    // the seam.
    struct FakeDecoder;
    impl VideoDecoder for FakeDecoder {
        fn decode(&mut self, frame: &CompressedFrame) -> Option<RgbaFrame> {
            match frame {
                CompressedFrame::Jpeg(f) => Some(RgbaFrame {
                    route: f.route.clone(),
                    width: f.width,
                    height: f.height,
                    source_width: f.source_width,
                    source_height: f.source_height,
                    rgba: f.jpeg.clone(),
                }),
                CompressedFrame::H264(au) if au.key => Some(RgbaFrame {
                    route: au.route.clone(),
                    width: 0,
                    height: 0,
                    source_width: 0,
                    source_height: 0,
                    rgba: au.data.clone(),
                }),
                CompressedFrame::H264(_) => None,
            }
        }
    }

    #[test]
    fn decoder_seam_handles_both_transports() {
        let mut dec = FakeDecoder;
        let jpeg = CompressedFrame::Jpeg(JpegFrame {
            route: "r".into(),
            seq: 0,
            width: 2,
            height: 1,
            source_width: 2,
            source_height: 1,
            jpeg: vec![9, 9],
        });
        assert_eq!(dec.decode(&jpeg).unwrap().rgba, vec![9, 9]);

        // A delta frame before any keyframe yields no picture...
        let delta = CompressedFrame::H264(H264Au {
            route: "r".into(),
            data: vec![1],
            key: false,
            pts_micros: 0,
        });
        assert!(dec.decode(&delta).is_none());
        // ...a keyframe does.
        let key = CompressedFrame::H264(H264Au {
            route: "r".into(),
            data: vec![2],
            key: true,
            pts_micros: 1_000,
        });
        assert_eq!(dec.decode(&key).unwrap().rgba, vec![2]);
        assert_eq!(key.route(), "r");
    }
}
