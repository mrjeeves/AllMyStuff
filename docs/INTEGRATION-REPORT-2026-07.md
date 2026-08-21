# Integration report — the encoder fork line, for Chris's review

> **Historical record from July 2026:** this report describes a prototype
> handoff against AllMyStuff 0.2.46. It is preserved for engineering
> provenance, not as documentation of the current release. See the
> [documentation map](README.md) for current material.

_Prepared from `nathanfraske/AllMyStuffEncoderFraske`, branch
`codex/video-pipeline-upstream-pr`, for review against the current
`mrjeeves/AllMyStuff` `main`. **Everything here is a prototype until
maintainer signoff.** No version numbers are claimed: every manifest
remains at the upstream 0.2.46 version._

The proposed tree is intentionally limited to production code,
automated regression tests, CI, and these two maintainer documents.
Fork handoffs, field probes, ignored benchmarks/soaks, generated output,
and packaged fixtures are excluded from the PR diff.

---

## 1 · The contract this line was built under

Two rules were treated as inviolable, and section 4 is the audit:

1. **The signaling layer carries zero media and zero new anything.**
   Every byte this line moves rides the ICE-negotiated STUN/TURN
   datapath through the daemon's existing surfaces: media on the
   `ChannelSendTo`/track-lane sends that already existed, control
   (feedback, tune, input) on `CHANNEL_CONTROL` — the same channel
   those messages always used. No new channels, no new daemon requests,
   nothing on the signaling path.
2. **MyOwnMesh is frozen.** The daemon in every prototype bundle is
   your pinned build, byte-identical (`myownmesh.exe`, 2026-07-17
   pin). All pacing/shaping is app-side: what bytes we hand the
   daemon, when, in what sample framing. The pacer was verified
   against `H264AuAssembler`'s per-marker reassembly + contiguity
   anchor semantics and depends on them exactly as documented — it
   never modifies them.

## 2 · The exact pipeline, as it ships today

### Sender (host) — per display route

```
DXGI duplication (damage-driven, cursor save-under)          win_capture.rs
  └─ D3D11 VideoProcessor BGRA→NV12, same device, zero CPU   gpu_pipeline.rs
     px touches; NV12 texture ring (6 slots, depth-2
     retirement); ClockKeeper boost-clock heartbeat
       └─ Encoder ladder (per posture)                        video.rs run_gpu_lane
          NVENC SDK (runtime-loaded nvEncodeAPI64)            nvenc.rs
            · Balanced: P-preset VBR, adaptive IDR cadence
            · Game:     P2+ULL, GDR intra-refresh (no IDR
                        walls), single-frame VBV, loss-aware
                        wave heals (3-frame short heal in a
                        lossy spell)
            · Studio:   P5 HQ, deep VBV, uncapped-by-mode
            · Studio·Lossless: HEVC Main constQP-0 +
                        transquant bypass (bit-exact)
          → AMF (loader staged, Radeon pending)               amf.rs
          → MF hardware MFT (vendor-neutral fallback)         mediafoundation.rs
          → openh264 software floor                            video.rs
          → MJPEG compatibility floor (untouched from
            upstream behavior)
       └─ In-place rate reconfigure seam (no reset, no IDR)   nvenc.rs set_bitrate
          driven by the AIMD controller — GAME POSTURE ONLY
          by default (see §5), never lossless, never
          user-pinned bitrates
  └─ App-side slice pacer                                     mesh.rs send_video_paced
     splits AUs at slice-NAL boundaries (encoders emit
     count-mode/byte-capped slices), then shapes them through
     one persistent per-route token bucket:
       · 96 KiB immediate quality/recovery allowance
       · bounded rate-relative drain (8 Mbps WAN / 16 Mbps LAN
         floor, 32 Mbps recovery-headroom ceiling unless the
         route's own target is higher)
       · bucket state survives frame boundaries, so a backed-up
         producer cannot spend a new burst allowance per frame
     reserved gaps executed via a high-resolution waitable
     timer + bounded spin (os_perf.rs precise_sleep;
     measured 435 µs worst overshoot on the dev box)
  └─ Daemon track-lane send (existing API, one send per        control_client.rs
     chunk; data chunks duration_us=0, followed by a valid-SEI
     end marker carrying the expected fragment count)
═══ MyOwnMesh (FROZEN) — RTP packetization, ICE/STUN/TURN ═══
```

### Viewer — per inbound route

