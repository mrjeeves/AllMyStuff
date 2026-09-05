# Video handoff: evidence and validation

## What was observed

CECWorkstation2 → Stream PC, AMS 0.2.111 / MyOwnMesh 0.3.15,
2026-09-05 around 00:28–00:30 UTC, detailed logging enabled:

- Sender: about 29.7 fps, H.264 1920×1080, encode p95 roughly 9–11 ms,
  zero reported encoder drops in the inspected windows.
- Viewer: repeated discontinuity/clean-entry recovery messages. Its delivered
  frame feedback fell as low as 0 fps even though sender output continued.
- Viewer also reported loss of the six-AU GUI queue's reference chain in other
  windows. A five-second average near 30 fps does not exclude short freezes.

Those logs establish downstream discontinuities, **not** the exact source of
every discontinuity. The old generic message conflated daemon-reported loss,
AMS fragment assembly failures, and AMS ingress queue overflow. They do not
justify reducing quality targets or changing the encoder/pacer constants.

## Reproduced defect and narrow fix

The daemon-to-AMS reader uses `try_send` into a four-complete-AU queue.
Already-buffered reads need not suspend. The reader could fill that queue and
discard subsequent reference-dependent AUs before a ready consumer ran once.

The regression feeds 12 complete AUs through the production reader on a
single-thread Tokio runtime with a ready consumer. Before the fix it delivered
**4 of 12**. After adding a cooperative yield when video/audio delivery work is
pending, it delivers **12 of 12 in order**. A second regression covers paced AUs
of eight fragments each plus interleaved audio. A stalled-video-consumer test
checks that the four-AU bound remains and audio still drains.

This is a scheduling fix, not a timed delay, a queue expansion, or a new rate
limiter. Yielding is not a hard scheduling guarantee under arbitrary overload;
the existing bounded recovery policy still applies to genuinely slow consumers.
The fix removes this reproduced local-loss mechanism. It is not evidence that
every field freeze has been fixed, nor a redesign of the encoder or decoder.

## Follow the next gap through the pipeline

No media payloads are logged. No per-frame diagnostic IPC is added.

1. Sender's existing capture/scale/encode/pacer lines locate source stalls.
2. AMS discontinuity messages now distinguish daemon-reported loss (transport
   **or daemon IPC**, still indistinguishable here), incomplete paced AUs, and
   AMS complete-AU ingress overflow.
3. `video in` includes the maximum gap between **accepted** AUs. It is not a
   raw network inter-arrival metric. It reports on the next accepted frame;
   complete silence cannot itself trigger this arrival-driven log.
4. GUI reference-chain overflow includes time since the last GUI poll.
5. With detailed logging enabled, the GUI log gets a `video stages` JSON line
   every five seconds: interval duration, input/decode/paint frame counts,
   maximum gaps at each stage, decoder queue depth and window visibility.
   Ongoing silence is included, and gap measurements cross report boundaries.
   These are local monotonic intervals, **not end-to-end latency**. A paused
   webview delays the reporting timer too; `elapsedMs` exposes that delay.

## Field acceptance still required

Compare baseline and candidate on the same source/viewer, quality settings,
foreground/window state, and repeatable high-motion camera sequence. Keep both
sides' logs and note the freeze times. Do not change multiple variables between
runs. Correlate the stages above rather than declaring success from average fps.

Quality targets remain Game up to 4K60 and Balanced up to 1440p30, subject to
the source display and both endpoints' settings/capabilities. Queue capacities,
bitrate/pacing limits, recovery timers and decoder selection are unchanged.
Acceptance requires fewer/shorter presentation gaps without increased latency,
traffic bursts, decode errors, or lowered negotiated quality. The new logs alone
cannot prove end-to-end latency or sub-poll-interval network burst performance;
those need separate measurements in the field comparison.

## Focused checks

```sh
cargo test --offline --manifest-path node/Cargo.toml --lib control_client::tests
node --test gui/src/video-stage-stats.test.mjs gui/src/video-poll-scheduler.test.mjs
```

No full capture/encode load test or remote deployment is required for these
regressions. They do not start AMS or touch a display/camera/audio device.
