# Performance Roadmap — Parsec-tier 4K60

> **Engineering roadmap:** this document records video-pipeline direction and
> implementation history. It is not a release promise or a user guide. Current
> user-visible behavior belongs in the [README](README.md), and stable process
> boundaries belong in [ARCHITECTURE.md](ARCHITECTURE.md).

A unified, prioritized plan for the video trifecta (throughput · latency · quality/bandwidth)
across **AllMyStuff** (capture / encode / decode / UI / product) and **MyOwnMesh** (transport).
The organizing spine is one **Performance↔Quality slider** that is a *pure quality/performance
axis* — it moves resolution / fps / bitrate (and 4:4:4 chroma at the top, where the selected codec
supports it). It **never swaps codec or encoder.** The codec/encoder is chosen **once, ahead of
time**, from the host's detected hardware (`allmystuff-inventory`) intersected with the peer's decode
capability, and locked for the session — we can always tell which codec is best for a given install,
so there is no reason to swap it at runtime. The MJPEG floor remains the automatic
guaranteed-operation fallback when no shared hardware-codec path can be established (or the link
collapses), not a quality step the user dials. MyOwnMesh has a companion doc for the transport items:
`MyOwnMesh/docs/PERFORMANCE-ROADMAP.md`.

This is a fill-in of a primed system, not a rewrite. Every item is grounded in the current
code with `file:line` anchors so it can be picked up cold.

## What already exists (don't rebuild it)

- **The slider is real today** — `gui/src/store.svelte.ts:401-510` (`consoleControlMode: "slider" | "pills"`,
  defaulting to slider), with named stops Speed/Smooth/Balanced/Crisp/Quality in
  `gui/src/ui/Console.svelte:197`.
- **Receiver→sender feedback already flows** — `RouteControl::VideoFeedback`
  (`crates/allmystuff-protocol/src/app.rs:673`) carries `recv_fps` / `decode_fails` / `queue_depth`;
  today it only adjusts IDR cadence (`node/src/video.rs:590`).
- **Capture is an in-house DXGI duplication session** on Windows (`node/src/win_capture.rs`),
  not a naïve screenshot loop; Wayland is a real PipeWire ScreenCast session
  (`node/src/wayland_capture.rs`).
- **The encoder already has a clean degrade ladder** — `make_encoder` (`node/src/video.rs:1392`)
  falls H.264 → MJPEG; new backends slot into the same ladder.
- **Per-host GPU vendor is already detected** — `allmystuff-inventory` (`crates/allmystuff-inventory/src/lib.rs:231`,
  `GpuVendor::{Nvidia,Amd,Intel}`); capability negotiation can read it.
- **A decode ladder already exists** — WebCodecs (hardware) → native openh264
  (`node/src/video_decode.rs`).

## Shipped so far

- **Hardware H.264 encode with a frame-send-tested step-down ladder** (item 3, the headline win).
  A `H264Codec` trait (`node/src/video.rs`) sits behind `H264Stream`; `make_h264_codec` walks the
  platform's hardware encoders best-first, opens each and **frame-send-tests it** (encode one grey
  frame), and the first that actually emits an access unit wins. Anything that won't open or won't
  produce a frame is stepped over, down to software **openh264** as the guaranteed floor. The
  hardware rung is split by platform so it ships to **any** config with **no extra build toolchain
  where it matters**:
  - **Windows — Media Foundation** (`node/src/mediafoundation.rs`): the GPU's own H.264 MFT
    (**NVIDIA H.264 Encoder MFT** = real NVENC on our fleet, else Intel QuickSync / AMD VCE),
    enumerated hardware-first by the OS and driven through the `windows` crate we already link for
    DXGI capture. **No FFmpeg, no pkg-config, no vcpkg** — MF ships inside Windows. This is the
    default HW path; it's always compiled on Windows and needs nothing added to `just dev`.
  - **Linux / macOS — FFmpeg** (`node/src/hwenc.rs`, the `hwenc` feature): **NVENC → VA-API →
    QuickSync** (Linux), **VideoToolbox** (macOS). Opt-in (`--features hwenc`, needs FFmpeg dev
    libs + pkg-config), since a plain build / a viewer box shouldn't have to install FFmpeg.

  Because hardware encode is no longer forced into `just dev`/`just serve`, a plain `cargo build`
  (and every viewer machine) compiles software-only with zero media toolchain — which is what
  unbroke the macOS and Windows builds that the forced FFmpeg dependency had wedged.
