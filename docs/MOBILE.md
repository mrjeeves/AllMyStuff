# AllMyStuff on iPhone & Android

This is a developer architecture and launch-status document. It is not an
installation guide for a released mobile app. Start with the
[documentation map](README.md) if you are looking for desktop usage or
contributor setup.

This document is the architecture and status of the official AllMyStuff
mobile app. It is meant to be exhaustive: the crux that makes it possible, the
one architectural inversion the desktop never needed, what actually shipped
(which is more than the original plan — and simpler), the state of the mobile
UI, the phased road to the stores, the risks, and the handful of decisions
that are the product owner's to make.

The app is **in the repo and runnable**:

- [`gui/mobile`](../gui/mobile) — the Tauri 2 mobile shell. It wraps the
  *exact* desktop Svelte UI and embeds the **whole stack in one process**:
  the MyOwnMesh daemon (in-process, v0.3.0's `myownmesh::embedded`), the same
  `allmystuff-node` engine the desktop's serve binary runs (built
  capture-less), and the desktop's full node-backed command surface —
  presence, routing, remote desktop, terminal, files, sites, rooms, fleet
  admin, CEC support. §11 has the one-command build.
- [`crates/allmystuff-mobile-core`](../crates/allmystuff-mobile-core) — the
  pure, transport-agnostic model of *what a phone is on the graph* (its
  viewer/controller capability set and `NodeProfile`), tested the same way
  every other AllMyStuff crate is (46 tests; `cargo test -p
  allmystuff-mobile-core`). Its role narrowed once the full engine moved
  in-process — see §4.

What remains before the App Store / Play Store is **device validation and
release plumbing** (§8, §10, §12), not architecture.

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
screen/camera/audio and reaches out with touch, keys, and file ops. It does
not host its own screen, a PTY, or input injection (desktop capture concerns
with no clean — or, for input injection, *any* — mobile story). The one
capture plane a phone runs for real is **audio** (cpal speaks CoreAudio /
AAudio), so inbound streams play and the mic can source a route.
Phone-screen-as-a-source (ReplayKit / MediaProjection) is a later, explicitly
opt-in capability; see §8.

---

## 1. The crux: can a phone join the mesh?

**Yes — as a first-class peer, with the engine embedded in-process.** This
was the whole reason the project was viable, and it is now settled *in
running code*, not just a probe.

MyOwnMesh is **pure-Rust WebRTC**: the `webrtc` crate (ICE/DTLS/SCTP/SRTP),
`ring` for crypto, ed25519 device identity, and Nostr-over-WebSocket for
signaling. None of that is desktop-locked. Two MyOwnMesh releases made the
embedding real rather than theoretical:

- **`myownmesh::embedded`** (v0.3.0) — the daemon as a library: the same
  mesh instance, network registry, hosted services, and control-socket
  listener that `myownmesh serve` runs, started as tasks on the caller's
  tokio runtime. Purpose-built for this app (iOS forbids spawning the daemon
  as a child process), but nothing in it is mobile-specific.
- **A discovery seam** with both the pure-Rust `mdns-sd` backend and a
  system **DNS-SD** backend — the phone rides the system responder, which
  matters on iOS where raw multicast sockets need Apple's managed
  entitlement (§10, R2).

### The cross-compile verdict (Risk R1)

The original probe stands: the **entire Rust dependency tree type-checks for
`aarch64-linux-android`** — `ring`, `tokio`, the webrtc-rs stack, and the
rest. The one stop was `ring`'s C build wanting the Android NDK toolchain —
build infrastructure, not portability. Today:

- CI (`gui-mobile` job) keeps the shell type-checking for
  `aarch64-linux-android` on every push, **NDK-free**, by building
  `--no-default-features` (the `mesh` feature is what links the engine and
  its C-building deps).
- On-device builds use the default features (engine in) and need the NDK +
  `cargo-ndk` for Android, Xcode on macOS for iOS — §11.
- The whole embedded stack (daemon + node engine + migration + dispatch) is
  exercised **on the host** by `gui/mobile`'s integration test
  (`boot_migrates_and_answers_dispatch_end_to_end`), which boots the real
  daemon on a real control socket and round-trips a real command.

### The options we did *not* take

