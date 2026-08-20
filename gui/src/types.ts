// TypeScript mirror of `allmystuff-graph`'s model + `allmystuff-protocol`'s
// presence shapes. Kept in sync by hand against the Rust source — a drift
// shows up as a decode error when the Tauri backend hands real data over.
// (Same discipline as the MyOwnMesh GUI's `types.ts`.)

export type MediaKind =
  | "audio"
  | "video"
  | "display"
  | "input"
  | "storage"
  | "clipboard"
  | "generic";
export type Flow = "source" | "sink" | "duplex";
export type GrantRole = "provide" | "consume" | "both";
export type NodeKind = "this" | "machine";

export interface Capability {
  id: string;
  node: string;
  label: string;
  media: MediaKind;
  flow: Flow;
  origin: string;
  /** `true` when this is the node's current default for its device category
   *  (the mic it captures from, the screen it drives first…). The drawer
   *  badges it and routing prefers it. Mirrors the Rust `Capability.default`;
   *  optional so older presence without the field still decodes. */
  default?: boolean;
}

export interface Person {
  id: string;
  name: string;
}

export interface Grant {
  id: string;
  media: MediaKind;
  role: GrantRole;
  capability?: string | null;
  label: string;
}

export interface Share {
  person: Person;
  grants: Grant[];
  /** Grants authored on this device: what this person may do with my stuff.
   *  Present on node snapshots from 0.2.89 onward; older nodes expose only
   *  the compatible `grants` union above. */
  out_grants?: Grant[];
  /** Grants authored by the other person: what I may do with their stuff. */
  in_grants?: Grant[];
}

/** NanoKVM JSON response envelope. HTTP success alone is not enough: `code`
 * is the appliance-level result. */
export interface KvmApiRsp<T = unknown> {
  code: number;
  msg?: string;
  data?: T;
}

export interface KvmVersion {
  current: string;
  latest?: string;
}

/** Result of a KVM API call made by Rust, outside the webview's CORS policy. */
export interface KvmApiCallResult {
  status: number;
  body: unknown;
  error: { kind: "timeout" | "connect" | "other"; message: string } | null;
}

export interface KvmWifiStatusRaw {
  supported?: boolean;
  apMode?: boolean;
  connected?: boolean;
  ssid?: string;
  wifi?: { ssid?: string } | null;
}

export interface KvmWifiStatus {
  supported: boolean;
  apMode: boolean;
  connected: boolean;
  ssid: string | null;
}

export interface KvmWifiNetwork {
  ssid: string;
  bssid?: string;
  signal?: number;
  security?: string;
  frequency?: number;
}

export interface HostWifi {
  supported: boolean;
  current: string | null;
  networks: KvmWifiNetwork[];
  note: string | null;
}

// Relationship is internally tagged on `kind`. `mine` = a device you own or
// manage; `shared` = someone you're connecting with for specific purposes;
// `unclaimed` = on your mesh but not yet classified (a GUI-only state for
// freshly-discovered peers — you claim it as mine or mark it shared). The
// Rust model only knows `mine`/`shared`; `unclaimed` never crosses the wire.
export type Relationship =
  | { kind: "mine" }
  | { kind: "unclaimed" }
  | ({ kind: "shared" } & Share);

/** A node's **standing** relative to you — the single, derived answer to
 *  "what is this device to me, right now?". Computed live from the
 *  authoritative reactive state (your fleet roster, the device's advertised
 *  owner + claimable flag, and any explicit share) so the graph, the drawer
 *  and every button read *one* coherent status instead of racing stored
 *  flags. Always recomputed from source — never stored. */
export interface Standing {
  /** This very device. */
  self: boolean;
  /** Running AllMyStuff (vs a bare mesh device with nothing to wire). */
  app: boolean;
  /** It's *yours* — a member of your fleet, or a device you own. */
  mine: boolean;
  /** A member of your fleet's signed roster. */
  inFleet: boolean;
  /** Its role in your fleet when in it: "owner" | "manager" | "member". */
  role: "owner" | "manager" | "member" | null;
  /** Whether *you* are the fleet owner — gates the role / evict controls. */
  iAmFleetOwner: boolean;
  /** It advertises *you* as its owner (claimed; roster may still be settling). */
  ownedByMe: boolean;
  /** It advertises someone else as its owner. */
  ownedByOther: boolean;
  /** It's offering itself for adoption *and* you can take it. */
  claimable: boolean;
  /** Raw "offering itself for adoption" flag (e.g. your own un-fleeted device). */
  offering: boolean;
  /** The person it's explicitly shared with, else null. */
  shared: Person | null;
  /** The single primary status, for the headline label / visual treatment. */
  kind: "self" | "mesh" | "shared" | "fleet" | "mine" | "claimable" | "theirs" | "free";
}