```
═══ MyOwnMesh (FROZEN) — delivers assembled samples ═══
handle_video_inbound                                          mesh.rs
  ├─ arrival timing: the pacer's chunk trains are timed
  │  packet trains → bandwidth estimate (dispersion EWMA)
  │  + one-way-delay trend (standing-queue early warning)
  ├─ paced-video v1 ingress (presence capability + activating
  │  RouteControl::Accept selection): validates timestamp,
  │  end marker, and fragment count, then reassembles
  │  one complete AU; torn trains are dropped before decode
  └─ DecodeBridge (native lane; webview WebCodecs path         video_decode.rs
     unchanged where it works)
       H.264 → openh264 (universal)
       HEVC  → NVDEC (NVIDIA, CUDA-warm)                      nvdec.rs
             → D3D11VA (vendor-neutral ID3D11VideoDecoder:    d3d11va.rs
               AMD/Intel/iGPU; in-house scoped HEVC parser,
               stateless DXVA submission, byte-exact-proven
               against NVENC on real silicon, 60/60)
       → NV12→RGBA threaded convert → IPC to the window
  └─ Feedback (receiver → sender, CHANNEL_CONTROL — the
     existing message, additively extended):
     recv_fps · decode_fails · queue_depth · lost_ts_us
     + est_kbps + delay_trend_us_per_s (serde-default)
       → sender: adaptive IDR cadence (existing), GDR wave
         restart w/ loss-aware length, AIMD bitrate
         (Game-only by default), resolution auto-scale
         (pre-existing, still opt-in)
```

### Observability (opt-in field diagnostics)

Normal service logging remains available at
`C:\ProgramData\AllMyStuff\logs\allmystuff-serve.log`. Verbose pipeline
diagnostics are disabled by default and can be enabled with
`ALLMYSTUFF_CWD_LOG=1` or the GUI preference. Field telemetry is also
disabled by default; it requires the `field-telemetry` feature or
`ALLMYSTUFF_TELEMETRY=1`. When enabled, the diagnostic layers name raw
and wire video bandwidth, pacing fidelity, receiver arrival/decode
rates, CPU usage, GPU-engine load, VRAM, and monitor topology.

## 3 · Blast radius — what was touched, ring by ring

### Ring 0 — new files (pure additions; nothing of yours modified)

| File | What it is |
|---|---|
| `node/src/nvenc.rs` | Direct NVENC SDK rung, runtime-loaded, FFI hand-transcribed from MIT ffnvcodec headers |
| `node/src/nvdec.rs` | NVDEC HEVC decode rung (nvcuvid), byte-exact-proven vs NVENC |
| `node/src/d3d11va.rs` | Vendor-neutral D3D11VA HEVC decode rung (stateless DXVA, in-house scoped parser) |
| `node/src/gpu_pipeline.rs` | D3D11 VideoProcessor convert + NV12 ring + device manager + ClockKeeper |
| `node/src/amf.rs` | AMD AMF loader/probe/vendor-gate (staged; Radeon box pending) |
| `node/src/telemetry.rs` | Opt-in vendor-neutral WDDM telemetry line |
| `node/src/diagnostics.rs` | Runtime gate for opt-in verbose diagnostics |
| `node/src/os_perf.rs` | Timer/priority/EcoQoS/CPU-set levers + Win11 process opt-outs + precise_sleep + MMCSS opt-in |
| `gui/src/fsr1.ts`, `gui/src/ui/ModePill.svelte` | Popout FSR1 upscaler; shared Mode control |
| `.github/workflows/node-check.yml` | Required cross-platform strict-clippy CI |
| `docs/INTEGRATION-REPORT-2026-07.md`, `docs/TESTER-KIT-2026-07.md` | Maintainer review and validation documentation |

### Ring 1 — media-plane files substantially extended (the fork's purpose)

| File | Nature |
|---|---|
| `node/src/video.rs` (+3877/−…) | Encoder ladder, postures, pacer support, stats, AIMD controller, wave chooser. **Your CPU pipeline remains intact as the fallback** — the GPU lane fails soft into it; the MJPEG floor is behaviorally unchanged. |
| `node/src/win_capture.rs` (+988) | GPU capture path added beside the existing one; CPU capture retained |
| `node/src/mediafoundation.rs` (+652) | Device-manager/texture feed, adapter pinning, in-place bitrate; the MFT remains the vendor-neutral default rung |
| `node/src/video_decode.rs` (+402) | Native decode bridge: HEVC ladder, layer-bandwidth stats |
| `node/src/mesh.rs` (+389) | The pacer (`send_video_paced`), arrival timing, feedback enrichment — all within the existing forwarder task; every daemon call is a pre-existing API |
| `node/pixels/src/lib.rs` (+312) | Convert-path speedups (from the July encoder pass) |
| `node/src/bin/serve.rs`, `hwenc.rs`, `videotoolbox.rs`, `input_inject.rs`, `control_client.rs` | Log location, QSV/VT low-latency opts, injector boost, bounded daemon-pipe write (a wedged-daemon guard — drops/reconnects instead of stalling forever) |

