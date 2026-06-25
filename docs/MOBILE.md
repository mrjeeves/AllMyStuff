# AllMyStuff on iPhone & Android

This document is the architecture and roadmap for the official AllMyStuff
mobile app. It is meant to be exhaustive: the crux that makes it possible, the
one architectural inversion the desktop never needed, exactly what code we
reuse, the wire contract the phone must speak byte-for-byte, the phased plan to
ship it, the risks, and the handful of decisions that are the product owner's
to make.

Two slices of it already live in the repo:

- the pure, transport-agnostic **core** of the mobile client,
  [`crates/allmystuff-mobile-core`](../crates/allmystuff-mobile-core), tested
  the same way every other AllMyStuff crate is (`cargo test -p
  allmystuff-mobile-core`); and
- a runnable **Tauri mobile shell**, [`gui/mobile`](../gui/mobile), that wraps
  the *exact* desktop Svelte UI (the graph/map, Console, Files, Terminal) and
  answers what it can honestly from the core today — chiefly `scan_self`, which
  puts a real phone node with the real viewer/controller capability set on the
  graph. It type-checks for `aarch64-linux-android` in CI (`gui-mobile` job),
  with no Android NDK, because it links no C-building crate yet. See §11 for
  the one-command build.

Everything in §3 marked "✅ in repo" is real today.

---

## 0. What we're building

The same thing AllMyStuff already is — *remote desktop, shell, and files for
every computer you own, over a private mesh* — reachable from your phone. You
open the app, your fleet is on the graph, you tap a machine and:

- **watch and control its screen**,
- **open a real shell** on it,
- **browse and move its files**,

with no VPN, no port forwarding, and **no central server** — the phone is a
true peer on your MyOwnMesh network, end-to-end encrypted to your machines.

The phone is a **viewer and controller**, not a host: it consumes a remote
screen/camera/audio and reaches out with touch, keys, and file ops. It does not
host its own screen, a PTY, or input injection (those are desktop capture
concerns with no clean — or, for input injection, *any* — mobile story).
Phone-as-a-source (its camera/mic/screen shared *to* the fleet) is a later,
explicitly opt-in capability; see §8.

---

## 1. The crux: can a phone join the mesh?

**Yes — as a first-class peer, embedding the engine in-process.** This is the
whole reason the project is viable, so it's worth being precise.

MyOwnMesh is **pure-Rust WebRTC**: the `webrtc` crate (v0.13 — ICE/DTLS/SCTP/
SRTP/data), `ring` 0.17 for crypto, ed25519 device identity, and Nostr-over-
WebSocket for signaling. None of that is desktop-locked, and `myownmesh-core`
is explicitly **an embeddable library** ("pure Rust, embed it in anything"),
not just a daemon. Its facade is small and exactly what a client needs:

```
Mesh::open(MeshConfig) -> MeshHandle
MeshHandle::join(NetworkConfig) -> JoinedNetwork
JoinedNetwork::{ channel::<T>(name), rpc(), peers(), roster_*(), advertise(caps), events() }
```

### The cross-compile verdict (Risk R1)

The only open question was whether that stack cross-compiles to mobile
targets. We probed it:

```
$ rustup target add aarch64-linux-android
$ cargo check -p myownmesh-core --target aarch64-linux-android
```

**Result: the entire Rust dependency tree type-checks for
`aarch64-linux-android`** — `ring`, `tokio`, `nix`, `socket2`, `mio`,
`parking_lot`, `dashmap`, `ed25519-dalek`, `p256`, `aes-gcm`, and the rest of
the webrtc-rs stack. The build stopped at exactly one place: `ring`'s build
script couldn't find `aarch64-linux-android-clang` — **the Android NDK isn't
installed in the probe environment.** That is a build-infrastructure gap, *not*
a Rust portability blocker. `ring` 0.17 and `webrtc-rs` both support
iOS/Android upstream.

**So the crux holds.** The remaining work is plumbing, not research:

1. **Build infra** — the Android NDK + `cargo-ndk` (sets `CC_aarch64-linux-android`,
   `AR`, the linker, and the sysroot); Xcode for the iOS targets
   (`aarch64-apple-ios`, `aarch64-apple-ios-sim`). iOS cross-compilation needs
   macOS and cannot be done from the Linux CI box that built the desktop.
2. **An FFI layer** over `myownmesh-core` (§5) — there is no `extern "C"`,
   `#[no_mangle]`, `uniffi`, or `cdylib` in any MyOwnMesh crate today.