export interface InventorySummary {
  os: string;
  cpu: string;
  ram_bytes: number;
  device_count: number;
  /** Product / model name ("OptiPlex 7090", "MacBook Pro") — the machine's
   *  own identity, so a CEC technician can tell one customer's box from
   *  another. Optional: absent from adverts by an AllMyStuff older than the
   *  release that added it. */
  product?: string;
}

export interface MeshNode {
  id: string;
  label: string;
  /** The node's real machine hostname (from its scan / presence advert).
   *  When `label` is a user override that differs from this, the UI shows
   *  "label (hostname)" so the true machine is always visible. */
  hostname?: string;
  kind: NodeKind;
  relationship: Relationship;
  online: boolean;
  /** Hardware thumbnail for the node card (from the peer's presence advert,
   *  or this machine's own scan). Not part of the Rust `MeshNode` — the GUI
   *  carries it alongside for display. */
  summary?: InventorySummary;
  /** `true` once we know this node is *running AllMyStuff*, not just a device
   *  on the mesh — set from the reliable mesh capability marker
   *  (`CAP_TAG_ALLMYSTUFF`, off the daemon peer list) or its AllMyStuff
   *  presence advert. A node known only as a bare daemon device (no marker, no
   *  presence) has no wireable capabilities, so the graph shows it but makes
   *  it un-targetable and visually quieter. The local machine is always an
   *  app node. */
  app?: boolean;
  /** The node id that owns this device, from its presence advert. `owner`
   *  equal to the local node means the device says *you* own it. */
  owner?: string | null;
  /** `true` when the device is in claim mode and unowned — it's offering
   *  itself for adoption, the only state in which a claim takes. */
  claimable?: boolean;
  /** App features this node advertises in presence ("terminal", …).
   *  Absent (an older peer) means none — the matching buttons stay hidden. */
  features?: string[];
  /** Direct connectivity to this device keeps failing and no TURN relay is
   *  in play — the daemon's ICE watchdog verdict (its `needs_turn`, or its
   *  no-TURN diagnostic event). The card shows a "needs relay" chip so the
   *  block is visible instead of the device just being quiet. */
  needsTurn?: boolean;
  /** The sites this node exposes — TCP services it's willing to reverse-proxy
   *  over the mesh (from its presence advert). The owner curates this; absent
   *  means none. The Sites sidebar lists them per machine. */
  sites?: SiteAdvert[];
  /** KVM-appliance state, present only on a node that advertises
   *  `FEATURE_KVM` (a NanoKVM-class device) — mirrors the Rust
   *  `NodeProfile.kvm` (a `KvmAdvert`). `attachedTo` is the graph node this
   *  KVM physically controls (absent = not bound to anything yet); `web` is
   *  the `SiteAdvert.id` serving the KVM's own web UI (absent = the UI falls
   *  back to the first web-scheme site); `joiningMesh` is the per-device
   *  `cec-kvm-…` mesh the KVM returns to when unclaimed/reset (the same name
   *  it shows on its screen); `meshes` is every mesh it's currently joined
   *  to, fleet included — the list a fleet owner curates from the drawer.
   *  Absent on an ordinary node or an older peer. */
  kvm?: {
    attachedTo?: string;
    web?: string;
    joiningMesh?: string;
    meshes?: string[];
    virtualMedia?: { source: string; label: string; file: string };
  };
  /** The AllMyStuff version this node is running, from its presence advert
   *  (e.g. "0.1.11"). Absent from an older peer (or the in-browser demo) —
   *  the upgrade affordance only appears once we know both this and the
   *  channel's latest release. */
  version?: string;
  /** Friendly names of the networks this device has been seen on. You can be
   *  on several networks at once and a device may share only some of them, so
   *  the graph shows which — it's never just "the" mesh. */
  networks?: string[];
  /** The device's fleet display name ("Casey"), from its presence advert.
   *  Lets the graph group + label its fleet straight from the advert. Absent
   *  (an older peer, or not in a fleet) means unknown. */
  fleetName?: string;
  /** The device's fleet **owner** (person) name, from its presence advert —
   *  the human who owns the fleet, not the owner device's hostname. Used to
   *  label "Casey's fleet" without resolving the owner device from the
   *  catalog. Absent means unknown. */
  fleetOwner?: string;
}

/** The mesh capability tag that marks a node as a real AllMyStuff app node
 *  (mirrors the Rust `CAP_TAG_ALLMYSTUFF`). Carried on the reliable mesh
 *  capability advert / daemon peer list, so `app` flips on from the polled
 *  peer view even when a presence advert is dropped. */
