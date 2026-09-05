//! Compressed video is a reference chain, not a latest-picture mailbox.
//! Local residence time measures a stalled handoff; AU count does not, because
//! RTP repair can deliver several frames between two healthy GUI polls.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

// Preserve the former six-AU queue's 30 fps residence budget, but measure it
// in local monotonic time. This is a discard deadline, never a playout delay.
const MAX_RESIDENCE: Duration = Duration::from_millis(200);
// Same scale as the media protocol's defensive frame cap. Includes metadata;
// not a bitrate/quality target. Batches also remain below node IPC's 256 MiB cap.
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enqueue {
    Enqueued { skipped: usize },
    AwaitingKey { started: bool },
    Converging { skipped: usize, started: bool },
}

struct Packet {
    data: Vec<u8>,
    at: Instant,
}
impl Packet {
    fn charge(&self) -> usize {
        self.data.len() + 4 + 2 * std::mem::size_of::<Self>()
    }
    fn key(&self) -> bool {
        self.data.first() == Some(&2) && self.data.get(1) == Some(&1)
    }
}

pub(crate) struct VideoHandoff {
    packets: VecDeque<Packet>,
    bytes: usize,
    awaiting_key: bool,
    convergence_requested: bool,
    max_age: Duration,
    max_bytes: usize,
}

impl Default for VideoHandoff {
    fn default() -> Self {
        Self {
            packets: VecDeque::new(),
            bytes: 0,
            awaiting_key: false,
            convergence_requested: false,
            max_age: MAX_RESIDENCE,
            max_bytes: MAX_BYTES,
        }
    }
}

impl VideoHandoff {
    pub fn len(&self) -> usize {
        self.packets.len()
    }
    fn clear(&mut self) {
        self.packets.clear();
        self.bytes = 0;
    }
    fn push(&mut self, packet: Packet) {
        self.bytes += packet.charge();
        self.packets.push_back(packet);
    }

    /// Self-contained JPEG/RGBA pictures may be superseded before paint.
    pub fn replace(&mut self, data: Vec<u8>) {
        self.clear();
        self.awaiting_key = false;
        self.convergence_requested = false;
        self.push(Packet {
            data,
            at: Instant::now(),
        });
    }

    pub fn push_h264(&mut self, data: Vec<u8>, now: Instant, gradual: bool) -> Enqueue {
        let packet = Packet { data, at: now };
        let key = packet.key();
        if self.awaiting_key && !gradual && !key {
            return Enqueue::AwaitingKey { started: false };
        }
        let too_large = packet.charge() > self.max_bytes;
        let expired = self
            .packets
            .front()
            .is_some_and(|p| now.duration_since(p.at) >= self.max_age);
        let full = self.bytes.saturating_add(packet.charge()) > self.max_bytes;
        if self.awaiting_key || expired || full {
            let mut skipped = 0;
            // Prefer the arriving key, then the newest queued key whose entire
            // suffix fits and is still fresh. Never trim arbitrary deltas.
            if !key && !too_large && !self.awaiting_key {
                if let Some(index) = self.packets.iter().rposition(Packet::key) {
                    let suffix_bytes: usize =
                        self.packets.iter().skip(index).map(Packet::charge).sum();
                    if now.duration_since(self.packets[index].at) < self.max_age
                        && suffix_bytes + packet.charge() <= self.max_bytes
                    {
                        for old in self.packets.drain(..index) {
                            skipped += 1;
                            self.bytes -= old.charge();
                        }
                        self.push(packet);
                        return Enqueue::Enqueued { skipped };
                    }
                }
            }
            skipped += self.packets.len();
            self.clear();
            if key && !too_large {
                self.awaiting_key = false;
                self.convergence_requested = false;
                self.push(packet);
                return Enqueue::Enqueued { skipped };
            }
            if gradual && !too_large {
                let started = !self.convergence_requested;
                self.convergence_requested = true;
                self.awaiting_key = false;
                self.push(packet);
                return Enqueue::Converging { skipped, started };
            }
            let started = !self.awaiting_key;
            self.awaiting_key = true;
            return Enqueue::AwaitingKey { started };
        }
        self.push(packet);
        Enqueue::Enqueued { skipped: 0 }
    }