3. **Secret storage** — `identity.json` (a 0600 file) moves to the iOS Keychain
   / Android Keystore. The mesh's `identity` loader assumes a filesystem path,
   so it needs a storage seam.

### The options we did *not* take

| Option | Verdict |
|---|---|
| **(A) Embed `myownmesh-core` in-process** | **Chosen.** The phone is a real peer: own identity, direct DTLS/SRTP to peers, signaling only. Fully preserves the no-server/e2e promise. |
| (B) Relay through a trusted node you own | Fallback only. Still e2e *to the gateway*, but the phone isn't a peer — media decrypts and re-encrypts on a second hop, and you must run an always-on box. Documented escape hatch if R1's NDK/iOS plumbing ever proves intractable; never the headline. |
| (C) Re-implement the protocol on a native WebRTC SDK (libwebrtc) | Rejected. Re-derives the ed25519 handshake, Nostr room derivation, and `MeshMessage` framing from scratch — huge surface, constant drift, zero leverage from the existing crates. |
| (D) Cloud bridge (hosted relay) | **Rejected outright.** A central server that can see your media. It breaks the one promise the product is built on. We do not build this under any framing. |

---

## 2. The one architectural inversion

On the desktop, the GUI is a **thin client of a process it spawns**:

```
GUI  ──(owner-only local socket)──►  allmystuff-serve  ──spawns──►  myownmesh serve
(webview)                            (the node engine)             (the mesh daemon)
```

`gui/src-tauri` connects to `~/.myownmesh/allmystuff-node.sock` (0600); the
node owns the `Mesh` and spawns the `myownmesh` daemon as a child process. The
GUI never speaks the mesh protocol directly — it issues `connect_route`,
`term_send`, … over that local socket and the node does the mesh work.

**iOS forbids a sandboxed app from spawning child processes.** So that model
can't cross over. The mobile app inverts it:

```
Mobile app (one process)
├─ Svelte UI  ──(Tauri commands / UniFFI)──►  embedded myownmesh-core  ──►  the mesh
└─ native H.264/Opus decoders (VideoToolbox / MediaCodec)
```

The phone **is** the node. It speaks the **peer-to-peer** protocol —
`allmystuff-protocol` control messages and `allmystuff-session` media frames —
over MyOwnMesh's typed channels directly, because there is no local node to
delegate to. This is the opposite of the desktop's "always a control-socket
client" rule, and it is the single fact that shapes everything below.

---

## 3. Framework & code reuse

**Decision: Tauri 2 Mobile, reusing the Svelte 5 UI and the pure Rust crates.
Video decoded natively, not via WebCodecs.**

The desktop is already Tauri **2.11** + Svelte **5** + Vite. Tauri 2 has
first-class `tauri ios` / `tauri android` targets — the mobile target has
simply never been initialized (no `gen/apple`, no `gen/android`). The Svelte
SPA already runs in a plain webview behind `isTauri()` guards. xterm.js (the
terminal) runs unchanged in a mobile webview. Choosing React Native or Flutter
would throw away the entire Svelte investment *and* still require the native
decode bridge — they buy nothing the Tauri + UniFFI combination doesn't. The
only credible alternative — fully native SwiftUI/Compose shells over the same
Rust core — stays in reserve for if WKWebView canvas performance for the video
stage proves inadequate (Risk R6).

**The one webview concern is pre-solved.** `VideoDecoder` (WebCodecs) is
unreliable in WKWebView / Android System WebView. We sidestep it the way the
desktop already does for WebKitGTK: decode H.264 **natively** (VideoToolbox on
iOS, MediaCodec on Android) from the compressed access units that already cross
the mesh, and hand the webview ready-to-paint RGBA. We never pull raw RGBA over
the network (Risk R6) and never depend on the webview's own codec.

### Reuse map