export const CAP_TAG_ALLMYSTUFF = "allmystuff";

/** The presence feature tag for mesh-native terminal hosting (mirrors the
 *  Rust `FEATURE_TERMINAL`). */
export const FEATURE_TERMINAL = "terminal";

/** The presence feature tag for mesh-native file hosting — the "Open
 *  Files" console (mirrors the Rust `FEATURE_FILES`). */
export const FEATURE_FILES = "files";

/** The presence feature tag for camera *streaming* (mirrors the Rust
 *  `FEATURE_CAMERA`): the node's video routes have a capture backend
 *  behind them. Cameras have always ridden presence as capabilities;
 *  without this tag the machine advertising them predates the transport,
 *  and the console says so instead of waiting on pixels that never come. */
export const FEATURE_CAMERA = "camera";

/** The presence feature tag for the sites plane (mirrors the Rust
 *  `FEATURE_SITES`): the node can reverse-proxy a TCP service it's listening
 *  on over the mesh. A node without it (an older build) never advertises
 *  sites and is never offered a site route. */
export const FEATURE_SITES = "sites";

/** The presence feature tag for a **KVM appliance** (mirrors the Rust
 *  `FEATURE_KVM`): a NanoKVM-class device that captures a target machine's
 *  HDMI and injects USB-HID into it, carrying its own web UI as a
 *  `SiteAdvert`. A node with it gets the KVM drawer — "Open KVM", the
 *  Power/Reset feature buttons, and the attach/detach affordance — and its
 *  binding rides presence in `MeshNode.kvm`. Absent (an older peer) means an
 *  ordinary app node. */
export const FEATURE_KVM = "kvm";

// ---- sites (the reverse-proxy plane) ----------------------------------

/** One site a node exposes — a TCP service it reverse-proxies over the mesh
 *  (mirrors the Rust `SiteAdvert`). The advertised set is the host's
 *  allow-list: it only proxies a port that appears here. */
export interface SiteAdvert {
  /** Stable id (`tcp:8080`), mirroring the scan's `ListeningService.id`. */
  id: string;
  /** Friendly label — "HTTP", "PostgreSQL", "Port 8080". */
  label: string;
  port: number;
  /** URL scheme a client reaches it with ("http", "ssh", …) or "" for a bare
   *  TCP service. A web scheme is what lets the UI offer "open in browser". */
  scheme?: string;
  /** `true` when the host bound it to loopback only (the prime proxy case). */
  loopback?: boolean;
}

/** Whether a site is a web service the UI can "open in browser". */
export function siteIsWeb(s: { scheme?: string }): boolean {
  return s.scheme === "http" || s.scheme === "https";
}

/** One TCP service discovered on *this* machine (mirrors the Rust
 *  `ListeningService`) — what the Sites sidebar lists under "this machine"
 *  so the owner can choose which to expose. */
export interface ListeningService {
  id: string;
  name: string;
  port: number;
  kind: string;
  scheme: string;
  loopback: boolean;
  process: string;
  /** The page `<title>` the probe fetched (http sites), offered as the
   *  default name when exposing. Empty when there was none. */
  title: string;
}

/** One service a fleet machine reports when you manage its exposure remotely
 *  (mirrors the Rust `SiteService`) — the same shape as a `ListeningService`
 *  minus the local-only `kind`. */
export interface SiteService {
  id: string;
  name: string;
  port: number;
  scheme: string;
  loopback: boolean;
  process: string;
  title: string;
}

/** A site this machine has mapped to a local port — the live reverse-proxy
 *  binding. `url` is what to open (a `http(s)://localhost:<localPort>` for a
 *  web site, else the bare `localhost:<localPort>` address to point a client
 *  at). UI state, not on any wire. */
export interface SiteMapping {
  /** The node hosting the site (canonical/display id). */
  node: string;
  /** The site id (`tcp:8080`). */
  site: string;
  /** The host's port (what it listens on). */
  port: number;
  /** The local port this machine bound for the tunnel. */
  localPort: number;
  scheme: string;
  label: string;
}

/** One site *this* machine currently exposes to the fleet — the persisted
 *  exposure (id + chosen name) joined with whatever the live scan still
 *  knows about it. Unlike a raw `ListeningService`, an entry survives the
 *  underlying server going offline (`online` flips false); the Sites tab
 *  keeps showing it with a red dot and a Stop control, so an exposed port
 *  is never a phantom in the count. UI-only, not on any wire. */
