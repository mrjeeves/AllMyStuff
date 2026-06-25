//! The client side of each media plane, as pure logic over the
//! [`allmystuff_session`] frame types.
//!
//! A phone is a *consumer and controller*, so each module here is one half of
//! a plane:
//!
//! * [`video`] — **decode** path: reassemble chunked MJPEG off the media
//!   channel ([`VideoSink`]) and define the [`VideoDecoder`] seam the platform
//!   H.264 decoder (VideoToolbox / MediaCodec) fills in for the RTP lane.
//! * [`input`] — **encode** path: turn touches, drags, wheels and key presses
//!   into normalized [`InputEvent`](allmystuff_session::InputEvent)s.
//! * [`term`] — **both** directions of a terminal: keystrokes + resizes up,
//!   PTY bytes down, for an xterm.js view.
//! * [`files`] — a request/reply [`FileClient`] that mints request ids,
//!   builds [`FileEvent`](allmystuff_session::FileEvent)s, and reassembles the
//!   `Chunk` stream a `Read` answers with.
//!
//! None of this touches a codec or a socket: the platform layer owns the
//! H.264/Opus/JPEG decoders and the radio. This is the part that has to be
//! right on the byte, so it lives where `cargo test` can prove it.

pub mod files;
pub mod input;
pub mod term;
pub mod video;

pub use files::{FileClient, FileReply};
pub use input::InputEncoder;
pub use term::TermPlane;
pub use video::{
    CompressedFrame, H264Au, JpegFrame, RgbaFrame, VideoDecoder, VideoSink, VideoUpdate,
};