| Crate / asset | On mobile | Notes |
|---|---|---|
| `allmystuff-graph` | ✅ reuse whole | `Catalog`, `Capability`, `Route`, `MediaKind`, `Flow`, `propose_route`, `match_endpoint`, the own-vs-share authorization model. Pure serde, `unsafe = forbid`. |
| `allmystuff-protocol` | ✅ reuse whole | `NodeProfile`, `ControlMessage`, `RouteControl`, channel/feature constants, `OwnedRoster`, `RoomMessage`. The wire vocabulary. |
| `allmystuff-session` | ✅ reuse whole | The media frame types (`VideoFrame`, `VideoAssembler`, `InputAction`, `TermEvent`, `FileEvent`, `AudioFrame`, `MediaPayload`) and the route state machine. The consumer-half spec. |
| `myownmesh-core` | ✅ embed (cross-compiled) | The mesh, in-process. The reason the plan works. |
| `allmystuff-mobile-core` | ✅ **in repo** (new) | The phone's capability model, the media decode/encode planes, route-offer helpers, and the `MeshClient` seam. Pure; tested. (§4) |
| `gui/mobile` (Tauri shell) | ✅ **in repo** (new) | The mobile Tauri 2 app: reuses the desktop `gui/src/**` Svelte UI verbatim (`frontendDist: "../dist"`), backed by `allmystuff-mobile-core`. Its own crate, so the desktop build is untouched; `tauri android/ios init` runs against it. (§11) |
| `allmystuff-bridge` | ↪ partial | Reuse the synthetic-endpoint *scheme*, not the hardware scan. The phone's capability builder is `allmystuff-mobile-core::caps` instead. |
| Svelte components | ↪ rework for touch | `Terminal.svelte` (xterm.js) ~free; `Graph.svelte`/`Files.svelte` need responsive + touch; `Console.svelte` renders fine but `input-keys.ts` is mouse/keyboard-only and must be rebuilt for touch. `App.svelte`'s multi-window routing becomes in-app navigation. |
| `node/` engine | ✖ excluded | `xcap`/`cpal`/`nokhwa`/`enigo`/`portable-pty` etc. are desktop *capture/injection*. A phone is a viewer, not a host. |
| `allmystuff-updater` | ✖ excluded (`cfg`) | Binary self-swap is forbidden on iOS / restricted on Android; the stores own updates. |
| Desktop Tauri shell | ✖ separate crate | The mobile shell is `gui/mobile`, *not* a `cfg`-gated `gui/src-tauri` — that keeps the desktop's tray / autostart / single-instance / OS-service / `externalBin` sidecars / `open_secondary_window` out of the mobile binary with zero risk to the green desktop build. The two share only the frontend. |

---

## 4. What's in the repo today: `allmystuff-mobile-core`

The transport-agnostic brain of the mobile client, pure Rust, fully unit
tested. It is written entirely against one seam — `MeshClient` — so every path
is exercised without a radio (an in-memory fake stands in for the embedded
engine in tests).

```
crates/allmystuff-mobile-core/src/
├── lib.rs        crate docs + prelude
├── caps.rs       the phone's Capability set (viewer/controller; opt-in host) + advertised features
├── node.rs       assemble the phone's NodeProfile for presence
├── connect.rs    build the RouteControl::Offer for screen / camera / audio / terminal / files
├── transport.rs  the MeshClient seam + classify(channel,from,payload) -> typed Inbound
└── media/
    ├── video.rs  VideoSink (MJPEG reassembly) + VideoDecoder seam + RgbaFrame/H264Au/JpegFrame
    ├── input.rs  touch/keys -> normalized InputAction/InputEvent (clamped, per-route seq)
    ├── term.rs   keystrokes/resize up, PTY bytes down
    └── files.rs  request/reply FileClient: req-id allocation + Read chunk reassembly
```

Two correctness facts this crate pins down, both verified by tests and matched
to the existing desktop/`amst` behaviour:

- **A phone needs a *display* sink, not just a video sink.** The graph routes a
  remote *screen* (`MediaKind::Display`) only to a display sink and a remote
  *camera* (`MediaKind::Video`) only to a video sink — exactly as the desktop
  lands a remote screen on its physical monitor but a camera on its synthetic
  `video-in`. A phone has no monitor to expose, so it advertises a synthetic
  `display-in` (Display sink) that means "render a remote desktop here," plus
  `video-in` for cameras. (`caps.rs`)
- **Terminal and files are synthetic `generic` routes.** They don't match
  advertised capabilities; the host recognizes `<host>:terminal` /
  `<host>:files` by id and authorizes them by *fleet membership*, not a grant.
  The offer is byte-identical to what `amst` and the GUI send, down to the
  route id `route:{from}→{to}`. (`connect.rs`)

---

## 5. The FFI layer (`myownmesh-ffi`, to build)

The embedded engine is reached through a thin FFI crate, added on the
**MyOwnMesh** side (the bindings belong with the dependency, and benefit any
future embedder). Recommended shape:

