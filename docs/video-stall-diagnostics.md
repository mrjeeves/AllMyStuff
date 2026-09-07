# Focused detailed-log capture for video stalls

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
are recorded and no queue, pacing, codec, recovery or quality policy changes.

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