export interface ExposedSite {
  /** Site id (`tcp:<port>`). */
  id: string;
  /** The name advertised to the fleet (the chosen name, or a derived default). */
  name: string;
  port: number;
  /** URL scheme from the live scan ("http", "ssh", …), "" when offline/unknown. */
  scheme: string;
  /** Whether the host bound it to loopback only — carried from the live scan. */
  loopback: boolean;
  /** The owning process, from the live scan; "" when offline/unknown. */
  process: string;
  /** `true` while the service is still listening (present in the live scan);
   *  `false` once it's gone — what drives the row's red "offline" dot. */
  online: boolean;
}

/** The TCP port encoded in a site id (`tcp:8080` → 8080), 0 if unparseable.
 *  Lets an exposed-but-offline site — gone from the scan, so only its id
 *  survives — still resolve a port to open / label by. */
export function sitePort(id: string): number {
  return Number.parseInt(id.slice(id.lastIndexOf(":") + 1), 10) || 0;
}

/** Whether a node is actually running AllMyStuff (vs. a bare mesh device).
 *  The local node and any node we've had presence from count as app nodes. */
export function isAppNode(n: { kind?: NodeKind; app?: boolean }): boolean {
  return n.kind === "this" || n.app === true;
}

export interface Route {
  id: string;
  from: string;
  to: string;
  media: MediaKind;
  /** Native receiver mount metadata for explicit Storage mappings. */
  drive?: { label: string; mount: string } | null;
}

export interface Catalog {
  nodes: MeshNode[];
  capabilities: Capability[];
  routes: Route[];
}

// ---- networks · identity · roster (mirror the daemon control shapes) --

/** This device's mesh identity (from `mesh_identity`). `label` is the
 *  user's display-name override; empty means "use the hostname". */
export interface IdentityInfo {
  device_id: string;
  pubkey?: string;
  label: string;
}

/** A network the daemon knows about (from `mesh_networks`). `config_id` is
 *  the stable local key for control ops; `network_id` is the shareable
 *  handle peers join with; `label` is an optional cosmetic name. */
export interface NetworkSummary {
  config_id: string;
  network_id: string;
  label: string;
  phase?: string;
}

/** The node-owned local claiming network (mirror of the Rust
 *  `allmystuff_protocol::LOCAL_CLAIM_NETWORK_ID` — FROZEN). Every AllMyStuff
 *  node joins this LAN-only mesh as the mDNS passthrough for claiming and
 *  local pairing; the UI shows it as on/off only (no venue, no invites, no
 *  leave). */
export const LOCAL_CLAIM_NETWORK_ID = "allmystuff-local-claim-v1";

/** Friendly name for a network: cosmetic label, else the joinable id, else
 *  the internal config id. */
export function networkDisplayName(n: {
  label?: string | null;
  network_id?: string;
  config_id?: string;
}): string {
  return n.label?.trim() || n.network_id?.trim() || n.config_id || "";
}

/** An approved member of a network's roster (from `mesh_roster_list`). */
export interface RosterPeer {
  device_id: string;
  label: string;
  approved_at?: number;
}

// ---- per-network transport config (signaling · STUN · TURN) -----------
//
// The daemon's NetworkConfig shape, round-tripped via `mesh_config_show` /
// `mesh_network_update`. Loosely typed — we preserve fields we don't edit.

export interface StunServerCfg {
  urls: string[];
}
export interface TurnServerCfg {
  urls: string[];
  username?: string | null;
  credential?: string | null;
}
export interface SignalingCfg {
  kind?: string;
  servers?: string[];
  redundancy?: number;
  denylist?: string[];
  public_fallback?: boolean;
}
/** A full network config as the daemon stores it. We only edit the server
 *  fields; everything else is preserved on round-trip. */
export interface NetworkConfigFull {
  id: string;
  network_id: string;
  label?: string | null;
  signaling?: SignalingCfg;
  stun_servers?: StunServerCfg[];
  turn_servers?: TurnServerCfg[];
  [key: string]: unknown;
}

/** A TURN entry as the Servers editor works with it (one url + optional
 *  creds), mapped to/from the daemon's `{ urls: [...] }` shape. */
export interface TurnEntry {
  url: string;
  username: string;
  credential: string;
}

/** A live peer on a network (from `mesh_peers`). We only surface the bits the
 *  approvals UI needs; `status === "pending_approval"` is the one to act on.
 *  The suffix + the two verification codes are the four cells of the approval
 *  code grid; `local_approve_sent` / `remote_approve_seen` drive the
 *  fresh / waiting-for-peer / confirm states (mirrors the daemon's PeerInfo). */