- **Crate:** `crates/myownmesh-ffi`, `crate-type = ["lib", "staticlib", "cdylib"]`,
  using **UniFFI** (proc-macro mode + `uniffi::setup_scaffolding!()`), with
  `aarch64-apple-ios` / `aarch64-linux-android` added to MyOwnMesh CI.
- **A JSON boundary, not a typed one.** AllMyStuff already serializes
  everything; the FFI surface should cross `String`/`bytes`, not the engine's
  rich types (`MeshEvent`, `PeerInfo`, `IceCandidateStats`, …). This keeps the
  binding tiny and avoids deriving UniFFI traits across the whole engine.
- **The surface** (one `MeshBridge` object; async via `#[uniffi::export(async_runtime = "tokio")]`):

  ```text
  MeshBridge.open(identity_seed: bytes?) -> MeshBridge      // load/generate identity (Keychain/Keystore-backed)
  MeshBridge.device_id() -> String
  MeshBridge.join(network_id: String, label: String) -> Result<()>
  MeshBridge.advertise(profile_json: String) -> Result<()>  // the NodeProfile from allmystuff-mobile-core
  MeshBridge.peers() -> [String]
  MeshBridge.send(peer: String, channel: String, payload_json: String) -> Result<()>
  MeshBridge.roster_approve(device_id: String, label: String) -> Result<()>   // pairing
  // inbound via a foreign callback interface:
  trait InboundSink { fn on_message(channel: String, from: String, payload_json: String); fn on_event(event_json: String); }
  ```

  The Rust side maps `send`/`on_message` onto `JoinedNetwork::channel::<serde_json::Value>(name)`
  (monomorphizing the generic `Channel<T>` to a JSON channel), and fans
  `events()` into `on_event`. The mobile app then drives it entirely through
  `allmystuff-mobile-core`: build the `NodeProfile`/offers there, hand JSON to
  `send`, and feed `on_message`'s `(channel, from, payload)` straight into
  `allmystuff_mobile_core::transport::classify`.
- **Secret storage seam:** `open(identity_seed)` lets the platform supply the
  ed25519 seed from Keychain/Keystore instead of the engine reading a file.

This crate is the right size to build and verify on host first (UniFFI compiles
on host), then wire into the cross-compile once the NDK/Xcode infra lands.

---

## 6. The wire contract the phone must honour

Everything below is what `allmystuff-mobile-core` already encodes; it's
restated here as the implementer's checklist. **All of it is JSON + base64 over
the mesh's typed channels, plus two RTP track lanes.** Forward-compatibility is
mandatory throughout: an unknown tag/kind decodes to `Unknown` (or `None`) and
is ignored — never error, or a peer vanishes.

**Channels** (`allmystuff-protocol`): `allmystuff/presence/v1`,
`/control/v1`, `/media/v1`, `/owned/v1`, `/rooms/v1`.

**Decode (inbound, the viewer half):**

| Plane | Transport | Decode |
|---|---|---|
| Screen / camera (H.264) | RTP lane (`video_inbound`) | Annex-B access units → VideoToolbox/MediaCodec. `key` = IDR entry point; PTS = `rtp_ts * 1000 / 90`. |
| Screen / camera (MJPEG fallback) | `t:"video"` on media channel | standalone baseline JPEGs, chunked across ~64 KiB messages sharing a `seq`; reassemble with `VideoAssembler` (done in `media::VideoSink`). |
| Capture status | `t:"vstat"` | render `VideoStatusState` instead of a black stage. |
| Audio (Opus) | RTP lane (`audio_inbound`) | 48 kHz. |
| Audio (PCM fallback) | untagged frame on media channel | interleaved S16LE, base64. |
| Terminal | `t:"term"` (`TermEvent::Data`) | raw VT bytes → xterm.js. |

**Encode (outbound, the controller half) — all plain JSON, no codec:**

| Plane | Frame | Built by |
|---|---|---|
| Input | `InputAction` (mouse_move normalized 0..1, mouse_button, wheel, key as DOM `key`/`code`) | `media::InputEncoder` |
| Terminal | `TermEvent::Data` / `Resize` | `media::TermPlane` |
| Files | `FileEvent` (list/read/write/mkdir/rename/delete, viewer-minted `req`) | `media::FileClient` |
| Route setup | `ControlMessage::Route(RouteControl::Offer{route,video,audio,session})` | `connect::offer_*` |

**Route ids** are `route:{from}→{to}` (note the literal `→`), derived
identically on both ends. **base64 is always the STANDARD engine.**

---

## 7. Connection & pairing

