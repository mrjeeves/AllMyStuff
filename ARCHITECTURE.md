# Architecture

AllMyStuff is a desktop app for connecting the computers you own over a
private mesh. It's deliberately split so the **model is pure and testable**
and the **mesh is a sidecar**, never an embedded dependency.

## One picture

```
   ┌─────────────────────────────┐     ┌─────────────────────────────┐
   │   AllMyStuff GUI (gui/)      │     │   allmystuff serve (node/)   │
   │ Svelte 5 graph ─invoke─►Tauri│     │   headless — no webview      │
   └──────────────┬──────────────┘     └──────────────┬──────────────┘
                  │  TauriSink (UiSink)               │  LogSink (UiSink)
                  └───────────────┬───────────────────┘
                                  ▼
                    ┌──────────────────────────────┐
                    │   allmystuff-node engine      │   one node per
                    │   presence · routes · every   │   machine, run by
                    │   media plane (screen/camera/ │   allmystuff-serve;
                    │   audio/input/terminal/files) │   the GUI drives it
                    └───────┬───────────────┬───────┘   as a thin client.
          scan_self()       │               │  control socket
      (inventory+bridge)    │               │  (line-delimited JSON)
                            ▼               ▼
        ┌───────────────────────────┐  ┌────────────────────────┐
        │   allmystuff-inventory    │  │   myownmesh serve       │
        │   allmystuff-bridge       │  │   (separate process,    │
        │   allmystuff-graph        │  │    pinned in            │
        │   allmystuff-protocol     │  │    .myownmesh-rev)      │
        └───────────────────────────┘  └────────────────────────┘
              the library workspace         the mesh, sidecarred
```

There is **one node per machine**, and it runs in the `allmystuff-serve`
binary — never in the GUI. That binary owns the `Mesh`, spawns and supervises
the `myownmesh serve` daemon itself (so `allmystuff service` needs only one
unit to bring the whole node up), and exposes its own **node control socket**
(`allmystuff_node::node_control`): length-prefixed frames carrying one request
per command and a stream of engine events. The desktop GUI is a *thin client*
of that socket — it issues commands with `NodeClient` and re-emits the event
stream onto Tauri's bus, rather than linking the engine. Events reach that
stream through the engine's `UiSink` seam: `allmystuff-serve` wires in a
`SocketSink` that logs each event and fans it out to every connected client.