### Ring 2 — shared/wire surfaces (review these closest)

| File | Exact change | Wire compatibility |
|---|---|---|
| `crates/allmystuff-protocol/src/app.rs` | `RouteControl::Tune` += `game: bool`, `mode: Option<String>`, **`ext: serde_json::Value`**; `RouteControl::VideoFeedback` += `lost_ts_us: Option<u64>`, **`ext: serde_json::Value`** — all `#[serde(default)]`, additive, nothing renamed/removed. **`ext` is the pipeline's opaque bag** (see the boundary note below): the protocol crate relays it verbatim and never inspects it, so future pipeline signals never add fields here. | Old peer receiving new fields: ignores them (serde). New peer receiving old messages: defaults (off/zero/null). Proven both directions by the session round-trip tests, including an explicit "ext relayed verbatim" assertion. |
| `crates/allmystuff-session/src/media.rs` (+7) | `InputAction::MouseMoveRel` (pointer-lock deltas) — additive enum variant behind the existing catch-all | Old receivers drop unknown actions by the pre-existing catch-all arm |
| `crates/allmystuff-session/src/lib.rs` (+34) | `Effect::VideoFeedback`/`TuneMedia` carry the new fields through; same gate logic (`is_active() && peer == from`) untouched | Internal to the app; wire shape is the protocol crate's |
| `crates/allmystuff-mobile-core/src/control.rs` (+7) | The phone's `video_feedback` constructor fills the new fields with absent/zero | Pure constructor parity |
| `gui/src-tauri/src/main.rs`, `gui/mobile/src/commands.rs` (+4 each) | Tune command plumbs `game`/`mode` through | GUI-internal |
| `node/src/node_control.rs` (+19) | `video_feedback` op passes through; no new ops | GUI-internal |

## 3.5 · The backend boundary (why future pipeline work won't touch your crates)

By design, all encoder/decoder/pacing tuning is confined to the node's
video modules; the protocol/session/GUI never need touching for it. The
enforcing seam: `VideoFeedback`/`Tune` carry an **opaque
`ext: serde_json::Value`** the protocol and session crates relay
verbatim and never inspect. The node backend owns its shape
(`video::PipelineFeedback::to_ext`/`from_ext` at the mesh edge). So a
new receiver-side signal is a field on that struct; a viewer-requested
encoder knob reads `Tune`'s `ext`; a Labs feature is `labs::on(...)`.
None cross into your crates. This keeps every future pipeline change's
blast radius inside `node/src/{video,nvenc,nvdec,d3d11va,amf,
mediafoundation,gpu_pipeline,win_capture,labs,os_perf}.rs` + the
`mesh.rs` pacer — review those, and pipeline evolution stays there.

## 4 · Untouched surfaces — the guarantees

- **Signaling: zero changes, zero new payloads.** Nothing in this line
  touches, wraps, or adds to the signaling path. All additions ride
  the ICE-negotiated STUN/TURN datapath on surfaces that already
  carried the same class of traffic (media → existing track lanes;
  control → the existing `CHANNEL_CONTROL` messages, additively
  extended). Verifiable: `git diff 78b1c76..HEAD` contains no
  signaling-layer code at all — the signaling implementation lives in
  MyOwnMesh, which is…
- **MyOwnMesh: not in this repo's diff, and the pinned daemon binary
  in every prototype bundle is byte-identical to yours.** The
  `.myownmesh-rev` pin is unchanged. `H264AuAssembler` semantics
  (emit per RTP marker, contiguity anchor across split writes) are
  *depended on*, verified against, and unmodified.