1. **Identity** — load or generate the phone's ed25519 identity into the
   Keychain/Keystore; this is its mesh device id.
2. **Join** — `Mesh::open` → `MeshHandle::join(network)`. Signaling is Nostr
   over WebSocket, reachable on cellular/Wi-Fi; STUN/TURN likewise.
3. **Pair into the fleet** — the phone joins via the existing claim/ownership
   model (`OwnedRoster`). A desktop owner approves it; the bilateral
   verification (the 5-char device suffix + the verification code each side
   reads back) is the out-of-band confirm. **No new auth mechanism** — this is
   the same device-trust primitive the desktop uses. Fleet membership is what
   gates terminal/files/input (owner-or-fleet), distinct from share-grants
   (which gate media from peers you don't own).
4. **Advertise** — publish the phone's `NodeProfile` (from
   `allmystuff-mobile-core::node::mobile_profile`) on presence.

**Background limits:** iOS aggressively suspends a persistent WebRTC node in
the background. v1–v3 treat the mesh node as **foreground-only** (live while the
app is open). Persistent background presence (push-wake, etc.) is a separate
effort and may never fully match the desktop; set expectations accordingly.

---

## 8. Phased plan

Sequenced by **codec dependency and risk** — the lightest plane first, the
hardest capture last.

**Phase 0 — De-risk & scaffold.**
- ✅ Cross-compile probe (R1): pure-Rust tree checks for Android; only the NDK is missing.
- ✅ `allmystuff-mobile-core` (this PR): the tested, transport-agnostic core.
- ⬜ Stand up the Android NDK + `cargo-ndk` and an iOS build host; cross-compile
  `myownmesh-core` clean on both (R1 finish).
- ⬜ `myownmesh-ffi` (§5); prove `open → join → exchange one message` with a desktop peer.

**Phase 1 — v1: Graph + Terminal (smallest shippable).**
- ✅ Mobile entry point + Tauri shell as its own crate (`gui/mobile`), reusing
  the desktop Svelte UI; `scan_self` backed by `allmystuff-mobile-core`;
  android-target CI check. `tauri android/ios init` runs against it (§11).
- ⬜ Embed `myownmesh-core` via the FFI; identity in Keychain/Keystore; fleet pairing.
- ⬜ Render `Graph.svelte` (responsive) for discovery; reuse the graph/protocol/session crates whole.
- **Terminal**: `offer_terminal` → `TermPlane` ↔ xterm.js, plus an on-screen
  modifier bar (Ctrl/Esc/arrows/Tab). Pure JSON, no codec, no decoder — that's
  why it's first.
- Ship to TestFlight / Play internal. **Deliverable: open a remote shell from your phone.**

**Phase 2 — v2: Remote desktop (view) + Files.**
- **Native video decode bridge** (highest-risk media work): VideoToolbox/MediaCodec
  fed Annex-B AUs; MJPEG fallback via `VideoSink`. Render to a `Console`-style canvas.
- **Touch input forwarding**: rebuild `input-keys.ts` for touch → `InputEncoder`
  (tap→click, drag→move, pinch/scroll→wheel, soft keyboard→`Key`). Now the phone *controls*.
- `tune_route` / `video_feedback` / `Refresh` for quality adaptation.
- **Files**: `FileClient` + a touch file browser; downloads stream to phone storage.

