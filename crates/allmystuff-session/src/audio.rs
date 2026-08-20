//! The audio frame carried over an active audio route.
//!
//! A route's media plane is a stream of [`AudioFrame`]s sent on the mesh's
//! typed channel. PCM samples are packed little-endian and base64'd so the
//! frame fits the daemon's JSON channel payload without ballooning into a
//! multi-thousand-element number array.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// One buffer of interleaved 16-bit PCM, tagged with its route + a
/// monotonic sequence number so the sink can detect drops/reorders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFrame {
    /// The route this frame belongs to (so a node carrying several audio
    /// routes can demux them off one channel).
    pub route: String,
    pub seq: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved i16 samples, base64 of little-endian bytes on the wire.
    #[serde(with = "pcm_b64")]
    pub pcm: Vec<i16>,
}

impl AudioFrame {
    pub fn new(
        route: impl Into<String>,
        seq: u64,
        sample_rate: u32,
        channels: u16,
        pcm: Vec<i16>,
    ) -> Self {
        AudioFrame {
            route: route.into(),
            seq,
            sample_rate,
            channels,
            pcm,
        }
    }

    /// Frames per second worth of audio in this buffer (per channel).
    pub fn frame_count(&self) -> usize {
        self.pcm.len() / self.channels.max(1) as usize
    }
}

mod pcm_b64 {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(pcm: &[i16], s: S) -> Result<S::Ok, S::Error> {
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i16>, D::Error> {
        let text = String::deserialize(d)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)?;
        let (samples, _) = bytes.as_chunks::<2>();
        Ok(samples.iter().copied().map(i16::from_le_bytes).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_through_json() {
        let f = AudioFrame::new(
            "route:a→b",
            7,
            48_000,
            2,
            vec![0, 1, -1, 32767, -32768, 100],
        );
        let json = serde_json::to_string(&f).unwrap();
        // PCM must travel as a base64 string, not a number array.
        assert!(json.contains("\"pcm\":\""));
        let back: AudioFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
        assert_eq!(back.frame_count(), 3); // 6 samples / 2 channels
    }
}