    pub fn take_batch(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.packets.iter().map(|p| p.data.len() + 4).sum());
        for packet in self.packets.drain(..) {
            out.extend_from_slice(&(packet.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&packet.data);
        }
        self.bytes = 0;
        self.convergence_requested = false;
        // Draining must NOT release an actual missing-reference fence.
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn au(key: bool, n: u8) -> Vec<u8> {
        vec![2, u8::from(key), n]
    }

    #[test]
    fn repaired_burst_between_healthy_polls_preserves_every_reference() {
        let mut q = VideoHandoff::default();
        let start = Instant::now();
        let mut expected = Vec::new();
        for n in 0..12 {
            let data = au(n == 0, n);
            expected.extend_from_slice(&(data.len() as u32).to_le_bytes());
            expected.extend_from_slice(&data);
            assert_eq!(
                q.push_h264(data, start + Duration::from_millis(n as u64), false),
                Enqueue::Enqueued { skipped: 0 }
            );
        }
        assert_eq!(q.take_batch(), expected);
        assert_eq!(q.bytes, 0);
    }

    #[test]
    fn real_unread_stall_fences_deltas_and_recovers_on_key() {
        let mut q = VideoHandoff::default();
        let start = Instant::now();
        q.push_h264(au(true, 1), start, false);
        let late = start + MAX_RESIDENCE;
        assert_eq!(
            q.push_h264(au(false, 2), late, false),
            Enqueue::AwaitingKey { started: true }
        );
        assert!(q.take_batch().is_empty());
        assert_eq!(
            q.push_h264(au(false, 3), late, false),
            Enqueue::AwaitingKey { started: false }
        );
        assert_eq!(
            q.push_h264(au(true, 4), late, false),
            Enqueue::Enqueued { skipped: 0 }
        );
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn stale_prefix_trims_only_to_a_fresh_complete_key_suffix() {
        let mut q = VideoHandoff::default();
        let start = Instant::now();
        q.push_h264(au(false, 1), start, false);
        q.push_h264(au(true, 2), start + Duration::from_millis(150), false);
        q.push_h264(au(false, 3), start + Duration::from_millis(180), false);
        assert_eq!(
            q.push_h264(au(false, 4), start + MAX_RESIDENCE, false),
            Enqueue::Enqueued { skipped: 1 }
        );
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn memory_bound_checks_the_whole_suffix_and_incoming_key() {
        let start = Instant::now();
        let mut q = VideoHandoff {
            max_bytes: Packet {
                data: au(true, 1),
                at: start,
            }
            .charge()
                * 2,
            ..VideoHandoff::default()
        };
        q.push_h264(au(true, 1), start, false);
        q.push_h264(au(false, 2), start, false);
        assert_eq!(
            q.push_h264(au(false, 3), start, false),
            Enqueue::AwaitingKey { started: true }
        );
        assert_eq!(q.bytes, 0);
        let mut oversized_key = vec![0; q.max_bytes + 1];
        oversized_key[..2].copy_from_slice(&[2, 1]);
        assert_eq!(
            q.push_h264(oversized_key, start, false),
            Enqueue::AwaitingKey { started: false }
        );
        q.push_h264(au(true, 4), start, false);
        assert!(q.bytes <= q.max_bytes);
    }

    #[test]
    fn gradual_stall_requests_one_wave_until_consumer_drains() {
        let mut q = VideoHandoff::default();
        let start = Instant::now();
        q.push_h264(au(false, 1), start, true);
        assert_eq!(
            q.push_h264(au(false, 2), start + MAX_RESIDENCE, true),
            Enqueue::Converging {
                skipped: 1,
                started: true
            }
        );
        assert_eq!(
            q.push_h264(au(false, 3), start + MAX_RESIDENCE * 2, true),
            Enqueue::Converging {
                skipped: 1,
                started: false
            }
        );
        q.take_batch();
        q.push_h264(au(false, 4), start + MAX_RESIDENCE * 3, true);
        assert_eq!(
            q.push_h264(au(false, 5), start + MAX_RESIDENCE * 4, true),
            Enqueue::Converging {
                skipped: 1,
                started: true
            }
        );
    }
}