| Option | Verdict |
|---|---|
| **(A) Embed the engine in-process** | **Chosen and shipped.** The phone is a real peer: own identity, direct DTLS/SRTP to peers, signaling only. Fully preserves the no-server/e2e promise. |
| (B) Relay through a trusted node you own | Fallback only. Still e2e *to the gateway*, but the phone isn't a peer — media decrypts and re-encrypts on a second hop, and you must run an always-on box. Documented escape hatch; never the headline. |
| (C) Re-implement the protocol on a native WebRTC SDK (libwebrtc) | Rejected. Re-derives the ed25519 handshake, room derivation, and message framing from scratch — huge surface, constant drift, zero leverage from the existing crates. |
| (D) Cloud bridge (hosted relay) | **Rejected outright.** A central server that can see your media. It breaks the one promise the product is built on. We do not build this under any framing. |

---

## 2. The one architectural inversion

On the desktop, the GUI is a **thin client of a process it spawns**:

```
GUI  ──(owner-only local socket)──►  allmystuff-serve  ──spawns──►  myownmesh serve
(webview)                            (the node engine)             (the mesh daemon)
```

**iOS forbids a sandboxed app from spawning child processes** — and offers no
shared background daemons that would make the separation worth anything. So
the mobile app piles the three processes every other platform separates into
**one**:

```
Mobile app (one process — gui/mobile)
├─ Svelte UI  ──(Tauri commands)──►  node_control::dispatch   (the same match the
│                                    desktop reaches over its socket, minus the socket)
├─ allmystuff-node engine            (capture-less: viewer/controller planes only,
│      │                              plus real audio I/O via cpal)
│      └──(control socket in $TMPDIR)──►  myownmesh daemon, in-process
│                                         (myownmesh::embedded::start)
└─ UiSink = TauriSink                (engine events land on the webview bus directly)
```

Everything above the process boundary is **byte-identical to desktop**: the
daemon wire protocol, the engine's bring-up (identity → profile → claim
networks → subscriptions → presence), and the frontend event contract
(`allmystuff://…`). The shared Svelte app cannot tell which platform answered.

Three sandbox details the desktop never had to care about
(`gui/mobile/src/engine.rs`):

- **State re-homing.** `$HOME/.myownmesh` isn't writable on iOS (the
  container root is not app-writable), so every engine store resolves
  through `MYOWNMESH_HOME`, pointed at the app-data directory.
- **The control socket lives in `$TMPDIR`.** Container paths overrun the
  104-byte `sun_path` limit for unix sockets; tmp is the one short-enough
  writable place, and a socket is ephemeral by nature (re-bound every
  launch).
- **No shutdown path.** iOS gives no "about to terminate" moment worth
  trusting; peers age the phone out through the same heartbeat that covers
  a battery dying.

An earlier build of the app ran a hand-rolled engine adapter with its own
identity seed and settings files; `engine.rs` migrates that state (same
ed25519 key → same device id, networks → daemon config) exactly once, with
a retry-safe marker scheme. New installs never touch that path.

### How this differs from the original plan (and why)