- **Fused RGBA→I420 encode** (item 2 below, the CPU-pass half). `scale_rgba_to_i420`
  (`node/pixels/src/lib.rs`) does the downscale + BT.601 conversion in one pass straight to a
  contiguous I420 buffer, fed to openh264 via a borrowing `YUVSource` — the old RGB intermediate and
  openh264's separate RGB→YUV walk are gone; the unchanged-frame compare runs on 1.5 B/px I420.
- **The LAN gate on the raised automatic dials** (the item-4 correction). `LinkClass` rides
  every stream's `Tune` (`node/src/video.rs`): the automatic 60 fps / 80 Mbps ceilings apply only
  when the daemon's nominated ICE pair for the viewer is host↔host (`PeerInfo.selected_pair`,
  seeded from `PeersList` into `peer_links`, `node/src/mesh.rs`); reflexive/relayed — and
  not-yet-classified — links keep the open-loop-safe 30 fps / 40 Mbps until BWE (#6) lands.
  Explicit viewer Tunes and the env dials bypass the gate; a class landing or flipping mid-life
  re-gates live streams via `retune_link` (no-op unless the automatic dials actually change).
- **Binary media IPC, both directions** (item 11 below). The base64+JSON per-frame tax on the
  node↔daemon socket is gone: H.264/Opus **sends** ride a dedicated binary `MediaTrackPipe`
  (`node/src/control_client.rs` ↔ MyOwnMesh `run_media_track_pipe`), and inbound H.264/Opus rides a
  dedicated binary `MediaSourcePipe` (MyOwnMesh per-client media sink → `run_media_source_pipe` ↔
  node `subscribe_media_source`). MJPEG, PCM and route signalling stay on the JSON pipe; the base64
  `video_inbound`/`audio_inbound` event path remains only as a version-skew fallback.

## The real critical path (this sets the ordering)

(State *before* the fixes above — the diagram the ordering was derived from.)

```

```
capture → CPU scale+color → SW openh264 (inline, ONE thread) → base64 → JSON-over-local-socket
        → MyOwnMesh daemon → base64-decode → write_sample → webrtc-rs (no congestion control, no pacer)
```

The common "the bottleneck is transport, not the encoder" diagnosis is **wrong for this stack**.
The transport's missing congestion control is a real wall, but it is the *second* wall: it cannot
starve a 4K60 stream that the **software encoder, serialized on one CPU core, cannot produce in the
first place** (`node/src/video.rs:1069-1087`, `1575-1581`). There is **no hardware encoder anywhere**
in the media path. Therefore the order is **encoder/throughput first, transport loop second,
codec/quality levers third**. Sequencing the transport BWE before the hardware encoder yields
~zero visible 4K60 gain.

---

## Prioritized list (ordered by trifecta impact)

Items 1–2 are cheap enablers; **#3 is the single biggest win.**

### Enablers — do first (days)

**1. Split encode off the capture thread.**
`node/src/video.rs:1025-1088`. Today capture and encode run on one thread sharing the 16.6 ms
4K60 budget — they cannot overlap. Move `encoder.encode()` + the fps-budget sleep onto a consumer
thread fed by the existing freshest-first `video_out` channel (already bounded `try_send`,
`node/src/mesh.rs:4569` — **keep it bounded**, or decoupling converts dropped frames into unbounded
latency). Free latency+fps multiplier; also stops masking #3's gains. *Effort: days. Prereq: none.*

**2. `VideoEncoder` trait + `FrameInput` enum (the seam).**
`node/src/video.rs:1410-1466`. Replace `enum StreamEncoder { Mjpeg, H264 }` with a trait, and
introduce `enum FrameInput { Rgba { data, w, h }, Gpu(GpuSurface) }` as the frame seam (today a bare
`(Vec<u8>, w, h)` tuple crosses at `:1072`). The reusable control logic — budget rebuild (`:1546`),
static-skip gate (`:1558`), adaptive IDR cadence (`:1570`), bitrate clamp (`:1621`) — already lives in
`H264Stream`, not in openh264. Define `FrameInput::Gpu` **now** so the zero-copy upgrade (#10) needs no
second refactor. *Effort: days–1 wk. Prereq: none. Unblocks 3, 10, 14.*

### The encoder + transport closed loop (weeks)

**3. Hardware H.264 backend (NVENC first), fed CPU NV12 — #1 throughput win.**
`node/src/video.rs`. Slot a `NvencH264` impl behind the trait; extend the ladder to
**NVENC → openh264 → MJPEG** (mirror today's soft-fail). First slice keeps the DXGI readback but does
one RGBA→NV12 pass (vs today's two: RGBA→RGB at `:1555` then RGB→YUV at `:1575`) into NVENC; output
stays Annex-B (`VideoPacket::H264`, `:1600`) so nothing downstream changes. Unlocks 4K60, drops encode
latency, better RD at equal bitrate. *Effort: weeks. Prereqs: 1, 2.*
- **DECISION (locked): native per-vendor SDKs**, not a single FFmpeg dependency. Backends:
  **NVENC** (NVIDIA Video Codec SDK FFI, e.g. `nvidia-video-codec-sdk`/`nvenc`), **Intel oneVPL/QuickSync**,
  **AMD AMF** (FFI), **VideoToolbox** (macOS), **VA-API** (Linux). This fixes `FrameInput::Gpu` to native
  handles (D3D11 texture / `IOSurface` / VA surface) rather than an `AVHWFramesContext`. Leaner runtime,
  full control of low-latency presets, best zero-copy story; the cost is more `unsafe` platform code and a
  bigger driver test matrix — accepted.
- **Risk:** consumer NVENC/QSV session caps + driver-version coupling → treat "no session / driver too
  old" as a **soft** failure degrading to openh264, or a multi-monitor host gets *worse* than software.
- Slider rung **R3** (HEVC/AV1 later via #13). Keep this slice H.264 Annex-B only.

**4. Raise the auto-bitrate cap + add 80 Mbps / 90 / 120 fps UI stops (LAN-gated).**
`node/src/video.rs:287` (clamp `8M..40M`), `gui/src/ui/Console.svelte:165,158,197` + mirror in
`gui/src/ui/VideoPopout.svelte`. The model itself computes ~79.6 Mbps for 4K60 then clamps to exactly
half; the encoder hard-clamp already allows 80 Mbps (`:1621`), and the backend already accepts 1–120 fps
(`:259`, `:331`). Pure quality/bandwidth headroom — but it only pays off once #3 sustains the framerate.
*Effort: days.*
- **Correction to a common assumption:** there are **no SDP bitrate caps to raise** — SDP is emitted
  verbatim (`MyOwnMesh transport/webrtc.rs:894`); no `b=AS`/`x-google-max-bitrate` exists. The lever is the
  encoder bitrate. Do **not** ship the raised cap WAN-wide on an open-loop transport — gate it to
  direct/LAN peers (the `h264_bitrate_for` doc comment already flags LAN as the target) or land it with #6.

**5. Surface transport RTT / loss / bitrate into `PeerDiag`.** *(MyOwnMesh — see companion doc.)*
`crates/myownmesh-core/src/transport/webrtc.rs:1061`, `transport/diag.rs:162`. `get_stats()` reads only
ICE candidate pairs today; `RemoteInboundRTP`/`OutboundRTP` (RTT, loss, jitter) are never read. Poll on a
~1 s tick — a real congestion signal with **zero fork risk**, useful immediately for diagnostics.
*Effort: days. Prereq for all ABR.*

**6. Close the loop: send-side estimator → cross-repo `target_bps` → in-place `set_bitrate`.**
MyOwnMesh `transport/webrtc.rs:320` (RTCP is drained and **discarded**; TWCC emitted, never consumed) +
AllMyStuff `node/src/mesh.rs` / `node/src/video.rs`. Build a GCC-style loss+delay estimate (custom
`Interceptor` — registry wired at `webrtc.rs:206` — or coarsely off #5) and pipe `target_bps` to the
encoder. Becomes the #1 bandwidth/latency lever the instant #3 hits 60 fps (open-loop send bursts
~400-packet 4K IDRs, `webrtc.rs:554`). *Effort: weeks. Prereqs: 3, 5.*
- **Critical correction:** routing the estimate through `note_feedback`/`retune` is wrong —
  `note_feedback` only swaps IDR cadence (`node/src/video.rs:590`), and `retune` (`:609`) **restarts
  capture = an IDR hiccup per step** → a keyframe storm under a 1 Hz loop. Add a **new in-place
  `set_bitrate`** (openh264 supports runtime bitrate); reserve `retune` for resolution changes.

**7. Pacer between `write_sample` and the wire.** *(MyOwnMesh — see companion doc.)*
`transport/webrtc.rs:982-1000`. A 4K IDR is ~400 packets bursting back-to-back (`:554`), concentrating
loss on the worst packets and triggering NACK storms.
- **Effort correction:** `TrackLocalStaticSample.write_sample` packetizes *internally* (`:993`), so a
  token bucket there only spaces whole access units (weak exactly where it hurts). A real per-packet pacer
  means switching that lane to `TrackLocalStaticRTP` + an app-side packetizer = **weeks**, not days.
  *Prereq: 6 (pace rate = target_bps × headroom).*

**8. Dedicated low-latency input/control lane (kill head-of-line blocking).** *(MyOwnMesh — see companion doc.)*
`crates/myownmesh-core/src/channels.rs` + `transport/webrtc.rs`; AllMyStuff `node/src/mesh.rs`. Split the
single reliable+ordered data channel: an **unreliable/unordered** channel for input HID + control
(`Tune`/`Refresh`/`VideoLane`), keeping bulk/file/clipboard on the reliable one. The biggest
"feels-like-local" pointer-latency win; independent and parallelizable. *Effort: weeks.*

**9. Syncing: dynamic audio playout target + true A/V PTS alignment.**
`node/src/audio.rs`, `crates/allmystuff-session/src/audio.rs`, `node/src/mesh.rs`. Two parts:
(a) **dynamic** playout target — estimate jitter from inter-arrival variance and adapt within a band
instead of the fixed ~80 ms / 200 ms constants (*days*; tightens clean links, fewer underruns on jittery
ones); (b) carry a presentation timestamp on the Opus lane and align it to the video RTP 90 kHz clock at
the sink for **true lip-sync** instead of independent live-edge chasing (*weeks*). Feel/quality, not raw
throughput — ranked below the video core but explicitly in scope.

**10. GPU-resident color/scale + zero-copy DXGI texture → HW encoder.**
`node/src/win_capture.rs:266-295` (the ~33 MB/frame `CopyResource`→`Map`→BGRA→RGBA loop, plus the GPU
texture it already creates at `:259` and throws away), `node/pixels/src/lib.rs`, `FrameInput::Gpu`. Keep
the duplicated `ID3D11Texture2D` on the GPU and feed NVENC/AMF/QSV D3D11 input directly, deleting the
readback and the two CPU passes. Large latency+fps. *Prereq: 3.*
- **Effort: weeks → months**, because of the architectural blocker below.
- **The blocker (verified):** capture+encode live in `node`; `write_sample`/webrtc live in the
  **separate MyOwnMesh daemon** (`node/src/control_client.rs`, `interprocess` local socket). A GPU surface
  cannot cross that socket. True zero-copy to the wire needs the **encoder relocated into the daemon** (or a
  shared-GPU-context design). Plus device-affinity (the encoder must use the same `ID3D11Device` that owns
  the texture, `win_capture.rs:259`) and a GPU-side rotation path (rotation is CPU-only today, DXGI-only).
  Static-skip diff (`video.rs:1563`) also has no cheap CPU buffer on a GPU path — rely on DXGI
  `LastPresentTime==0` damage (`win_capture.rs:254`) or keep a small CPU thumbnail.

**11. Kill the per-frame base64+JSON IPC tax on the bitstream. ✅ DONE (both directions).**
Every encoded frame used to be base64'd (~1.33×) + JSON-serialized across the node↔daemon process
boundary, twice (once each way), on the hot path — real CPU competing with the encoder. Now:
- **Sends** (`node/src/mesh.rs` `send_video_track`/`send_audio_track` → `MediaTrackPipe` in
  `node/src/control_client.rs`) ride a dedicated binary connection: a one-line handshake, then
  length-prefixed raw frames (`allmystuff_protocol::control` media-frame codec). The daemon reads them
  in `run_media_track_pipe` (`MyOwnMesh/crates/myownmesh/src/control.rs`).
- **Inbound** (MyOwnMesh pumps → a per-client binary sink on `ClientHandle` → `run_media_source_pipe`)
  reaches the node over `MediaSourcePipe` (`subscribe_media_source`), decoded straight to raw bytes —
  no base64 `video_inbound`/`audio_inbound` JSON. That event path stays only as a version-skew fallback.

MJPEG, PCM and route signalling deliberately stay on the JSON pipe (the floor + control plane), so the
binary pipes carry only the high-rate H.264/Opus. The frame codecs are round-trip + truncation tested
on both sides and kept byte-for-byte identical. *Shipped; was estimated weeks.*

### Codec dimension + the unified slider (weeks–months)

**12. Leave the codec system alone — extend "auto" to pick the best the hardware can do.**
The codec machinery already exists and stays: `consoleCodec` defaults to `"auto"`
(`gui/src/store.svelte.ts:493`); the `Offer.video` accepts-list already does "the offerer lists what it
can consume, the streamer picks the best it can produce, else MJPEG" (`crates/allmystuff-protocol/src/app.rs:596`).
There is **no new handshake to build**. Two clearly separate pieces:

- **Encoder backend = host-local, never negotiated.** "Auto picks the best for the hardware" is just the
  `make_encoder` ladder (`node/src/video.rs:1392`) trying the best encoder for the GPU already detected in
  `crates/allmystuff-inventory/src/lib.rs:231` (`GpuVendor::{Nvidia,Amd,Intel}`) → NVENC/AMF/QSV/VideoToolbox,
  falling back to openh264. **This is the actual 4K60 win (it *is* #3), and it touches nothing on the wire** —
  NVENC encoding H.264 rides the existing H.264 lanes unchanged.
- **Wire codec = the existing accepts-list, just add options.** Add `"hevc"`/`"av1"` to the list the
  offerer advertises (today the hardcoded `['h264']` at `gui/src/tauri.ts:176`) and make the producer-side
  auto-pick rank them; viewer decode capability comes from the WebCodecs probe (`gui/src/ui/Console.svelte:270`)
  + native decode (`node/src/video_decode.rs`). Backward-compatible: an absent/unmatched list already
  degrades to MJPEG (`app.rs:603`). The slider never revisits the choice.

*Effort: the encoder-backend half is #3 (weeks, the big win). The wire-codec half is small **except** it
depends on #13 below to carry a non-H.264 codec on the lanes — so HEVC/AV1-on-the-wire is a separable
later step, not part of the "bam, done" path.*

**13. Codec-parametric RTP lanes in MyOwnMesh.** *(MyOwnMesh — see companion doc.)*
`transport/webrtc.rs:305` (every lane is `MIME_TYPE_H264`; the assembler is H.264-specific FU-A at
`:524-657`). Register HEVC/AV1, carry codec per lane, codec-keyed depacketizer. `send_video` is already
codec-agnostic. *Effort: weeks + a mandatory spike.*
- **Verified feasibility:** the `rtp` crate ships `codecs::h265` (HEVC depacketizer exists) — but
  webrtc-rs issue #779 flags H265 packetizer bugs to verify, and AV1 depacketizer support in the pinned
  0.13 line is unconfirmed. **Do HEVC first (verified-available); treat AV1 as spike-gated.**
- **Simplified by the "codec is chosen once" rule (#12):** lanes are provisioned **before SDP with no
  renegotiation** (`webrtc.rs:293`) — normally a problem, but because the codec is fixed at offer time we
  just **provision the one negotiated codec's lane** and never swap mid-session. No multi-codec lane pool,
  no runtime payload-type renegotiation.

**14. Hardware decode + HEVC/AV1 WebCodecs decode.**
`node/src/video_decode.rs:121-228` (native fallback is software openh264 — on exactly the WebKitGTK/Linux
hosts that lack WebCodecs, where 4K60 SW decode is over budget) + `gui/src/ui/Console.svelte:322`
(avc1-only SPS parse). Add VA-API/VideoToolbox/D3D11VA hwaccel producing NV12 surfaces, openh264 last
resort. Add HEVC (`hvc1`/`hev1`) + AV1 (`av01`) WebCodecs strings + per-codec probes feeding #12.
*Effort: weeks. Prereqs: 12, 13.*

**15. Slider auto-traversal (the capstone) — quality only, never codec.**
`gui/src/ui/Console.svelte:197` (`QUALITY_STOPS`), `crates/allmystuff-protocol/src/app.rs:654`
(`Tune` is `{max_edge, bitrate, fps}`), `node/src/video.rs`, `node/src/mesh.rs`. Wire `VideoFeedback`
(already flowing receiver→sender, `app.rs:673`) into an automatic quality controller that, under load,
steps **bitrate (in-place `set_bitrate`) → fps → resolution** — and stops there. **Codec is never an
adaptive step** (it was chosen once in #12 and is locked); the only "codec" event is the
connection-establishment fallback to the MJPEG floor when the chosen path can't be set up at all.
Add 4:4:4 as a high-quality *profile of the already-selected codec* where it supports it (a quality knob,
not a codec swap). The slider's `Tune` stays `{max_edge, bitrate, fps}` (+ an optional `chroma` flag) —
**no codec field, no teardown/re-offer black-out** (`store.svelte.ts:2464`) on the quality path.
*Effort: weeks atop its prereqs.*

---

## Two orthogonal axes

**Axis A — codec/encoder: auto-selected once, not on the slider.** Decided at session setup (#12) from
host hardware ∩ peer decode capability, then locked. The user never picks it. The selection table:

| Both ends support | Selected codec/encoder |
|---|---|
| HW HEVC (or AV1) encode on host + matching HW decode on viewer | **HW HEVC** (AV1 once #13 spike clears) |
| HW H.264 encode + H.264 decode | **HW H.264** (NVENC/AMF/QSV/VideoToolbox/VA-API) |
| H.264 decode only on viewer | **SW openh264** |
| nothing shared / setup fails / link collapses | **MJPEG floor** (automatic) |

**Axis B — the Performance↔Quality slider: quality only, within the selected codec.** This is the one
the user moves; it sets resolution / fps / bitrate (and 4:4:4 at the top where the selected codec
supports it), and the auto-controller (#15) traverses it under load — bitrate → fps → resolution.

| Slider stop | Res / fps / bitrate | Chroma |
|---|---|---|
| **Floor** | MJPEG fallback, ~960–1280 / 15–24 | — |
| **Smooth** | 1920 / 30 / 8M | 4:2:0 |
| **Balanced** | 1920 / 60 / 15M | 4:2:0 |
| **Crisp** | 2560 / 60 / 25M | 4:2:0 |
| **Quality** | 3840 / 60 / 40–60M (LAN →80M) | 4:2:0 |
| **Max** | 3840 / 60 / up to ~80M | **4:4:4 if the selected codec/pair supports it** |

The slider stops are quality levels rendered on **whatever codec Axis A selected** — moving the slider
never changes the codec. **4:4:4 is a real axis the code is blind to today** (pipeline assumes 4:2:0
everywhere: `fit_within_even`, `node/src/video.rs:1637`; native decode `write_rgba8` from 4:2:0,
`node/src/video_decode.rs:191`); enabling it at the top stop touches the selected encoder's profile +
depacketizer + decoder, gated on the setup-time capability check.

## Cross-cutting prerequisites (each unlocks several items)

1. **`VideoEncoder` trait + `FrameInput::{Rgba,Gpu}`** (#2) → unblocks 3, 10, 14. Hardest seam to change
   later — define now.
2. **Capability handshake** (#12) → selects the one best codec for the hardware pair *once* and gates the
   hardware-codec paths (13, 14). The slider (15) never touches codec. Must stay backward-compatible.
3. **Transport telemetry into `PeerDiag`** (#5) → prereq for 6, 7, 15; zero fork risk; ship first.
4. **In-place encoder `set_bitrate`** (in #6) → without it every ABR step restarts capture (IDR storm).
5. **Version-pinned webrtc-rs 0.13 spike** → gates AV1 scope in 13/14. HEVC verified-available; AV1
   unconfirmed.
6. **Resolve the node↔daemon process boundary** → gates 10 and 11 (encoder likely moves into the daemon).

## Suggested first slice

`#1 → #2 → #3 (NVENC, CPU-NV12) → #4 (LAN-gated bitrate)` — a few weeks, no protocol changes, gets
sustained hardware-encoded 4K60 on NVIDIA hosts at the existing slider's top stop. Then `#5 → #6 → #7`
make it survive real networks; the Tier-2 block turns the slider into the full MJPEG→AV1/4:4:4 ladder.

---

*Diagnosis verified against the source on branch `claude/parsec-4k60-performance-ktkyfn`. Effort
estimates are engineering judgement to validate with `ALLMYSTUFF_VIDEO_STATS=1` per-stage counters
(`node/src/video.rs:164`) on real hardware.*