- **RTP/packetization/FEC/retransmission:** untouched (daemon's) and
  explicitly out of scope for this PR.
- **Your CPU capture/encode pipeline:** intact and load-bearing — it
  is the fallback every new lane fails soft into, and the only path on
  non-Windows/adapter-pinned/duplication-denied routes.
- **The MJPEG compatibility floor, presence/rooms/terminal/files/
  clipboard/audio planes:** behaviorally unchanged (audio untouched
  this line beyond the pre-existing plane; the deferred "studio sound"
  uplift was explicitly parked).
- **Defaults philosophy:** Balanced stays the default posture with
  your stream shape; everything aggressive is posture- or env-gated
  (see §5). The resolution auto-scaler you'd find in `AutoAdapt`
  remains **opt-in** exactly as before.

## 5 · Automatic behaviors and their gates (the reservation rule)

Ranked by how much they act without being asked:

| Behavior | Default | Gate/dial |
|---|---|---|
| AIMD bitrate (closed loop) | **Game posture only** — the one mode whose use case (smoothness > quality, always) makes an automatic cut correct in every case. Balanced/Studio never auto-change quality. | `ALLMYSTUFF_RATE_ADAPT`: unset=game-only · `1`=all lossy postures (field A/B) · `0`=off. Never lossless, never user-pinned bitrates. |
| Resolution auto-scale | OFF (pre-existing) | `ALLMYSTUFF_AUTO_ADAPT=1` |
| Adaptive IDR cadence | ON (pre-existing, benign recovery lever: 2 s↔8 s) | — |
| GDR wave + loss-aware length | Game posture only (GDR streams) | posture-gated |
| Link-fitted pacer drain | ON (LAN keeps original constants; WAN stops inheriting them) | `ALLMYSTUFF_PACED_SLICES=0` off · `ALLMYSTUFF_PACE_DRAIN_MBPS` pin |
| Direct NVENC SDK rung | OFF in production; the vendor hardware MFT is the normal H.264 default. Studio·Lossless remains selectable but fails soft to high-bitrate lossy Studio until its HEVC media framing is implemented and gated. | `ALLMYSTUFF_NVENC=1` enables direct SDK for normal postures inside the experimental GPU lane · `0` pins the vendor MFT |
| GPU zero-copy lane | OFF after production A/B isolated unbounded driver waits in live shared-texture submission; CPU-DXGI capture still feeds NVIDIA/Intel/AMD hardware encoders. The failing HEVC-over-H.264-track path is quarantined with it. | `ALLMYSTUFF_GPU_LANE=1` enables the experimental lane · `0` is an unconditional kill switch |
| MMCSS scheduling class | OFF | `ALLMYSTUFF_MMCSS=1` |
| HEVC decode rung choice | auto (NVDEC→D3D11VA) | `ALLMYSTUFF_HEVC_DECODER=nvdec\|d3d11va` |

Full dial inventory: `AUTO_ADAPT, CWD_LOG, GAME_MODE,
GPU_HEARTBEAT[_MS|_PX], GPU_LANE, GUI_LOG, HEVC_DECODER, LOG,
MJPEG_MAX_EDGE, MMCSS, NVENC, NVENC_PRESET, PACED_SLICES,
PACE_DRAIN_MBPS, RATE_ADAPT, SERVE_BIN, TELEMETRY[_SECS],
VIDEO_BITRATE, VIDEO_ENCODE_ADAPTER, VIDEO_FPS, VIDEO_MAX_EDGE,
VIDEO_STATS`.

## 6 · Verification contract

The exact proposed diff is always derived from the current upstream
branch rather than a frozen historical merge-base:

```bash
git fetch upstream main
git diff --stat upstream/main...HEAD
git diff --check upstream/main...HEAD
```

The required GitHub workflows cover strict Clippy on Windows, macOS,
and Ubuntu, the default and hardware-feature node test matrices,
no-default-features portability, the root workspace, the GUI check and
build, Android mobile-core, and the Windows GUI backend. The companion
tester kit contains the matching local commands and the current PR
hygiene audit.

Hardware-dependent encoder/decoder paths retain automated regression
tests that skip only when the relevant runtime or device is absent.
Ad-hoc field probes, ignored timing benchmarks, long soaks, output-dump
hooks, and benchmark-only pixel generators are deliberately not part
of the upstream-facing tree.

## 7 · Suggested review order

1. `crates/allmystuff-protocol/src/app.rs` — the only wire-visible
   surface (30 additive lines).
2. `node/src/mesh.rs` — `send_video_paced` + `note_video_arrival` +
   `send_video_feedback`: everything that touches the daemon boundary.
3. `node/src/video.rs` — postures, ladder, AIMD gate
   (`rate_adapt_mode`), wave chooser.
4. `node/src/nvenc.rs` / `nvdec.rs` / `d3d11va.rs` — self-contained
   FFI rungs, each with its proving tests at the bottom.
5. `node/src/os_perf.rs` — the OS levers, all best-effort.
6. `docs/TESTER-KIT-2026-07.md` — reproducible validation and PR
   hygiene checks.