**Phase 3 — v3: Audio + Camera + Clipboard + Rooms (toward parity).**
- Opus + PCM decode (and Opus encode if the phone's mic becomes a source).
- **Camera-as-source** (opt-in host scope): phone camera → H.264 → offer a video
  route. First time the phone *hosts* capture (AVFoundation/Camera2); last and optional.
- Clipboard sync; Rooms; background-presence investigation.

---

## 9. The decisions that are the product owner's to make

These shape the build and are hard to reverse. Defaults below are what the
code and this plan already assume; flag any you'd change.

1. **Mesh path — embedded peer (A).** *Default: A.* The only path that keeps
   "the phone is a true p2p peer, no central server." B (relay) is the
   documented fallback if the iOS/NDK plumbing ever proves intractable. **D
   (cloud bridge) is off the table** — it breaks the core promise.
2. **Scope — viewer/controller, host opt-in later.** *Default: viewer/controller
   for v1–v2; camera-as-source opt-in in v3; never a screen/PTY/input-injection
   host.* Determines whether the phone ever advertises *source* capabilities.
   (`MobileScope` already encodes both.)
3. **UI — Tauri + Svelte.** *Default: reuse the Svelte UI.* Going native
   (SwiftUI/Compose) only if WKWebView video performance is unacceptable after
   the Phase 2 spike (R6).
4. **Background presence — foreground-only.** *Default: foreground-only v1–v3.*
   Persistent presence fights the OS and burns battery; a separate effort.
5. **FFI home — the MyOwnMesh repo.** *Default: `myownmesh-ffi` lives with the
   transport.* Same owner as AllMyStuff, so coordination is light, but it is a
   second-repo change.

---

## 10. Risks & the cheapest experiment for each

| # | Risk | Cheapest experiment | Status |
|---|---|---|---|
| R1 | `myownmesh-core` cross-compiles to iOS/Android? | `cargo check --target` on both | ✅ Android Rust tree checks; only NDK missing. iOS needs a macOS host. |
| R2 | Can webrtc-rs's ICE enumerate candidates in the iOS sandbox / on cellular? Entitlements? | minimal iOS app, one peer on Wi-Fi then cellular | ⬜ run right after R1 finishes |
| R3 | App Store viability of an embedded full WebRTC stack | internal TestFlight build on a real signed device | ⬜ at first TestFlight |
| R4 | VideoToolbox/MediaCodec accept openh264's profile/level? | capture real `video_inbound` AUs, feed a standalone decoder harness on-device | ⬜ start of Phase 2 |
| R5 | MyOwnMesh owner accepts the FFI crate; idle media-lane battery cost | socialize the FFI plan (same owner); battery-measure an idle joined node | ⬜ before Phase 1 |
| R6 | WKWebView canvas throughput for the video stage | decode-on-device → canvas paint benchmark; native shell fallback if poor | ⬜ Phase 2 |

---

## 11. Build & release deltas

### Build the shell now

The shell is in `gui/mobile`. It reuses the desktop frontend, so the only
extra inputs are the platform toolchains. From `gui/mobile`:

```sh
# one-time: the mobile Rust targets + the Tauri CLI for this package
rustup target add aarch64-linux-android armv7-linux-androideabi \
                  aarch64-apple-ios aarch64-apple-ios-sim
pnpm install            # just the Tauri CLI; the UI installs in ../ on build

# Android — needs the Android SDK + NDK on PATH (ANDROID_HOME, NDK_HOME) and
# cargo-ndk (`cargo install cargo-ndk`):
pnpm tauri android init          # generates gen/android (git-ignored)
pnpm tauri android dev           # run on a device/emulator
pnpm tauri android build         # -> .apk / .aab

# iOS — needs macOS + Xcode + an Apple Developer signing identity:
pnpm tauri ios init              # generates gen/apple (git-ignored)
pnpm tauri ios dev               # run in the simulator / on device
pnpm tauri ios build             # -> .ipa
```

`tauri android/ios init` regenerates the native Gradle/Xcode projects under
`gen/` from the shell + config, so they're intentionally **not** committed.
CI's `gui-mobile` job proves the Rust backend keeps type-checking for Android
on every push (no NDK needed until the embedded engine lands). Desktop smoke
test of the same shell on a GTK box: `pnpm tauri dev`.

### Release deltas

Adding mobile means, beyond the desktop's existing Linux CI + minisign-signed
releases:

- **Android**: NDK + `cargo-ndk`; build the `.aab`/`.apk`; sign with a Play
  upload key; Play Console internal → closed → open tracks.
- **iOS**: a macOS build host with Xcode; `tauri ios build` → `.ipa`; an Apple
  Developer account, signing certs + provisioning profiles; TestFlight → App
  Review. (Cannot be produced on the Linux desktop-CI box.)
- **`cfg` gating**: `tauri.conf.json`'s `externalBin` sidecars, `build.rs`
  sidecar bundling, and the desktop-shell Tauri commands are desktop-only and
  must be excluded from the mobile target. The updater crate is excluded
  entirely.

---

*The bottom line: a Tauri Mobile app in this repo that embeds `myownmesh-core`
as a true mesh peer via a small UniFFI layer, reuses the pure
graph/protocol/session crates and the Svelte UI, decodes media natively, and
ships terminal-first → desktop-view → audio/camera. The crux is settled; the
core (`allmystuff-mobile-core`) and a runnable Tauri shell (`gui/mobile`,
android-checked in CI) are in the repo; the next gate is the embedded-engine
FFI crate (wiring the `MeshClient` seam to `myownmesh-core`) plus the
NDK/iOS cross-compile (R1's tail).*