The first design (§5 of the old revision of this document) put
`allmystuff-mobile-core` behind a `MeshClient` seam and bridged it to an
embedded `myownmesh-core` — the phone would re-implement the
viewer/controller half of the wire protocol client-side, with a UniFFI crate
sketched as the fallback for a pure-native app. Building it revealed the
better move: **the node engine itself compiles capture-less** (the capture
planes swap for stubs behind cargo features — the `node` workspace's
`--no-default-features` build, checked in CI), and reusing it wholesale buys
the entire battle-tested route/media/persistence machinery — including
everything that has landed since (fleet governance, KVM, sites, CEC, the
video pipeline's live-edge work) — for free, forever. The wire contract
stops being something the phone must re-honour byte-for-byte; it's the same
code on both ends. No UniFFI, no FFI crate, no seam to keep in sync.

---

## 3. Framework & code reuse

**Decision: Tauri 2 Mobile, reusing the Svelte 5 UI and the node engine
whole. Video decoded by the engine (openh264 → RGBA), not by the webview.**

The desktop is already Tauri + Svelte + Vite; Tauri 2 has first-class
`tauri ios` / `tauri android` targets, and the mobile shell (`gui/mobile`)
runs the desktop frontend verbatim (`frontendDist: "../dist"`). React Native
or Flutter would throw away the entire Svelte investment and still need the
same engine embed. The only credible alternative — fully native
SwiftUI/Compose shells over the same Rust engine — stays in reserve for if
webview canvas performance ever proves inadequate (R6).

**The webview codec concern is pre-solved, desktop-style.** `VideoDecoder`
(WebCodecs) is unreliable in WKWebView / Android System WebView. The node
engine already carries the answer, built for Linux WebKitGTK: a **native
openh264 decoder per inbound display route** (`node/src/video_decode.rs`)
that turns H.264 access units into ready-to-paint RGBA the webview blits
with `putImageData`. The webview asks for it per-route (`video_watch`'s
`decode` flag), so a webview with working WebCodecs can still decode itself.
MJPEG stays what it is everywhere: the fallback for genuinely old peers.
Hardware mobile decode (VideoToolbox / MediaCodec) is a **battery/perf
optimization to slot behind the same seam later**, not a launch requirement
(R4/R6).

### Reuse map

| Crate / asset | On mobile | Notes |
|---|---|---|
| `allmystuff-node` (the engine) | ✅ **embed whole, capture-less** | `default-features = false` swaps capture/inject (xcap, nokhwa, enigo, portable-pty…) for stubs; `audio-io` stays on (cpal → CoreAudio/AAudio: playback + mic-as-source are real). CI checks this exact config. |
| `myownmesh` + `myownmesh-core` (the daemon) | ✅ embed (`embedded::start`, v0.3.0) | In-process, same control socket + wire protocol, inside the app sandbox. |
| `allmystuff-graph` / `-protocol` / `-session` | ✅ via the engine | The engine links them; the phone speaks the identical wire because it runs the identical code. |
| `allmystuff-mobile-core` | ✅ the phone's *model* | Capability set + `NodeProfile` (`scan_self`'s honest fallback while the engine boots) and the pure spec of the viewer/controller planes. §4. |
| `allmystuff-inventory` | ✅ | `scan_full`, GUI-side exactly like desktop. |
| Svelte UI (`gui/src/**`) | ✅ shared, mobile-adapted at runtime | One bundle; `isMobile()` gates behaviour, media queries gate layout. §7 is the full inventory. |
| `allmystuff-cec-protocol` / `-consent` | ✅ via the engine | The CEC relay rides the node engine, so the phone can hold either end of a help call (§7's CEC note). |
| `allmystuff-updater` | ✖ excluded | Binary self-swap is forbidden on iOS / restricted on Android; the stores own updates. The Settings tab shows read-only "About" on mobile. |
| Desktop Tauri shell (`gui/src-tauri`) | ✖ separate crate | `gui/mobile` is its own workspace, so tray / autostart / single-instance / OS-service / sidecars / secondary windows never enter the mobile binary, and the green desktop build is untouched. The two share only the frontend. |

---

## 4. `allmystuff-mobile-core`: the phone's model

The pure Rust core (serde + the three library crates, `unsafe = forbid`,
46 unit tests + a doctest). Since the engine moved in-process, its job is
narrower and sharper:

- **What ships in the app:** `caps` + `node` — the phone's capability set
  and `NodeProfile`. This is what `scan_self` answers from while the engine
  is still booting (under the `"this"` placeholder id the store re-homes),
  and it pins down the two graph facts a phone must get right:
  - **A phone needs a *display* sink, not just a video sink.** The graph
    routes a remote *screen* (`MediaKind::Display`) only to a display sink
    and a remote *camera* (`MediaKind::Video`) only to a video sink. A phone
    has no monitor to expose, so it advertises a synthetic `display-in`
    ("render a remote desktop here") plus `video-in` for cameras. (`caps.rs`)
  - **Scope is explicit.** `MobileScope::ViewerController` vs the opt-in
    `ViewerControllerHost` decides whether the phone ever advertises
    *source* capabilities (§8).
- **What stands as the spec:** `connect` / `control` / `transport` /
  `media/*` — the client-side offer builders, control surface, and media
  planes, written against the `MeshClient` seam and exercised without a
  radio. The embedded engine supersedes them at runtime, but they remain
  the executable, tested description of the viewer/controller wire
  contract — and the starting point if a pure-native (non-Tauri) client is
  ever built. Known, deliberate gaps at this altitude: no audio/clipboard/
  site-tunnel client planes, no `Reject`/`DeadLane`/ownership-claim
  builders — all live in the engine.

It is **current with the model** (it compiles against the same workspace
HEAD as the protocol crates — a missed `NodeProfile` field is a compile
error, not silent drift; the recently-added `product` inventory field
landed here in the same commit that added it to the protocol).

---

## 5. The engine in the shell: what `gui/mobile` actually does

- **`engine.rs`** — the boot (`serve.rs` minus the two spawns): start the
  embedded daemon on the sandbox socket, bring up `allmystuff_node::mesh::
  Mesh` against it with a **TauriSink** (every engine event —
  `allmystuff://session`, `…/video-ready`, `…/term-exit`, `cec://…` — lands
  on the webview bus directly), share the park store, run the adapter-era
  migration. `Engine::request` / `request_bytes` hand each Tauri command to
  `node_control::dispatch` — the same match the desktop reaches over its
  socket.
- **`commands.rs`** — the desktop GUI's node-backed command surface,
  verbatim: same names, same argument shapes, same JSON. Routing, claims,
  KVM, shares, input, clipboard, video (watch/poll/unwatch/refresh/
  feedback/tune), terminal, files, sites, rooms + shared files, fleet
  (including hubs), CEC (status/dial/queue/approve/deny/revoke/grants/
  chat), forget-node, and the full mesh_* config surface. What is *not*
  mirrored, deliberately: secondary windows, the self-updater, the
  Always-On service, tray/autostart/window-behaviour, and the
  save-dialog-backed network export (the UI hides or in-apps those on
  mobile — §7).
- **`lib.rs`** — `scan_self` (engine-first, model fallback), `scan_full`,
  `client_log`, and the boot task. The whole stack starts in the
  background so the UI is up immediately; commands answer "node not ready"
  until it is, and the frontend's `tryInvoke` degrades exactly as it does
  on a desktop whose node hasn't answered yet.
- **`logging.rs`** — the `tracing` stream goes to the device console *and*
  an on-phone `allmystuff.log` (one previous run kept). On iOS it lands in
  Documents, surfaced by the Files app (*On My iPhone → AllMyStuff*) via
  `UIFileSharingEnabled` — diagnostics you can actually reach with no Mac.
- **`Info.ios.plist`** — merged into the generated project: local-network
  permission strings (`NSLocalNetworkUsageDescription`, `NSBonjourServices`
  for `_myownmesh._tcp`), the mic usage string (the one real capture
  plane), and the Files-app exposure keys.
- The CI-facing split: the `mesh` cargo feature (default **on**) links the
  daemon + engine; `--no-default-features` is the NDK-free CI check that
  keeps the shell + frontend embed type-checking for Android on every push.

---

## 6. The wire contract

There is deliberately nothing to restate here anymore: **the phone runs the
same engine, so it speaks the same wire by construction** — channels,
route ids, frame types, base64 discipline, forward-compat rules and all.
The protocol reference lives with the code (`allmystuff-protocol` /
`allmystuff-session` rustdoc) and in [ARCHITECTURE.md](../ARCHITECTURE.md).
`allmystuff-mobile-core`'s tests double as the client-side spec of the
viewer/controller subset (§4).

---

## 7. The mobile UI: one Svelte app, two postures

The desktop frontend ships on the phone unmodified — same bundle, same
components. Mobile is a **runtime posture**, decided by `isMobile()` (UA +
`maxTouchPoints`) and CSS:

**Structure.** Desktop opens consoles/terminals/files/rooms/chat as
secondary OS windows (`open_*_window`); a phone has one window, so the
store keeps every surface **in-app** (`isTauri() && !isMobile()` gates each
`open…` call), and `html.is-mobile` + `@media` compact the chrome: the
status pills dock to a thumb-reach bottom bar in portrait, panels
(Sidebar/NodeDrawer) float over the graph and **swipe-to-close**
(`swipe.ts`), Files/Terminal go fullscreen, and every fixed edge honours
`env(safe-area-inset-*)` (with a fixed floor where WKWebView under-reports).

**The graph** pans with a finger (radial) or scrolls natively (grid),
**pinch-zooms** in radial view (two fingers zoom about their midpoint;
the zoombar buttons remain as the accessible fallback), opens node menus
from a tap-able gear (never right-click), and reserves drag-to-share for
the desktop (a finger drag is navigation).

**The remote console** is the deepest adaptation (`console-touch.ts` — a
trackpad model, not tap-the-pixel): one finger steers a relative cursor,
tap clicks at the cursor, tap-then-hold drags, long-press right-clicks,
two fingers scroll, pinch zooms the stage. `ConsoleKeys.svelte` is the
soft keyboard: an invisible input summons the OS keyboard and translates
what it produces into key events, with a strip for what it can't say
(Esc/Tab/arrows) and one-shot sticky modifiers — pinned above the OS
keyboard by tracking `visualViewport`.

**The terminal** gets the same treatment with `TerminalKeys.svelte`:
xterm.js already owns the typing path (its hidden textarea takes the OS
keyboard), so the strip adds Esc/Tab/one-shot-Ctrl/arrows — arrows honour
the emulator's application-cursor mode, Ctrl folds into the next typed
character as a control byte — plus the ⌨ button whose tap is the user
gesture iOS requires to summon the keyboard at all. The pane shrinks to
stay clear of the strip and the OS keyboard under it.

**Files** opens folders on the second single tap of a selected row (no
double-tap timing window on touch), and the hover-reveal row actions get
finger-sized targets on touch builds.

**Hidden on mobile, on purpose:** the Updates tab (read-only "About" — the
stores own updates), the Always-On service tab, network/venue **Export**
(needs the desktop save dialog; Copy invite / Import cover sharing), and
the desktop's pop-out buttons.

**CEC on a phone:** the engine carries the full relay, and the command
surface is registered, so the *customer* verbs (approve/deny/revoke,
grants) and the *technician* verbs (queue, dial, chat) all work. The
technician queue tab itself is still summoned by a keyboard shortcut
(Ctrl+Alt+Shift+C) — fine for a technician with a paired keyboard,
invisible otherwise; giving it a touch entry point is an open product
decision (§9.5).

### Known gaps (not yet done)

- Long-press context menus (graph nodes and file rows currently put every
  action in the drawer / row buttons — workable, not native-feeling).
- The soft keyboard does not follow focus automatically anywhere but
  Console/Terminal (forms are fine; xterm needed the explicit summon).
- Room member-picker grid and a few settings tables get cramped under
  ~360px widths.
- `LayersSheet`'s hover cross-highlight is decorative-only on touch (all
  content remains visible).