export interface PeerInfo {
  device_id: string;
  label: string;
  status: string;
  /** The daemon's ICE watchdog concluded direct connectivity to this peer
   *  keeps failing with zero relay candidates — the link needs a TURN
   *  server. Absent on older daemons. */
  needs_turn?: boolean;
  /** How far this peer's wall clock reads from ours (ms; positive = the
   *  peer is ahead), the daemon's passive heartbeat estimate. Absent on
   *  older daemons. */
  clock_skew_ms?: number | null;
  device_suffix?: string;
  verification_code_received?: string | null;
  verification_code_sent?: string | null;
  local_approve_sent?: boolean;
  remote_approve_seen?: boolean;
  /** The peer's mesh capability advert, exchanged in the handshake and
   *  carried in the peer list (reliable, unlike the bespoke presence advert).
   *  The `allmystuff` tag (CAP_TAG_ALLMYSTUFF) marks it as an app node; the
   *  remaining tags are its advertised features. `app_version` is its build. */
  capabilities?: {
    tags?: string[];
    app_version?: string | null;
    /** Embedder-defined data. The daemon's `CapabilityAdvert` is a typed struct
     *  that only forwards `tags`/`app_version`/`max_connections` and this opaque
     *  `extra` bag — anything app-specific must ride here or serde drops it.
     *  Carried on the reliable peer list, unlike the bespoke presence advert. */
    extra?: {
      /** The device summary (OS / CPU / RAM / device count). */
      summary?: InventorySummary | null;
      /** The wireable endpoints (control / audio / video / display sinks &
       *  sources) rooms and remote-control resolve a route through. */
      endpoints?: Capability[] | null;
    } | null;
  } | null;
}

// ---- owned fleet (the gossiped "Owned" roster) ------------------------

/** One device in your owned fleet — a machine you claimed, sharing the
 *  fleet's key. `device` is the canonical (bare-pubkey) id; the graph
 *  reconciles it to the right node. */
export interface OwnedMember {
  device: string;
  label: string;
  /** Governance role in the fleet's closed network: "member" | "controller"
   *  (a "manager") | "owner". Drives the drawer's grant/withdraw controls. */
  role?: "member" | "controller" | "owner";
}

/** The fleet roster (from `owned_roster` / the `allmystuff://owned` event):
 *  the shared key that links your co-owned devices and the members it links.
 *  The members are projected from the fleet's closed-network **signed
 *  roster** — authenticated membership, not gossip. An empty `key` means you
 *  haven't claimed anything yet. */
export interface OwnedRoster {
  key: string;
  /** The fleet's display name ("Casey") — cosmetic. Absent/empty = unnamed. */
  name?: string;
  version: number;
  members: OwnedMember[];
  /** Whether *this* device is the fleet owner (founder / key-holder). Only the
   *  owner can rename the fleet, grant/withdraw roles, or evict a device. */
  is_owner?: boolean;
  /** The fleet's closed-network id (the word-salad name). Lets the meshes list
   *  spot — and lock — the fleet mesh: you leave it by leaving the fleet. */
  network_id?: string;
  /** The single membership truth, computed by the backend: whether *this*
   *  device is in a fleet at all. True when it holds the key (founder or
   *  adopted member) **or** has been claimed (owned) — so an owned-but-keyless
   *  device, claimed but still awaiting its owner's key handoff, reads as in a
   *  fleet. Everything that asks "am I in a fleet" (the drawer, the settings
   *  pane, the leave control) reads this one flag so they can't disagree. */
  in_fleet?: boolean;
  /** Whether **this device** participates in claiming over the public mesh
   *  (claim-code rendezvous over remote signaling). Strictly device-local:
   *  set only on this machine, never synced from the fleet, never settable
   *  by a remote peer. Off (absent) = LAN-local claiming only — the default. */
  public_claims?: boolean;
  /** This device's formatted remote claim ID while it is both claimable and
   *  participating in public-mesh claiming. Local IPC only: it is never part
   *  of fleet governance or peer presence. */
  claim_code?: string | null;
  /** Governed topology (daemon ≥ 0.2.36): the owner-signed, network-wide
   *  shape the fleet mesh runs — `hubs` once infra hubs are designated,
   *  `full_mesh` after an explicit reset. Null/absent when ungoverned (the
   *  pre-hubs default) or when the local daemon predates governed topology.
   *  Every member's daemon converges onto this via the signed log; it is
   *  read-only here except through the owner's hub toggles. */
  topology?:
    | { kind: "hubs"; hubs: string[]; spoke_redundancy: number | null }
    | { kind: "full_mesh" }
    | { kind: "ring"; n_preferred: number | null }
    | { kind: "star"; hub: string }
    | null;
}

// ---- self-update (mirrors `allmystuff-updater`) -----------------------

export type InstallKind = "raw" | "package_manager";