This is why running both the headless service and the desktop app no longer
collides: there is exactly one `Mesh` advertising one identity. (The GUI used
to build its own `Mesh` with a Tauri-backed `UiSink`; two nodes under one
identity is precisely what stopped machines connecting.) See
[The headless node](CONTRIBUTING.md#the-headless-node).

**The one deliberate exception is the phone.** iOS forbids a sandboxed app
from spawning child processes, so the mobile shell (`gui/mobile`) runs the
identical pieces as tasks in its own process: the `myownmesh` daemon
in-process (v0.3.0's `embedded` module), the same `allmystuff-node` engine
built capture-less (the capture/inject planes swap for stubs; audio I/O
stays real), and the same command surface dispatched straight into
`node_control::dispatch` — the desktop's socket match, minus the socket.
Everything above the process boundary — the daemon wire protocol, the
engine bring-up, the frontend event contract — is byte-identical, and the
shared Svelte UI cannot tell which platform answered. The full story is in
[docs/MOBILE.md](docs/MOBILE.md).

The library workspace (`crates/`) compiles and tests with nothing but
`cargo` — no webview, no daemon, no network. The GUI is its own Cargo
workspace (`gui/src-tauri`) so a root `cargo build --workspace` never drags
in Tauri. Same split MyOwnMesh uses.

## Crates

```
crates/
├── allmystuff-inventory     # scan a machine for everything plugged in
├── allmystuff-graph         # the device graph + authorization model
├── allmystuff-protocol      # wire types: myownmesh control mirror + app messages
├── allmystuff-bridge        # Inventory ──► graph Capabilities (+ presence summary)
├── allmystuff-session       # live presence + the route offer/accept handshake + media frame types (audio/video/input/terminal/files/clipboard)
├── allmystuff-term          # `amst` — the command-line terminal (a mesh PTY in your own terminal)
├── allmystuff-cec-protocol  # CEC Support's wire types: support ids, consent control, the CEC room ids
├── allmystuff-cec-consent   # a customer's standing technician approvals (once / 3 hours / forever)
├── allmystuff-mobile-core   # the phone's model: viewer/controller capability set + NodeProfile (docs/MOBILE.md)
├── allmystuff-updater       # self-update: release feed, SHA-256 verify, stage-then-apply
├── allmystuff-service       # install/manage the OS background service (systemd / launchd / Windows SCM)
└── allmystuff-cli           # `allmystuff` — opens the GUI, or scan / capabilities / update / serve / service

node/                      # the headless node engine (its own workspace, heavy media deps)
└── allmystuff-node        # the engine the GUI links + `allmystuff-serve` (runs it with no webview)
```

The library workspace above stays lightweight on purpose. The node engine —
the part that captures screens, encodes H.264/Opus, injects input, hosts PTYs
— carries heavy native dependencies (xcap, cpal, openh264, nokhwa, enigo,
portable-pty, …), so like the GUI it lives in its own Cargo workspace
(`node/`) that the root `cargo build --workspace` never drags in.

### allmystuff-inventory

Cross-platform hardware scan. Returns one `Inventory` value with stable
device ids.

- **Linux is the reference platform** and reads `/proc` + `/sys` directly:
  `/sys/class/drm` + EDID for displays, `/proc/asound` for audio (with USB
  stream channel counts → mic-array detection), `/sys/class/video4linux` for
  cameras, `/proc/bus/input/devices` for keyboards/mice, `/sys/bus/usb` for
  peripherals, `sysinfo` for CPU/RAM/disks/networks.
- **Every probe degrades to "nothing here"** on a missing file, so a scan in
  a locked-down container returns the same well-formed shape as a loaded
  desktop — just with fewer devices.
- **The fiddly decoders are pure functions with fixture tests** — EDID timing
  descriptors, ALSA capture channels, input-device classification — so
  correctness doesn't depend on the hardware being present.
- macOS / Windows reuse the `sysinfo` host basics and scaffold the device
  classes (`system_profiler`, CIM).

### allmystuff-graph

The conceptual core, and the part that makes sharing safe. Pure data + pure
rules, no I/O.

- **Capability** — one routable thing on one node: a `(media, flow)` pair.
  `media` ∈ {audio, video, display, input, storage, generic}; `flow` ∈
  {source, sink, duplex}. Carries a `default` flag — whether it's the node's
  **current default** for its category — which the UI badges and
  `match_endpoint` prefers (after a synthetic machine endpoint) when
  auto-picking where a connection lands.
- **Route** — wires a source capability to a sink of compatible media. Only
  ever minted by `Catalog::propose_route`, which is where media, flow, and
  authorization are all checked.
- **Relationship** — `Mine` (a device you own or manage) or `Shared { person,
  grants }`. A `Grant` authorizes a shared endpoint to play one role
  (`Provide` = they source, `Consume` = they sink) for one media, optionally
  pinned to one capability. The role names **the end of the route being
  authorized**, not who benefits: `Catalog::authorize` checks `route.from`
  (the source end) for `Provide` and `route.to` (the sink end) for `Consume`,
  so "let them see my screen" is a `Provide` grant over *my screen*, and
  reading it as "they consume, therefore Consume" denies every screen share
  ever granted. A grant is **to the person, not one machine**:
  authorization unions the grants across every node shared with the same
  person (people bring fleets — what you allow works to whichever of their
  devices is handy), and the GUI keys the person by the *owner* the devices
  advertise, so a machine of theirs that appears later folds into the same
  share. `required_grants` returns the minimal grant that would unblock a
  denied route — the "one-tap allow."
- **A share reaches the whole receiving fleet**, which takes two halves that
  are each useless alone. The invite lands on one device, so that device
  **relays the grants to its own fleet** (`ShareControl::FleetShare`, keyed
  off the granting device so every sibling lands on the same person record,
  and accepted only from this machine's owner or a signed-roster fleet
  member). And the sharer only ever authorizes peer nodes recorded on the
  share, so the accepter's ack **vouches for its siblings**
  (`ShareControl::Accept.fleet` → `Shares::vouch_siblings`). The trust rule
  is unchanged: a vouch widens only the share its authenticated sender
  already belongs to, a device can never add itself, and the sibling list is
  built from *verified* membership (the signed roster, plus the owner's own
  admit records) — advertised ownership is self-asserted and deliberately
  never trusted here, being the conscription vector the signed roster exists
  to close.
- **Ownership** (in `allmystuff-protocol` + the GUI) — distinct from a
  relationship: a device *advertises* who owns it and whether it's
  *claimable*. You can't flat-take a box — a claim only lands if the device
  was started in **claim mode** (the `ALLMYSTUFF_CLAIMABLE` flag, or its own
  "allow adoption" toggle) and is still unowned. The recorded owner is the
  authenticated claimer the mesh delivered, persisted next to the identity.
- **Claiming is LAN-first.** Every node joins a well-known claim rendezvous
  (`allmystuff-local-claim-v1`) whose daemon signaling is mDNS-only
  (`strategy: "none", mdns: true`) — no relays, no STUN/TURN, clock-free —
  and claimable presence is advertised **only** there by default. Claims
  arriving over any other network are declined, and the claimer refuses to
  send one, unless **claims over the public mesh** are enabled — a strictly
  **device-local** setting (the Fleet pane's toggle, persisted in the
  ownership store, or `ALLMYSTUFF_PUBLIC_CLAIMS` for a headless box). It is
  never fleet-synced and no remote peer can flip it: nothing on the wire can
  open a device to public claiming. With it on, the WAN path is a **claim
  code**: the claimable device mints an unguessable code (`amsclaim-<code>`
  rendezvous network over Nostr+mDNS signaling, `Ownership::ensure_claim_code`),
  shows it out-of-band (service log; a device UI elsewhere), and the owner
  types it into the Fleet pane (`Mesh::claim_via_code`) — the rendezvous is
  private to the code-holders and is torn down (and the code rotated) once
  the claim lands. The old wide-open "claimable on the public mesh" model is
  gone.

See `crates/allmystuff-graph/src/lib.rs` tests for the full behaviour,
including the person-wide grant union and the endpoint auto-pick.

### allmystuff-protocol

Everything AllMyStuff puts on a wire, with no dependency heavier than
`serde`:

- **`control`** — a hand-kept mirror of the MyOwnMesh daemon's control socket
  (`Request` / `Response` / `ServerOut`, `ClientId`). Mirroring rather than
  importing `myownmesh-core` keeps the build independent of the engine
  workspace — the exact discipline the MyOwnMesh GUI's `control_client.rs`
  documents.
- **`app`** — AllMyStuff's own peer-to-peer messages that ride *inside* the
  daemon's typed channels: a `NodeProfile` presence advert,
  `ControlMessage`s for route setup and share negotiation, the
  `OwnedRoster` fleet gossip (`CHANNEL_OWNED`), and the **virtual-rooms
  plane** (`CHANNEL_ROOMS`): `RoomMessage`s carrying a room's invites,
  join/leave presence, chat, and the knock pair (`knock` — a machine
  holding the room's id but no invite asking the host in; `deny` — the
  host's no), with each invite restating the host's `RoomAccess` knock
  policy (`open` admits a knock on the spot, `invite` — the default, and
  what an older host's field-less invites read as — queues it for a
  human). Only the membership + chat plane; a room's media is ordinary
  routes. Peers advertise the `rooms` feature tag, so an older build
  simply never sees the channel (and one that predates a message kind
  drops just that message). Authorization is never on the wire — a node
  only advertises or accepts what its local `Catalog` already permits.
- **The owned fleet** — claiming a device doesn't just flip a flag: the two
  machines start sharing an `OwnedRoster` on `CHANNEL_OWNED` — the set of
  devices one owner has claimed, all linked by a single shared **fleet key**.
  The owner mints the key on its first claim and hands it down on each
  adoption; every co-owned device gossips the roster and converges by version,
  exactly like a mesh roster. The roster also carries the fleet's **name**
  ("Casey") — set from the Fleet pane by any member, replacing with the
  version like membership does (an empty name is skipped on the wire, so
  older peers see the exact roster shape they always did). The name labels
  the graph's fleet section and is what new rooms are titled after. For now the key only groups devices internally
  (a later edition links it to other things). It's persisted next to the
  ownership record.
- **The fleet's signed membership** — under the app-level `OwnedRoster`
  gossip, the fleet is a **closed MyOwnMesh network** whose membership is
  cryptographically signed, not merely gossiped. It's a three-tier cert
  chain: the fleet **owner** (the device that minted the fleet key) founds the
  closed network and signs each device in; a device the owner promotes to
  **manager** (Fleet pane → "make manager" — a MyOwnMesh *controller*) can
  also admit members; plain **members** hold no roster authority. Owner and
  manager changes ride a strict, owner-signed governance log (owners issue
  managers); member admits/removes ride a separate **union-merged member log**
  (managers issue members), so two managers can admit devices concurrently —
  even with the owner offline — and converge without forking. Each device
  re-derives the whole roster from the *verified* logs, so a co-member shows
  up reliably from the signed log alone instead of depending on the owner
  being online to re-gossip it — the dependency that used to make a refreshed
  device briefly think a co-member was "in another fleet." Keys are
  per-device identities (no shared secrets); see MyOwnMesh
  `docs/NETWORK-TYPES.md` for the governance contract.

### allmystuff-bridge

The one place hardware vocabulary meets graph vocabulary. Turns an
`Inventory` into graph `Capability`s — physical devices (mic → audio source,
camera → video source, monitor → display sink, …) plus a synthetic
per-machine set (**screen** = display source, **control** = input sink,
**keyboard & mouse** = input source — a console forwards "whatever this
machine's user does", never one scanned device, so driving a remote works
even where the input scan finds nothing (macOS) — **system audio** =
duplex, **video in** = video sink — the app shows inbound camera
streams in a window, so "can receive a camera" is a property of the
machine, exactly like control — and **clipboard** = duplex, the endpoint
a console pushes a paste into and pulls a remote copy/cut back out of) so
whole-computer flows (screen-share,
camera feeds, room calls, remote control) have something to land on *and*
start from. The GUI also passes its own monitor enumeration in
(`capabilities_with_screens`): each monitor beyond the primary becomes a
`screen:<id>` display source — one console tab per screen — with ids the
capture side resolves back to the same monitor. Shared by the CLI and the
Tauri backend so "what this machine exposes" is computed once.

## The GUI (`gui/`)

Tauri 2 + Svelte 5, a client of the daemon.

> **Where the backend code lives.** Everything in this section below — `mesh`,
> `video`, `audio`, `camera_capture`, `wayland_capture` / `win_capture`,
> `input_inject`, `terminal`, `files`, `clipboard`, `sites`, `control_client`,
> `daemon_spawn`, … — now lives in the **`allmystuff-node`** crate
> (`node/src/`), not under `gui/src-tauri/src/`. The GUI's `src-tauri` is a
> thin Tauri shell: `main.rs` (the `#[tauri::command]` surface + the window
> builder) plus a `TauriSink` that wires the engine's events onto Tauri's
> event bus. The same modules, linked headless, are what `allmystuff serve`
> runs. The module names below are unchanged — just read them under
> `node/src/` rather than `gui/src-tauri/src/`.

- **Backend** (`node/`, linked by `gui/src-tauri` and `allmystuff-serve`) —
  `scan_self` (inventory + bridge), the live
  `mesh::Mesh` (subscribes to the presence/control/media channels, drives the
  `allmystuff-session` state machine, emits `allmystuff://session`
  snapshots), the `audio` bridge (capture → mesh → playback for active
  audio routes: cpal for mics and playback, the OS loopback — WASAPI /
  pulse monitor — when the source is the machine's `system-audio`), the
  `video` bridge (one capture thread per sourced display/camera route —
  screens via the platform capture sessions, cameras via `camera_capture`:
  nokhwa over V4L2 / AVFoundation / Media Foundation, decoded to RGBA and
  fed through the same encoder), `connect_route`/`disconnect_route`
  commands, and `daemon_spawn`. The `myownmesh` daemon ships **bundled with the app**:
  `build.rs` fetches the rev pinned in `.myownmesh-rev` and stages it as a
  Tauri sidecar (`binaries/myownmesh-<triple>`, `externalBin`), so the mesh
  is there for free — `daemon_spawn` resolves the bundled binary and
  auto-spawns it. (That covers source builds and the OS bundles; the
  portable `curl | sh` install is a bare binary with no sidecar inside, so
  there the *installer* guarantees the daemon instead — reusing an
  installed `myownmesh` that meets the pin, asking an older one to update
  itself, or installing one next to the app, which `daemon_spawn` finds in
  the same next-to-the-binary slot.) `update_*` commands drive
  `allmystuff-updater`.
- **Front-end** (`src/`) — the graph. `catalog.ts` is a faithful TypeScript
  port of the graph crate's rules, so the canvas is fully interactive on
  demo data with no backend; when the backend is present it validates the
  same way in Rust and fires the real route over the mesh. Live presence +
  route snapshots merge into the catalog so the graph fills with real peers.
  A node only known from the daemon's roster (not running AllMyStuff) is
  shown but quieted and un-targetable, since it exposes no capabilities. The
  **remote console** (`Console.svelte`) is the pikvm-style session handle for
  a machine: a video-inputs tab bar over its screen + cameras, plus audio and
  control toggles, each owning the real route it set up. On the desktop each
  console opens as its **own OS window** (`open_console_window` →
  `?console=<node>` → `ConsoleHost.svelte`), so several machines can be on
  screen at once; the web preview keeps the in-page popover. The **remote
  terminal** (`Terminal.svelte`) is its sibling: the drawer's **Open
  Terminal** opens a tabbed xterm.js window per machine
  (`open_terminal_window` → `?terminal=<node>` → `TerminalHost.svelte`),
  where every tab is its own mesh route to a PTY the far side spawns — see
  the terminal paragraph under the data flow below. The **remote files**
  window (`Files.svelte`) completes the console family: the drawer's **Open
  Files** (between Remote Control and Open Terminal) opens a finder-like
  window per machine (`open_files_window` → `?files=<node>` →
  `FilesHost.svelte`) — browse, preview, upload, download, rename, delete
  over one mesh route — see the files paragraph below. The stage is a
  live video sink — it registers a per-route IPC channel (`video_watch`) and
  the backend queues each inbound packet as raw bytes (a fixed header + the
  payload; no JSON or base64 on the per-frame path) for exactly the window
  that's watching. Decode walks a ladder: WebCodecs with hardware
  preference, rebuilt on software preference after a stall, and — after a
  second stall, or from the start in a webview with no WebCodecs at all
  (Linux WebKitGTK) — the watch re-registers with `decode: true` and the
  **backend decodes natively** (`video_decode.rs`: openh264, one thread per
  route), queueing ready-to-paint RGBA frames freshest-wins that the canvas
  just blits. The tab bar shows one tab per video input the remote
  advertises — its screen, each extra monitor (`screen:<id>`), and its
  cameras. While control is on the stage captures pointer/key events,
  normalizes coordinates onto the streamed frame, and forwards them down
  the control route via `send_input`. Hovering a tab reveals a **pop-out**
  (`⧉`): the input lifts into its **own OS window**
  (`open_video_window` → `?video=cap:<capability id>` →
  `VideoPopoutHost.svelte`), which wires and *owns* its own route — so a
  camera can sit on a second monitor while the console stays on the
  screen — with its own hover controls (quality pills, a stream chip,
  fullscreen) and input forwarding whenever a live control route to that
  machine exists. Forwarding follows the KVM conventions people's hands
  know: it's live only over a *desktop* picture (pointer coordinates
  normalize onto the streamed frame, and only a screen's frame maps back
  onto the remote desktop — a camera feed is a pure viewing surface,
  console stage and room tiles included), and **the control surface under
  the mouse claims the OS focus** — keyboard events only ever reach the
  focused window, so hovering a popout is what aims your keys at the
  machine you're pointing at, no click in between (a click would go to
  the remote). The tab left behind holds a big **"Return video
  here"** (the stage shows it while that input is selected): one click
  asks the popout — found or not, fullscreen or not — to tear down and
  close, and the stage re-wires. Room tiles pop out the same way
  (`?video=share:<route id>`), except the popout only *watches* the
  sender's existing route — frame-watch claims replace each other by
  design (a route shows in one window), so the pixels simply move.
  Windows coordinate over a small local lane
  (`allmystuff://video-local`, the room-local pattern): popouts announce
  `opened`/`closed` and answer a `hello` census ping (consoles defer
  their first wire a beat for it, with both sides self-healing if the
  census loses the race), and `return-ask` is the Return button's
  message. Both the console stage and every room tile also grow a hover
  **fullscreen** control in the player corner (bottom-right): the video
  takes the whole window over (chrome hidden) and, in an OS window, the
  window goes fullscreen with it — Esc steps out unless control is
  live, where every key belongs to the far machine and the hover
  control is the way back. Beside it, over a desktop we're driving, is
  **relative mouse**: a remote fullscreen app or game captures the cursor
  on *its* machine — warping it to centre each frame and reading the delta
  — which absolute coordinates fight, since every move we send yanks the
  cursor back to where our pointer is. Capturing the pointer here streams
  `mouse_move_rel` deltas instead (the injector applies them
  `Coordinate::Rel`). It is deliberately its own switch rather than a
  consequence of fullscreen: whether the *viewer* is fullscreen has
  nothing to do with whether the far app captures its cursor, and tying
  the two meant the only way to drive such an app was to give up your own
  screen. Esc releases the lock and the mode drops with it, so one press
  hands the cursor back for good. The top bar's gear
  opens a unified **Settings panel** (`SettingsPanel.svelte`) with Networks,
  Fleet (the owned roster's shared key + members), and Updates (the
  `allmystuff-updater` controls). The **Networks** tab is itself split into
  sub-tabs: **Status** (identity, create/join, approvals,
  add-a-device), **Servers** (per-network signaling / STUN / TURN, defaulting
  to MyOwnMesh's reference servers), and **Devices** (every machine and which
  network(s) it's on). Multiple networks are first-class — you're joined to
  however many, a peer may share only some, and the graph, drawer, and top bar
  all show the per-device network membership rather than pretending it's one
  flat mesh. A device asking to join surfaces as an outlined, pulsing nudge
  that opens the **approvals popup** (`ApprovalsPopup.svelte`) — the bilateral
  code grid (each side's suffix + verification code) with Approve and Decline
  (a cancel, not a deny).

  **Virtual rooms** (`RoomsBar.svelte` + `RoomPanel.svelte`, bottom-left)
  are the multi-machine layer over the same plumbing: a room is a named,
  locally-persisted member list (invites ride `CHANNEL_ROOMS`), and joining
  opens a zoom-like call where **everything starts off** — mic, camera,
  screen share. On the desktop the call gets its **own OS window** per
  room (`open_room_window` → `?room=<id>` → `RoomHost.svelte`), movable
  and full-screenable exactly like the console / terminal / files
  windows — closing the window hangs up, and stopping being in the room
  closes the window; the web preview keeps the in-page overlay. The
  panel itself follows the conventions every call app trains people on:
  a dark stage with a tile per person present (a presence *echo* — a
  member already in the call answers a newcomer's `join` straight back —
  keeps late joiners' galleries truthful), screen shares letterboxed
  with the people on a filmstrip, a red-slashed mic/camera for the muted
  state, a green **"You're sharing your screen"** banner with the red
  stop right on it, a centred bottom control bar with Leave in red at
  the far right, chat and People panels on the right, a call timer, and
  `m` / `v` mute/camera keys. Room mechanics lean on the **person, not
  the machine**, wherever the thing isn't machine-specific: chat
  bylines, host labels, rosters and tiles lead with the fleet's owner
  name or a share's person ("Casey · desk"), with the machine dimmed
  alongside where it matters (whose *screen*, whose *sound*). A fresh
  room defaults to being named after the fleet's owner ("Casey's room");
  its maker is its owner and renames it inline from the panel title
  (members converge via the re-stated invite).

  Every room is **hosted by its maker** — a room of just your own node is
  fine (invite machines later from the panel). Hosting means the room's
  *identity and control plane* are the host's, not that anything flows
  through it: the id is minted under the host's canonical device id
  (`room:{host}:{nonce}`); the roster and name answer to the host alone
  (invites from anyone else are ignored — the mesh authenticates senders,
  so that's a real check); replacement rosters propagate removals (a
  member no longer listed drops the room, the fleet pattern); and the
  host deleting the room **closes it for everyone** (`close`), where a
  guest deleting merely forgets a local copy. An invite, once received,
  sits like a **roster slot**: the room stays listed and rejoinable on
  that device until the host removes it or closes the room. Rooms also
  admit from *outside* the roster: the maker picks **open or
  invite-only** at creation (flippable later from the panel), the room's
  id is the pasteable invite (Copy invite in the panel, "Join with an
  id" in the bar), and a pasted id **knocks** — an open room's host
  admits it automatically, an invite-only host gets an Admit / Deny ask
  in the People panel (`deny` so the asker isn't left waiting; a knock
  from a machine already on the roster just gets the invite re-stated,
  healing a lost one). Members still talk directly
  to each other for everything streamed — join/leave presence, chat, and
  the media routes — and rooms are **stream-only**: nothing is stored, and
  any future history/storage would live with the host. Being in several
  rooms at once is fine: join state, send toggles and the routes they own
  are all per-room, the panel just shows one room at a time, and
  minimizing it (as opposed to leaving) keeps everything live. The app's
  windows stay in step over a small local Tauri-event lane
  (`allmystuff://room-local`): a room window announces join/leave so the
  bar reads "you're in", the bar's Leave reaches whichever window holds
  the call, saved-list changes are reloaded everywhere, and an answered
  knock clears in every window.
  Each toggle fans ordinary routes out to the members — but **room sharing
  is scoped to the room**: membership is the consent, so room legs validate
  structurally via `propose_room_route` / `proposeRoomRoute` without the
  share-grant gate, **no standing grant is ever minted**, and leaving tears
  every room-wired leg down. What happens in a room changes nothing about
  what its members may do to each other outside it. (Input injection keeps
  its backend owner/fleet gate regardless — a guest's control events are
  still dropped.) The toggles: **Mic** is the call
  (your voice → their speakers); **Share sound** is deliberately separate —
  the machine's loopback (`system-audio`), never the mic; **Share screen**
  streams to each member's display, rendered in *their* panel as live tiles
  (`RoomTile.svelte`, the console pipeline's native-decode rung); **Share
  control** wires each member's keyboard & mouse to this machine so they
  can drive it over your tile — injection still gated by the far side's
  owner/fleet rule; file sending opens the existing files session
  (owner/fleet gated); chat is a `RoomMessage` to every member. The graph
  itself has two layouts switched from the zoom controls: the radial
  default and a **grouped grid** — one labelled section per fleet (yours,
  each owner's, "unknown fleet" for devices advertising no owner). The
  top bar's **network pill** opens a menu listing every network with an
  on/off switch — disable parks the network's full config in
  `allmystuff-networks.json` (under `~/.myownmesh`) and leaves it
  daemon-side; enable re-joins from the parked config; roster files
  survive on disk in between, so approvals aren't lost (the daemon has no
  dormant-network notion — `network_set_enabled` in the Tauri backend is
  what holds the ticket).

  New/joined networks default their signaling relay + STUN + TURN to
  MyOwnMesh's semi-public reference servers (`wss://myownmesh.com`,
  `stun:stun.myownmesh.com:3478`, `turn:turn.myownmesh.com:3478` with the
  shared `guest` credential) so two devices rendezvous on the *same* relay and
  traverse NAT out of the box — and any of them is editable per network. The
  backend learns which network each peer lives on (from the channel a frame
  arrives on) and addresses control/media there, so a connection follows a
  device onto whichever network you actually share with it.

## Data flow: connecting a device

1. User taps a capability's connect dot → `store.startCapConnect(capId)`.
2. User taps a target node → `store.connectCapToNode` finds the matching
   endpoint and calls `catalog.proposeRoute`.
3. If both ends are **mine**, a `Route` appears immediately — and the
   console that manages that kind of session pops for the far machine
   (`popConsoleFor`: remote control for screen/audio/control media, the
   file manager for storage), so sending something *to* a node hands you
   its session.
4. If a **shared** endpoint isn't covered, `requiredGrants` raises the share
   sheet ("Let Alex receive your screen?"). Approving adds exactly that grant
   and completes the connection. The **share builder** (`ShareFlow.svelte`)
   is the deliberate version of the same thing — one of *your* devices to a
   whole receiving fleet — and it composes in the units a person actually
   decides in: **Remote control** is one toggle that mints all four console
   grants (screen, sound, keyboard/mouse, clipboard), because nobody shares a
   display without input on purpose and offering them apart mostly produced a
   console that came up half-dead; **Folder** picks one folder rather than
   handing over the whole `:files` console. Underneath, each grant is still
   scoped to its own capability and revocable on its own.
5. With a live daemon, the backend sends a `RouteControl::Offer` to the peer
   over `CHANNEL_CONTROL`. The peer accepts; both sides go `Active`. For an
   audio route, the source captures what its capability names — the
   machine's own playback for the synthetic `system-audio` (WASAPI
   loopback on Windows, the pulse server's monitor of the default sink on
   Linux, the default input as macOS's honest stand-in), the default
   input for a scanned mic — and streams it
   to the sink — as Opus on **MyOwnMesh's RTP audio track lane** (48 kHz
   mono, 20 ms frames) when the offer asked for it and both daemons
   speak the lane (myownmesh ≥ 0.2.4 — the actually-bundled daemon pin is
   v0.3.3, see `.myownmesh-rev`), as PCM `AudioFrame`s over
   `CHANNEL_MEDIA` otherwise, so any version skew degrades to working
   sound exactly like video's MJPEG floor. The sink's playout ring aims
   ~80 ms behind the live edge and trims itself, so audio keeps step
   with the video stream. Both ends log which device they opened, a
   *mic* capture whose first seconds are pure zeros names the OS
   microphone permission (macOS/Windows deny with silence, not an error —
   a silent system capture is just a quiet desktop), and a playback
   that's never fed says so once. The console's audio passthrough is
   deliberately **listen-only** — the remote's `system-audio` to your
   speakers, nothing back. It's a console, not a call: the far side's
   loopback captures *everything* it plays, so any audio the console
   injected (a mic leg) would ride that loopback straight back and land
   one round trip later as a trailing echo. Until echo cancellation
   exists, the console simply never opens a microphone — wiring a mic
   somewhere stays a deliberate act on the graph. A display route
   streams the routed screen — the primary for the synthetic `screen`, the
   named monitor for a `screen:<id>` capability — from a persistent capture
   session (in-house DXGI Output Duplication on Windows — xcap's recorder
   duplicates every output it walks past and leaks the handle, so only the
   first session per process ever worked — and `xcap`'s recorder elsewhere:
   PipeWire ScreenCast / AVFoundation, retried once with a fresh monitor
   handle before settling for the paced per-frame grab that is the X11 path
   and universal fallback), unchanged frames skipped, a bounded queue
   dropping stale packets under backpressure. The *transport* is negotiated
   per route: the viewer always offers `h264` (decode is covered everywhere
   — see below); when the peer's lane is free — a lane whose holding route
   already ended is taken over, and the console serializes its tab switches
   so the teardown precedes the next offer — frames ride **MyOwnMesh's
   H.264 video track lane**: a hardware encoder where one is present —
   NVENC/QuickSync/AMD via **Media Foundation** on Windows (the GPU's own H.264
   MFT, no FFmpeg toolchain — `mediafoundation.rs`), NVENC/VA-API/QuickSync
   (Linux) and VideoToolbox (macOS) via the FFmpeg `hwenc` feature
   (`hwenc.rs`) — chosen by a frame-send-tested step-down ladder
   (`make_h264_codec`) that falls to software **openh264** as the floor —
   in screen-content mode at native resolution up to 4K (capture RGBA is
   converted straight to I420 in one fused pass — no RGB intermediate, no
   separate RGB→YUV walk), bitrate
   budgeted from the monitor's true pixel count (~0.16 bpp, 8–40 Mbps), real
   RTP on the wire. When the local daemon advertises the binary media pipes
   (Status `media_pipes` — a capability flag, since the feature predates a
   release and the `.myownmesh-rev` pin can't gate it), access units reach (and
   return from) it over a **dedicated binary media pipe** — `MediaTrackPipe`
   outbound, `MediaSourcePipe` inbound — with no base64 or per-frame JSON on the
   IPC and no 64 KiB ceiling. An older daemon on the socket keeps the legacy
   base64 `video_send`/`video_inbound` ops, so it still streams (just with the
   base64 tax) instead of a black screen. (MJPEG, PCM and route signalling keep
   the JSON control pipe regardless.)
   Otherwise they fall back to the v1 **MJPEG stream** over `CHANNEL_MEDIA`
   (1280 edge, chunked JPEGs), so any version skew degrades to working
   video. Either way the console window renders packets it *pulls* per
   display tick (raw bytes): WebCodecs decodes H.264 where the webview has
   it, `createImageBitmap` the JPEGs, and otherwise the backend's **native
   openh264 decoder** hands the window RGBA frames ready to blit — maximum
   frames and minimum latency don't depend on the webview's codec support.
   The console footer's **quality pills** (Resolution / FPS / Rate /
   Codec, each defaulting to Auto) ride `RouteControl::Tune` to the
   streaming side, which restarts its capture with the picks; the codec
   pill re-offers the route on the chosen transport and picks where H.264
   is decoded. A decoder that loses its place — a corrupt unit, a rebuilt
   WebCodecs instance, a dumped native-decoder backlog — sends
   `RouteControl::Refresh` and gets an IDR in ~one round trip instead of
   sitting out the periodic interval (both asks are rate-limited and
   silently dropped by older peers). Stream integrity itself is the
   daemon's job: myownmesh ≥ 0.2.2 reassembles access units
   sequence-aware, so packet loss or a late NACK retransmit costs one
   frame, never a corrupt unit in a decoder. (These are floor thresholds;
   the actually-bundled daemon pin is v0.3.3 — see `.myownmesh-rev`.)
   Set `ALLMYSTUFF_VIDEO_STATS=1` to print each stream's per-stage
   pipeline counters (fps, scale/encode/decode ms, bitrate, audio levels,
   skip/drop causes) every few seconds on both ends — quiet by default;
   `ALLMYSTUFF_VIDEO_FPS` / `ALLMYSTUFF_VIDEO_MAX_EDGE` /
   `ALLMYSTUFF_VIDEO_BITRATE` dial the H.264 stream without a rebuild. A
   **camera route** (`MediaKind::Video`) is the same stream with a lens
   behind it instead of a monitor: the source capability names a scanned
   camera, `camera_capture` opens it (nokhwa: V4L2 / AVFoundation / Media
   Foundation, asking for the device's smoothest rate and the largest
   picture at it — the 1080p30 / 720p60 webcam sweet spot — decoded to
   RGBA whatever the sensor speaks, MJPG included), and the frames enter
   the identical encoder/transport/tune/refresh pipeline — H.264 lane,
   MJPEG floor, quality pills, `vstat` reports (two camera-specific
   states: *no camera*, *camera failed* — held by another app, or an OS
   permission) and all. The landing spot is the peer's synthetic
   `video-in` sink (every machine running the app can *show* a camera, so
   the bridge advertises one per machine, ranked with the other
   whole-machine endpoints), and the watching window — a console's camera
   tab, a room tile — renders it through the very same pull-and-paint
   plane as a screen. Hosts advertise the `camera` feature tag; a console
   pointed at an older build's camera says "update that machine" instead
   of wiring a route that would never carry pixels (and an older *viewer*
   never wires one — its build has no `video-in` sink to land on). An input
   route carries `InputEvent`s the other direction: normalized mouse moves /
   buttons / wheel / key events — each move naming which remote screen
   it's normalized over, so control follows the console's selected tab —
   injected at the sink with `enigo` (plus a hand-raised
   `MOUSEEVENTF_VIRTUALDESK` move on Windows, where enigo's absolute
   coordinates can't reach past the primary monitor) — but only after the
   gate: the route must be live *and* the sender must be the device's
   recorded owner or a co-owned fleet member, so a route that merely
   auto-accepted can never type into your machine. Both ends assume key
   *combinations* are the norm: every key event carries the physical
   DOM `code` next to the layout-resolved `key`, plain typing injects
   the typist's character while a chord (Ctrl/Alt/Meta held) resolves
   through the physical key (Ctrl+C is the C key, not whatever the held
   modifiers composed), the sink remembers what each route pressed so a
   keyup releases exactly what its keydown pressed (Shift+1 goes down
   "!", comes up "1") and a dying route lifts everything it still holds,
   and the senders burst-release held keys whenever their keyups can no
   longer arrive (window blur, control toggled off, session close).
   A **clipboard route** (`MediaKind::Clipboard`, the synthetic per-machine
   `clipboard` endpoint) rides the same plane as a third console toggle
   next to audio and control, default-on in a session, and it **syncs** —
   you copy somewhere, you paste somewhere else, and it works. Opening the
   link pushes what's on the initiator's clipboard right then; from there
   both ends stay level whichever side does the copying, with no chord
   intercepted. Change detection is the platform's own
   (`clipboard-rs`'s `ClipboardWatcherContext` on its own thread), never a
   poll: a poll cheap enough to run often can't see an image change, and one
   that can costs a PNG encode every tick, so this costs nothing until
   someone copies. The **echo** is the whole difficulty — applying the peer's
   clipboard changes ours, which the OS reports exactly like a user copy, so
   forwarding it would send it straight back and one copy would ping-pong
   forever. Both the applying and the sending side stamp a fingerprint of
   what they handled (`Mesh::clip_synced`) and a change matching the stamp is
   our own doing; files fingerprint by path and size, so noticing a change
   never means reading every file on the clipboard. Acceptance is symmetric —
   an arriving clipboard is ordinary traffic on a live route, so a live
   clipboard route *is* the standing consent (it still has to be active, a
   clipboard route, and from its peer). **Text** rides one
   frame; an **image** or a set of **files** rides a chunked transfer (`open`
   manifest → base64 `chunk`s → `close`, split under the channel's ~64 KiB
   ceiling like a video frame), file bytes streaming from disk and landing in
   a per-transfer staging dir whose paths the receiver's OS clipboard is then
   pointed at — so a paste in a file manager materializes them. The OS-clipboard side
   (reading copied file references, handing files back for paste — formats
   the bundled clipboard-manager plugin can't touch) is `clipboard-rs`,
   driven on one dedicated thread (`clipboard.rs`) that owns the single
   clipboard context for the app's life, which is what keeps an X11
   selection served after the paste.

**A terminal session** is one more route on the same plumbing — and no sshd
anywhere. A node that can host shells advertises `"terminal"` in its
presence `features` (an additive field older peers ignore; they never show
the button). Opening a tab offers a **generic** route from the host's
virtual `…:terminal` endpoint to a viewer endpoint minted per tab — these
are deliberately *not* catalog capabilities (generic would match every
auto-wiring picker), so the graph and "Connected now" render them through a
display-only stand-in (`capabilityForDisplay`). Because a shell is exactly
as privileged as input injection, the same rule guards it **before
auto-accept can answer**: a terminal offer from a sender who isn't the
device's owner/fleet is `Reject`ed in the control handler without the
session ever seeing it, the spawn re-checks, and every inbound byte
re-checks. On accept the host spawns the user's shell in a real PTY
(`terminal.rs`, `portable-pty`: openpty on Unix, **ConPTY** on Windows;
`$SHELL -l` with fallbacks, `pwsh` → `powershell` → `cmd` on Windows) and
pumps output as `"term"`-tagged frames over `CHANNEL_MEDIA` (≤16 KiB
chunks, base64 like every media frame; unknown tags are dropped by older
peers, never errors). Flow control is end-to-end by construction: reader
thread → bounded channel → awaited sends — a slow viewer fills the queue,
blocks the reader, fills the kernel PTY buffer, and stalls the shell,
exactly like ssh. The viewer window *pulls* bytes with the video plane's
poke-then-pull watcher (`term_watch`/`term_poll` + `allmystuff://term-ready`
— the queue is created eagerly when the route activates, so the prompt that
races the window boot is buffered, not dropped) and feeds xterm.js;
keystrokes and resizes ride back as the same frames via `term_send`. The
shell's exit (a dedicated wait-thread — ConPTY readers don't EOF until the
master drops) reports `Exit { code }` to the viewer's overlay and tears the
route down; a host whose viewer vanished silently kills the session after
60 s of failed sends.

**A files session** rides the very same plumbing, request/response instead
of byte-stream. A node that can serve its disk advertises `"files"` in
presence `features`; opening the window offers a generic route from the
host's virtual `…:files` endpoint to a per-window viewer endpoint (display
stand-ins again — never catalog capabilities). Handing over a disk is as
privileged as a shell, so the identical owner/fleet gate screens the offer
before auto-accept, and every inbound request re-checks. Requests (`list`,
`read`, `write`, `mkdir`, `rename`, `delete`) and responses (`entries`,
`chunk`, `ok`, `err`) travel as `"file"`-tagged frames on `CHANNEL_MEDIA`,
each carrying a viewer-minted `req` id so a listing, a preview and an
upload never tangle. Host-side (`files.rs`) each op runs on its own
blocking thread feeding a bounded channel — a big `read` is throttled by
the mesh send, with a per-route cancel flag so teardown ends it mid-stream;
upload pieces are the one inline op (they must apply in arrival order, and
each is one small append). Viewer-side the window pulls frames with the
same poke-then-pull watcher (`file_watch`/`file_poll` +
`allmystuff://file-ready`) — except **downloads, which never cross the
webview**: `file_download` registers a backend sink first, the chunks
stream straight into the local Downloads folder (unique-ified names,
partials deleted on failure or teardown), and the window just renders
`allmystuff://file-progress` / `file-saved` events.

**A mapped drive** reuses those exact file frames but changes the authority
and root. Every machine advertises a synthetic `…:storage-in` sink; connecting
one scanned physical storage capability to it creates a `Storage` route. The
source binds that route id to the matching, currently mounted inventory volume
when it activates. Requests see a virtual `/` rooted there, never the host's
home or absolute paths; `..`, symlink escapes, and deleting/renaming the mapped
root are refused. The active route is the explicit lease, so the receiver may
browse and edit that one volume without gaining the whole-machine `:files`
permission. AllMyStuff exposes this in Settings → Drives, while CECSupport uses
the same route and frame contract in its Drive mapping card. No KVM participates.

**A shared folder** is that same lease made *durable*, and is what a share
hands over instead of the whole-machine `:files` console (which stays
owner/fleet-only). Sharing a folder mints an id for it in a durable registry
(`folders.rs`) and the grant pins to `<node>:folder:<id>`, so the file half of
a share is one named folder and nothing else on the disk.

The load-bearing rule is that **the path never crosses the wire**. Opening one
is `AppControl::MapFolder`, which names the id and nothing else; only the
source's own registry turns that back into a path, on the machine that owns
the disk. That is the difference from the mapped-drive pull it otherwise
mirrors: `MapDrive` carries a `root`, which is fine because it is owner/fleet
gated — naming a path on your own machine is no more than you could already do
— while a share reaches *outside* the fleet, where a receiver that could name a
root could name `/`. Ids are minted rather than derived from the path, so a
folder keeps its identity across a rename and its grants survive with it.

Authorization is its own gate (`sender_may_open_folder`), deliberately neither
of the neighbouring two: owner/fleet would refuse the very person the folder
was shared with, and the Files plane is the whole disk. It matches on the
**id** rather than the whole capability string, because the node prefix comes
in two forms depending on which side minted the grant. Both the grant and the
folder are re-checked at the offer, so unsharing the folder and revoking the
grant each close it independently, and a refusal is silent and identical either
way so nobody can probe which ids exist. The receiver picks its own mount
point; folder routes register no reconnect, since a folder is re-opened by id
rather than by a remembered root.

Minting happens on the machine that holds the folder — the builder shares any
device you own, so `AppControl::ShareFolder` asks that device to share it and
hand back the id (owner/fleet only; a path *there* travels inward, the owner
naming to their own device what to share). Browsing to find it needs nothing
new: `RemoteFolderPicker` already walks a device's disk over the ordinary files
plane.

## Persistent state

AllMyStuff rides on MyOwnMesh's identity + roster (under `~/.myownmesh/`,
overridable via `MYOWNMESH_HOME`). Its own additions — relationships, grants,
rooms, and saved routes — are app state layered on top; the mesh provides
the cryptographic identity that those grants attach to. Two of them are
durable today: rooms (each device keeps its own list, localStorage-side)
and **disabled networks** (`allmystuff-networks.json` — the parked configs
the network pill's off-switch holds). **Device ownership**
is already persisted there (`allmystuff-ownership.json`): the recorded owner
survives restarts, while claim mode is deliberately transient (re-asserted
each start by the flag) so a box never sits silently adoptable across reboots.
That same record now also holds the **owned fleet** — the shared key, the
fleet's display name, and the roster of co-owned devices — so a fleet survives
restarts and re-converges via gossip on the next start. Roster convergence is by version with *replacement*
on a strictly newer copy (that's how a **leave or kick** propagates — a union
could only ever add), equal versions union, and a newer roster that no longer
lists this device means it was kicked: the fleet drops locally and ownership
is released. Membership is the permission: the Fleet pane offers **Leave**
(and per-member **Kick**) only while this device is in the roster — you can't
kick devices from a fleet you aren't in. A **claim-status check** (sanitize
stale fleet residue → re-stamp the live profile → re-assert presence + roster)
runs at session start, after every claim/release/fleet change, and *targeted*
at each peer the moment its connection is approved — so two machines agree on
who owns what within a handshake, not a polling interval. Gossip is
**event-driven, with no heartbeat**: presence carries a per-run `boot` id, and
a peer seeing a boot it hasn't recorded (your app restarted while the daemon
link stayed up) answers with its own presence + roster directly — the mesh
carries traffic when something happens, never on a timer. The fleet roster
(it holds the grouping key) is only ever *handed* to fleet members; presence
goes to everyone.

## Next milestones

- **Per-device audio routing** — map a specific scanned device to a `cpal`
  device (audio still uses the default input/output; monitors are routed
  per-screen and cameras per-device now), and an audio codec (Opus) so
  the media channel isn't raw PCM.
- **Folder-share reconnect** — a mapped drive rebuilds itself when its source
  returns with a fresh incarnation, by re-sending the root it remembers. A
  shared folder has no root to remember (that's the point), so it is re-opened
  by id instead; making that automatic is the remaining half.

Done since this list was last true: **share-grant-gated control** (input
injection honours a shared person's explicit control grant — `may_drive`), and
**persisted relationships + grants** (`shares.rs`, durable beside ownership,
which is what lets a restart reclassify a peer as *shared* with its grants
rather than forgetting them).

Deliberately out of scope: embedding `myownmesh-core` at the source level —
AllMyStuff is a control-socket client by design, matching the rest of the
family.