- No haptics, no share-sheet integration (log/file export goes through the
  Files app instead).

---

## 8. Phased plan

Sequenced by codec dependency and risk. Phases 0–2 are **code-complete in
the repo**; what stands between here and the stores is device validation
and release plumbing, not feature work.

**Phase 0 — De-risk & scaffold. ✅ done**
- ✅ Cross-compile probe (R1): pure-Rust tree checks for Android; NDK is
  build infra. CI guards it (`gui-mobile`, NDK-free).
- ✅ `allmystuff-mobile-core`: the tested model + client-side spec.
- ✅ The architecture pivot that ended the FFI question: `myownmesh`
  v0.3.0's `embedded` module + the capture-less node build → the whole
  engine in-process (§2). Host integration test boots the real stack.
- ✅ Identity & state in the sandbox (`MYOWNMESH_HOME`, tmp socket,
  adapter-era migration).

**Phase 1 — v1: Graph + Terminal (smallest shippable). ✅ code-complete**
- ✅ The shell runs the shared UI; `scan_self` honest from first frame.
- ✅ Presence/routing/fleet pairing through the embedded engine.
- ✅ Terminal end-to-end (xterm.js ↔ PTY over the mesh) **plus the mobile
  keyboard story** (`TerminalKeys`): summon, Esc/Tab/Ctrl/arrows.