/** Updater status (from `update_status`). */
export interface UpdateStatus {
  current_version: string;
  install_kind: InstallKind;
  enabled: boolean;
  /** "stable" | "beta". */
  channel: string;
  /** Auto-apply policy: "patch" | "minor" | "all" | "none". */
  auto_apply: string;
  check_interval_hours: number;
  last_check_at: number | null;
  staged_version: string | null;
  release_url: string;
  release_url_overridden: boolean;
}

export interface ComponentVersionRow {
  id: string;
  label: string;
  current: string | null;
  pinned: string | null;
  /** Optional on-disk version when it differs from the live process. */
  installed?: string | null;
  detail: string;
}

export interface ComponentStatus {
  rows: ComponentVersionRow[];
}

/** Result of a manual check (from `update_check`). Tagged on `outcome`. */
export interface CheckOutcome {
  outcome:
    | "disabled"
    | "package_manager"
    | "not_due"
    | "up_to_date"
    | "policy_blocked"
    | "staged"
    /** Newer release exists, but this install can't swap its own binaries. */
    | "manual_update_available";
  current?: string;
  latest?: string;
  policy?: string;
  version?: string;
}

/** The bits of updater config the UI can change (sent to `update_set_prefs`). */
export interface UpdatePrefs {
  enabled?: boolean;
  channel?: string;
  auto_apply?: string;
  check_interval_hours?: number;
  stable_url?: string;
  beta_url?: string;
}

// ---- console media (mirrors `allmystuff-session`'s media frames) ------

/** One keyboard/mouse event the console forwards down an input route.
 *  Tagged exactly like the Rust `InputAction` (serde `kind`, snake_case).
 *  Mouse coordinates are normalized 0..1 over the remote screen the
 *  console is showing; `screen` names which one (the `screen:<id>`
 *  capability's id), absent for the primary — so control follows the
 *  selected tab. Key events carry the physical `code` alongside the
 *  layout-resolved `key`: combinations resolve through it on the far
 *  side (Ctrl+C must land on the C key, whatever character the held
 *  modifiers composed here). */
export type InputAction =
  | { kind: "mouse_move"; x: number; y: number; screen?: number }
  | { kind: "mouse_move_rel"; dx: number; dy: number }
  | { kind: "mouse_button"; button: number; down: boolean }
  | { kind: "wheel"; dx: number; dy: number }
  | { kind: "key"; key: string; code?: string; down: boolean };

/** One terminal event a terminal window sends down its route. Tagged
 *  exactly like the Rust `TermEvent` (serde `kind`, snake_case); `bytes`
 *  is base64 (the wire is JSON). `exit` is the host's word only — it never
 *  travels viewer → host. */
export type TermEvent =
  | { kind: "data"; bytes: string }
  | { kind: "resize"; cols: number; rows: number }
  | { kind: "exit"; code?: number | null };

/** One open terminal session a host advertises for the multi-attach picker
 *  (mirrors the Rust `TerminalSessionInfo`): the `session_id` an attach
 *  Offer names, a friendly `title`, when it was created (unix seconds), and
 *  how many viewers are currently attached (`> 1` = already shared). */
export interface TerminalSessionInfo {
  session_id: string;
  title: string;
  created_unix: number;
  attachers: number;
}

/** One entry of a remote directory listing (mirrors the Rust `FileEntry`). */
export interface FileEntry {
  name: string;
  dir: boolean;
  size: number;
  modified?: number | null;
  symlink?: boolean;
}

export interface FileVolume {
  name: string;
  path: string;
  size: number;
  removable: boolean;
}

/** One event of a files route — the request/response conversation between
 *  the file-manager viewer and the host whose disk it browses. Tagged
 *  exactly like the Rust `FileEvent` (serde `kind`, snake_case); `data`
 *  is base64 (the wire is JSON). Every event carries the viewer-minted
 *  request id (`req`) it belongs to. */
export type FileEvent =
  | { kind: "volumes"; req: number }
  | { kind: "list"; req: number; path: string }
  | { kind: "read"; req: number; path: string }
  | { kind: "stat"; req: number; path: string }
  | { kind: "read_range"; req: number; path: string; offset: number; len: number }
  | { kind: "fetch"; req: number; token: string }
  | { kind: "write"; req: number; path: string; data: string; append?: boolean; eof?: boolean }
  | { kind: "write_range"; req: number; path: string; offset: number; data: string; truncate?: boolean }
  | { kind: "mkdir"; req: number; path: string }
  | { kind: "rename"; req: number; from: string; to: string }
  | { kind: "delete"; req: number; path: string }
  | { kind: "entries"; req: number; path: string; home: string; entries: FileEntry[] }
  | { kind: "volume_list"; req: number; volumes: FileVolume[] }
  | { kind: "metadata"; req: number; entry: FileEntry }
  | { kind: "chunk"; req: number; data: string; total: number; eof?: boolean }
  | { kind: "ok"; req: number }
  | { kind: "err"; req: number; reason: string };

