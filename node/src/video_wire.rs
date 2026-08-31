//! Encoded-video metadata shared by capture-capable and receive-only builds.
//!
//! This is transport syntax, not capture or decode policy. Keeping it outside
//! the host-gated `video` module means a phone/viewer build enforces the exact
//! same access-unit identity contract as a desktop sender.

/// How this access unit's active encoder repairs a broken reference chain.
/// This is emitted from what actually opened, not from the requested posture:
/// Game falling back to Media Foundation therefore honestly says `Reset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuRecovery {
    /// The decoder must wait for an independently decodable entry (IDR).
    Reset,
    /// The encoder can converge through a gradual intra-refresh wave while
    /// dependent pictures continue to flow.
    Gradual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuIdentity {
    pub sequence: u64,
    pub recovery: AuRecovery,
}

/// In-band identity for one complete encoded access unit. The marker is a
/// valid H.264/HEVC user-data-unregistered prefix SEI, so an older receiver
/// safely hands it to its decoder while a newer receiver can prove that an
/// entire AU went missing and select the active encoder's recovery contract.
/// The mode byte and sequence are ASCII to keep the RBSP free of start-code
/// emulation without another escaping layer.
const AU_IDENTITY_MARKER_UUID: &[u8; 16] = b"AMS-AU-SEQ-V2!!!";
const H264_AU_IDENTITY_MARKER_LEN: usize = 41;
const HEVC_AU_IDENTITY_MARKER_LEN: usize = 42;

pub(crate) fn annexb_nals(data: &[u8]) -> Vec<(usize, u8)> {
    let mut nals = Vec::new();
    for p in memchr::memchr_iter(1, data) {
        if p < 2 || data[p - 1] != 0 || data[p - 2] != 0 {
            continue;
        }
        let at = if p >= 3 && data[p - 3] == 0 {
            p - 3
        } else {
            p - 2
        };
        if let Some(&header) = data.get(p + 1) {
            nals.push((at, header));
        }
    }
    nals
}

pub(crate) fn insert_au_identity_marker(data: &mut Vec<u8>, identity: AuIdentity, hevc: bool) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut marker = Vec::with_capacity(if hevc {
        HEVC_AU_IDENTITY_MARKER_LEN
    } else {
        H264_AU_IDENTITY_MARKER_LEN
    });
    if hevc {
        marker.extend_from_slice(&[0, 0, 0, 1, 0x4e, 0x01]); // HEVC prefix SEI
    } else {
        marker.extend_from_slice(&[0, 0, 0, 1, 0x06]); // H.264 SEI
    }
    marker.extend_from_slice(&[0x05, 33]); // user_data_unregistered, UUID + mode + 16 hex digits
    marker.extend_from_slice(AU_IDENTITY_MARKER_UUID);
    marker.push(match identity.recovery {
        AuRecovery::Reset => b'R',
        AuRecovery::Gradual => b'G',
    });
    for shift in (0..16).rev() {
        marker.push(HEX[((identity.sequence >> (shift * 4)) & 0x0f) as usize]);
    }
    marker.push(0x80); // rbsp_trailing_bits

    // Prefix SEI belongs before the first VCL NAL. Parameter sets and AUD stay
    // first; the marker lands immediately before the picture it identifies.
    // Trailing prefix SEI is not a valid AU shape for every decoder.
    let insert_at = annexb_nals(data)
        .into_iter()
        .find(|&(_, header)| {
            if hevc {
                ((header >> 1) & 0x3f) <= 31
            } else {
                matches!(header & 0x1f, 1..=5)
            }
        })
        .map(|(at, _)| at)
        .unwrap_or(data.len());
    data.splice(insert_at..insert_at, marker);
}

fn find_au_identity_marker(data: &[u8]) -> Option<(usize, usize, AuIdentity)> {
    for (at, header) in annexb_nals(data) {
        let (payload_at, end) = if header == 0x06
            && data.get(at..at + 7) == Some(&[0, 0, 0, 1, 0x06, 0x05, 33])
            && at + H264_AU_IDENTITY_MARKER_LEN <= data.len()
        {
            (at + 7, at + H264_AU_IDENTITY_MARKER_LEN)
        } else if header == 0x4e
            && data.get(at..at + 8) == Some(&[0, 0, 0, 1, 0x4e, 0x01, 0x05, 33])
            && at + HEVC_AU_IDENTITY_MARKER_LEN <= data.len()
        {
            (at + 8, at + HEVC_AU_IDENTITY_MARKER_LEN)
        } else {
            continue;
        };
        let marker = &data[payload_at..end];
        if &marker[..16] != AU_IDENTITY_MARKER_UUID || marker[33] != 0x80 {
            continue;
        }
        let recovery = match marker[16] {
            b'R' => AuRecovery::Reset,
            b'G' => AuRecovery::Gradual,
            _ => continue,
        };
        let mut sequence = 0u64;
        let mut valid = true;
        for &digit in &marker[17..33] {
            let value = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                _ => {
                    valid = false;
                    break;
                }
            };
            sequence = (sequence << 4) | u64::from(value);
        }
        if !valid {
            continue;
        }
        return Some((at, end, AuIdentity { sequence, recovery }));
    }
    None
}

/// Read our exact AU identity without changing decoder input. The local IPC
/// freshness boundary uses this to apply the correct recovery behavior before
/// the mesh consumer has removed the marker.
pub(crate) fn peek_au_identity_marker(data: &[u8]) -> Option<AuIdentity> {
    find_au_identity_marker(data).map(|(_, _, identity)| identity)
}

/// Remove and return our exact AU identity marker. Ordinary encoder SEI NALs
/// are untouched, and malformed/truncated markers remain decoder input rather
/// than being mistaken for transport metadata.
pub(crate) fn take_au_identity_marker(data: &mut Vec<u8>) -> Option<AuIdentity> {
    let (at, end, identity) = find_au_identity_marker(data)?;
    data.drain(at..end);
    Some(identity)
}