- ⬜ Prove it on hardware: a real iPhone + a real Android phone joining a
  real fleet (R2 is here — iOS local-network + multicast entitlements).
- Ship to TestFlight / Play internal. **Deliverable: open a remote shell
  from your phone.**

**Phase 2 — v2: Remote desktop (view + control) + Files. ✅ code-complete**
- ✅ Video: engine-side openh264 → RGBA → canvas (the WebKitGTK path,
  reused); RTP lanes via the embedded daemon; MJPEG for old peers. The
  live-edge/quality work (tune / refresh / feedback, dead-lane recovery,
  latest-wins MJPEG) rides the engine and is already wired to the UI.
- ✅ Touch input forwarding (`console-touch.ts` + `ConsoleKeys`): the phone
  *controls*.
- ✅ Files: browse/preview/rename/delete/transfer, touch-adapted.
- ⬜ On-device validation: decode throughput + battery on real hardware
  (R4/R6 — and whether hardware decode needs pulling forward).

**Phase 3 — v3: toward parity.**
- ✅ Audio playback (cpal → CoreAudio; the one real capture plane).
- ⬜ Clipboard sync UX on touch (the commands are wired; the interaction
  needs design — there's no Ctrl+C to intercept).
- ⬜ Camera/mic-as-source (opt-in host scope, AVFoundation/Camera2 —
  `MobileScope` already encodes it). Phone-screen-as-source (ReplayKit /
  MediaProjection) stays out unless a real need appears.
- ⬜ Background presence investigation (push-wake vs foreground-only —
  §9.4).
- ⬜ Rooms polish on small screens.

**Phase 4 — Launch plumbing (the actual gate now).**
- ⬜ Android: NDK + `cargo-ndk` in a release pipeline; signed `.aab`; Play
  internal → closed → open tracks.
- ⬜ iOS: a macOS build host; signing certs + provisioning profiles +
  the multicast entitlement request; TestFlight → App Review (R3).
- ⬜ Store listings, privacy labels (easy honesty: no accounts, no
  analytics, no server), screenshots.
- ⬜ Version/update discipline: the phone advertises its version like every
  node; peers' "upgrade" verb must map to "go to the store" on mobile.

**Then: CEC Support for phones** — the reason this app's architecture was
kept generic. The CECSupport customer app is already a thin shell over the
same node engine; its mobile build reuses this exact pattern (embedded
daemon + capture-less engine + Tauri mobile shell + the same sandbox
lessons) wholesale. The one honest caveat: a phone *customer* can't share
its screen without ReplayKit/MediaProjection work, so the first CECSupport
mobile deliverable is the customer's **companion** surface (ask for help,
approve/deny, see who's connected, revoke, chat) and the technician's
pocket console — both of which this app's engine + command surface already
prove out.

---

## 9. The decisions that are the product owner's to make

Defaults below are what the code already assumes; flag any you'd change.

1. **Mesh path — embedded engine (A).** *Shipped.* The only path that keeps
   "the phone is a true p2p peer, no central server." B (relay) remains the
   documented fallback; **D (cloud bridge) is off the table.**
2. **Scope — viewer/controller, host opt-in later.** *Default unchanged*:
   viewer/controller for v1–v2; camera/mic-as-source opt-in in v3; never a
   screen/PTY/input-injection host. (`MobileScope` encodes both; audio I/O
   is already real.)
3. **UI — Tauri + shared Svelte.** *Shipped.* Going native (SwiftUI/
   Compose over the same engine) only if webview video performance is
   unacceptable on hardware (R6); `allmystuff-mobile-core` §4 is the
   starting point if so.
4. **Background presence — foreground-only.** *Default: foreground-only
   through v3.* Persistent presence fights the OS and burns battery; a
   separate effort with its own design (push-wake, App Refresh budgets).
5. **CEC touch entry point.** The technician queue is behind a keyboard
   shortcut today. Decide: keep the phone technician-capable but
   undiscoverable (current), or give the CEC tab a Settings toggle /
   deep-link so a technician's phone is a first-class pocket console.
6. **Store identity.** `works.allmystuff.mobile` bundle id, listing name,
   and whether Android ships on Play only or also as a signed APK from
   allmystuff.works (the desktop's install story suggests yes).

---

## 10. Risks & the cheapest experiment for each

| # | Risk | Cheapest experiment | Status |
|---|---|---|---|
| R1 | Engine cross-compiles to iOS/Android | `cargo check --target` | ✅ settled (tree checks; NDK/Xcode are infra). CI guards the Android config on every push. |
| R2 | ICE + mDNS inside the iOS sandbox: local-network prompt, the managed multicast entitlement, cellular paths | one signed dev build on a real iPhone joining a fleet on Wi-Fi, then cellular | ⬜ **the top open risk.** Permission strings + Bonjour service types are already in `Info.ios.plist`; the system-DNS-SD discovery backend exists to duck the raw-multicast entitlement; the entitlement request should still be filed early. |
| R3 | App Store viability of an embedded full WebRTC stack | internal TestFlight build | ⬜ at first TestFlight. Precedent (Tailscale et al.) is good. |
| R4 | openh264 software decode: throughput + battery on phone SoCs | play a 1080p route on mid-range hardware, measure | ⬜ Phase 2 validation. Fallback is wiring VideoToolbox/MediaCodec behind the engine's existing decode seam. |
| R5 | Idle battery: a joined mesh node holding DTLS + signaling in the foreground | battery-measure an idle joined phone | ⬜ before TestFlight; informs how aggressively the app should drop/rejoin on backgrounding. |
| R6 | Webview canvas throughput for the video stage (`putImageData` at 30–60fps) | decode-on-device → canvas benchmark | ⬜ Phase 2 validation; native-shell reserve if poor (§9.3). |
| R7 | The one-process pile-up under memory pressure (iOS jetsam) | instrument RSS on-device during a 4K route | ⬜ new since the embed; the engine's buffers were sized for desktops. |

---

## 11. Build & release

### Build the shell now

The shell is in `gui/mobile`. It reuses the desktop frontend, so the only
extra inputs are the platform toolchains. From `gui/mobile`:

```sh
# one-time: the mobile Rust targets + the Tauri CLI for this package
rustup target add aarch64-linux-android armv7-linux-androideabi \
                  aarch64-apple-ios aarch64-apple-ios-sim
pnpm install            # just the Tauri CLI; the UI installs in ../ on build

# Android — needs the Android SDK + NDK on PATH (ANDROID_HOME, NDK_HOME) and
# cargo-ndk (`cargo install cargo-ndk`); the default `mesh` feature links the
# engine, whose C deps (ring, openh264) are what the NDK is for:
pnpm tauri android init          # generates gen/android (git-ignored)
pnpm tauri android dev           # run on a device/emulator
pnpm tauri android build         # -> .apk / .aab

# iOS — needs macOS + Xcode + an Apple Developer signing identity:
pnpm tauri ios init              # generates gen/apple (git-ignored)
./patch-xcode-project.sh         # after EVERY init — see below
pnpm tauri ios dev               # run in the simulator / on device
pnpm tauri ios build             # -> .ipa
```

`patch-xcode-project.sh` re-applies what `tauri ios init` resets: it turns
Xcode 16's user-script sandboxing off (Tauri's build phase writes outside
the sandbox, so a sandboxed build dies with "Operation not permitted"),
stamps the full-bleed brand icon (`gui/src-tauri/icons/icon-ios.png`) into
the generated asset catalog, and copies `ios/HideKeyboardAccessory.m` into
the generated sources — a self-installing swizzle that removes the
keyboard's prev/next/Done accessory bar over WKWebView text fields (the
Console/Terminal key strips replace it).

