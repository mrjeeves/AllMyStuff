# Focused detailed-log capture for video stalls

This branch now includes behavioral pipeline fixes as well as the original
instrumentation: frame-completion pacing (below), clean-entry cancellation
handling, phase-locked capture cadence, DXGI frame lifetime, and deferred
route tuning/reconnect handling. Game uses the Balanced algorithm with
25 Mbps/native-up-to-4K/60 fps defaults; the former Game algorithm and Studio
modes are experimental choices behind Dev Mode. Statements below about
unchanged policy refer to the timing hooks themselves, not the entire branch.

Use the existing detailed-logging setting on both ends. No packet capture,
new UI setting, timer task, or per-packet log is added. AMS enables the narrow
MyOwnMesh timing targets when launching its daemon, unless an explicit
`MYOWNMESH_LOG` override takes precedence. The daemon needs the companion
timing instrumentation; an older daemon simply has no matching log sites.

## Added evidence

- `video RTP receive timing`: one line per active track per five seconds.
  Reports maximum RTP read-await and assembly/handoff duration, forward
  sequence skips, reordered/duplicate arrivals, and largest assembler release.
- Existing RTP abandonment lines add `engine_event_age_ms`: time between
  the assembler detecting a discontinuity and the engine consuming its event.
- `media IPC writer timing`: one line per active daemon-to-app pipe per five
  seconds, with queue residence and socket-write maxima plus body/byte counts.
- Existing IPC overflow lines add queued pictures, other messages (including
  audio), samples and bytes. This is occupancy at logging time, not an atomic
  snapshot of the failed admission; the writer may drain concurrently.
- `media IPC reader timing`: one line per active AMS reader per five seconds,
  separating header wait, body read, framing/forwarding, and scheduler yield.
- `media sender transport wait`: only sends taking at least 100 ms, at most
  once per five seconds per sender pipe, including whether the send failed.

For one viewer this is three periodic lines per five seconds, plus bounded
slow-send warnings and extra fields on existing recovery warnings. No payloads
are recorded; these timing hooks do not change queue, pacing, codec, recovery
or quality policy.

### Whole-frame timing

The AMS diagnostic branch also records `video AU send timing` and
`video AU assembly timing`. Both select the same existing AU sequence modulo
60 (about one line/second per endpoint at 60fps), plus a local slow-frame
exception of at least 50ms, rate-limited to one per five seconds per route
on send/legacy receive, or per reader on binary IPC receive. These DEBUG
targets are enabled by detailed logging. No per-packet
logs, new wire metadata, timers, or payload copies are introduced.

- Sender: AU sequence, bytes/chunks, selected drain rate and paced/unpaced
  mode; total duration from entering send through the closing-marker write,
  requested pacing time, actual pacing time, daemon write time and residual
  work. This does **not** include capture, encode or outbound queue residence.
- Receiver: matching AU sequence, RTP timestamp, bytes/chunks, first-fragment
  to validated closing-marker duration and maximum gap between fragments,
  including the end marker. This does **not** include time before the first
  fragment, and only completed valid AUs emit this line. Existing loss logs
  continue to explain damaged/discarded pictures.

The receiver's normal binary IPC path assembles before the mesh's four-AU
queue, and logs peer/lane there. The legacy path logs its route at the mesh
assembler. Both are covered, without assembling a frame twice.

Match route or peer/lane/session, sequence, byte count and fragment count across endpoints;
sequence counters can restart with a new route. Compare local durations, not
wall-clock subtraction. A long total with short fragment gaps demonstrates
slow completion despite continuous traffic. A matching sender duration dominated
by requested pacing isolates intentional shaping; long daemon writes or a
longer receiver interval point to different boundaries. Slow exceptions are
independently selected, so not every exception has a matching opposite-side
line. Deterministic samples do. Missing matches are not evidence of packet loss.

## Interpretation

Compare the sender's existing encode/pacing logs and the viewer's existing
input/decode/paint metrics with these boundaries around the same incident.

1. Slow sender transport await identifies delay after AMS submits media.
2. Large RTP read wait with normal sender work puts the missing evidence
   inside delivery/read scheduling; it alone does not prove network loss.
3. Large RTP handoff time or recovery-event age identifies local processing
   or engine-consumer delay before IPC, not an IPC overflow root cause.
4. Large IPC residence/write time identifies a stalled daemon-to-app handoff.
   Reader handling/yield spans help distinguish application consumption from
   time awaiting socket bytes. Queue snapshots test audio versus picture/byte
   pressure rather than assuming every overflow is a video-fragment burst.
5. If those stages remain timely but paint stalls, investigate the frontend.

Durations use local monotonic clocks; do not subtract clocks between hosts.
Read waits can be benign when a source is idle. Forward skips can be repaired,
so they are not permanent-loss counts. Maxima need not describe the same
packet, and five-second windows are not synchronized across stages. Summaries
are emitted on progress, so a blocked await is reported after it returns.
If the earliest delay remains inside an awaited operation, a narrowly scoped
OS/runtime trace may still be required; these summaries do not prove its cause.

Record the incident time and retain logs from both ends. Do not tune limits
based only on an abandonment/overflow count or average throughput.
# LAN frame-completion pacing (diagnostic branch)

The matched CECWorkstation2 → Stream PC capture on 2026-09-08 showed a
546,196-byte AU spending 224 ms in AMS send, including 222 ms of pacing sleep;
the receiver assembled the same sequence/chunk count in 225 ms. Two further
large AUs repeated this at roughly 215–221 ms. This is sender-imposed delay,
not a measurement of NIC saturation or cross-host one-way latency.

LAN pacing now uses the encoded AU size and requested FPS to provide a
one-frame-interval drain target, bounded by a **shared 256 Mbps ceiling across
paced LAN routes in the node**. The existing 96 KiB token allowance persists
across AUs; each frame does not get a fresh burst. The shared reservation is
made after the per-route wait, immediately before submitting the fragment.
This ceiling is a safety bound, not an estimate of available network capacity.
Oversized AUs or competing traffic may still miss the frame interval.

WAN/unknown paths retain the rate-relative policy. Explicit diagnostic drain
overrides remain route limits (LAN still has the shared ceiling). Existing
unsplittable-AU bypass behavior is unchanged. No decode buffering, codec
reference shedding, bitrate/resolution reduction, or wire change is involved.
The AU send/assembly timing logs remain enabled through detailed logging for
before/after comparison. This policy alone does not guarantee 60 unique source
frames per second or remove capture, encoding, networking and display latency.
