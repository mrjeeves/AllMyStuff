# Reference-safe delivery after 0.2.112

This candidate follows the field evidence in `video-pipeline-evidence.md`.
The 0.2.112 capture showed a six-AU GUI overflow only 12 ms after a poll,
plus daemon-reported discontinuities and accepted-frame gaps over one second.
Sender encode throughput continued near 30 fps. Those facts do not establish
raw network saturation or prove which upstream failure caused every episode.

## Pipeline contract

| Boundary | Change | What remains bounded |
| --- | --- | --- |
| MyOwnMesh engine → subscribers | Cooperative handoff after each video event, not only in the upstream RTP reader | Existing 16-sample broadcast queue; real lag still signals loss |
| MyOwnMesh subscribers → client socket | Give ready writers a scheduling turn between samples/fragments | Existing eight-sample socket queue; no indefinite send wait |
| AMS node → GUI decoder | Keep complete reference chains across bunched arrivals | Actual local unread age and memory, not a six-AU count |
| WebCodecs submission → output | Detect elapsed lack of decode progress, not 20 calls in one JS turn or a paint slowdown | Existing hardware → software → native recovery ladder |
| Decoded output → paint | Unchanged: retain the newest decoded picture | One pending paint frame |

The two MyOwnMesh changes live in the paired MyOwnMesh PR. They do not change
RTP, retransmission deadlines, packet pacing, or codec framing. Cooperative
scheduling is not a hard guarantee under arbitrary overload: real pressure and
real packet loss still require the existing ordered discontinuity recovery.

## AMS queue policy

A compressed AU cannot safely be discarded just because several valid AUs
arrive in the same polling interval. The GUI queue now accepts that chain and
drains immediately on the existing arrival-triggered poll. It adds no playout
delay and does not preserve a replay queue in a stuck window.

The unread-residence guard is **200 ms of local monotonic time**, based on the
old six-AU queue's 30 fps window. Previously its nominal window varied from
100 ms at 60 fps to 200 ms at 30 fps. This explicitly changes that safety
envelope to a fixed time budget; it is not an end-to-end latency target.
The memory ceiling is 64 MiB including per-packet accounting, matching the scale
of the existing media-frame defensive cap and staying below node IPC's cap.

When actual pressure occurs, retain a fresh, fitting keyframe suffix if one
exists. Otherwise reset-mode streams quarantine dependent deltas until a clean
entry. Gradual-refresh streams retain the live delta and request one convergence
wave. Both GDR recovery paths now issue the arrival wakeup for the retained AU.
The cap applies to the complete retained suffix, including the arriving AU;
an oversized keyframe cannot bypass it. Self-contained JPEG/RGBA keeps latest-wins.

## Decoder progress

Submitting 20 AUs without yielding JavaScript is not proof that a decoder has
failed. The existing one-second health sweep now requires outstanding decode
work with at least one second without output progress. Idle periods do not age
new work; hidden windows do not trigger fallback, and visibility resume grants
a fresh progress window. Paint cadence is no longer used to accuse the decoder.
A genuine decoder rebuild explicitly requests a clean entry rather than waiting
for the periodic keyframe. Configure/decode error handling remains in place.

## Durable diagnostics

GUI stage reports and decoder decisions travel over local node IPC into the
existing node file logger. Child MyOwnMesh diagnostics are mirrored into that
logger when detailed logging is enabled. Records are bounded and escaped.
Detailed logging also enables the narrow MyOwnMesh IPC-bridge debug target;
explicit operator log filters still win. No packet-level trace is enabled.

The paired daemon change distinguishes RTP discontinuity, engine sequence gap,
and socket-queue overflow in its diagnostics. This closes the previous stdout-
only blind spot on installed Windows GUI launches; it does not reconstruct
daemon/frontend details that were never saved by 0.2.112.

## Focused verification and release gate

- Pure queue tests: 12-AU/12-ms burst, genuine unread expiry, safe key suffix,
  whole-suffix memory bound, oversized key, and single-flight GDR recovery.
- Decoder tests: 32 submissions before callbacks, genuine missing output,
  new work after idle and visibility resume; existing poll/stage tests retained.
- Logging regression: frontend and daemon lines reach a tracing writer, with
  record separators escaped and Unicode-safe truncation.
- Paired MyOwnMesh regressions reproduce pre-fix losses and assert post-fix
  delivery: 16/32 → 32/32 at broadcast, 8/32 → 32/32 at socket handoff.
  Stalled-consumer and existing gap-ordering coverage remain.

Full GUI build/typecheck and platform checks are left to CI. Three broader
MyOwnMesh bridge fixtures failed to establish PeerApproved locally; this is
not represented as a passing network integration test.

The combined fix pins MyOwnMesh v0.3.16, including the daemon handoff changes
from MyOwnMesh PR #129. The desktop sidecar pin and mobile Cargo dependencies
must stay aligned. Wait for its required release assets before cutting AMS.

Field acceptance still needs the same high-motion sequence, foreground state,
source display, and negotiated quality on both versions. Compare presentation
gaps, recovery frequency, latency, decode failures and traffic bursts. Game and
Balanced resolution/FPS targets and bitrate/pacing settings are unchanged.
These regressions prove the corrected mechanisms, not that all field stutter
has been eliminated.