/** A route's live negotiation state from the session snapshot — what a
 *  terminal tab watches to tell "connecting" from "rejected (reason)". */
export interface RouteLiveState {
  state: string;
  reason?: string;
}

/** One video packet of a display route, decoded off its IPC channel (a
 *  fixed binary header + the payload — see `watchVideo`). Either a
 *  standalone JPEG frame (the MJPEG transport) or one H.264 access unit
 *  from the mesh's track lane, for WebCodecs. */
export interface VideoFrameMsg {
  /** `raw` = RGBA the backend already decoded (paint it, no codec). */
  kind: "jpeg" | "h264" | "raw";
  /** H.264: this unit is an IDR — a safe decoder entry point. */
  key: boolean;
  /** JPEG/raw only — an H.264 unit carries its dimensions in the SPS. */
  width: number;
  height: number;
  sourceWidth: number;
  sourceHeight: number;
  /** JPEG: the frame's seq. H.264/raw: presentation timestamp in µs. */
  seq: number;
  /** The payload bytes — never base64. */
  data: Uint8Array<ArrayBuffer>;
}

// ---- virtual rooms ----------------------------------------------------
//
// A room is a lightweight, user-minted gathering of machines — a zoom-like
// call you join with everything off. Its membership + chat plane rides the
// `allmystuff/rooms/v1` channel (mirrors the Rust `RoomMessage`); the media
// itself (mic, screen, sound, control) is ordinary routes, proposed and
// authorized exactly like any other connection.

/** How a room admits a machine that knocks (asks to join with the room's
 *  id but no invite). Absent — an older host, an old save — reads as
 *  `invite`: never more open than the host meant. */
export type RoomAccess = "open" | "invite";

/** A room as the rooms bar lists it. `members` are canonical (bare-pubkey)
 *  node ids and include this machine. Once a room lands here — made here,
 *  or invited into — it stays like a roster slot: listed and rejoinable
 *  until the host removes this device or closes the room. */
export interface VirtualRoom {
  id: string;
  name: string;
  members: string[];
  /** Canonical id of the room's **host** — its maker. The room's
   *  identity is minted under this device (`room:{host}:{nonce}`), and
   *  its control plane (roster, name, closing it) answers to it alone.
   *  Absent on rooms minted before the field (or stubbed from a stray
   *  chat), which leaves those controls open to whoever holds the copy. */
  owner?: string;
  /** The host's knock policy, restated on every invite. */
  access?: RoomAccess;
}

/** One line of a room's chat (kept in memory for the session). */
export interface RoomChatLine {
  /** Canonical node id of the sender ("" for system notes). */
  from: string;
  /** Display name resolved at receive time, so lines survive a peer
   *  dropping off the graph. */
  fromLabel: string;
  text: string;
  at: number;
}

/** One file a member offers into a room's **Shared Files** area, as the
 *  uploader states it (mirrors the Rust `SharedFileMeta`). `token` is an
 *  opaque fetch handle — a downloader pulls the bytes straight from the
 *  uploader by token; they never pass through the host. */
export interface SharedFileMeta {
  token: string;
  name: string;
  size: number;
}

/** One entry of the host's aggregated Shared Files list — a file plus the
 *  uploader to fetch it from (mirrors the Rust `SharedEntry`). The host
 *  hosts the *list*; the uploader hosts the *bytes*, and only while it's
 *  online. */
export interface SharedEntry {
  /** Canonical node id of the uploader — whom to open the fetch route to. */
  from: string;
  token: string;
  name: string;
  size: number;
}

/** A wire message of the rooms plane (mirrors the Rust `RoomMessage` —
 *  tagged on `kind`, with the room id + name restated on every message).
 *  `invite` (the roster/name/access replacement), `close`, `deny` and
 *  `shares` (the host's authoritative Shared Files list) are the **host's**
 *  alone; receivers ignore them from anyone else. `knock` and `share_list`
 *  (a member telling the host what it's offering) travel the other way. */
export type RoomWireMessage = { room: string; name?: string } & (
  | { kind: "invite"; members: string[]; access?: RoomAccess }
  | { kind: "join" }
  | { kind: "leave" }
  | { kind: "chat"; text: string }
  | { kind: "close" }
  | { kind: "knock" }
  | { kind: "deny" }
  | { kind: "merge"; into: string; members: string[]; access?: RoomAccess }
  | { kind: "share_list"; files: SharedFileMeta[] }
  | { kind: "shares"; files: SharedEntry[] }
);