`tauri android/ios init` regenerates the native Gradle/Xcode projects under
`gen/` from the shell + config, so they're intentionally **not** committed.
CI's `gui-mobile` job proves the Rust backend keeps type-checking for
Android on every push (NDK-free via `--no-default-features`); the `node`
job additionally checks the capture-less engine config the phone links.
Desktop smoke test of the same shell on a GTK box: `pnpm tauri dev`.

### Release deltas

Beyond the desktop's existing Linux CI + minisign-signed releases:

- **Android**: an NDK-equipped release job; sign with a Play upload key;
  Play Console internal → closed → open tracks.
- **iOS**: a macOS build host with Xcode; Apple Developer account, signing
  certs + provisioning profiles (+ the multicast entitlement, R2);
  TestFlight → App Review. Cannot be produced on the Linux desktop-CI box.
- **Store-owned updates**: the updater crate stays out of the mobile
  binary; the Settings tab already shows "About" instead. Fleet "upgrade
  this machine" pointed at a phone should surface "update from the store."
- The daemon pin: the embedded engine follows `.myownmesh-rev` (v0.3.3
  today) via the git tags in `gui/mobile/Cargo.toml` — bump both together,
  exactly like the desktop's bundled-daemon pin.

---

## 12. What launch actually needs (the short list)

1. **R2 on hardware** — one signed iOS dev build joining a real fleet
   (local-network prompt, multicast entitlement or system-DNS-SD path,
   then cellular). The single biggest unknown left.
2. **An Android release build** through NDK + `cargo-ndk`, on a device.
3. **Phase-2 validation numbers** — decode fps, battery, RSS (R4–R7).
4. **Signing + store plumbing** for both stores (§11).
5. **A TestFlight / Play-internal round** with the fleet-owning humans this
   product already has.

Everything else on the critical path is already merged.

---

*The bottom line: the app in `gui/mobile` is the desktop, folded into one
process — the same daemon, the same node engine (capture-less), the same
Svelte UI grown a touch posture — with `allmystuff-mobile-core` pinning
down the model of what a phone is on the graph. The crux is settled in
running code; the remaining gate is device validation (iOS networking
entitlements above all) and store release plumbing.*