/** The presence feature tag for the rooms plane (mirrors the Rust
 *  `FEATURE_ROOMS`). A member without it (an older build) never sees the
 *  room's invites or chat — the room panel badges them. */
export const FEATURE_ROOMS = "rooms";

// ---- visual helpers ---------------------------------------------------

/** How a node's name reads on screen: the chosen name, with the real machine
 *  hostname in parens when the name is an override. Implements the naming
 *  rule — default is the hostname; an override shows as "Override (hostname)". */
export function displayName(n: { label: string; hostname?: string }): string {
  const host = n.hostname?.trim();
  if (host && host !== n.label) return `${n.label} (${host})`;
  return n.label;
}

export const MEDIA: Record<MediaKind, { label: string; color: string; icon: string }> = {
  audio: { label: "Audio", color: "var(--m-audio)", icon: "🎙" },
  video: { label: "Video", color: "var(--m-video)", icon: "🎬" },
  display: { label: "Screen", color: "var(--m-display)", icon: "🖥" },
  input: { label: "Controls", color: "var(--m-input)", icon: "⌨️" },
  storage: { label: "Files", color: "var(--m-storage)", icon: "🗂" },
  clipboard: { label: "Clipboard", color: "var(--m-clipboard)", icon: "📋" },
  generic: { label: "Data", color: "var(--m-data)", icon: "📦" },
};

export function mediaColor(m: MediaKind): string {
  return MEDIA[m].color;
}

/** A friendly glyph for a capability based on what kind of device it is. */
export function originIcon(origin: string, media: MediaKind): string {
  const map: Record<string, string> = {
    microphone: "🎙",
    speaker: "🔊",
    camera: "📷",
    display: "🖥",
    screen: "🪟",
    control: "🕹",
    controller: "⌨️",
    terminal: "📟",
    "terminal-view": "📟",
    files: "🗂",
    "files-view": "🗂",
    keyboard: "⌨️",
    mouse: "🖱",
    touchpad: "🖱",
    gamepad: "🎮",
    clipboard: "📋",
    storage: "🗂",
    system: "🔉",
    viewer: "📺",
    site: "🌐",
  };
  return map[origin] ?? MEDIA[media].icon;
}

/** A glyph for a site, by its scheme — a globe for the web, a plug for a
 *  bare TCP service, and recognisable marks for the common protocols. */
export function siteIcon(scheme: string | undefined): string {
  switch (scheme) {
    case "http":
    case "https":
      return "🌐";
    case "ssh":
      return "🔑";
    case "postgres":
    case "mysql":
    case "mongodb":
    case "redis":
      return "🗄";
    default:
      return "🔌";
  }
}

export function flowArrow(flow: Flow): string {
  return flow === "source" ? "→" : flow === "sink" ? "←" : "↔";
}

/** "out", "in", or "both" — plain words for the consumer UI. */
export function flowWord(flow: Flow): string {
  return flow === "source" ? "sends" : flow === "sink" ? "receives" : "both ways";
}

/** Compare two semver-ish versions the way the Rust `compare_semver` does:
 *  a numeric MAJOR.MINOR.PATCH compare, with a bare version outranking a
 *  pre-release of the same core and pre-releases ordered lexicographically.
 *  Returns -1 / 0 / 1 for a<b / a==b / a>b. Kept in lockstep with
 *  `allmystuff-updater`'s policy so the GUI's "is it behind?" answer matches
 *  the updater's. */
export function compareVersions(a: string, b: string): number {
  const split = (v: string): { core: number[]; pre: string } => {
    const dash = v.indexOf("-");
    const core = (dash >= 0 ? v.slice(0, dash) : v)
      .split(".")
      .slice(0, 3)
      .map((p) => Number.parseInt(p, 10) || 0);
    while (core.length < 3) core.push(0);
    return { core, pre: dash >= 0 ? v.slice(dash + 1) : "" };
  };
  const x = split(a);
  const y = split(b);
  for (let i = 0; i < 3; i += 1) {
    if (x.core[i] !== y.core[i]) return x.core[i] < y.core[i] ? -1 : 1;
  }
  if (x.pre === y.pre) return 0;
  // A bare version (no pre-release) outranks a pre-release of the same core.
  if (x.pre === "") return 1;
  if (y.pre === "") return -1;
  return x.pre < y.pre ? -1 : 1;
}

/** True when version `a` is strictly older than `b`. Empty/unknown versions
 *  are never "older" — there's nothing to compare. */
export function isOlderVersion(a: string | undefined, b: string | undefined): boolean {
  if (!a || !b) return false;
  return compareVersions(a, b) < 0;
}

export function humanBytes(bytes: number): string {
  if (!bytes) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB", "PB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}
