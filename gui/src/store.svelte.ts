// The single source of UI truth. Holds the graph catalog plus the
// transient interaction state (what's selected, what's being dragged,
// which sheet is open) as Svelte 5 runes, and exposes the verbs the
// components call. The connection rules live in `catalog.ts`; this layer
// is about *intent* and *feedback*.

import {
  canSink,
  canSource,
  capabilityForDisplay,
  matchEndpoint,
  proposeRoomRoute,
  proposeRoute,
  requiredGrants,
  type GrantRequest,
} from "./catalog";
import { demoCatalog } from "./mock";
import {
  exportNetworkSettings,
  networkAddPayloadFromEnvelope,
  tryParseNetworkSettings,
  venuesFromEnvelope,
} from "./network-settings";
import {
  loadNetworkVenues,
  loadVenues,
  newVenueId,
  PUBLIC_VENUE_ID,
  saveNetworkVenues,
  saveVenues,
  unionServers,
  type Venue,
} from "./venues";
import { fetchVenueServers } from "./venue-settings";
import * as cecApi from "./cec";
import type { CecSnapshot, CecHelpSession, CecPrivateLine } from "./cec";
import { canonicalNetworkId, generateNetworkPhrase } from "./network-phrase";
import {
  buildNetworkConfig,
  claimNode,
  clientLog,
  closeThisWindow,
  connectRoute,
  tuneRoute,
  type StreamTune,
  type VideoLocalEvent,
  consoleWindowTarget,
  disabledNetworks,
  exportNetworkFile,
  disconnectRoute,
  emitRoomLocal,
  emitVideoLocal,
  fileDownload,
  fileSend,
  fleetKick,
  fleetLeave,
  fleetSetName,
  isTauri,
  onFileProgress,
  onFileSaved,
  openFilesWindow,
  pickFilesToShare,
  roomShareFiles,
  roomSetSharePeers,
  roomUnshare,
  meshIdentity,
  meshIdentitySetLabel,
  meshConfigShow,
  meshNetworkAdd,
  meshNetworkRemove,
  meshNetworks,
  meshNetworkUpdate,
  meshPeers,
  meshRosterApprove,
  meshRosterList,
  meshRosterRemove,
  onOwned,
  onOwnership,
  onRoom,
  onRoomLocal,
  onSession,
  onSubscription,
  onVideoLocal,
  openConsoleWindow,
  openExternal,
  openRoomWindow,
  openTerminalWindow,
  openVideoWindow,
  ownedRoster,
  roomSend,
  siteScan,
  siteExposed,
  siteSetExposed,
  siteMap,
  siteUnmap,
  siteMappings,
  siteRemoteList,
  siteRemoteSetExposed,
  onNodeSites,
  type NodeSitesEvent,
  roomWindowTarget,
  terminalWindowTarget,
  filesWindowTarget,
  scanSelf,
  clipboardPaste,
  sendInput,
  sessionSnapshot,
  setClaimable,
  setNetworkEnabled,
  updateApply,
  updateCheck,
  updateLatestVersion,
  updateSetPrefs,
  updateStatus,
  upgradeNode,
  type SessionSnapshot,
} from "./tauri";
import {
  FEATURE_CAMERA,
  FEATURE_FILES,
  FEATURE_ROOMS,
  FEATURE_SITES,
  FEATURE_TERMINAL,
  isAppNode,
  isOlderVersion,
  networkDisplayName,
  siteIsWeb,
  type Capability,
  type Catalog,
  type CheckOutcome,
  type Grant,
  type IdentityInfo,
  type ListeningService,
  type SiteAdvert,
  type SiteMapping,
  type InputAction,
  type MediaKind,
  type MeshNode,
  type NetworkConfigFull,
  type NetworkSummary,
  type OwnedRoster,
  type PeerInfo,
  type Person,
  type Relationship,
  type RoomAccess,
  type RoomChatLine,
  type RoomWireMessage,
  type RosterPeer,
  type Route,
  type SharedEntry,
  type SharedFileMeta,
  type RouteLiveState,
  type VirtualRoom,
  type TurnEntry,
  type UpdatePrefs,
  type UpdateStatus,
} from "./types";

/** Which pane the settings panel is showing. */
export type SettingsTab = "networks" | "venues" | "account" | "updates" | "fleet" | "sharing";

/** Sub-pane within the Networks settings tab (MyOwnLLM-style sub-tabs). */
export type NetworksSubtab = "status" | "servers" | "devices";

/** A device waiting to be let onto a network — surfaced across *all* joined
 *  networks for the "new device joining" approval nudge. */
export interface PendingJoin {
  networkId: string;
  networkName: string;
  peer: PeerInfo;
}

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** A short, readable device id for labels when no friendly name is known. */
function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 10)}…` : id;
}

/** The stable machine identity inside a mesh device id: the bare pubkey,
 *  with MyOwnMesh's 5-char display suffix (`-AB12C`) stripped. The daemon's
 *  roster/peer list reports a device by its bare pubkey, while AllMyStuff
 *  presence and `IdentityShow` report the *display id* (`pubkey-SUFFIX`).
 *  Keying graph nodes by this canonical form collapses both views of one
 *  machine into a single node. Mirrors `myownmesh-core`'s
 *  `signing::pubkey_part` (suffix = 5 alphanumeric chars after the last `-`). */
function canonicalNodeId(id: string): string {
  const dash = id.lastIndexOf("-");
  if (dash > 0) {
    const suffix = id.slice(dash + 1);
    if (suffix.length === 5 && /^[0-9a-zA-Z]+$/.test(suffix)) return id.slice(0, dash);
  }
  return id;
}

/** Whether two mesh ids name the same machine (same pubkey, any suffix). */
function sameMachine(a: string, b: string): boolean {
  return canonicalNodeId(a) === canonicalNodeId(b);
}

let seq = 0;
const newId = (p: string) => `${p}:${Date.now().toString(36)}:${seq++}`;

/** localStorage key for this device's rooms list. */
const ROOMS_STORE_KEY = "allmystuff.rooms.v1";

/** Daemon peer statuses that mean "reachable right now". `active` is the
 *  obvious one; `shelved` is the topology selector parking a healthy link
 *  to bound connection count — its data channel stays open (heartbeats,
 *  routes and consoles all still ride it), so painting the machine
 *  offline would be wrong. Everything else is either transient
 *  (`sighted` / `handshaking` / `reconnecting`) or terminal
 *  (`offline` / `error`). */
const CONNECTED_STATUSES = new Set(["active", "shelved"]);

/** How long a machine that was just connected keeps reading online while
 *  its daemon status dips through a transient state (an ICE restart, a
 *  re-handshake after a network blip) or it briefly vanishes from the
 *  peer list (its daemon restarting). Five graph polls' worth: long
 *  enough to swallow every routine transport rebuild, short enough that
 *  a genuinely gone machine isn't painted reachable for long. An
 *  explicit `offline`/`error` status skips the grace entirely. */
const PRESENCE_GRACE_MS = 15_000;

/** The send channels a room can have live, each owning the routes its
 *  toggle created. `mic` is the call (your voice); `sound` is the
 *  machine's loopback — kept strictly apart on purpose. */
const ROOM_CHANNELS = ["mic", "cam", "screen", "sound", "control"] as const;
type RoomChannel = (typeof ROOM_CHANNELS)[number];

function emptyRoomRoutes(): Record<RoomChannel, string[]> {
  return { mic: [], cam: [], screen: [], sound: [], control: [] };
}

/** One joined room's send toggles — everything off until flipped. */
interface RoomSendState {
  mic: boolean;
  cam: boolean;
  screen: boolean;
  sound: boolean;
  control: boolean;
}

const ROOM_SEND_OFF: RoomSendState = {
  mic: false,
  cam: false,
  screen: false,
  sound: false,
  control: false,
};

/** One Shared Files download as the room panel shows it. */
interface SharedDownload {
  token: string;
  name: string;
  /** Bytes written so far / the file's full size (for the progress bar). */
  done: number;
  total: number;
  state: "fetching" | "done" | "error";
  /** Where it landed (on `done`) or the host's reason (on `error`). */
  note: string;
}

/** Mint a room id under its host's canonical device id — the identity
 *  itself says whose room it is. */
function newRoomId(host: string): string {
  return `room:${host}:${Math.random().toString(36).slice(2, 10)}`;
}

/** The host a room id is anchored under (`room:{host}:{nonce}`), if it
 *  parses. A fallback only — a room's recorded `owner` always wins (older
 *  ids carry a timestamp where the host now goes). */
function roomHostFromId(id: string): string | null {
  const m = /^room:([^:]+):[^:]+$/.exec(id);
  return m ? m[1] : null;
}

export interface Toast {
  id: number;
  kind: "ok" | "info" | "warn";
  text: string;
}

/** A connection the user started but that needs a permission grant first.
 *  Drives the "Let Alex…?" share sheet. */
export interface PendingShare {
  from: string;
  to: string;
  fromLabel: string;
  toLabel: string;
  requests: GrantRequest[];
}

/** One person/fleet you're sharing with, gathered for the Sharing settings
 *  pane: who they are, which of their nodes you know, and every grant
 *  you've given them (with the node it's recorded on, for revocation). */
export interface SharePartner {
  person: Person;
  nodes: MeshNode[];
  grants: { node: MeshNode; grant: Grant }[];
}

/** A graph with nothing in it — the starting point under the real backend,
 *  where every node + capability comes from the live scan and mesh presence
 *  (no demo stand-ins). */
function emptyCatalog(): Catalog {
  return { nodes: [], capabilities: [], routes: [] };
}

class AppStore {
  // Under the real app the graph is built entirely from the live scan + mesh
  // presence, so it starts empty and fills with *your* stuff. The demo
  // catalog is only a stand-in for the browser/preview build (no Tauri
  // backend) so the marketing page is never blank.
  catalog = $state<Catalog>(isTauri() ? emptyCatalog() : demoCatalog());

  // ---- interaction state ------------------------------------------
  selectedNodeId = $state<string | null>(null);
  /** Capability the user is dragging a wire from, if any. */
  dragFrom = $state<string | null>(null);
  pendingShare = $state<PendingShare | null>(null);
  /** The unified Settings panel (networks · updates · fleet) and which pane
   *  it's showing. The top-bar gear opens it; the Networks button deep-links
   *  to the networks pane. */
  settingsOpen = $state(false);
  settingsTab = $state<SettingsTab>("networks");
  /** The "a new device wants to join" approval popup (the code-grid nudge). */
  approvalsOpen = $state(false);
  /** The "claim a device" sheet — the forefront adoption surface, opened from
   *  the top-bar nudge or a device's drawer. Claiming is the step right after
   *  joining a network, so it gets the same prominence the join nudge has. */
  claimOpen = $state(false);
  manageShareNodeId = $state<string | null>(null);
  toasts = $state<Toast[]>([]);
  backendConnected = $state(false);

  // ---- remote console (the pikvm-style session popup) -------------
  /** The remote machine a console session is open on, if any. */
  consoleNodeId = $state<string | null>(null);
  /** The video input (a remote display/camera source) currently selected in
   *  the console's input tab bar. */
  consoleInput = $state<string | null>(null);
  /** Whether audio passthrough is on for the session. */
  consoleAudio = $state(false);
  /** Whether keyboard & mouse control is being sent to the remote. */
  consoleControl = $state(false);
  /** Whether clipboard passthrough is on — when it is, a paste in the
   *  console pushes this machine's clipboard to the remote first, so the
   *  paste lands our content. */
  consoleClipboard = $state(false);
  // Route ids the console owns, by channel, so it tears down exactly what it
  // set up (and nothing a different connection made).
  private consoleVideoRouteId: string | null = null;
  private consoleAudioRouteId: string | null = null;
  private consoleControlRouteId: string | null = null;
  private consoleClipboardRouteId: string | null = null;
  /** The *live* display route the console renders frames for — also set when
   *  the route pre-existed (owned-for-teardown is tracked separately). */
  consoleVideoLive = $state<string | null>(null);
  /** The console's codec pill: which transport to *offer* for its video
   *  route. "auto" and "h264" both offer H.264 (auto lets the decode
   *  ladder pick where it's decoded); "mjpeg" forces the fallback. */
  consoleCodec = $state<"auto" | "h264" | "mjpeg">("auto");
  /** The console's quality pills — absent fields are Automatic. Sent to
   *  the streaming side, which restarts its capture with them. */
  consoleTune = $state<StreamTune>({});
  /** The live outbound control route console input events ride on. */
  consoleControlLive = $state<string | null>(null);
  /** The live outbound clipboard route a paste pushes our clipboard down. */
  consoleClipboardLive = $state<string | null>(null);

  // ---- video popouts (one stream in its own OS window) --------------
  /** Streams currently held in their own popout window, by key
   *  (`cap:<capability id>` for a console input, `share:<route id>` for a
   *  room share). Synced across this app's windows over the video-local
   *  lane — popouts announce `opened`/`closed`, and answer a `hello` ping
   *  so a console/room window that opens later still learns of them. The
   *  tab/tile for a popped stream shows "Return video here" instead of
   *  the video. */
  poppedVideos = $state<Record<string, true>>({});
  /** When *this window* is a popout: its key (set by the popout host). */
  videoPopoutKey = $state<string | null>(null);
  /** The live route the popout renders — wired and owned by the popout
   *  for a `cap:` key (torn down on close), merely watched for `share:`. */
  videoPopoutLive = $state<string | null>(null);
  /** Owned-for-teardown route id (`cap:` popouts that created theirs). */
  private videoPopoutRouteId: string | null = null;
  /** Bumped when this popout should re-assert its frame watch — a console
   *  window booting may have briefly claimed the same route's watch slot
   *  before the popout census told it to back off (watch claims replace
   *  each other by design: a route shows in one window). */
  videoPopoutRewatch = $state(0);

  // ---- terminal (the mesh-native shell) ----------------------------
  /** The remote machine the in-page terminal popover is open on (web
   *  preview only — the desktop opens a dedicated window per machine). */
  terminalNodeId = $state<string | null>(null);
  /** Live negotiation state per route id, straight from the last session
   *  snapshot. A terminal tab watches its own route here to tell
   *  "connecting" from "active" from "rejected (reason)" / "torn_down". */
  routeStates = $state<Record<string, RouteLiveState>>({});
  /** Per-app-run counter so each terminal tab mints a unique viewer-side
   *  endpoint (`{me}:term-view:…`) — unique endpoint, unique route id. */
  private termViewSeq = 0;

  // ---- files (the mesh-native file manager) -------------------------
  /** The remote machine the in-page files popover is open on (web preview
   *  only — the desktop opens a dedicated window per machine). */
  filesNodeId = $state<string | null>(null);
  /** Per-app-run counter so each files session mints a unique viewer-side
   *  endpoint (`{me}:files-view:…`) — unique endpoint, unique route id. */
  private filesViewSeq = 0;

  // ---- virtual rooms ------------------------------------------------
  /** Every room this device knows — created here or invited into. Members
   *  are canonical node ids (this machine included). Persisted locally. */
  rooms = $state<VirtualRoom[]>([]);
  /** The room whose call panel is open (you've "joined"), if any. */
  roomOpenId = $state<string | null>(null);
  /** Chat lines per room id (session memory — history isn't synced). */
  roomChat = $state<Record<string, RoomChatLine[]>>({});
  /** Unread chat per room id, for the rooms bar badge. */
  roomUnread = $state<Record<string, number>>({});
  /** Canonical ids currently *in* each room (they broadcast a join). */
  roomPresence = $state<Record<string, string[]>>({});
  /** Whether the open room's chat sidebar is showing. */
  roomChatOpen = $state(false);
  /** Whether the open room's participants sidebar is showing. */
  roomPeopleOpen = $state(false);
  /** Whether the open room's Shared Files sidebar is showing. */
  roomFilesOpen = $state(false);
  /** The "make a room" composer in the rooms bar. */
  roomDraftOpen = $state(false);
  /** Rooms this device is currently *in* — being in several at once is
   *  fine; the panel (`roomOpenId`) just shows one at a time, and closing
   *  the panel doesn't hang up. */
  joinedRoomIds = $state<string[]>([]);
  /** Rooms joined by *another window* of this app (the dedicated room
   *  windows announce join/leave on the local bus) — so the rooms bar
   *  reads "you're in" no matter which window holds the call. */
  roomsJoinedElsewhere = $state<string[]>([]);
  /** When each joined room was joined (ms epoch) — the call timer. */
  roomJoinedAt = $state<Record<string, number>>({});
  /** Pending knocks per room this device hosts: machines asking to join
   *  an invite-only room, waiting for admit/deny in the room panel. */
  roomKnocks = $state<Record<string, { from: string; label: string; at: number }[]>>({});
  /** Room ids this device knocked on (join-by-id), awaiting the host's
   *  answer — an arriving invite for one of these auto-joins. */
  pendingKnocks = $state<string[]>([]);
  /** Per-joined-room send toggles. A room joins like a muted call:
   *  nothing is wired until a toggle is deliberately turned on — and each
   *  room's toggles are its own (mic live in one room stays live while
   *  you look at another). */
  roomSend = $state<Record<string, RoomSendState>>({});
  /** Route ids each room's toggles created (keyed by room id, then
   *  channel), so leaving a room tears down exactly what *that room*
   *  wired (never a route the user made on the graph, never another
   *  room's legs). */
  private roomRoutes: Record<string, Record<RoomChannel, string[]>> = {};
  /** Files *this* device is offering into each room's Shared Files area
   *  (the uploader's own list). Cleared when we leave; not persisted —
   *  shares are stream-only, like everything else in a room. */
  roomMyShares = $state<Record<string, SharedFileMeta[]>>({});
  /** The **host** side of the Shared Files list: for a room we host, every
   *  member's current offerings (room id → uploader node id → files). We
   *  aggregate these and restate the whole as the room's `shares` list. */
  private roomHostShares: Record<string, Record<string, SharedFileMeta[]>> = {};
  /** The Shared Files list as a **non-host** member received it from the
   *  room's host (room id → entries). The host is the catalog; we just
   *  render and download from this. */
  roomSharesFromHost = $state<Record<string, SharedEntry[]>>({});
  /** In-flight / finished shared-file downloads, keyed by fetch token: what
   *  the panel shows as a progress bar and "Saved to …". */
  sharedDownloads = $state<Record<string, SharedDownload>>({});
  /** `"<routeId>:<req>"` → fetch token, so a backend `file-saved` /
   *  `file-progress` (which name the route + req) updates the right row. */
  private sharedReqToken: Record<string, string> = {};
  private sharedViewSeq = 0;
  private sharedReqSeq = 1;

  // ---- sites (the reverse-proxy plane) ------------------------------
  /** Which sidebar tab is showing — the rooms/sites bar is one tabbed
   *  panel now. */
  sidebarTab = $state<"rooms" | "sites">("rooms");
  /** This machine's discovered listening TCP services (the full set), so
   *  the Sites tab can list them under "this machine" with expose toggles.
   *  Seeded with demo data in web mode, replaced by a real scan under the
   *  backend. */
  myListening = $state<ListeningService[]>([]);
  /** *This* machine's services currently advertised to the mesh, as
   *  id → display name (empty = the classified default). Mirrors the
   *  backend's persisted set; the local node's advertised `sites` follow it. */
  exposedSites = $state<Record<string, string>>({});
  /** Sites this device has mapped to a local port — the live reverse-proxy
   *  bindings, keyed for lookup by `"<node>::<site>"`. */
  siteMappings = $state<SiteMapping[]>([]);
  /** A fleet machine's full site list + exposed map, fetched on demand when
   *  you open its drawer to manage exposure remotely. Keyed by canonical
   *  node id; filled by the `allmystuff://node-sites` reply. */
  remoteSites = $state<Record<string, { services: ListeningService[]; exposed: Record<string, string> }>>({});

  /** This window's id on the same-device room bus — local events echo to
   *  every window (the sender included), and this is how we drop ours. */
  private readonly windowToken = `w_${Math.random().toString(36).slice(2, 10)}`;
  /** Whether this store runs in the app's main window. Every window's
   *  store hears every mesh event, so host-side *decisions* that answer
   *  one (admitting a knock) run only here — once, not once per window. */
  private readonly isMainWindow =
    !consoleWindowTarget() && !terminalWindowTarget() && !filesWindowTarget() && !roomWindowTarget();

  /** The local machine's node id. `"this"` in demo/web mode; the real mesh
   *  device id once the backend session is up. The graph centres on it. */
  localId = $state("this");

  // ---- networks / identity / roster (live mesh control) -----------
  /** This device's mesh identity. `label` is the display-name override. */
  identity = $state<IdentityInfo | null>(null);
  networks = $state<NetworkSummary[]>([]);
  /** config_id of the network the live session is currently on. */
  sessionNetwork = $state<string | null>(null);
  /** Which sub-pane of the Networks settings tab is showing. */
  networksSubtab = $state<NetworksSubtab>("status");
  /** Full per-network configs (signaling/STUN/TURN) from the daemon — the
   *  Servers pane reads + round-trips these. Keyed implicitly by `id`. */
  networkConfigs = $state<NetworkConfigFull[]>([]);
  /** config_id currently selected in the Servers/Venue pane. */
  serversNetwork = $state<string | null>(null);
  /** All venues — the named signaling/STUN/TURN sets a mesh "calls out" at,
   *  built-in Public first. App-side only (localStorage); compiled into each
   *  mesh's per-network config as the union of its venues. */
  venues = $state<Venue[]>(loadVenues());
  /** network_id → the venue ids that mesh uses (effective servers = union). */
  networkVenues = $state<Record<string, string[]>>(loadNetworkVenues());
  /** The venue open in the Venues tab editor (null = list view). */
  venueDraft = $state<Venue | null>(null);
  /** Networks switched *off* but kept — their full parked configs. The
   *  pill menu lists these under the live ones; enabling re-joins. */
  disabledNets = $state<NetworkConfigFull[]>([]);
  /** The network pill's dropdown (enable/disable without deleting). */
  netMenuOpen = $state(false);
  /** The network whose roster/approvals the Networks panel is showing. */
  rosterNetwork = $state<string | null>(null);
  roster = $state<RosterPeer[]>([]);
  livePeers = $state<PeerInfo[]>([]);
  /** Devices waiting to join, gathered across *every* joined network — what
   *  the approval nudge + popup act on. Refreshed by the mesh poll. */
  pendingJoins = $state<PendingJoin[]>([]);
  /** device ids the user declined in the popup this session. Declining is a
   *  cancel, not a deny: the device stays listed under Settings → Networks so
   *  it can still be approved later; it just stops nagging from the nudge. */
  dismissedJoins = $state<string[]>([]);

  // ---- owned fleet (the gossiped "Owned" roster) ------------------
  /** The shared key + members linking the devices you've claimed. */
  ownedFleet = $state<OwnedRoster | null>(null);

  /** The fleet's display name ("Casey"), empty when unnamed. */
  fleetName = $derived.by(() => this.ownedFleet?.name?.trim() ?? "");

  /** Name (or rename) the fleet — members only (the backend enforces it;
   *  the demo mirrors the rule). The renamed roster gossips out and every
   *  member converges, exactly like a kick. */
  async setFleetName(name: string) {
    const clean = name.trim();
    if (this.backendConnected) {
      try {
        await fleetSetName(clean);
        this.toast("ok", clean ? `Fleet named “${clean}”` : "Fleet name cleared");
      } catch (e) {
        this.toast("warn", `Couldn't name the fleet: ${String(e)}`);
      }
      return;
    }
    // Demo/web: apply the same membership rule locally.
    if (!this.ownedFleet || !this.isFleetMember(this.localId)) {
      this.toast("warn", "You can't name a fleet you aren't in");
      return;
    }
    if ((this.ownedFleet.name ?? "") !== clean) {
      this.ownedFleet = {
        ...this.ownedFleet,
        name: clean,
        version: this.ownedFleet.version + 1,
      };
    }
    this.toast("ok", clean ? `Fleet named “${clean}” (demo)` : "Fleet name cleared (demo)");
  }

  // ---- self-update -------------------------------------------------
  updateInfo = $state<UpdateStatus | null>(null);
  updateBusy = $state(false);
  /** Result of the last manual "check now", for the Updates pane. */
  updateOutcome = $state<CheckOutcome | null>(null);
  /** The channel's latest release version, learned once (read-only) so the
   *  drawer can tell which of your fleet machines are behind it. Null until
   *  loaded; stays null in web mode / if the feed can't be reached. */
  latestRelease = $state<string | null>(null);
  private latestReleaseLoading = false;

  /** Safety-net poll that keeps the graph's mesh members fresh. */
  private meshPoll: ReturnType<typeof setInterval> | null = null;

  /** When each machine (canonical pubkey) was last seen in a connected
   *  daemon status — the memory behind [`PRESENCE_GRACE_MS`], so the
   *  graph holds a recently-live node online through transport blips
   *  instead of flapping it offline on every 3 s poll that lands
   *  mid-rebuild. */
  private lastConnectedAt = new Map<string, number>();

  // ---- derived -----------------------------------------------------
  selectedNode = $derived(
    this.selectedNodeId ? this.catalog.nodes.find((n) => n.id === this.selectedNodeId) ?? null : null,
  );

  /** The machine a console session is currently open on, if any. */
  consoleNode = $derived(
    this.consoleNodeId ? this.catalog.nodes.find((n) => n.id === this.consoleNodeId) ?? null : null,
  );

  /** Routes running between this machine and the console's remote — the
   *  live session the console's footer chips show. */
  consoleSessionRoutes = $derived.by(() => {
    const remote = this.consoleNodeId;
    if (!remote) return [] as Route[];
    return this.catalog.routes.filter((r) => {
      const f = this.capability(r.from);
      const t = this.capability(r.to);
      if (!f || !t) return false;
      const ends = [f.node, t.node];
      return ends.includes(remote) && ends.includes(this.localId);
    });
  });

  mineCount = $derived(this.catalog.nodes.filter((n) => n.relationship.kind === "mine").length);
  sharedCount = $derived(this.catalog.nodes.filter((n) => n.relationship.kind === "shared").length);

  /** The network the live session is on (or the first one configured).
   *  Guarded against a non-array `networks` — this derived renders in the top
   *  bar every frame, so a bad backend shape here would wedge the whole UI. */
  activeNetwork = $derived.by(() => {
    const nets = Array.isArray(this.networks) ? this.networks : [];
    return nets.find((n) => n.config_id === this.sessionNetwork) ?? nets[0] ?? null;
  });
  /** Devices waiting to be let onto the roster network. */
  pendingPeers = $derived(
    (Array.isArray(this.livePeers) ? this.livePeers : []).filter(
      (p) => p.status === "pending_approval",
    ),
  );

  /** Pending joins (across all networks) the user hasn't declined this
   *  session — what the top-bar nudge counts and the popup shows. */
  freshJoins = $derived(
    this.pendingJoins.filter((j) => !this.dismissedJoins.includes(canonicalNodeId(j.peer.device_id))),
  );

  /** Devices offering themselves for adoption that you can actually take —
   *  running AllMyStuff, still unclaimed, in claim mode, and not already owned
   *  by someone else. The mirror of `freshJoins` for the claim step: what the
   *  top-bar "ready to claim" nudge counts and the Claim sheet lists. */
  claimables = $derived(
    this.catalog.nodes.filter(
      (n) =>
        isAppNode(n) &&
        n.relationship.kind === "unclaimed" &&
        n.claimable === true &&
        !(n.owner && !this.isMe(n.owner)),
    ),
  );

  /** Canonical pubkeys of every device in your owned fleet (you included), so
   *  the graph/drawer can badge co-owned machines as one group. */
  fleetMemberIds = $derived.by(() => {
    const set = new Set<string>();
    for (const m of this.ownedFleet?.members ?? []) set.add(canonicalNodeId(m.device));
    return set;
  });

  /** Whether a node is part of your owned fleet (linked by the shared key). */
  isFleetMember(nodeId: string): boolean {
    return this.fleetMemberIds.has(canonicalNodeId(nodeId)) && this.fleetMemberIds.size > 1;
  }

  /** Whether an id refers to this very machine (any suffix form). */
  isMe(id: string): boolean {
    return sameMachine(id, this.localId);
  }

  capsOf(nodeId: string): Capability[] {
    return this.catalog.capabilities.filter((c) => c.node === nodeId);
  }

  node(nodeId: string): MeshNode | undefined {
    return this.catalog.nodes.find((n) => n.id === nodeId);
  }

  /** Find the node representing the same machine as `id`, preferring an
   *  exact match (so a presence advert lands on its own node) and falling
   *  back to the canonical pubkey (so the daemon's bare-pubkey view and the
   *  presence display-id view of one machine resolve to a single node). */
  private nodeByCanonical(id: string): MeshNode | undefined {
    return (
      this.catalog.nodes.find((n) => n.id === id) ??
      this.catalog.nodes.find((n) => sameMachine(n.id, id))
    );
  }

  capability(id: string): Capability | undefined {
    return this.catalog.capabilities.find((c) => c.id === id);
  }

  /** Like `capability`, but synthesizes display stand-ins for terminal
   *  endpoints (which are never real catalog entries) — used by the views
   *  that render routes, so a live terminal session shows up. */
  capabilityForDisplay(id: string): Capability | undefined {
    return capabilityForDisplay(this.catalog, id);
  }

  // ---- lifecycle ---------------------------------------------------

  /** Wire up live backend data, if there is a backend. No-op (keeps the
   *  demo graph) in web mode. Called once on mount. */
  async init() {
    this.loadRooms();
    if (!isTauri()) {
      this.seedDemoFleet();
      this.seedDemoNetworks();
      this.seedDemoRoom();
    }
    // The rooms plane goes live *before* the first data pull. A room window
    // joins the instant its identity lands (set by `hydrateFromBackend`,
    // just below), and a `join` broadcast before onRoom is listening drops
    // the presence replies peers send straight back — leaving whoever joined
    // *second* with a roster of only themselves (their listener wasn't up to
    // hear who was already in the call; the first-joiner never noticed
    // because theirs was). Room presence has no snapshot to heal the gap the
    // way routes do, so the miss is permanent. Subscribing up front closes
    // the race; the same-device sibling bus rides along for symmetry.
    await onRoom(({ from, message }) => this.handleRoomMessage(from, message));
    await onRoomLocal((e) => this.handleRoomLocal(e));
    // Shared-file downloads stream straight to disk backend-side; these are
    // how the room panel learns how far each got and where it landed. Both
    // name the route + req, which we map back to the fetch token. (A files
    // *window* has its own listeners for its routes; these only touch the
    // `:shared` downloads this store registered, so the two never collide.)
    await onFileProgress((e) => this.onSharedProgress(e));
    await onFileSaved((e) => this.onSharedSaved(e));
    await this.hydrateFromBackend();
    await this.loadIdentity();
    await this.refreshNetworks();
    await this.syncMeshGraph();
    // Pull the *current* session state once — snapshots are otherwise only
    // emitted on changes, so a freshly-opened window (a per-machine console)
    // would see peers without their presence detail (no capabilities, no
    // ownership) until something next changed, and wrongly refuse to open.
    await this.pullSessionSnapshot();
    await this.loadOwnedFleet();
    await this.loadSites();
    await onNodeSites((e) => this.applyNodeSites(e));
    await this.loadUpdateStatus();
    await this.loadDisabledNetworks();
    await this.loadCec();
    this.startMeshPolling();
    await onSubscription((s) => {
      const live = s.status === "live";
      // When the mesh comes up, re-scan + reload networks/identity: the first
      // pass at mount can run before the session is ready.
      if (live) {
        void this.hydrateFromBackend();
        void this.loadIdentity();
        void this.refreshNetworks().then(() => this.syncMeshGraph());
        void this.pullSessionSnapshot();
        void this.loadOwnedFleet();
        void this.loadSites();
        void this.loadDisabledNetworks();
        void this.loadCec();
      }
      this.backendConnected = live;
    });
    await onSession((snap) => this.applySessionSnapshot(snap));
    // (The rooms plane — invites, join/leave presence, chat, knocks — and
    // its same-device sibling bus are subscribed at the top of init, before
    // the identity pull, so a room window can't join before onRoom listens.)
    // The video-popout sibling: which streams live in their own windows,
    // and the "Return video here" ask that puts one back.
    await onVideoLocal((e) => this.handleVideoLocal(e));
    // The fleet roster converges live — a claim, or gossip catching up, pushes
    // a fresh copy. This is what makes a claim visibly *do* something.
    await onOwned((r) => {
      this.ownedFleet = r;
      this.reconcileFleetRelationships();
    });
    await onOwnership((o) => {
      const who = this.catalog.nodes.find((n) => sameMachine(n.id, o.from))?.label ?? "A device";
      if (o.message.kind === "claimed") this.toast("ok", `${who} joined your fleet`);
      else if (o.message.kind === "declined")
        this.toast("warn", `Couldn't claim ${who}: ${o.message.reason ?? "not claimable"}`);
    });
  }

  /** Fetch the live session state (peers' presence + routes) and merge it
   *  into the graph — the on-demand twin of the `allmystuff://session`
   *  event, for windows that opened after the last change was emitted. */
  private async pullSessionSnapshot() {
    const snap = await sessionSnapshot();
    if (snap) this.applySessionSnapshot(snap);
  }

  /** Pull a fresh session snapshot now. For views whose state hangs off
   *  route negotiation (a terminal tab's connecting → live → ended): the
   *  `allmystuff://session` event is the latency win, but a pull is the
   *  truth — the same doctrine the video plane settled on after lost
   *  pushes froze streams. */
  refreshSession(): Promise<void> {
    return this.pullSessionSnapshot();
  }

  /** Poll the daemon's mesh membership as a safety net (peer/roster changes
   *  don't all arrive as session snapshots). Mirrors the MyOwnMesh client. */
  private startMeshPolling() {
    if (!isTauri() || this.meshPoll) return;
    this.meshPoll = setInterval(() => void this.syncMeshGraph(), 3000);
  }

  /** Build the graph's machine nodes from the daemon's *actual* mesh
   *  membership — the roster of known devices plus currently-live peers,
   *  across every joined network. This is what makes "others on the mesh"
   *  appear; the bespoke presence channel only layers device detail on top
   *  when a peer also runs AllMyStuff. Mirrors how MyOwnMesh builds its map. */
  async syncMeshGraph() {
    if (!isTauri()) return;
    // Live peers are keyed by the full device id (`{pubkey}-{suffix}`) — the
    // same id presence + capabilities use, so they merge into one node.
    const live = new Map<string, { label: string; online: boolean }>();
    const rosterAll: RosterPeer[] = [];
    const joins: PendingJoin[] = [];
    // Which networks each machine is seen on (canonical pubkey → network
    // names), so the graph can show that you're on several networks and a
    // device may share only some of them.
    const deviceNets = new Map<string, Set<string>>();
    const addNet = (deviceId: string, name: string) => {
      const k = canonicalNodeId(deviceId);
      (deviceNets.get(k) ?? deviceNets.set(k, new Set()).get(k)!).add(name);
    };
    const nets = Array.isArray(this.networks) ? this.networks : [];
    for (const net of nets) {
      const netName = networkDisplayName(net);
      let peers: PeerInfo[] = [];
      let roster: RosterPeer[] = [];
      try {
        peers = await meshPeers(net.config_id);
      } catch {
        /* network still settling */
      }
      try {
        roster = await meshRosterList(net.config_id);
      } catch {
        /* roster optional */
      }
      for (const p of peers) {
        if (p.status === "pending_approval") {
          // Surfaced as a "new device wants to join" nudge + popup, not on the
          // graph. Gathered across every network so the nudge catches them all.
          joins.push({ networkId: net.config_id, networkName: netName, peer: p });
          continue;
        }
        addNet(p.device_id, netName);
        const e = live.get(p.device_id) ?? { label: p.label?.trim() || shortId(p.device_id), online: false };
        if (p.label?.trim()) e.label = p.label.trim();
        const canon = canonicalNodeId(p.device_id);
        if (CONNECTED_STATUSES.has(p.status)) {
          e.online = true;
          this.lastConnectedAt.set(canon, Date.now());
        } else if (p.status === "offline" || p.status === "error") {
          // The daemon's explicit verdict — no grace, and no lingering
          // marker for the vanish sweep below to resurrect it with.
          this.lastConnectedAt.delete(canon);
        } else if (this.withinPresenceGrace(canon)) {
          // Transient (sighted / handshaking / reconnecting): a link
          // mid-rebuild. Recently-connected machines hold online through
          // it so an ICE blip doesn't flog the graph offline/online.
          e.online = true;
        }
        live.set(p.device_id, e);
      }
      for (const r of roster) addNet(r.device_id, netName);
      rosterAll.push(...roster);
    }
    this.pendingJoins = joins;
    // Forget declines for devices that are no longer pending (approved or
    // gone), so a device that comes back later nudges afresh.
    const pendingCanon = new Set(joins.map((j) => canonicalNodeId(j.peer.device_id)));
    if (this.dismissedJoins.some((id) => !pendingCanon.has(id))) {
      this.dismissedJoins = this.dismissedJoins.filter((id) => pendingCanon.has(id));
    }
    // The roster stores the bare pubkey; a live peer's id is `{pubkey}-{suffix}`.
    // Only surface a roster entry as its own offline node when no live peer
    // already covers that pubkey — otherwise one machine would show twice.
    const liveIds = [...live.keys()];
    const known = new Map(live);
    for (const r of rosterAll) {
      const covered = liveIds.some((id) => id === r.device_id || id.startsWith(`${r.device_id}-`));
      if (covered || known.has(r.device_id)) continue;
      known.set(r.device_id, { label: r.label?.trim() || shortId(r.device_id), online: false });
    }
    // Upsert a node per known device (never the local machine). Discovered
    // devices start *unclaimed* — they're on the mesh but not yet yours; you
    // claim them (only if they offer it) or mark them shared from their
    // drawer. A device known only from the daemon's roster/peers isn't
    // running AllMyStuff yet (`app: false`) — presence is what flips that on,
    // so we never downgrade a node the bespoke channel already enriched.
    for (const [id, info] of known) {
      // "Self" is recognised by the live local id *and* the daemon identity's
      // device id — the latter is known as soon as the socket is up, before a
      // scan has re-homed the local node off its `"this"` placeholder. Without
      // it, this machine's own roster entry (bare pubkey) would spawn a
      // "not on AllMyStuff" twin at startup and the real "this device" node
      // wouldn't be recognised.
      if (this.isLocalMachine(id)) continue;
      // The daemon reports the bare pubkey; a presence advert may already
      // have created this machine's node under its display id. Resolve by
      // canonical pubkey so we update that one node rather than spawning a
      // bare-pubkey twin that reads as "not on AllMyStuff".
      const nodeNets = [...(deviceNets.get(canonicalNodeId(id)) ?? [])].sort();
      const node = this.nodeByCanonical(id);
      if (!node) {
        this.catalog.nodes.push({
          id,
          label: info.label,
          kind: "machine",
          relationship: { kind: "unclaimed" },
          online: info.online,
          app: false,
          networks: nodeNets,
        });
      } else {
        node.online = info.online;
        node.networks = nodeNets;
        if (!node.hostname && info.label) node.label = info.label;
      }
    }
    // The local machine is on every network we've joined.
    const me = this.node(this.localId) ?? this.catalog.nodes.find((n) => n.kind === "this");
    if (me) me.networks = nets.map((n) => networkDisplayName(n)).sort();
    // A machine that's no longer in any roster/peer set has dropped offline.
    // Compare by canonical pubkey so a presence node (display id) isn't wrongly
    // marked offline just because the daemon lists it under the bare pubkey.
    // Recently-connected machines get the same grace as a transient status:
    // a daemon restarting mid-poll reports *nobody* for a few seconds, and
    // without the grace that blanks the whole graph offline and back.
    const knownCanon = new Set([...known.keys()].map(canonicalNodeId));
    for (const n of this.catalog.nodes) {
      const canon = canonicalNodeId(n.id);
      if (n.kind !== "this" && !this.isLocalMachine(n.id) && !knownCanon.has(canon)) {
        n.online = this.withinPresenceGrace(canon);
      }
    }
    // A freshly-discovered device may belong to someone we already share
    // with — fold it into that share.
    this.reconcileShares();
  }

  /** Whether `canon` (a canonical pubkey) was in a connected status
   *  recently enough that a transient dip should not read as offline. */
  private withinPresenceGrace(canon: string): boolean {
    const seen = this.lastConnectedAt.get(canon);
    return seen !== undefined && Date.now() - seen < PRESENCE_GRACE_MS;
  }

  /** Whether `id` names this machine — by the live local id or the daemon
   *  identity's device id (known before a scan re-homes the local node). */
  private isLocalMachine(id: string): boolean {
    if (sameMachine(id, this.localId)) return true;
    const did = this.identity?.device_id;
    return !!did && sameMachine(id, did);
  }

  /** Pull a real scan from the backend and re-home the local node onto its
   *  real mesh id + real devices. */
  async hydrateFromBackend() {
    const scan = await scanSelf();
    if (!scan) return;
    this.backendConnected = true;

    const prevId = this.localId;
    const newId = scan.node_id || "this";
    this.localId = newId;

    // Adopt this machine as "this device". Find the one true local node —
    // by its new id, else *the* existing "this" node (its id may have drifted
    // from `localId` after a session snapshot), else its previous id — and
    // re-home that one rather than spawning a duplicate.
    const host = scan.hostname || scan.label || "This device";
    // Display name follows the naming rule: the override if the user set one,
    // else the machine hostname.
    const label = this.identity?.label?.trim() || host;
    const existing =
      this.catalog.nodes.find((n) => n.id === newId) ??
      this.catalog.nodes.find((n) => n.kind === "this") ??
      this.catalog.nodes.find((n) => n.id === prevId);
    const me: MeshNode = existing ?? {
      id: newId,
      label,
      hostname: host,
      kind: "this",
      relationship: { kind: "mine" },
      online: true,
      app: true,
      summary: scan.summary,
    };
    me.id = newId;
    me.kind = "this";
    me.label = label;
    me.hostname = host;
    me.summary = scan.summary;
    me.online = true;
    me.app = true;
    if (!existing) this.catalog.nodes.push(me);
    // Exactly one local node: keep the one at `newId`, drop any other "this"
    // node and any peer twin of this machine (an early daemon poll may have
    // added it under a bare id). Match by id, never by reference — a node just
    // pushed into the `$state` array comes back as a proxy, so `n === me`
    // would be false and would silently delete the local node.
    this.catalog.nodes = this.catalog.nodes.filter(
      (n) => n.id === newId || (n.kind !== "this" && !sameMachine(n.id, newId)),
    );
    // Local capabilities are exactly what the scan reports; drop any tied to
    // the old or new local id so a re-scan replaces rather than accumulates.
    this.catalog.capabilities = [
      ...scan.capabilities,
      ...this.catalog.capabilities.filter((c) => c.node !== newId && c.node !== prevId),
    ];
    // A console window scans too (it needs the local sinks to wire routes),
    // but only the main window announces it.
    if (!consoleWindowTarget()) this.toast("ok", "Scanned this machine");
  }

  /** Point the graph's local identity at `id`, re-homing the "this" node and
   *  its capabilities so everything keyed by the local id (graph centring,
   *  endpoint matching) stays consistent. The first scan can label the local
   *  node with a placeholder id; the live session then hands us the real one,
   *  and without this the node and `localId` drift apart — leaving the graph
   *  with no machine in the centre. */
  private setLocalId(id: string) {
    if (this.localId === id) return;
    this.localId = id;
    const me = this.catalog.nodes.find((n) => n.kind === "this");
    if (!me || me.id === id) return;
    const old = me.id;
    me.id = id;
    // Re-key this machine's capabilities from `old` to `id`.
    for (const c of this.catalog.capabilities) {
      if (c.node === old) {
        c.id = id + c.id.slice(old.length);
        c.node = id;
      }
    }
    // Fold any bare-pubkey twin of this machine into the local node. Match by
    // id, not reference (`$state` proxies the array's objects).
    this.catalog.nodes = this.catalog.nodes.filter((n) => n.id === id || !sameMachine(n.id, id));
  }

  /** Merge a live session snapshot into the graph: presence peers become
   *  nodes (keeping any relationship the user already set), and live route
   *  states are reflected. */
  applySessionSnapshot(snap: SessionSnapshot) {
    if (!snap.ready) return;
    if (snap.me) this.setLocalId(snap.me);
    if (snap.network !== undefined) this.sessionNetwork = snap.network ?? null;

    for (const p of snap.peers ?? []) {
      // Resolve by canonical pubkey so presence lands on the same node the
      // daemon's roster/peer view created (the bare-pubkey "not on AllMyStuff"
      // twin), rather than a second node keyed by the display id.
      let node = this.nodeByCanonical(p.node);
      if (!node) {
        // A freshly-discovered peer starts unclaimed — claim it (only if it
        // offers itself) or mark it shared from its drawer.
        node = {
          id: p.node,
          label: p.label,
          hostname: p.hostname,
          kind: "machine",
          relationship: { kind: "unclaimed" },
          online: true,
        };
        this.catalog.nodes.push(node);
      } else {
        // Adopt the presence display id so this peer's capabilities (keyed by
        // `p.node`) resolve to this node.
        if (node.id !== p.node) node.id = p.node;
        node.label = p.label;
        node.hostname = p.hostname;
        node.online = true;
      }
      node.summary = p.summary;
      // Presence means it's running AllMyStuff — it has wireable stuff.
      node.app = true;
      // Ownership the device advertises about itself (Task 4).
      node.owner = p.owner ?? null;
      node.claimable = p.claimable ?? false;
      // App features it supports ("terminal", …) — absent from an older
      // peer means none, and the matching buttons stay hidden.
      node.features = p.features ?? [];
      // Sites it exposes for reverse-proxying (the Sites sidebar lists
      // them) — absent/empty from an older peer or one exposing nothing.
      node.sites = p.sites ?? [];
      // The AllMyStuff version it's running — let it tell when the machine
      // is behind the channel and offer an upgrade. Absent (older peer) =
      // unknown, and the upgrade button stays hidden.
      node.version = p.version;
      // A device that says *we* own it is ours; one owned by someone else
      // stays a guest/unclaimed (you can't flat-claim it). Never auto-flip a
      // relationship the user already set, and never auto-adopt.
      if (p.owner && sameMachine(p.owner, this.localId) && node.relationship.kind === "unclaimed") {
        node.relationship = { kind: "mine" };
      }
      // Collapse any other view of this same machine into the one node we just
      // settled on (id `p.node`) — heals an already-split graph. Match by id,
      // not reference: a freshly-pushed node is proxied by `$state`, so
      // `n === node` would be false and would delete the peer we just added.
      this.catalog.nodes = this.catalog.nodes.filter(
        (n) => n.id === p.node || !sameMachine(n.id, p.node),
      );
      // Refresh this peer's capabilities.
      this.catalog.capabilities = [
        ...this.catalog.capabilities.filter((c) => c.node !== p.node),
        ...p.capabilities,
      ];
    }

    // Reflect live routes (active ones become catalog routes), and keep
    // the per-route negotiation states for whoever watches one (a
    // terminal tab telling "connecting" from "rejected" by its reason).
    const states: Record<string, RouteLiveState> = {};
    for (const lr of snap.routes ?? []) {
      states[lr.route.id] = lr.state;
      const active = lr.state.state === "active";
      const id = lr.route.id;
      const exists = this.catalog.routes.some((r) => r.id === id);
      if (active && !exists) {
        this.catalog.routes.push({ ...lr.route });
      } else if (!active && exists) {
        this.catalog.routes = this.catalog.routes.filter((r) => r.id !== id);
      }
    }
    this.routeStates = states;

    // Reconcile site mappings against what each host now advertises: a host
    // that's online but no longer lists a site we'd mapped has stopped
    // exposing it — tear our local mapping down so the dead port is freed
    // (and the row disappears). Only when the host is online, so a brief
    // drop-off doesn't unmap.
    for (const m of [...this.siteMappings]) {
      const host = this.machineByAnyId(m.node);
      if (host?.online && !(host.sites ?? []).some((s) => s.id === m.site)) {
        void this.unmapSite(m.node, m.site);
      }
    }

    // A console waiting on its video backbone: the route just went
    // active, so bring the session's default legs (audio, control,
    // clipboard) up now — sequenced behind the picture instead of racing
    // it at open.
    if (
      this.consoleAutoLegs &&
      this.consoleVideoLive &&
      states[this.consoleVideoLive]?.state === "active"
    ) {
      this.startConsoleAutoLegs();
    }

    this.reconcileFleetRelationships();
    this.reconcileShares();
  }

  /** Fleet membership implies the relationship. Ownership is *directional*
   *  — your owner machine advertises no owner of its own — so on a claimed
   *  device its owner would read "unclaimed" forever even while wearing the
   *  fleet badge (mutually exclusive states on screen). Any co-member of
   *  your fleet is *yours*; one that left (or kicked you) and doesn't claim
   *  us as owner reverts to unclaimed. A relationship the user set to
   *  `shared` is never touched. */
  private reconcileFleetRelationships() {
    const meInFleet = this.isFleetMember(this.localId);
    for (const n of this.catalog.nodes) {
      if (n.kind === "this" || this.isMe(n.id)) continue;
      const inFleet = meInFleet && this.isFleetMember(n.id);
      const ownedByMe = !!n.owner && sameMachine(n.owner, this.localId);
      if (n.relationship.kind === "unclaimed" && inFleet) {
        n.relationship = { kind: "mine" };
      } else if (n.relationship.kind === "mine" && !inFleet && !ownedByMe) {
        n.relationship = { kind: "unclaimed" };
      }
    }
  }

  // ---- selection ---------------------------------------------------
  selectNode(id: string | null) {
    this.selectedNodeId = id;
  }

  // ---- connecting --------------------------------------------------

  /** Begin dragging a wire from a capability. The next node tapped on the
   *  graph becomes the destination (the "path dot" interaction). */
  startCapConnect(capId: string) {
    this.dragFrom = capId;
    const c = this.capability(capId);
    this.toast("info", `Connecting ${c?.label ?? "this"} — tap where it should go`);
  }

  cancelConnect() {
    this.dragFrom = null;
  }

  /** Finish a drag onto a node: auto-pick the matching endpoint there. */
  dropConnectOnNode(nodeId: string) {
    const capId = this.dragFrom;
    this.dragFrom = null;
    if (!capId) return;
    const cap = this.capability(capId);
    // Dragging a remote machine's *screen* onto this device is the "watch /
    // control that machine here" gesture — open its console rather than just
    // drawing a wire.
    if (cap && cap.origin === "screen" && cap.node !== this.localId && nodeId === this.localId) {
      this.openConsole(cap.node);
      return;
    }
    this.connectCapToNode(capId, nodeId);
  }

  /** Wire one capability to whichever endpoint on `nodeId` fits — a source
   *  reaches the node's matching sink, a sink is fed by its source. A
   *  connection that lands pops the console that manages it (the remote
   *  control for screens/audio/control, the file manager for storage), so
   *  sending something *to* a machine immediately hands you its session.
   */
  connectCapToNode(capId: string, nodeId: string) {
    const cap = this.capability(capId);
    if (!cap) return;
    if (cap.node === nodeId) {
      this.toast("warn", "Pick a different device");
      return;
    }
    // A device on the mesh that isn't running AllMyStuff has nothing to wire
    // to — keep it un-targetable (Task 1).
    const target = this.node(nodeId);
    if (target && !isAppNode(target)) {
      this.toast("warn", `${target.label} isn't running AllMyStuff`);
      return;
    }
    // The console belongs to whichever end of the wire isn't this machine.
    const remote = !this.isMe(nodeId) ? nodeId : !this.isMe(cap.node) ? cap.node : null;
    if (canSource(cap.flow)) {
      const sink = matchEndpoint(this.catalog, nodeId, cap.media, "consume");
      if (sink) {
        if (this.connect(capId, sink.id) && remote) this.popConsoleFor(remote, cap.media);
        return;
      }
    }
    if (canSink(cap.flow)) {
      const src = matchEndpoint(this.catalog, nodeId, cap.media, "provide");
      if (src) {
        if (this.connect(src.id, capId) && remote) this.popConsoleFor(remote, cap.media);
        return;
      }
    }
    const where = this.node(nodeId)?.label ?? "that device";
    this.toast("warn", `${where} has nowhere to put ${cap.label}`);
  }

  /** Open the console that manages connections of this kind with `nodeId`
   *  — the pikvm-style remote control for screen/audio/control media, the
   *  file manager for storage. Quiet when that console isn't right for the
   *  node (no gate-failure toasts after a *successful* connect) — and only
   *  for machines that are *yours*: the remote-control console pulls the
   *  far screen, which on a guest's machine would surprise both of you
   *  with a fresh permission ask. */
  private popConsoleFor(nodeId: string, media: MediaKind) {
    const node = this.node(nodeId);
    if (!node || this.isMe(nodeId) || !isAppNode(node)) return;
    if (media === "storage") {
      if (this.filesAllowed(node)) this.openFiles(nodeId);
      return;
    }
    if (media === "display" || media === "video" || media === "input" || media === "audio") {
      if (node.relationship.kind === "mine") this.openConsole(nodeId);
    }
  }

  /** Try to wire `from` → `to`. On success the route appears (and `true`
   *  comes back); if it needs a shared person's permission, raises the
   *  share sheet instead. */
  connect(from: string, to: string, codec?: "auto" | "h264" | "mjpeg"): boolean {
    const res = proposeRoute(this.catalog, from, to);
    if (res.ok) {
      this.addRoute(res.route.from, res.route.to);
      this.fireBackendConnect(res.route.from, res.route.to, res.route.media, codec);
      const f = this.capability(from)?.label ?? from;
      const t = this.capability(to)?.label ?? to;
      this.toast("ok", `Connected ${f} → ${t}`);
      return true;
    }
    if (res.denied && res.denied.length) {
      this.pendingShare = {
        from,
        to,
        fromLabel: this.capability(from)?.label ?? from,
        toLabel: this.capability(to)?.label ?? to,
        requests: res.denied,
      };
      return false;
    }
    this.toast("warn", res.reason);
    return false;
  }

  /** User approved the pending share: add exactly the requested grants,
   *  then complete the connection (popping the session's console, the same
   *  as a connect that never needed to ask). */
  approvePendingShare() {
    const p = this.pendingShare;
    if (!p) return;
    for (const req of p.requests) this.grant(req.node, requestToGrant(req));
    const res = proposeRoute(this.catalog, p.from, p.to);
    if (res.ok) {
      this.addRoute(res.route.from, res.route.to);
      this.fireBackendConnect(res.route.from, res.route.to, res.route.media);
      this.toast("ok", `Shared — connected ${p.fromLabel} → ${p.toLabel}`);
      const ends = [this.capability(p.from)?.node, this.capability(p.to)?.node];
      const remote = ends.find((n) => n && !this.isMe(n));
      if (remote) this.popConsoleFor(remote, res.route.media);
    }
    this.pendingShare = null;
  }

  dismissPendingShare() {
    this.pendingShare = null;
  }

  /** When a real backend is connected, fire the actual mesh route offer.
   *  The backend's session snapshots then keep the route's live state in
   *  sync; in web mode this is a no-op and the local route stands in. */
  private fireBackendConnect(
    from: string,
    to: string,
    media: MediaKind,
    codec?: "auto" | "h264" | "mjpeg",
  ) {
    if (this.backendConnected) void connectRoute(from, to, media, codec);
  }

  private addRoute(from: string, to: string) {
    const cap = this.capability(from);
    const id = `route:${from}→${to}`;
    if (this.catalog.routes.some((r) => r.id === id)) return;
    this.catalog.routes.push({ id, from, to, media: cap?.media ?? "generic" });
  }

  /** Tear a route down. The local catalog updates synchronously; the
   *  returned promise settles when the backend disconnect has been sent —
   *  callers that must outlive the call (a closing console window) await
   *  it, everyone else ignores it. */
  disconnect(routeId: string): Promise<unknown> {
    const sent = this.backendConnected ? disconnectRoute(routeId) : Promise.resolve(null);
    this.catalog.routes = this.catalog.routes.filter((r) => r.id !== routeId);
    return sent;
  }

  // ---- remote console (the pikvm-style session) -------------------

  /** A remote machine's video-capable sources — its screen plus any cameras
   *  — ordered so the screen leads and the default sits near the front. This
   *  is the console's "video inputs" tab bar. */
  consoleVideoInputs(nodeId: string): Capability[] {
    return this.capsOf(nodeId)
      .filter((c) => (c.media === "display" || c.media === "video") && canSource(c.flow))
      .sort((a, b) => {
        const rank = (c: Capability) => (c.origin === "screen" ? 0 : c.default ? 1 : 2);
        return rank(a) - rank(b) || a.id.localeCompare(b.id);
      });
  }

  /** Whether a machine's cameras actually *stream* when selected: its
   *  build advertises the camera feature. Cameras as capabilities predate
   *  the transport, so an older host still shows its camera tab — the
   *  console explains the update instead of wiring a route that can never
   *  carry pixels. */
  cameraStreamSupported(node: MeshNode | undefined): boolean {
    return !!node && isAppNode(node) && (node.features ?? []).includes(FEATURE_CAMERA);
  }

  /** Open a console session on a remote machine — the single handle for its
   *  screen, its audio passthrough and keyboard/mouse control. On the
   *  desktop this opens a *dedicated OS window* per machine (so several
   *  consoles can be up side by side); the web preview keeps the in-page
   *  popover. */
  openConsole(nodeId: string) {
    const node = this.node(nodeId);
    if (!this.consoleAllowed(node, nodeId)) return;
    if (isTauri()) {
      void openConsoleWindow(nodeId);
      return;
    }
    this.openConsoleHere(nodeId);
  }

  /** Start the console session *in this window* — the body of a console
   *  window (and the web preview's popover). Wires the backbone video route
   *  to this machine's display first; audio passthrough and keyboard &
   *  mouse control are *assumed on* for a remote session — a console is
   *  the whole session by default, like sitting down at the machine. The
   *  legs are sequenced (video, then the rest) instead of racing each
   *  other onto the wire at open, but sequencing only ever *delays* them:
   *  a video route that's already live (or a snapshot that never comes)
   *  still ends with control on. The toggles inside are the off-switches. */
  openConsoleHere(nodeId: string) {
    const node = this.node(nodeId);
    if (!this.consoleAllowed(node, nodeId)) return;
    this.consoleNodeId = nodeId;
    this.consoleAudio = false;
    this.consoleControl = false;
    this.consoleClipboard = false;
    this.consoleVideoRouteId = null;
    this.consoleAudioRouteId = null;
    this.consoleControlRouteId = null;
    this.consoleClipboardRouteId = null;
    this.consoleVideoLive = null;
    this.consoleControlLive = null;
    this.consoleClipboardLive = null;
    this.consoleInput = this.consoleVideoInputs(nodeId)[0]?.id ?? null;
    this.consoleCodec = "auto";
    this.consoleTune = {};
    if (isTauri()) {
      // Census before the first wire: ping for popout windows and give
      // their `opened` answers a beat to land, so a console (re)opening
      // onto an input that already lives in its own window shows "Return
      // video here" instead of briefly stealing the stream's watch slot.
      // (The `opened` handler still self-heals the race either way.)
      this.helloVideoLane();
      setTimeout(() => {
        if (this.consoleNodeId !== nodeId) return; // closed meanwhile
        void this.wireConsoleFirstVideo();
      }, 180);
    } else {
      void this.wireConsoleFirstVideo();
    }
    this.toast("ok", `Console open on ${node!.label}`);
  }

  /** The console's opening video wire + the auto-legs decision — split
   *  from [`openConsoleHere`] so the desktop can hold it through the
   *  popout census above. */
  private async wireConsoleFirstVideo() {
    await this.applyConsoleVideo();
    if (
      this.consoleVideoLive &&
      this.backendConnected &&
      this.routeStates[this.consoleVideoLive]?.state !== "active"
    ) {
      // The usual case: video is on the wire but not active yet — the
      // snapshot that flips it active triggers the remaining legs (see
      // applySessionSnapshot). The timer is the backstop for a snapshot
      // that never comes (a lost push, a route active since before this
      // window looked): control comes up regardless, just unsequenced.
      this.consoleAutoLegs = true;
      this.consoleAutoLegsFallback = setTimeout(() => this.startConsoleAutoLegs(), 3000);
    } else {
      // Video already active, absent (the input may be popped out), or
      // local-only (no backend) — nothing to sequence behind; bring the
      // legs up now. An audio-only console is still a session.
      this.startConsoleAutoLegs();
    }
  }

  /** Pending "bring audio + control up once video is live" — set at console
   *  open, consumed by the first snapshot showing the video route active
   *  (or by the fallback timer when no such snapshot ever lands). */
  private consoleAutoLegs = false;
  private consoleAutoLegsFallback: ReturnType<typeof setTimeout> | null = null;

  /** The console's default session legs. Only ever turns things *on* — a
   *  toggle the user already flipped stays exactly as they left it. */
  private startConsoleAutoLegs() {
    this.consoleAutoLegs = false;
    if (this.consoleAutoLegsFallback) {
      clearTimeout(this.consoleAutoLegsFallback);
      this.consoleAutoLegsFallback = null;
    }
    if (!this.consoleNodeId) return;
    if (!this.consoleAudio) this.toggleConsoleAudio();
    if (!this.consoleControl) this.toggleConsoleControl();
    // Clipboard is newer than audio/control: only auto-enable it when the
    // remote advertises a clipboard endpoint, so a console onto a
    // not-yet-updated peer doesn't warn about a path that can't exist yet.
    // An explicit toggle still reports it.
    if (
      !this.consoleClipboard &&
      matchEndpoint(this.catalog, this.consoleNodeId, "clipboard", "consume")
    ) {
      this.toggleConsoleClipboard();
    }
  }

  /** The gate both console entries share: a known remote machine that runs
   *  AllMyStuff and is yours (or shared with you). */
  private consoleAllowed(node: MeshNode | undefined, nodeId: string): node is MeshNode {
    if (!node) return false;
    if (nodeId === this.localId) {
      this.toast("warn", "That's this device");
      return false;
    }
    if (!isAppNode(node)) {
      this.toast("warn", `${node.label} isn't running AllMyStuff`);
      return false;
    }
    if (node.relationship.kind === "unclaimed") {
      this.toast("warn", `Claim ${node.label} first, or mark it shared`);
      return false;
    }
    return true;
  }

  /** Find the machine node `id` refers to, under any id form (exact, or the
   *  same canonical pubkey) — how a console window resolves its target. */
  machineByAnyId(id: string): MeshNode | undefined {
    return this.nodeByCanonical(id);
  }

  /** Close the console, tearing down exactly the routes it created. The UI
   *  state resets synchronously; the returned promise settles once the
   *  backend disconnects are on the wire, so a console *window* can hold
   *  its close until then. */
  closeConsole(): Promise<unknown> {
    const pending: Promise<unknown>[] = [];
    if (this.consoleVideoRouteId) pending.push(this.disconnect(this.consoleVideoRouteId));
    if (this.consoleAudioRouteId) pending.push(this.disconnect(this.consoleAudioRouteId));
    if (this.consoleControlRouteId) pending.push(this.disconnect(this.consoleControlRouteId));
    if (this.consoleClipboardRouteId) pending.push(this.disconnect(this.consoleClipboardRouteId));
    this.consoleVideoRouteId = null;
    this.consoleAudioRouteId = null;
    this.consoleControlRouteId = null;
    this.consoleClipboardRouteId = null;
    this.consoleVideoLive = null;
    this.consoleControlLive = null;
    this.consoleClipboardLive = null;
    this.consoleNodeId = null;
    this.consoleInput = null;
    this.consoleAudio = false;
    this.consoleControl = false;
    this.consoleClipboard = false;
    this.consoleAutoLegs = false;
    if (this.consoleAutoLegsFallback) {
      clearTimeout(this.consoleAutoLegsFallback);
      this.consoleAutoLegsFallback = null;
    }
    return Promise.allSettled(pending);
  }

  /** Forward one keyboard/mouse event down the console's control route.
   *  Fire-and-forget — at pointer-move rates a lost event is meaningless. */
  sendConsoleInput(action: InputAction) {
    if (!this.consoleControlLive) return;
    void sendInput(this.consoleControlLive, action);
  }

  /** Switch which remote source the console is showing. */
  setConsoleInput(capId: string) {
    this.consoleInput = capId;
    void this.applyConsoleVideo();
  }

  /** Bumped per video (re)wire; an apply that awaited a teardown checks it
   *  before connecting, so rapid tab clicks can't interleave two wires. */
  private consoleVideoEpoch = 0;

  private async applyConsoleVideo() {
    const epoch = ++this.consoleVideoEpoch;
    if (this.consoleVideoRouteId) {
      const old = this.consoleVideoRouteId;
      this.consoleVideoRouteId = null;
      this.consoleVideoLive = null;
      // Await the teardown so it's on the wire *before* the next offer:
      // the sender frees its one H.264 lane per peer when the teardown
      // arrives, and channel order then guarantees the next screen takes
      // the lane over instead of racing it and landing on MJPEG.
      await this.disconnect(old);
    }
    this.consoleVideoLive = null;
    if (epoch !== this.consoleVideoEpoch) return; // a newer switch took over
    const inp = this.consoleInput ? this.capability(this.consoleInput) : null;
    if (!inp) return;
    // An input that lives in its own popout window stays there — the
    // stage shows "Return video here" instead of competing for the
    // stream's one watch slot.
    if (this.isVideoPopped(`cap:${inp.id}`)) return;
    // A camera tab on a host whose build predates camera streaming: skip
    // the wire — the route could never carry pixels, and the stage
    // explains the update instead.
    if (inp.media === "video" && !this.cameraStreamSupported(this.machineByAnyId(inp.node))) {
      return;
    }
    // The remote screen (display) lands on this machine's display sink, a
    // camera (video) on its synthetic video-in sink — either way a real
    // route the backend streams frames down.
    const sink = matchEndpoint(this.catalog, this.localId, inp.media, "consume");
    if (!sink) return;
    const leg = this.ownedConnect(inp.id, sink.id, this.consoleCodec);
    // Render whatever's live; only own the route for teardown if this call
    // created it.
    this.consoleVideoLive = leg?.id ?? null;
    this.consoleVideoRouteId = leg?.created ? leg.id : null;
    // Carry the quality pills onto the fresh route (the sender restarts
    // its capture with them; harmless no-op when everything is Auto).
    if (leg && this.hasTune()) void tuneRoute(leg.id, this.consoleTune);
  }

  private hasTune(): boolean {
    const t = this.consoleTune;
    return t.maxEdge != null || t.bitrate != null || t.fps != null;
  }

  /** One pill changed: remember it and re-tune the live stream. */
  setConsoleTune(patch: StreamTune) {
    this.consoleTune = { ...this.consoleTune, ...patch };
    if (this.consoleVideoLive) void tuneRoute(this.consoleVideoLive, this.consoleTune);
  }

  /** The codec pill changed: re-offer the video route on that transport. */
  setConsoleCodec(codec: "auto" | "h264" | "mjpeg") {
    if (this.consoleCodec === codec) return;
    this.consoleCodec = codec;
    void this.applyConsoleVideo();
  }

  /** Audio passthrough: play what the remote machine is playing — its
   *  system audio, loopback-captured on the far side — on this machine's
   *  speakers. Deliberately listen-only, not a call: the console never
   *  opens a microphone. The far side's loopback captures *everything*
   *  it plays, so any audio we injected would come straight back one
   *  round trip later as an echo (there's no echo cancellation yet) —
   *  the clean design is that nothing ever flows that way. */
  toggleConsoleAudio() {
    const remote = this.consoleNodeId;
    if (!remote) return;
    if (this.consoleAudio) {
      if (this.consoleAudioRouteId) this.disconnect(this.consoleAudioRouteId);
      this.consoleAudioRouteId = null;
      this.consoleAudio = false;
      return;
    }
    const from = matchEndpoint(this.catalog, remote, "audio", "provide");
    const to = matchEndpoint(this.catalog, this.localId, "audio", "consume");
    const leg = from && to ? this.ownedConnect(from.id, to.id) : null;
    if (leg) {
      // Own the route for teardown only if this call created it (never a
      // pre-existing one the user wired from the graph).
      this.consoleAudioRouteId = leg.created ? leg.id : null;
      this.consoleAudio = true;
    } else {
      this.toast("warn", "No audio path to that machine");
    }
  }

  /** Send this machine's keyboard & mouse to the remote (input injection on
   *  the far side is a follow-up; the route is real and shows active). */
  toggleConsoleControl() {
    const remote = this.consoleNodeId;
    if (!remote) return;
    if (this.consoleControl) {
      if (this.consoleControlRouteId) this.disconnect(this.consoleControlRouteId);
      this.consoleControlRouteId = null;
      this.consoleControlLive = null;
      this.consoleControl = false;
      return;
    }
    const mySrc = matchEndpoint(this.catalog, this.localId, "input", "provide");
    const remoteSink = matchEndpoint(this.catalog, remote, "input", "consume");
    const leg = mySrc && remoteSink ? this.ownedConnect(mySrc.id, remoteSink.id) : null;
    if (leg) {
      this.consoleControlRouteId = leg.created ? leg.id : null;
      this.consoleControlLive = leg.id;
      this.consoleControl = true;
    } else {
      this.toast("warn", "No control path to that machine");
    }
  }

  /** Clipboard passthrough: with it on, a paste in the console first pushes
   *  this machine's clipboard to the remote (see [`sendConsoleClipboard`]),
   *  so the paste lands our content there. The route is outbound only —
   *  local clipboard → remote clipboard — and, like control, sends nothing
   *  until you actually paste, so each machine keeps its own clipboard. */
  toggleConsoleClipboard() {
    const remote = this.consoleNodeId;
    if (!remote) return;
    if (this.consoleClipboard) {
      if (this.consoleClipboardRouteId) this.disconnect(this.consoleClipboardRouteId);
      this.consoleClipboardRouteId = null;
      this.consoleClipboardLive = null;
      this.consoleClipboard = false;
      return;
    }
    const mySrc = matchEndpoint(this.catalog, this.localId, "clipboard", "provide");
    const remoteSink = matchEndpoint(this.catalog, remote, "clipboard", "consume");
    const leg = mySrc && remoteSink ? this.ownedConnect(mySrc.id, remoteSink.id) : null;
    if (leg) {
      this.consoleClipboardRouteId = leg.created ? leg.id : null;
      this.consoleClipboardLive = leg.id;
      this.consoleClipboard = true;
    } else {
      this.toast("warn", "No clipboard path to that machine");
    }
  }

  /** Push this machine's clipboard down the live clipboard route — called
   *  the instant the console forwards a paste, so the remote pastes our
   *  content (text, an image, or files). The backend reads the clipboard and
   *  streams it; this resolves once it's all on the wire, and the caller then
   *  releases the paste keystroke, keeping the order the remote needs (write
   *  clipboard, then inject paste). No-op when clipboard passthrough is off. */
  async sendConsoleClipboard(): Promise<void> {
    const route = this.consoleClipboardLive;
    if (!route) return;
    await clipboardPaste(route);
  }

  /** Connect one session leg (a console channel, a room toggle) through
   *  the normal route path, so authorization and the backend offer still
   *  apply. Returns the route id when it's now live, and whether *this*
   *  call created it — so a session reads its channel as on only when
   *  something is actually wired, and tears down only the routes it made
   *  (never a pre-existing one the user set up, and never a leg that was
   *  blocked behind a share prompt). */
  private ownedConnect(
    from: string,
    to: string,
    codec?: "auto" | "h264" | "mjpeg",
  ): { id: string; created: boolean } | null {
    const id = `route:${from}→${to}`;
    const existedBefore = this.catalog.routes.some((r) => r.id === id);
    this.connect(from, to, codec);
    const existsNow = this.catalog.routes.some((r) => r.id === id);
    if (!existsNow) return null; // blocked / denied — nothing got wired
    return { id, created: !existedBefore };
  }

  // ---- video popouts (one stream in its own OS window) --------------

  /** Wire a popout's *own* video leg. Unlike [`ownedConnect`], this skips the
   *  GUI's route authorization (the structural unclaimed gate and the share
   *  gate): a popout only ever *continues* a stream that was already live and
   *  authorized where it was popped out from, and the host enforces owner/fleet
   *  itself. A fresh popout window boots its own store and may not have
   *  re-derived the source's ownership yet, so routing it through `proposeRoute`
   *  would refuse the stream with a bogus "isn't yours yet — claim it first" —
   *  the route never lands and never heals. Here the leg is wired directly;
   *  if the far side really wouldn't allow it, the host rejects and the popout
   *  shows that. Returns the route id and whether this call created it. */
  private wirePopoutLeg(from: string, to: string): { id: string; created: boolean } | null {
    const src = this.capability(from);
    const sink = this.capability(to);
    if (!src || !sink) return null;
    const id = `route:${from}→${to}`;
    const existedBefore = this.catalog.routes.some((r) => r.id === id);
    if (!existedBefore) {
      this.addRoute(from, to);
      this.fireBackendConnect(from, to, src.media);
    }
    return { id, created: !existedBefore };
  }

  /** Whether the stream behind `key` is held in a popout window. */
  isVideoPopped(key: string): boolean {
    return !!this.poppedVideos[key];
  }

  /** Ping the lane for popouts: each answers `opened`, so this window's
   *  popped set converges on the windows that actually exist. Called by
   *  the console / room panel on mount (no-op in web mode). */
  helloVideoLane() {
    void emitVideoLocal({ token: this.windowToken, kind: "hello" });
  }

  /** Lift a console video input out into its own OS window. If the
   *  console is showing that input right now, its route is torn down
   *  *first* (awaited, so the teardown precedes the popout's fresh offer
   *  on the wire — the same ordering the console's tab switches keep, so
   *  the popout takes the H.264 lane over instead of racing it). */
  async popOutConsoleInput(capId: string) {
    if (!isTauri()) return;
    const cap = this.capability(capId);
    if (!cap) return;
    const key = `cap:${capId}`;
    this.poppedVideos = { ...this.poppedVideos, [key]: true };
    if (this.consoleInput === capId && this.consoleVideoLive) {
      const owned = this.consoleVideoRouteId;
      this.consoleVideoRouteId = null;
      this.consoleVideoLive = null;
      if (owned) await this.disconnect(owned);
    }
    const machine = this.machineByAnyId(cap.node);
    void openVideoWindow(key, `${cap.label} · ${machine?.label ?? "AllMyStuff"}`);
  }

  /** Lift a room share's tile out into its own OS window. The popout only
   *  *watches* the route (the sender owns it), so nothing re-negotiates —
   *  the frames simply land in the new window instead of the tile. */
  popOutRoomShare(route: Route, member: MeshNode) {
    if (!isTauri()) return;
    const key = `share:${route.id}`;
    this.poppedVideos = { ...this.poppedVideos, [key]: true };
    const who = this.roomWho(member.id);
    const what = route.media === "video" ? "camera" : "screen";
    void openVideoWindow(key, `${who.who}'s ${what} · AllMyStuff`);
  }

  /** The tab's "Return video here": ask whichever popout holds `key` to
   *  put the stream back (it tears down / unwatches, emits `closed`, and
   *  closes itself; the `closed` handler re-wires the tab). If no popout
   *  answers — it's already gone and the popped mark is stale — un-pop
   *  locally after a beat so the button always resets. */
  askReturnVideo(key: string) {
    void emitVideoLocal({ token: this.windowToken, kind: "return-ask", key });
    setTimeout(() => {
      if (this.poppedVideos[key]) this.videoPopoutGone(key);
    }, 1500);
  }

  /** One event off the same-device video-popout lane (another window of
   *  this app talking; own echoes are dropped by token). */
  private handleVideoLocal(e: VideoLocalEvent) {
    if (e.token === this.windowToken) return;
    switch (e.kind) {
      case "hello": {
        // A console/room window booted and asked who's out there. If this
        // window is a popout, answer — and re-assert the frame watch a
        // beat later: the asker may have briefly claimed our route's
        // watch slot while it didn't yet know (claims replace each other;
        // see videoPopoutRewatch).
        if (this.videoPopoutKey) {
          void emitVideoLocal({
            token: this.windowToken,
            kind: "opened",
            key: this.videoPopoutKey,
          });
          setTimeout(() => (this.videoPopoutRewatch += 1), 400);
        }
        break;
      }
      case "opened": {
        if (!e.key) break;
        this.poppedVideos = { ...this.poppedVideos, [e.key]: true };
        // Boot race: this console wired an input before learning it was
        // popped out (the census answer landed after the auto-wire).
        // Back off — release the route if this window owns it, and let
        // the stage show "Return video here" instead.
        const capId = e.key.startsWith("cap:") ? e.key.slice(4) : null;
        if (capId && this.consoleInput === capId && this.consoleVideoLive) {
          const owned = this.consoleVideoRouteId;
          this.consoleVideoRouteId = null;
          this.consoleVideoLive = null;
          if (owned) void this.disconnect(owned);
        }
        break;
      }
      case "closed": {
        if (e.key) this.videoPopoutGone(e.key);
        break;
      }
      case "return-ask": {
        if (e.key && this.videoPopoutKey === e.key) void this.closeVideoPopout();
        break;
      }
    }
  }

  /** A popout ended (its `closed`, or a stale mark timing out): un-pop the
   *  key, and if this is the console window sitting on that input's tab,
   *  wire the stream back into the stage. Room tiles re-watch reactively. */
  private videoPopoutGone(key: string) {
    const { [key]: _gone, ...rest } = this.poppedVideos;
    this.poppedVideos = rest;
    const capId = key.startsWith("cap:") ? key.slice(4) : null;
    if (capId && this.consoleNodeId && this.consoleInput === capId) {
      void this.applyConsoleVideo();
    }
  }

  /** Boot this window as the popout for `key` — called by the popout host
   *  once the stream's facts have landed. A `cap:` key wires (and then
   *  owns) a fresh route from that capability to this machine's matching
   *  sink, exactly as the console stage would; a `share:` key only
   *  watches the sender's existing route. Announces `opened` either way. */
  initVideoPopout(key: string) {
    this.videoPopoutKey = key;
    if (key.startsWith("cap:")) {
      const cap = this.capability(key.slice(4));
      const sink = cap ? matchEndpoint(this.catalog, this.localId, cap.media, "consume") : null;
      const leg = cap && sink ? this.wirePopoutLeg(cap.id, sink.id) : null;
      this.videoPopoutLive = leg?.id ?? null;
      this.videoPopoutRouteId = leg?.created ? leg.id : null;
    } else if (key.startsWith("share:")) {
      this.videoPopoutLive = key.slice(6);
    }
    void emitVideoLocal({ token: this.windowToken, kind: "opened", key });
  }

  /** End this popout (the Return ask, or the window's OS ✕): tear down the
   *  route it created (never one it merely watched), tell the lane, and
   *  close the window. The await keeps the teardown ahead of the console's
   *  re-wire — `closed` is only emitted once it's on the wire. */
  async closeVideoPopout() {
    const key = this.videoPopoutKey;
    if (!key) return;
    this.videoPopoutKey = null;
    this.videoPopoutLive = null;
    const owned = this.videoPopoutRouteId;
    this.videoPopoutRouteId = null;
    if (owned) await this.disconnect(owned);
    await emitVideoLocal({ token: this.windowToken, kind: "closed", key });
    void closeThisWindow();
  }

  /** The live outbound input route to `nodeId`, if any — what lets a
   *  video surface (room tile, popout) forward clicks and keys over the
   *  picture. Exists exactly while that machine wired *our* keyboard &
   *  mouse to its control sink (a room's "share control", the console's
   *  control toggle); injection stays gated on the far side regardless. */
  controlRouteTo(nodeId: string): string | null {
    for (const r of this.catalog.routes) {
      if (r.media !== "input") continue;
      const from = this.capability(r.from);
      const to = this.capability(r.to);
      if (from && to && this.isMe(from.node) && sameMachine(to.node, nodeId)) return r.id;
    }
    return null;
  }

  // ---- terminal (the mesh-native shell) ----------------------------

  /** Whether `node` can host a terminal at all: it runs AllMyStuff and its
   *  presence advertises the feature (an older build simply doesn't). */
  terminalSupported(node: MeshNode | undefined): boolean {
    return !!node && isAppNode(node) && (node.features ?? []).includes(FEATURE_TERMINAL);
  }

  /** The gate for "Open Terminal" — a mirror of the host's own rule
   *  (`sender_may_control`): only the node's recorded owner or a co-owned
   *  fleet member gets a shell. Deliberately *not* `relationship.kind`,
   *  which is a local label the user can set freely; this checks the same
   *  facts the far side will enforce, so the button never promises what
   *  the host would refuse. */
  terminalAllowed(node: MeshNode | undefined): boolean {
    if (!node || this.isMe(node.id) || !this.terminalSupported(node)) return false;
    const ownerIsMe = !!node.owner && this.isMe(node.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node.id);
    return ownerIsMe || coFleet;
  }

  /** Open a terminal on a remote machine. On the desktop this opens (or
   *  focuses) the machine's dedicated terminal window — tabs inside it are
   *  separate shells; the web preview keeps an in-page popover. */
  openTerminal(nodeId: string) {
    const node = this.node(nodeId);
    if (!node) return;
    if (this.isMe(nodeId)) {
      this.toast("warn", "That's this device");
      return;
    }
    if (!this.terminalSupported(node)) {
      this.toast("warn", `${node.label} doesn't support terminals (older AllMyStuff?)`);
      return;
    }
    if (!this.terminalAllowed(node)) {
      this.toast("warn", `Terminals are owner/fleet only — ${node.label} isn't yours`);
      return;
    }
    if (isTauri()) {
      void openTerminalWindow(nodeId);
      return;
    }
    this.terminalNodeId = nodeId;
  }

  /** Close the in-page terminal popover (web preview). The desktop's
   *  terminal windows close themselves, tearing their tabs down first. */
  closeTerminal() {
    this.terminalNodeId = null;
  }

  /** Open one terminal *session* (a tab) to `hostNodeId`: a generic route
   *  from the host's `…:terminal` endpoint to a viewer endpoint minted for
   *  this tab — unique endpoint, unique route id, so tabs to one machine
   *  never collide. Deliberately not `connect()`/`proposeRoute`: terminal
   *  endpoints aren't catalog capabilities (see `capabilityForDisplay`),
   *  and the binding authorization runs host-side against the owner/fleet
   *  rule. Returns the route id the tab watches, or null in web mode
   *  (no backend — nothing can flow). */
  terminalConnect(hostNodeId: string): string | null {
    if (!this.backendConnected) return null;
    const from = `${hostNodeId}:terminal`;
    const n = ++this.termViewSeq;
    const to = `${this.localId}:term-view:${Date.now().toString(36)}-${n}`;
    void connectRoute(from, to, "generic");
    return `route:${from}→${to}`;
  }

  /** Tear one terminal session down (tab closed / window closing). The
   *  returned promise settles when the disconnect is on the wire, so a
   *  closing window can hold its close until then. */
  terminalDisconnect(routeId: string): Promise<unknown> {
    return this.disconnect(routeId);
  }

  // ---- files (the mesh-native file manager) -------------------------

  /** Whether `node` can host a files session at all: it runs AllMyStuff
   *  and its presence advertises the feature (an older build doesn't). */
  filesSupported(node: MeshNode | undefined): boolean {
    return !!node && isAppNode(node) && (node.features ?? []).includes(FEATURE_FILES);
  }

  /** The gate for "Open Files" — the same owner/fleet rule as the
   *  terminal (browsing a disk is as privileged as a shell), checked
   *  against the facts the far side will enforce. */
  filesAllowed(node: MeshNode | undefined): boolean {
    if (!node || this.isMe(node.id) || !this.filesSupported(node)) return false;
    const ownerIsMe = !!node.owner && this.isMe(node.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node.id);
    return ownerIsMe || coFleet;
  }

  /** Open the file manager on a remote machine. On the desktop this opens
   *  (or focuses) the machine's dedicated files window; the web preview
   *  keeps an in-page popover. */
  openFiles(nodeId: string) {
    const node = this.node(nodeId);
    if (!node) return;
    if (this.isMe(nodeId)) {
      this.toast("warn", "That's this device");
      return;
    }
    if (!this.filesSupported(node)) {
      this.toast("warn", `${node.label} doesn't support file browsing (older AllMyStuff?)`);
      return;
    }
    if (!this.filesAllowed(node)) {
      this.toast("warn", `Files are owner/fleet only — ${node.label} isn't yours`);
      return;
    }
    if (isTauri()) {
      void openFilesWindow(nodeId);
      return;
    }
    this.filesNodeId = nodeId;
  }

  /** Close the in-page files popover (web preview). The desktop's files
   *  windows close themselves, tearing their route down first. */
  closeFiles() {
    this.filesNodeId = null;
  }

  /** Open one files *session* to `hostNodeId`: a generic route from the
   *  host's `…:files` endpoint to a viewer endpoint minted for this
   *  window. Deliberately not `connect()`/`proposeRoute` — files
   *  endpoints aren't catalog capabilities (see `capabilityForDisplay`),
   *  and the binding authorization runs host-side against the owner/fleet
   *  rule. Returns the route id the window watches, or null in web mode. */
  filesConnect(hostNodeId: string): string | null {
    if (!this.backendConnected) return null;
    const from = `${hostNodeId}:files`;
    const n = ++this.filesViewSeq;
    const to = `${this.localId}:files-view:${Date.now().toString(36)}-${n}`;
    void connectRoute(from, to, "generic");
    return `route:${from}→${to}`;
  }

  /** Tear one files session down (window closing). The returned promise
   *  settles when the disconnect is on the wire. */
  filesDisconnect(routeId: string): Promise<unknown> {
    return this.disconnect(routeId);
  }

  // ---- sites (the reverse-proxy plane) ------------------------------

  /** Whether `node` can host sites at all: it runs AllMyStuff and its
   *  presence advertises the feature (an older build doesn't). */
  sitesSupported(node: MeshNode | undefined): boolean {
    return !!node && isAppNode(node) && (node.features ?? []).includes(FEATURE_SITES);
  }

  /** The gate for reaching a peer's sites — the same owner/fleet rule as the
   *  terminal and files (a reverse proxy into a machine's services is just
   *  as privileged), checked against the facts the far side enforces. Sites
   *  shared with another *person* via a grant ride the pending
   *  share-enforcement work; today, like control/terminal/files, access is
   *  fleet-direct. */
  sitesAllowed(node: MeshNode | undefined): boolean {
    if (!node || this.isMe(node.id) || !this.sitesSupported(node)) return false;
    const ownerIsMe = !!node.owner && this.isMe(node.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node.id);
    return ownerIsMe || coFleet;
  }

  /** The machines whose sites this device can reach, each with its exposed
   *  sites — what the Sites tab groups by. Only fleet/owned machines that
   *  actually expose something appear (the gate the far side enforces). */
  sitesByMachine = $derived.by<{ node: MeshNode; sites: SiteAdvert[] }[]>(() => {
    const out: { node: MeshNode; sites: SiteAdvert[] }[] = [];
    for (const n of this.catalog.nodes) {
      if (this.isMe(n.id)) continue;
      const sites = n.sites ?? [];
      if (sites.length === 0 || !this.sitesAllowed(n)) continue;
      out.push({ node: n, sites });
    }
    return out;
  });

  /** The live mapping for one site, if this device has mapped it. */
  siteMappingFor(nodeId: string, siteId: string): SiteMapping | undefined {
    return this.siteMappings.find((m) => sameMachine(m.node, nodeId) && m.site === siteId);
  }

  /** The address a mapped site is reachable at locally — a clickable URL for
   *  a web site, else the bare `localhost:<port>` to point a client at. */
  siteUrl(m: SiteMapping): string {
    return siteIsWeb(m) ? `${m.scheme}://localhost:${m.localPort}` : `localhost:${m.localPort}`;
  }

  /** Load this machine's listening services + exposed set + live mappings
   *  from the backend (called once the session is up). In web mode it seeds
   *  believable demo data so the Sites tab is alive in the preview. */
  async loadSites() {
    // Gate on the runtime, not `backendConnected` — this runs during init,
    // before the subscription flips that flag, but the commands themselves
    // degrade to empty if the session isn't ready yet (a later call refills).
    if (!isTauri()) {
      this.seedDemoSites();
      return;
    }
    const [listening, exposed, mappings] = await Promise.all([
      siteScan(),
      siteExposed(),
      siteMappings(),
    ]);
    this.myListening = listening;
    this.exposedSites = exposed;
    this.siteMappings = mappings.map((m) => this.mappingFromInfo(m));
  }

  /** Seed the Sites tab with demo data in web mode (no scan to run). */
  private seedDemoSites() {
    this.myListening = [
      { id: "tcp:5173", name: "HTTP", port: 5173, kind: "http", scheme: "http", loopback: true, process: "vite", title: "My Project — Dev" },
      { id: "tcp:8000", name: "HTTP", port: 8000, kind: "http", scheme: "http", loopback: true, process: "python", title: "" },
      { id: "tcp:22", name: "SSH", port: 22, kind: "ssh", scheme: "ssh", loopback: false, process: "sshd", title: "" },
    ];
    this.exposedSites = { "tcp:5173": "My Project — Dev" };
  }

  /** The default name to offer when exposing a discovered service: the page
   *  `<title>` the probe found, else the classified service name. */
  defaultSiteName(svc: ListeningService): string {
    return svc.title.trim() || svc.name;
  }

  private mappingFromInfo(info: { node: string; port: number; localPort: number }): SiteMapping {
    // Resolve the advert so the mapping carries a label + scheme for the UI.
    const advert = this.node(info.node)?.sites?.find((s) => s.port === info.port);
    return {
      node: info.node,
      site: advert?.id ?? `tcp:${info.port}`,
      port: info.port,
      localPort: info.localPort,
      scheme: advert?.scheme ?? "",
      label: advert?.label ?? `Port ${info.port}`,
    };
  }

  /** Whether this machine currently advertises a discovered service. */
  isExposed(siteId: string): boolean {
    return siteId in this.exposedSites;
  }

  /** The name this machine advertises a service under (empty = default). */
  exposeName(siteId: string): string {
    return this.exposedSites[siteId] ?? "";
  }

  /** Expose one of this machine's listening services to the mesh under
   *  `name` (the opt-in exposure choice). Idempotent re-naming: calling
   *  again with a new name just updates it. Pushes to the backend, which
   *  re-broadcasts presence so peers' Sites lists update. */
  async expose(siteId: string, name: string) {
    await this.pushExposed({ ...this.exposedSites, [siteId]: name });
  }

  /** Stop advertising a service. */
  async unexpose(siteId: string) {
    const next = { ...this.exposedSites };
    delete next[siteId];
    await this.pushExposed(next);
  }

  private async pushExposed(next: Record<string, string>) {
    this.exposedSites = next;
    if (this.backendConnected) {
      this.exposedSites = await siteSetExposed(next);
    }
  }

  /** Map a peer's site to a local port — sets up the reverse-proxy and binds
   *  a local listener, then records the mapping. Re-mapping an already-mapped
   *  site just returns its mapping. */
  async mapSite(nodeId: string, site: SiteAdvert) {
    const node = this.node(nodeId);
    if (!node) return;
    if (!this.sitesAllowed(node)) {
      this.toast("warn", `Sites are owner/fleet only — ${node.label} isn't yours`);
      return;
    }
    const existing = this.siteMappingFor(nodeId, site.id);
    if (existing) return;
    if (!this.backendConnected) {
      // Web preview: simulate a mapping so the flow is demoable.
      const localPort = this.demoLocalPort(site.port);
      this.siteMappings = [
        ...this.siteMappings,
        { node: nodeId, site: site.id, port: site.port, localPort, scheme: site.scheme ?? "", label: site.label },
      ];
      this.toast("ok", `Mapped ${node.label}'s ${site.label} to localhost:${localPort} (demo)`);
      return;
    }
    const r = await siteMap(nodeId, site.port);
    if (!r) {
      this.toast("warn", `Couldn't map ${site.label} from ${node.label}`);
      return;
    }
    this.siteMappings = [
      ...this.siteMappings,
      { node: nodeId, site: site.id, port: site.port, localPort: r.localPort, scheme: site.scheme ?? "", label: site.label },
    ];
    this.toast("ok", `${node.label}'s ${site.label} is at localhost:${r.localPort}`);
  }

  /** A demo local port that doesn't collide with an existing demo mapping. */
  private demoLocalPort(preferred: number): number {
    const taken = new Set(this.siteMappings.map((m) => m.localPort));
    if (preferred >= 1024 && !taken.has(preferred)) return preferred;
    let p = 47000;
    while (taken.has(p)) p += 1;
    return p;
  }

  /** Tear a site mapping down — unbinds the local listener and drops the
   *  route. */
  async unmapSite(nodeId: string, siteId: string) {
    const m = this.siteMappingFor(nodeId, siteId);
    if (!m) return;
    this.siteMappings = this.siteMappings.filter((x) => x !== m);
    if (this.backendConnected) await siteUnmap(m.node, m.port);
  }

  /** Open a mapped site in the system browser — a plain navigation to its
   *  local address (`http(s)://localhost:<port>`, defaulting to http for a
   *  bare TCP service so the button always does something). */
  openSite(m: SiteMapping) {
    const scheme = siteIsWeb(m) ? m.scheme : "http";
    void openExternal(`${scheme}://localhost:${m.localPort}`);
  }

  /** Copy a mapped site's `localhost:<port>` address to the clipboard — for
   *  pasting into whatever client speaks it (a DB tool, an ssh command). */
  copySite(m: SiteMapping) {
    const url = this.siteUrl(m);
    void navigator.clipboard?.writeText(url).then(
      () => this.toast("ok", `Copied ${url}`),
      () => this.toast("warn", `Reach it at ${url}`),
    );
  }

  // ---- managing a device's exposure (this machine *or* a fleet member) ---
  //
  // The drawer's "Its sites" controls work the same on your own machine and a
  // co-owned fleet member — locally it's the persisted set, remotely it's a
  // gated control message — so these verbs take the node id and dispatch.

  /** A managed machine's full discovered services (this machine: the live
   *  scan; a fleet member: its last reported list). */
  deviceServices(nodeId: string): ListeningService[] {
    if (this.isMe(nodeId)) return this.myListening;
    return this.remoteSites[canonicalNodeId(nodeId)]?.services ?? [];
  }

  private deviceExposed(nodeId: string): Record<string, string> {
    if (this.isMe(nodeId)) return this.exposedSites;
    return this.remoteSites[canonicalNodeId(nodeId)]?.exposed ?? {};
  }

  deviceIsExposed(nodeId: string, siteId: string): boolean {
    return siteId in this.deviceExposed(nodeId);
  }

  deviceExposeName(nodeId: string, siteId: string): string {
    return this.deviceExposed(nodeId)[siteId] ?? "";
  }

  /** Fetch a fleet member's site list so its drawer can manage exposure (a
   *  no-op for this machine, whose list is already live). The reply repaints
   *  the drawer via {@link applyNodeSites}. */
  ensureDeviceSites(nodeId: string) {
    if (this.isMe(nodeId) || !this.backendConnected) return;
    void siteRemoteList(nodeId);
  }

  /** Expose a service on a managed machine under `name` — locally persisted,
   *  or a gated control message to a fleet member. */
  async exposeOnDevice(nodeId: string, siteId: string, name: string) {
    if (this.isMe(nodeId)) {
      await this.expose(siteId, name);
      return;
    }
    await this.pushRemoteExposed(nodeId, { ...this.deviceExposed(nodeId), [siteId]: name });
  }

  /** Stop exposing a service on a managed machine. */
  async unexposeOnDevice(nodeId: string, siteId: string) {
    if (this.isMe(nodeId)) {
      await this.unexpose(siteId);
      return;
    }
    const next = { ...this.deviceExposed(nodeId) };
    delete next[siteId];
    await this.pushRemoteExposed(nodeId, next);
  }

  private async pushRemoteExposed(nodeId: string, next: Record<string, string>) {
    // Optimistic: reflect it locally so the drawer updates immediately; the
    // member re-advertises and a fresh list will confirm.
    const key = canonicalNodeId(nodeId);
    const cur = this.remoteSites[key];
    if (cur) this.remoteSites = { ...this.remoteSites, [key]: { ...cur, exposed: next } };
    if (this.backendConnected) await siteRemoteSetExposed(nodeId, next);
  }

  /** A fleet member answered with its site list (the `node-sites` reply). */
  private applyNodeSites(e: NodeSitesEvent) {
    const services: ListeningService[] = (e.services ?? []).map((s) => ({
      id: s.id,
      name: s.name,
      port: s.port,
      // SiteService carries no `kind`; the drawer keys off the scheme.
      kind: "",
      scheme: s.scheme,
      loopback: s.loopback,
      process: s.process,
      title: s.title,
    }));
    this.remoteSites = {
      ...this.remoteSites,
      [canonicalNodeId(e.from)]: { services, exposed: e.exposed ?? {} },
    };
  }

  // ---- ownership / claiming ---------------------------------------

  /** Adopt a device as one of yours. Honours Task 4: this only works when
   *  the device is *claimable* (booted in claim mode, still unowned) or
   *  already says you own it — you can't flat-out take a box that has an
   *  owner or was never offered. */
  claim(nodeId: string) {
    const n = this.node(nodeId);
    if (!n) return;
    // Compare ownership by canonical pubkey: the device advertises its owner
    // as a bare-pubkey id, while our `localId` is the display id.
    const ownedByMe = !!n.owner && sameMachine(n.owner, this.localId);
    if (n.owner && !ownedByMe) {
      this.toast("warn", `${n.label} is owned by another device — you can't take it`);
      return;
    }
    if (!n.claimable && !ownedByMe) {
      this.toast(
        "warn",
        `${n.label} isn't in claim mode. Start it claimable on the device itself to adopt it.`,
      );
      return;
    }
    if (this.backendConnected) {
      // The device confirms by re-advertising presence with owner = us (the
      // `claimed` ownership event toasts the success); a delivery failure
      // rejects, so the ask never silently goes nowhere.
      claimNode(nodeId).catch((e) => {
        this.toast("warn", `Couldn't ask ${n.label} to join: ${String(e)}`);
      });
      this.toast("info", `Asking ${n.label} to join your fleet…`);
    } else {
      // Demo/web: the claimable device accepts and joins the fleet, so the
      // "Owned" roster visibly grows under a shared key — exactly what the
      // backend does over the wire.
      n.owner = this.localId;
      n.claimable = false;
      n.relationship = { kind: "mine" };
      this.addToDemoFleet(n);
      this.toast("ok", `${n.label} joined your fleet`);
      this.reauthorize();
    }
  }

  /** Demo/web only: grow the simulated fleet roster so claiming groups devices
   *  under a shared key without a backend (the real one gossips it instead). */
  private addToDemoFleet(node: MeshNode) {
    const key = this.ownedFleet?.key || `demo-${Math.random().toString(16).slice(2, 10)}`;
    const members = [...(this.ownedFleet?.members ?? [])];
    const add = (id: string, label: string) => {
      if (!members.some((m) => sameMachine(m.device, id))) members.push({ device: id, label });
    };
    const me = this.node(this.localId);
    if (me) add(me.id, me.label);
    add(node.id, node.label);
    this.ownedFleet = { key, version: (this.ownedFleet?.version ?? 0) + 1, members };
  }

  /** Demo/web only: seed the fleet from the machines already marked yours, so
   *  the Fleet view isn't empty before you claim anything in the preview. */
  private seedDemoFleet() {
    const members = this.catalog.nodes
      .filter((n) => n.relationship.kind === "mine")
      .map((n) => ({ device: n.id, label: n.label }));
    if (members.length > 1) {
      this.ownedFleet = { key: "demo-fleet-key-7f3a91c2", version: 1, members };
    }
  }

  /** Demo/web only: a starter room so the rooms bar isn't empty in the
   *  preview (a stored rooms list wins — this never overwrites yours). */
  private seedDemoRoom() {
    if (this.rooms.length > 0) return;
    this.rooms.push({
      id: "room:this:demo",
      name: "Movie night",
      members: ["this", "desk", "tv"],
      owner: "this",
    });
  }

  /** Demo/web only: stand in two networks with their server configs and spread
   *  the demo devices across them, so the multi-network UI (the Servers +
   *  Devices panes, the per-node network chips) is alive in the preview. */
  private seedDemoNetworks() {
    this.networks = [
      { config_id: "net-home", network_id: "home-7f3a91c2x", label: "Home", phase: "joined" },
      { config_id: "net-work", network_id: "work-22ab90f1y", label: "Work", phase: "joined" },
    ];
    this.networkConfigs = [
      {
        id: "net-home",
        network_id: "home-7f3a91c2x",
        label: "Home",
        signaling: { servers: ["wss://myownmesh.com"] },
        stun_servers: [{ urls: ["stun:stun.myownmesh.com:3478"] }],
        turn_servers: [
          { urls: ["turn:turn.myownmesh.com:3478"], username: "guest", credential: "theguestpassword" },
        ],
      },
      {
        id: "net-work",
        network_id: "work-22ab90f1y",
        label: "Work",
        signaling: { servers: ["wss://relay.example.org"] },
        stun_servers: [{ urls: ["stun:stun.example.org:3478"] }],
        turn_servers: [],
      },
    ];
    this.serversNetwork = "net-home";
    // Spread the demo machines across the two networks — note some are on only
    // one, which is the whole point: you're not on a single "mesh".
    const assign: Record<string, string[]> = {
      this: ["Home", "Work"],
      desk: ["Home"],
      tv: ["Home"],
      studio: ["Work"],
      nuc: ["Work"],
      garage: ["Home"],
      alex: ["Work"],
    };
    for (const n of this.catalog.nodes) if (assign[n.id]) n.networks = assign[n.id];
  }

  /** Put *this* device into (or out of) claim mode so another of your
   *  machines can adopt it. */
  async setLocalClaimable(on: boolean) {
    const me = this.node(this.localId);
    if (this.backendConnected) {
      try {
        const now = await setClaimable(on);
        if (me) me.claimable = now ?? on;
        this.toast(
          on ? "info" : "ok",
          on ? "This device can now be adopted by another of your machines" : "Adoption turned off",
        );
      } catch (e) {
        this.toast("warn", `Couldn't change claim mode: ${errMsg(e)}`);
      }
    } else {
      if (me) me.claimable = on;
      this.toast("info", on ? "Adoption on (demo)" : "Adoption off (demo)");
    }
  }

  // ---- virtual rooms ------------------------------------------------
  //
  // A room is a zoom-like call between machines you pick. Joining wires
  // *nothing*: mic, camera and screen share all start off, and each
  // toggle fans ordinary routes out to the members (so authorization,
  // share prompts, and the live backend offer all apply unchanged).
  // Membership + chat ride the lightweight rooms channel; deleting a
  // room only forgets it on this device.

  /** Whether `node` speaks the rooms plane (invites, chat). An older
   *  build simply doesn't — the panel badges it. */
  roomsSupported(node: MeshNode | undefined): boolean {
    return !!node && isAppNode(node) && (node.features ?? []).includes(FEATURE_ROOMS);
  }

  /** The canonical id of a room's **host** — the device the room's
   *  identity, ownership and control plane belong to. The recorded owner
   *  wins; the id's anchor segment is the fallback for stubs. */
  roomHost(room: VirtualRoom): string | null {
    return room.owner ?? roomHostFromId(room.id);
  }

  /** Whether this device hosts `room`. A room with no traceable host (a
   *  pre-hosting save) answers to whoever holds the copy. */
  isRoomHost(room: VirtualRoom): boolean {
    const host = this.roomHost(room);
    return !host || this.isMe(host);
  }

  /** A room host's display label, for "hosted by …" lines — the person
   *  when one is known (the fleet's owner name, a share's person), the
   *  machine otherwise. */
  roomHostLabel(room: VirtualRoom): string {
    const host = this.roomHost(room);
    if (!host || this.isMe(host)) return "you";
    const node = this.machineByAnyId(host);
    return this.personNameFor(node) ?? node?.label ?? shortId(host);
  }

  /** The *person* behind a machine, when one is known: your fleet's
   *  owner name for machines of your own fleet, the share's person for
   *  someone else's. Null when only the machine itself is known. */
  personNameFor(node: MeshNode | undefined): string | null {
    if (!node) return null;
    if (node.relationship.kind === "shared") return node.relationship.person.name.trim() || null;
    if (node.relationship.kind === "mine" || this.isMe(node.id) || this.isFleetMember(node.id)) {
      return this.fleetName || null;
    }
    return null;
  }

  /** How a machine reads inside a room: **the owner's name first**
   *  ("Casey", with the machine dimmed alongside), because a call is
   *  between people — the machine name leads only when no person is
   *  known. Machine-specific surfaces (whose *screen*, whose *sound*)
   *  keep the machine visible as the secondary line. */
  roomWho(id: string): { who: string; machine: string | null; me: boolean } {
    const me = this.isMe(id);
    const node = this.machineByAnyId(id);
    const machine = node?.label ?? shortId(id);
    if (me) {
      const person = this.fleetName;
      return { who: person ? `${person} (you)` : "You", machine, me: true };
    }
    const person = this.personNameFor(node);
    if (person && person !== machine) return { who: person, machine, me: false };
    return { who: machine, machine: null, me: false };
  }

  /** A chat line's byline — the person when this device can resolve one,
   *  the label stamped at receive time otherwise (so lines survive a
   *  peer dropping off the graph). */
  roomChatWho(line: RoomChatLine): { who: string; machine: string | null } {
    if (this.isMe(line.from)) return { who: "You", machine: null };
    const node = this.machineByAnyId(line.from);
    if (!node) return { who: line.fromLabel, machine: null };
    const w = this.roomWho(line.from);
    return { who: w.who, machine: w.machine };
  }

  /** Whether this device is currently in `roomId` (joined the call). */
  isJoined(roomId: string): boolean {
    return this.joinedRoomIds.includes(roomId);
  }

  /** One room's send toggles (all-off until joined and flipped). */
  roomSendState(roomId: string): RoomSendState {
    return this.roomSend[roomId] ?? ROOM_SEND_OFF;
  }

  // The open room's toggles, for the panel's strip.
  roomMic = $derived.by(() => !!this.roomOpenId && this.roomSendState(this.roomOpenId).mic);
  roomCam = $derived.by(() => !!this.roomOpenId && this.roomSendState(this.roomOpenId).cam);
  roomScreen = $derived.by(() => !!this.roomOpenId && this.roomSendState(this.roomOpenId).screen);
  roomSound = $derived.by(() => !!this.roomOpenId && this.roomSendState(this.roomOpenId).sound);
  roomControl = $derived.by(() => !!this.roomOpenId && this.roomSendState(this.roomOpenId).control);

  private setRoomSend(roomId: string, channel: RoomChannel, on: boolean) {
    this.roomSend = {
      ...this.roomSend,
      [roomId]: { ...this.roomSendState(roomId), [channel]: on },
    };
  }

  /** The routes one room's toggles own, by channel. */
  private legsOf(roomId: string): Record<RoomChannel, string[]> {
    return (this.roomRoutes[roomId] ??= emptyRoomRoutes());
  }

  /** The room whose panel is open, if any. */
  openRoom = $derived.by(() =>
    this.roomOpenId ? this.rooms.find((r) => r.id === this.roomOpenId) ?? null : null,
  );

  /** The open room's members other than this machine, resolved to graph
   *  nodes (members we've never seen stay as bare ids — see the panel). */
  roomMemberNodes = $derived.by((): { id: string; node: MeshNode | undefined }[] => {
    const room = this.openRoom;
    if (!room) return [];
    return room.members
      .filter((m) => !this.isMe(m))
      .map((id) => ({ id, node: this.machineByAnyId(id) }));
  });

  /** Inbound shares for the open room: every active display (screen
   *  share) or video (camera) route from a member's machine into this
   *  one. These are the panel's video tiles — the same pull-and-paint
   *  plane the console uses. */
  roomInboundShares = $derived.by((): { route: Route; member: MeshNode }[] => {
    const room = this.openRoom;
    if (!room) return [];
    const out: { route: Route; member: MeshNode }[] = [];
    for (const r of this.catalog.routes) {
      if (r.media !== "display" && r.media !== "video") continue;
      const from = this.capabilityForDisplay(r.from);
      const to = this.capabilityForDisplay(r.to);
      if (!from || !to || !this.isMe(to.node) || this.isMe(from.node)) continue;
      const member = room.members.find((m) => sameMachine(m, from.node));
      const node = member ? this.machineByAnyId(member) : undefined;
      if (node) out.push({ route: r, member: node });
    }
    return out;
  });

  /** What a member is sending into this machine right now ("talking",
   *  "sharing sound"), read off the live audio routes' source origins. */
  roomMemberSends(memberNodeId: string): { mic: boolean; sound: boolean } {
    let mic = false;
    let sound = false;
    for (const r of this.catalog.routes) {
      if (r.media !== "audio") continue;
      const from = this.capability(r.from);
      const to = this.capability(r.to);
      if (!from || !to || !this.isMe(to.node) || !sameMachine(from.node, memberNodeId)) continue;
      if (from.origin === "system") sound = true;
      else mic = true;
    }
    return { mic, sound };
  }

  /** Nodes you can put in a room: machines on the graph running
   *  AllMyStuff, other than this one. (Unclaimed ones can chat once
   *  invited, but can't be routed to until claimed or shared.) */
  roomCandidateNodes = $derived.by(() =>
    this.catalog.nodes.filter((n) => !this.isMe(n.id) && isAppNode(n)),
  );

  /** What a fresh room is called when its maker doesn't say: named after
   *  the fleet's owner ("Casey's room"), falling back to this machine's
   *  label while the fleet is unnamed. */
  defaultRoomName(): string {
    const base = this.fleetName || this.node(this.localId)?.label || "My";
    return `${base}'s room`;
  }

  /** How `room` admits a knock. Absent (an old save, an older host's
   *  invite) reads invite-only — never more open than the host meant. */
  roomAccess(room: VirtualRoom): RoomAccess {
    return room.access ?? "invite";
  }

  /** Make a room — you're its **host**: the id is minted under this
   *  device, the roster and name answer to it, and closing it ends the
   *  room for everyone. A room of just this node is fine; invite
   *  machines later from its panel. `access` is the knock policy: an
   *  `open` room admits anyone who pastes its id; an `invite` room asks
   *  you first. */
  createRoom(name: string, memberIds: string[], access: RoomAccess = "invite") {
    const clean = name.trim() || this.defaultRoomName();
    const me = canonicalNodeId(this.localId);
    const members = [me, ...memberIds.map(canonicalNodeId).filter((m) => !this.isMe(m))];
    const room: VirtualRoom = { id: newRoomId(me), name: clean, members, owner: me, access };
    this.rooms.push(room);
    this.saveRooms();
    this.roomDraftOpen = false;
    this.toast(
      "ok",
      access === "open"
        ? `Made the open room “${clean}” — anyone you give its id can join`
        : `Made the room “${clean}” — you host it`,
    );
    this.broadcastRoom(room, this.inviteMessage(room));
  }

  /** The host's roster/name/access re-statement — every invite broadcast
   *  goes through here so no path forgets a field. */
  private inviteMessage(room: VirtualRoom): RoomWireMessage {
    return {
      room: room.id,
      name: room.name,
      kind: "invite",
      members: room.members,
      access: this.roomAccess(room),
    };
  }

  /** Add members to a room you host. Everyone (the new members included)
   *  converges on the re-stated roster. */
  addRoomMembers(roomId: string, memberIds: string[]) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room) return;
    if (!this.isRoomHost(room)) {
      this.toast("warn", "Only the room's host can invite members");
      return;
    }
    const add = memberIds
      .map(canonicalNodeId)
      .filter((m) => !this.isMe(m) && !room.members.some((x) => sameMachine(x, m)));
    if (add.length === 0) return;
    room.members = [...room.members, ...add];
    this.saveRooms();
    this.toast("ok", `Invited ${add.length} machine${add.length === 1 ? "" : "s"} to “${room.name}”`);
    this.broadcastRoom(room, this.inviteMessage(room));
    // The new members may now fetch what we're offering — widen the gate.
    this.refreshSharePeers(roomId);
  }

  /** Remove a member from a room you host. The replacement roster goes to
   *  the *old* member set, so the removed machine hears the roster it's
   *  absent from and drops the room. */
  removeRoomMember(roomId: string, memberId: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room) return;
    if (!this.isRoomHost(room)) {
      this.toast("warn", "Only the room's host can remove members");
      return;
    }
    const target = canonicalNodeId(memberId);
    if (this.isMe(target)) return; // the host closes, never removes itself
    const before = room.members;
    room.members = room.members.filter((m) => !sameMachine(m, target));
    if (room.members.length === before.length) return;
    this.saveRooms();
    this.presenceDrop(room.id, target);
    const label = this.machineByAnyId(target)?.label ?? shortId(target);
    this.toast("info", `Removed ${label} from “${room.name}”`);
    if (this.backendConnected) {
      const others = before.filter((m) => !this.isMe(m));
      if (others.length) void roomSend(others, this.inviteMessage(room));
    }
    // Drop their files from the list we host, stop allowing them to fetch
    // ours, and restate the pruned list to the (remaining) members.
    this.setHostShare(roomId, target, []);
    this.refreshSharePeers(roomId);
    this.rebroadcastHostShares(roomId);
  }

  /** Whether this device may rename `room` — the host's privilege. */
  canRenameRoom(room: VirtualRoom): boolean {
    return this.isRoomHost(room);
  }

  /** Flip a room you host between open and invite-only. Members converge
   *  on the re-stated invite; opening up also admits everyone already
   *  knocking (they asked, the door's now open). */
  setRoomAccess(roomId: string, access: RoomAccess) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room || !this.isRoomHost(room) || this.roomAccess(room) === access) return;
    room.access = access;
    this.saveRooms();
    this.toast(
      "ok",
      access === "open"
        ? `“${room.name}” is now open — its id is the invite`
        : `“${room.name}” is invite-only again`,
    );
    this.broadcastRoom(room, this.inviteMessage(room));
    if (access === "open") {
      for (const k of this.roomKnocks[roomId] ?? []) this.admitKnock(roomId, k.from);
    }
  }

  /** Rename a room (host only). Members converge via the re-stated
   *  invite, which carries the room's name and roster. */
  renameRoom(roomId: string, name: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    const clean = name.trim();
    if (!room || !clean || room.name === clean) return;
    if (!this.canRenameRoom(room)) {
      this.toast("warn", "Only the room's host can rename it");
      return;
    }
    room.name = clean;
    this.saveRooms();
    this.toast("ok", `Room renamed to “${clean}”`);
    this.broadcastRoom(room, this.inviteMessage(room));
  }

  /** Delete a room. From its **host** this closes the room for everyone
   *  (the room *is* the host's); from anyone else it only forgets the
   *  room on this device — you can't delete someone else's room. */
  deleteRoom(roomId: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room) return;
    const hosted = this.isRoomHost(room);
    const members = [...room.members];
    if (this.isJoined(roomId)) {
      this.unjoinRoom(roomId);
      if (!hosted) this.broadcastTo(members, room, { room: room.id, name: room.name, kind: "leave" });
    }
    if (hosted) {
      this.broadcastTo(members, room, { room: room.id, name: room.name, kind: "close" });
    }
    this.rooms = this.rooms.filter((r) => r.id !== roomId);
    const { [roomId]: _knocks, ...restKnocks } = this.roomKnocks;
    this.roomKnocks = restKnocks;
    this.saveRooms();
    this.toast(
      "info",
      hosted ? `Closed “${room.name}” for everyone` : `Removed “${room.name}” from this device`,
    );
  }

  /** Copy a room's join id — the `room:…` handle others paste into "Join
   *  with an id" to knock — to the clipboard. */
  async copyRoomId(roomId: string) {
    try {
      await navigator.clipboard.writeText(roomId);
      this.toast("ok", "Join ID copied");
    } catch {
      this.toast("warn", "Couldn't copy the join ID");
    }
  }

  /** Join a room (or bring an already-joined one back on screen). On the
   *  desktop this opens the room's *dedicated OS window* — the call lives
   *  there, movable and full-screenable like a console window; re-joining
   *  focuses it. The web preview (and the room windows themselves, via
   *  [`AppStore.joinRoomHere`]) keep the call in-page. */
  joinRoom(roomId: string) {
    if (!this.rooms.some((r) => r.id === roomId)) return;
    if (isTauri() && !roomWindowTarget()) {
      void openRoomWindow(roomId);
      return;
    }
    this.joinRoomHere(roomId);
  }

  /** Join the call *in this window* — the body of a room window (and the
   *  web preview's panel). Like sitting down muted: nothing is wired
   *  until a toggle is turned on. Being in several rooms at once is fine —
   *  the panel just shows one; use [`AppStore.closeRoomPanel`] to look
   *  away without hanging up. */
  joinRoomHere(roomId: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room) return;
    if (!this.isJoined(roomId)) {
      this.joinedRoomIds = [...this.joinedRoomIds, roomId];
      this.roomSend = { ...this.roomSend, [roomId]: { ...ROOM_SEND_OFF } };
      this.roomRoutes[roomId] = emptyRoomRoutes();
      this.roomJoinedAt = { ...this.roomJoinedAt, [roomId]: Date.now() };
      this.presenceAdd(roomId, canonicalNodeId(this.localId));
      this.callLog(`join ${roomId} — announcing presence to ${room.members.length - 1} member(s)`);
      this.broadcastRoom(room, { room: room.id, name: room.name, kind: "join" });
      void emitRoomLocal({ token: this.windowToken, kind: "join", room: roomId });
      // Re-announce any files we were already offering here (a rejoin), so
      // the host folds them back into the room's Shared Files list.
      if ((this.roomMyShares[roomId]?.length ?? 0) > 0) this.publishMyShares(roomId);
    }
    this.roomOpenId = roomId;
    this.roomChatOpen = false;
    this.roomPeopleOpen = false;
    this.roomUnread = { ...this.roomUnread, [roomId]: 0 };
  }

  /** Put the panel away without leaving — every joined room (and whatever
   *  it's sending) stays live. */
  closeRoomPanel() {
    this.roomOpenId = null;
  }

  /** Hang up one room: its toggles go off (tearing down exactly the
   *  routes that room wired — no other room's), and members see us go. */
  leaveRoom(roomId: string | null = this.roomOpenId) {
    if (!roomId || !this.isJoined(roomId)) return;
    const room = this.rooms.find((r) => r.id === roomId);
    this.unjoinRoom(roomId);
    if (room) this.broadcastRoom(room, { room: room.id, name: room.name, kind: "leave" });
  }

  /** Whether this device is in `roomId` in *any* of this app's windows. */
  isJoinedAnywhere(roomId: string): boolean {
    return this.isJoined(roomId) || this.roomsJoinedElsewhere.includes(roomId);
  }

  /** Hang up no matter which window holds the call: this window leaves
   *  directly; a room window is asked over the local bus (it leaves and
   *  closes itself). The rooms bar's "Leave". */
  leaveRoomEverywhere(roomId: string) {
    if (this.isJoined(roomId)) this.leaveRoom(roomId);
    if (this.roomsJoinedElsewhere.includes(roomId)) {
      void emitRoomLocal({ token: this.windowToken, kind: "leave-ask", room: roomId });
    }
  }

  /** The silent half of leaving (also the close / removed-by-host path):
   *  tear down this room's legs and drop the joined state, no broadcast. */
  private unjoinRoom(roomId: string) {
    for (const channel of ROOM_CHANNELS) this.dropRoomLegs(roomId, channel);
    delete this.roomRoutes[roomId];
    const { [roomId]: _gone, ...rest } = this.roomSend;
    this.roomSend = rest;
    const { [roomId]: _at, ...restAt } = this.roomJoinedAt;
    this.roomJoinedAt = restAt;
    this.joinedRoomIds = this.joinedRoomIds.filter((id) => id !== roomId);
    this.presenceDrop(roomId, canonicalNodeId(this.localId));
    // Stop offering our files here and forget the room's shared lists (the
    // `leave` we send tells the host to drop us from the list it hosts).
    this.clearRoomShares(roomId);
    if (this.roomOpenId === roomId) this.roomOpenId = null;
    void emitRoomLocal({ token: this.windowToken, kind: "leave", room: roomId });
  }

  /** One event off the same-device room bus (another window of this app
   *  talking; our own echoes are dropped by token). */
  private handleRoomLocal(e: { token: string; kind: string; room?: string; from?: string }) {
    if (e.token === this.windowToken) return;
    switch (e.kind) {
      case "knock-done": {
        if (e.room && e.from) this.dropKnock(e.room, e.from, false);
        break;
      }
      case "join": {
        if (e.room && !this.roomsJoinedElsewhere.includes(e.room)) {
          this.roomsJoinedElsewhere = [...this.roomsJoinedElsewhere, e.room];
        }
        break;
      }
      case "leave": {
        if (e.room) {
          this.roomsJoinedElsewhere = this.roomsJoinedElsewhere.filter((id) => id !== e.room);
        }
        break;
      }
      case "leave-ask": {
        // Whichever window holds the call joined hangs up; everyone else
        // has nothing joined and no-ops.
        if (e.room && this.isJoined(e.room)) this.leaveRoom(e.room);
        break;
      }
      case "sync": {
        // The saved rooms list changed in another window (a rename, a
        // delete, a fresh invite). Reload it; a joined room that vanished
        // (the host here closed it from the main window) unjoins quietly —
        // its room window notices and closes itself.
        this.loadRooms();
        for (const id of [...this.joinedRoomIds]) {
          if (!this.rooms.some((r) => r.id === id)) this.unjoinRoom(id);
        }
        break;
      }
    }
  }

  /** Talk to the room: your **microphone** to every member's speakers —
   *  the call itself. (Sharing what this machine is *playing* is the
   *  separate "share sound" toggle.) */
  toggleRoomMic() {
    const roomId = this.roomOpenId;
    if (!roomId) return;
    if (this.roomSendState(roomId).mic) {
      this.dropRoomLegs(roomId, "mic");
      this.setRoomSend(roomId, "mic", false);
      return;
    }
    const from = this.localAudioSource("mic");
    if (!from) {
      this.toast("warn", "No microphone on this machine");
      return;
    }
    const wired = this.wireRoomLegs(roomId, "mic", from, "audio");
    this.setRoomSend(roomId, "mic", wired > 0);
    if (wired > 0) this.toastLegs("Your mic is live", wired);
    else this.toast("warn", "Nobody in the room can receive audio right now");
  }

  /** Share this machine's **sound** — what it's playing, captured off the
   *  loopback — to every member's speakers. Deliberately not the mic:
   *  use the mic toggle to talk. */
  toggleRoomSound() {
    const roomId = this.roomOpenId;
    if (!roomId) return;
    if (this.roomSendState(roomId).sound) {
      this.dropRoomLegs(roomId, "sound");
      this.setRoomSend(roomId, "sound", false);
      return;
    }
    const from = this.localAudioSource("system");
    if (!from) {
      this.toast("warn", "This machine exposes no system audio");
      return;
    }
    const wired = this.wireRoomLegs(roomId, "sound", from, "audio");
    this.setRoomSend(roomId, "sound", wired > 0);
    if (wired > 0) this.toastLegs("Sharing this machine's sound", wired);
    else this.toast("warn", "Nobody in the room can receive audio right now");
  }

  /** This machine's shareable screens — the display sources behind the
   *  "Share screen" picker. A multi-monitor machine advertises one per
   *  monitor (`screen` for the primary, `screen:<id>` for the rest — see
   *  the bridge's `capabilities_with_screens`), so the picker is how you
   *  choose *which* to share, the way every call app does. The primary
   *  (the `default` one) sorts first. */
  roomScreenSources = $derived.by((): Capability[] =>
    this.capsOf(this.localId)
      .filter((c) => c.media === "display" && canSource(c.flow) && c.origin === "screen")
      .sort((a, b) => Number(b.default ?? false) - Number(a.default ?? false)),
  );

  /** Share your screen with the room: this machine's screen to every
   *  member's display. Members see it as a tile in their room panel.
   *  `sourceId` picks one of [`roomScreenSources`] (the screen-selection
   *  popup); omitted, it shares the primary — the single-monitor path,
   *  where there's nothing to choose. */
  toggleRoomScreen(sourceId?: string) {
    const roomId = this.roomOpenId;
    if (!roomId) return;
    if (this.roomSendState(roomId).screen) {
      this.dropRoomLegs(roomId, "screen");
      this.setRoomSend(roomId, "screen", false);
      return;
    }
    const sources = this.roomScreenSources;
    const from = sourceId ? sources.find((c) => c.id === sourceId) : sources[0];
    if (!from) {
      this.toast("warn", "This machine exposes no screen");
      return;
    }
    const wired = this.wireRoomLegs(roomId, "screen", from, "display");
    this.setRoomSend(roomId, "screen", wired > 0);
    if (wired > 0) this.toastLegs(`Sharing ${sources.length > 1 ? from.label : "your screen"}`, wired);
    else this.toast("warn", "Nobody in the room can receive a screen right now");
  }

  /** Send your camera to the room: this machine's default camera to every
   *  member's video sink. Members see it as a live tile, exactly like a
   *  screen share — same routes, same transport, a camera behind the
   *  capture instead of a monitor. */
  toggleRoomCam() {
    const roomId = this.roomOpenId;
    if (!roomId) return;
    if (this.roomSendState(roomId).cam) {
      this.dropRoomLegs(roomId, "cam");
      this.setRoomSend(roomId, "cam", false);
      return;
    }
    const from = this.capsOf(this.localId)
      .filter((c) => c.media === "video" && canSource(c.flow) && c.origin === "camera")
      .sort((a, b) => Number(b.default ?? false) - Number(a.default ?? false))[0];
    if (!from) {
      this.toast("warn", "No camera on this machine");
      return;
    }
    const wired = this.wireRoomLegs(roomId, "cam", from, "video");
    this.setRoomSend(roomId, "cam", wired > 0);
    if (wired > 0) this.toastLegs("Your camera is live", wired);
    else this.toast("warn", "Nobody in the room can receive camera video right now");
  }

  /** Let the room drive this machine: each member's keyboard & mouse is
   *  wired to this machine's control. Members then click and type over
   *  your screen-share tile. Injection stays gated on the far side's
   *  facts: only your owner/fleet can actually move things (a guest's
   *  events are dropped until share-gated control lands). */
  toggleRoomControl() {
    const roomId = this.roomOpenId;
    if (!roomId) return;
    if (this.roomSendState(roomId).control) {
      this.dropRoomLegs(roomId, "control");
      this.setRoomSend(roomId, "control", false);
      return;
    }
    const mySink = matchEndpoint(this.catalog, this.localId, "input", "consume");
    if (!mySink) {
      this.toast("warn", "This machine exposes no control endpoint");
      return;
    }
    let wired = 0;
    for (const { node } of this.roomMemberNodes) {
      if (!node || !isAppNode(node) || !node.online) continue;
      if (node.relationship.kind === "unclaimed") continue;
      const theirSrc = matchEndpoint(this.catalog, node.id, "input", "provide");
      if (!theirSrc) continue;
      const leg = this.roomConnect(theirSrc.id, mySink.id);
      if (leg?.created) this.legsOf(roomId).control.push(leg.id);
      if (leg) wired += 1;
    }
    this.setRoomSend(roomId, "control", wired > 0);
    if (wired > 0) this.toastLegs("Members can drive this machine", wired);
    else this.toast("warn", "No member can send control right now");
  }

  /** Send a chat line to the room. */
  sendRoomChat(text: string) {
    const room = this.openRoom;
    const line = text.trim();
    if (!room || !line) return;
    this.appendRoomChat(room.id, {
      from: canonicalNodeId(this.localId),
      fromLabel: this.node(this.localId)?.label ?? "Me",
      text: line,
      at: Date.now(),
    });
    this.broadcastRoom(room, { room: room.id, name: room.name, kind: "chat", text: line });
  }

  // ---- Shared Files (the call's shared-download area) ----------------
  //
  // A call's file sharing is deliberately *not* the file manager: you
  // offer specific files into a room area members download from — never a
  // window onto your disk, and never a way to edit or browse anyone's
  // files. The room's **host** hosts the *list* (it aggregates every
  // member's offerings and restates the whole, like the roster), so a
  // file stays listed as long as its uploader is in the call; the bytes
  // ride peer-to-peer straight from the uploader, never through the host.

  /** One entry of the open room's Shared Files area, resolved for the
   *  panel: the file, who offered it, and whether that's you. */
  roomSharedFiles = $derived.by(
    (): { from: string; me: boolean; who: string; machine: string | null; file: SharedFileMeta }[] => {
      const room = this.openRoom;
      if (!room) return [];
      // The host renders from its own aggregate (filtered to who's still
      // present/online — a file is offered only while its uploader is);
      // everyone else renders the list the host sent them. Our *own*
      // offerings are merged in directly too, so they show the instant we
      // share (before the host's list echoes back, and in demo mode).
      const me = canonicalNodeId(this.localId);
      const authoritative: SharedEntry[] = this.isRoomHost(room)
        ? this.hostSharedEntries(room.id)
        : this.roomSharesFromHost[room.id] ?? [];
      const seen = new Set<string>();
      const entries: SharedEntry[] = [];
      for (const f of this.roomMyShares[room.id] ?? []) {
        seen.add(f.token);
        entries.push({ from: me, ...f });
      }
      for (const e of authoritative) {
        if (seen.has(e.token)) continue;
        seen.add(e.token);
        entries.push(e);
      }
      return entries.map((e) => {
        const who = this.roomWho(e.from);
        return {
          from: e.from,
          me: this.isMe(e.from),
          who: this.isMe(e.from) ? "You" : who.who,
          machine: this.isMe(e.from) ? null : who.machine,
          file: { token: e.token, name: e.name, size: e.size },
        };
      });
    },
  );

  /** The host's aggregated Shared Files list for `roomId`, flattened to
   *  entries and pruned to uploaders that are still present *and* online —
   *  the "available as long as the uploader is online" rule. */
  private hostSharedEntries(roomId: string): SharedEntry[] {
    const byUploader = this.roomHostShares[roomId] ?? {};
    const present = this.roomPresence[roomId] ?? [];
    const out: SharedEntry[] = [];
    for (const [from, files] of Object.entries(byUploader)) {
      const here = this.isMe(from) || present.some((m) => sameMachine(m, from));
      const online = this.isMe(from) || !!this.machineByAnyId(from)?.online;
      if (!here || !online) continue;
      for (const f of files) out.push({ from, ...f });
    }
    return out;
  }

  /** Offer files into the open room's Shared Files area. Opens the OS file
   *  picker, registers the picked files with the backend (allowing the
   *  room's members to fetch them), adds them to our list, and publishes
   *  the change — to the host if we're a member, or straight into the room
   *  if we host it. In demo/web mode there's no picker or transport, so it
   *  drops in a placeholder so the area is still explorable. */
  async shareRoomFiles() {
    const room = this.openRoom;
    if (!room) return;
    if (!this.backendConnected) {
      const n = (this.roomMyShares[room.id]?.length ?? 0) + 1;
      this.addMyShares(room.id, [{ token: `demo_${Date.now().toString(36)}`, name: `shared-file-${n}.txt`, size: 1024 * n }]);
      this.toast("info", "Demo mode — sharing files needs the desktop app on a live mesh");
      return;
    }
    const paths = await pickFilesToShare();
    if (paths.length === 0) return;
    const metas = await roomShareFiles(room.members, paths);
    if (metas.length === 0) {
      this.toast("warn", "Couldn't read those files to share them");
      return;
    }
    this.addMyShares(room.id, metas);
    this.toast("ok", `Sharing ${metas.length} file${metas.length === 1 ? "" : "s"} with the room`);
  }

  /** Stop offering one of *your* shared files (the ✕ on your entry).
   *  Drops it from the backend registry and re-publishes the list. */
  unshareRoomFile(roomId: string, token: string) {
    const mine = this.roomMyShares[roomId] ?? [];
    if (!mine.some((f) => f.token === token)) return;
    this.roomMyShares = { ...this.roomMyShares, [roomId]: mine.filter((f) => f.token !== token) };
    if (this.backendConnected) void roomUnshare([token]);
    this.publishMyShares(roomId);
  }

  /** Add files to our offer list for a room and publish the change. */
  private addMyShares(roomId: string, metas: SharedFileMeta[]) {
    const mine = this.roomMyShares[roomId] ?? [];
    this.roomMyShares = { ...this.roomMyShares, [roomId]: [...mine, ...metas] };
    this.publishMyShares(roomId);
  }

  /** Make our current offer list for `roomId` known. If we host the room,
   *  fold it into the aggregate and restate the whole list to members; if
   *  we're a member, tell the host (it's the catalog). */
  private publishMyShares(roomId: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room) return;
    const mine = this.roomMyShares[roomId] ?? [];
    if (this.isRoomHost(room)) {
      this.setHostShare(roomId, canonicalNodeId(this.localId), mine);
      this.rebroadcastHostShares(roomId);
    } else {
      const host = this.roomHost(room);
      if (host && this.backendConnected) {
        void roomSend([host], { room: roomId, name: room.name, kind: "share_list", files: mine });
      }
    }
  }

  /** Host side: record one uploader's offer list in the aggregate. */
  private setHostShare(roomId: string, uploader: string, files: SharedFileMeta[]) {
    const room = (this.roomHostShares[roomId] ??= {});
    if (files.length === 0) delete room[canonicalNodeId(uploader)];
    else room[canonicalNodeId(uploader)] = files;
  }

  /** Host side: restate the room's whole Shared Files list to its members
   *  (replacement semantics — exactly like the roster). Pruned to present,
   *  online uploaders, so a file drops off the moment its uploader leaves. */
  private rebroadcastHostShares(roomId: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room || !this.isRoomHost(room) || !this.backendConnected) return;
    const files = this.hostSharedEntries(roomId);
    this.broadcastRoom(room, { room: roomId, name: room.name, kind: "shares", files });
  }

  /** Download one shared file — peer-to-peer from its uploader, by token.
   *  Opens a `:shared` fetch route to them, registers the disk sink, and
   *  sends the fetch; progress + completion arrive on the file-progress /
   *  file-saved events, and the route is torn down once it lands. */
  async downloadSharedFile(from: string, file: SharedFileMeta) {
    if (this.isMe(from)) return; // your own file already lives here
    if (!this.backendConnected) {
      this.toast("info", "Demo mode — downloading shared files needs the desktop app");
      return;
    }
    const existing = this.sharedDownloads[file.token];
    if (existing && existing.state === "fetching") return; // already going
    const routeId = this.sharedConnect(from);
    const req = this.sharedReqSeq++;
    try {
      const dest = await fileDownload(routeId, req, file.name);
      this.sharedReqToken[`${routeId}:${req}`] = file.token;
      this.sharedDownloads = {
        ...this.sharedDownloads,
        [file.token]: { token: file.token, name: file.name, done: 0, total: file.size, state: "fetching", note: dest },
      };
      await fileSend(routeId, { kind: "fetch", req, token: file.token });
    } catch (e) {
      void disconnectRoute(routeId);
      this.toast("warn", `Couldn't start the download: ${errMsg(e)}`);
    }
  }

  /** Mint a fresh Shared Files fetch route to `host` — a generic route
   *  from their `:shared` endpoint to a viewer endpoint minted here.
   *  Mirrors [`AppStore.filesConnect`], but `:shared` is token-gated, not
   *  owner/fleet, so any room member may open one. One route per download;
   *  it's torn down when the file lands ([`AppStore.onSharedSaved`]). */
  private sharedConnect(host: string): string {
    const fromEp = `${host}:shared`;
    const n = ++this.sharedViewSeq;
    const toEp = `${this.localId}:shared-view:${Date.now().toString(36)}-${n}`;
    void connectRoute(fromEp, toEp, "generic");
    return `route:${fromEp}→${toEp}`;
  }

  /** A shared download reported progress — find its row by route+req. */
  private onSharedProgress(e: { route: string; req: number; written: number; total: number }) {
    const token = this.sharedReqToken[`${e.route}:${e.req}`];
    const cur = token ? this.sharedDownloads[token] : undefined;
    if (!token || !cur) return;
    this.sharedDownloads = {
      ...this.sharedDownloads,
      [token]: { ...cur, done: e.written, total: e.total || cur.total },
    };
  }

  /** A shared download finished (or failed) — land the row's final state
   *  and tear the one-shot fetch route down. */
  private onSharedSaved(e: { route: string; req: number; path: string | null; error: string | null }) {
    const key = `${e.route}:${e.req}`;
    const token = this.sharedReqToken[key];
    const cur = token ? this.sharedDownloads[token] : undefined;
    if (!token || !cur) return;
    delete this.sharedReqToken[key];
    void disconnectRoute(e.route);
    this.sharedDownloads = {
      ...this.sharedDownloads,
      [token]: e.error
        ? { ...cur, state: "error", note: e.error }
        : { ...cur, state: "done", done: cur.total, note: e.path ?? cur.note },
    };
    if (e.error) this.toast("warn", `Download failed: ${e.error}`);
    else this.toast("ok", `Saved “${cur.name}” to your Downloads`);
  }

  /** Tear down our shared-files state for a room that's ending for us
   *  (we left, were removed, or the host closed it): stop offering our
   *  files (backend) and forget every shared-files list. No broadcast —
   *  our `leave` already tells the host to drop us, and the room may be
   *  gone. Idempotent. */
  private clearRoomShares(roomId: string) {
    const mine = this.roomMyShares[roomId] ?? [];
    if (mine.length && this.backendConnected) void roomUnshare(mine.map((f) => f.token));
    const { [roomId]: _mine, ...restMine } = this.roomMyShares;
    this.roomMyShares = restMine;
    const { [roomId]: _host, ...restFromHost } = this.roomSharesFromHost;
    this.roomSharesFromHost = restFromHost;
    delete this.roomHostShares[roomId];
  }

  /** Refresh the backend's allow-list for our offered files when a room we
   *  *host* changes roster — the new member set may now (or no longer) be
   *  allowed to fetch what we're sharing. */
  private refreshSharePeers(roomId: string) {
    const mine = this.roomMyShares[roomId] ?? [];
    const room = this.rooms.find((r) => r.id === roomId);
    if (mine.length && room && this.backendConnected) {
      void roomSetSharePeers(mine.map((f) => f.token), room.members);
    }
  }

  /** Handle one inbound room-plane message. */
  handleRoomMessage(from: string, msg: RoomWireMessage) {
    const sender = canonicalNodeId(from);
    if (this.isMe(sender)) return;
    const senderLabel = this.machineByAnyId(sender)?.label ?? shortId(sender);
    const existing = this.rooms.find((r) => r.id === msg.room);
    switch (msg.kind) {
      case "invite": {
        // The roster as the host states it — replacement semantics, so a
        // member that's no longer listed is *out*. Never force-add anyone.
        const members = [...new Set(msg.members.map(canonicalNodeId))];
        if (!members.some((m) => sameMachine(m, sender))) members.push(sender);
        const listsMe = members.some((m) => this.isMe(m));
        if (existing) {
          // The room is its host's: roster, name and access answer to the
          // host alone (the mesh authenticates `from`, so this is real).
          const host = this.roomHost(existing);
          if (host && !sameMachine(host, sender)) return;
          if (!listsMe) {
            // The host's new roster no longer lists us — we're out.
            if (this.isJoined(existing.id)) this.unjoinRoom(existing.id);
            this.clearRoomShares(existing.id);
            this.rooms = this.rooms.filter((r) => r.id !== existing.id);
            this.saveRooms();
            this.toast("info", `${senderLabel} removed this device from “${existing.name}”`);
            return;
          }
          existing.name = msg.name?.trim() || existing.name;
          existing.members = members;
          existing.access = msg.access ?? existing.access;
          // Adopt the inviter as host on a copy that predates the field
          // (a chat-minted stub, an old save).
          existing.owner ??= sender;
        } else {
          if (!listsMe) return; // someone else's room — not ours to keep
          this.rooms.push({
            id: msg.room,
            name: msg.name?.trim() || "Room",
            members,
            owner: sender,
            access: msg.access,
          });
          this.toast("info", `${senderLabel} added you to “${msg.name?.trim() || "a room"}”`);
        }
        this.saveRooms();
        // The roster the host just restated may add or drop members the
        // files we're offering are allowed to reach — refresh that gate.
        this.refreshSharePeers(msg.room);
        // A knock answered: the roster now lists us — walk right in.
        if (this.pendingKnocks.includes(msg.room)) {
          this.pendingKnocks = this.pendingKnocks.filter((id) => id !== msg.room);
          this.toast("ok", `You're in “${msg.name?.trim() || existing?.name || "the room"}”`);
          this.joinRoom(msg.room);
        }
        break;
      }
      case "join": {
        this.callLog(
          `recv join from ${senderLabel} for ${msg.room}${existing ? "" : " — unknown room, ignored"}`,
        );
        if (existing) {
          const knewThem = (this.roomPresence[existing.id] ?? []).includes(sender);
          this.presenceAdd(msg.room, sender);
          // Presence echo: a newcomer can't know who was already in the
          // call (joins are only broadcast as they happen) — so if *we're*
          // in, say so straight back to them. Echoes terminate because
          // only a first appearance triggers one.
          if (!knewThem && this.isJoined(existing.id) && this.backendConnected) {
            this.callLog(`  echoing our presence back to ${senderLabel}`);
            void roomSend([sender], { room: existing.id, name: existing.name, kind: "join" });
          }
          // We host the list: a newcomer can't know what's already shared,
          // so restate the room's Shared Files list now they're present.
          if (this.isRoomHost(existing)) this.rebroadcastHostShares(existing.id);
        }
        break;
      }
      case "leave": {
        this.callLog(`recv leave from ${senderLabel} for ${msg.room}`);
        this.presenceDrop(msg.room, sender);
        // The list lives with the host: when an uploader leaves, their
        // files come off it (the bytes were only theirs to serve).
        if (existing && this.isRoomHost(existing)) {
          this.setHostShare(existing.id, sender, []);
          this.rebroadcastHostShares(existing.id);
        }
        break;
      }
      case "close": {
        // The host ended the room for everyone. From anyone else it's
        // noise (the authenticated sender must be the host).
        if (!existing) return;
        const host = this.roomHost(existing);
        if (host && !sameMachine(host, sender)) return;
        if (this.isJoined(existing.id)) this.unjoinRoom(existing.id);
        this.clearRoomShares(existing.id);
        this.rooms = this.rooms.filter((r) => r.id !== existing.id);
        this.saveRooms();
        this.toast("info", `${senderLabel} closed “${existing.name}”`);
        break;
      }
      case "share_list": {
        // A member tells us (the host) what it's offering. Only the host
        // aggregates; from anyone to a non-host it's noise.
        if (!existing || !this.isRoomHost(existing)) return;
        this.setHostShare(existing.id, sender, msg.files);
        this.rebroadcastHostShares(existing.id);
        break;
      }
      case "shares": {
        // The host's authoritative Shared Files list — believed only from
        // the host (the mesh authenticates `from`).
        if (!existing) return;
        const host = this.roomHost(existing);
        if (host && !sameMachine(host, sender)) return;
        this.roomSharesFromHost = { ...this.roomSharesFromHost, [existing.id]: msg.files };
        break;
      }
      case "chat": {
        // A chat for a room we don't know yet still lands — mint a stub
        // (the proper roster arrives with the next invite).
        if (!existing) {
          this.rooms.push({
            id: msg.room,
            name: msg.name?.trim() || "Room",
            members: [canonicalNodeId(this.localId), sender],
          });
          this.saveRooms();
        }
        this.appendRoomChat(msg.room, {
          from: sender,
          fromLabel: senderLabel,
          text: msg.text,
          at: Date.now(),
        });
        break;
      }
      case "knock": {
        // Someone holding the room's id (no invite) asks to join. Only
        // the room's host answers; a knock on a room we don't host (or
        // don't know) is noise. Every window of the host queues the ask
        // (the room window's People panel is where it's admitted), but
        // anything *sent* in reply happens in the main window alone — a
        // host with the room's window open mustn't answer twice.
        if (!existing || !this.isRoomHost(existing)) return;
        if (existing.members.some((m) => sameMachine(m, sender))) {
          // Already on the roster — their invite must have gone missing
          // (a reinstall, an offline gap). Re-state it just to them.
          if (this.isMainWindow && this.backendConnected) {
            void roomSend([sender], this.inviteMessage(existing));
          }
          return;
        }
        if (this.roomAccess(existing) === "open") {
          if (this.isMainWindow) {
            existing.members = [...existing.members, sender];
            this.saveRooms();
            this.toast("ok", `${senderLabel} joined the open room “${existing.name}”`);
            this.broadcastRoom(existing, this.inviteMessage(existing));
          }
          return;
        }
        // Invite-only: queue the ask for the panel's admit/deny.
        const cur = this.roomKnocks[existing.id] ?? [];
        if (!cur.some((k) => sameMachine(k.from, sender))) {
          this.roomKnocks = {
            ...this.roomKnocks,
            [existing.id]: [...cur, { from: sender, label: senderLabel, at: Date.now() }],
          };
          this.toast("info", `${senderLabel} asks to join “${existing.name}” — admit them from the room`);
        }
        break;
      }
      case "deny": {
        // The host's "no" to our knock — only believable from the host.
        if (!this.pendingKnocks.includes(msg.room)) return;
        const host = existing ? this.roomHost(existing) : roomHostFromId(msg.room);
        if (host && !sameMachine(host, sender)) return;
        this.pendingKnocks = this.pendingKnocks.filter((id) => id !== msg.room);
        this.toast("warn", `The host declined your ask to join${existing ? ` “${existing.name}”` : ""}`);
        break;
      }
    }
  }

  /** Ask to join a room this device wasn't invited to, by its pasted id
   *  (`room:<host>:<nonce>` — the host's device id is the anchor). An
   *  open room admits you on the spot; an invite-only host is asked and
   *  can admit or deny. A room already on the list just joins. */
  async knockRoom(code: string): Promise<boolean> {
    const id = code.trim();
    if (!id) return false;
    const known = this.rooms.find((r) => r.id === id);
    if (known) {
      this.joinRoom(known.id);
      return true;
    }
    const host = roomHostFromId(id);
    if (!host) {
      this.toast("warn", "That doesn't look like a room id (room:<host>:<code>)");
      return false;
    }
    if (this.isMe(host)) {
      this.toast("warn", "That's one of this device's own rooms — but not on its list anymore");
      return false;
    }
    if (!this.backendConnected) {
      this.toast("info", "Demo mode — knocking needs the desktop app on a live mesh");
      return false;
    }
    if (!this.pendingKnocks.includes(id)) {
      this.pendingKnocks = [...this.pendingKnocks, id];
    }
    const sent = await roomSend([host], { room: id, kind: "knock" });
    if (sent === 0) {
      this.pendingKnocks = this.pendingKnocks.filter((r) => r !== id);
      this.toast("warn", "Couldn't reach the room's host — are you on a shared network?");
      return false;
    }
    this.toast("ok", "Asked to join — if the room is open you'll be let straight in");
    return true;
  }

  /** Admit one knock on a room you host: onto the roster, roster restated
   *  to everyone (the knocker included — that invite is their way in).
   *  The answered ask is cleared in every window of this app. */
  admitKnock(roomId: string, from: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room || !this.isRoomHost(room)) return;
    this.dropKnock(roomId, from);
    if (!room.members.some((m) => sameMachine(m, from))) {
      room.members = [...room.members, canonicalNodeId(from)];
      this.saveRooms();
      this.broadcastRoom(room, this.inviteMessage(room));
      // The admitted machine may now fetch what we're offering.
      this.refreshSharePeers(roomId);
    }
    const label = this.machineByAnyId(from)?.label ?? shortId(from);
    this.toast("ok", `Let ${label} into “${room.name}”`);
  }

  /** Turn one knock away (the asker hears a `deny`, not silence). */
  denyKnock(roomId: string, from: string) {
    const room = this.rooms.find((r) => r.id === roomId);
    if (!room || !this.isRoomHost(room)) return;
    this.dropKnock(roomId, from);
    if (this.backendConnected) {
      void roomSend([from], { room: room.id, name: room.name, kind: "deny" });
    }
  }

  private dropKnock(roomId: string, from: string, announce = true) {
    const cur = this.roomKnocks[roomId] ?? [];
    this.roomKnocks = {
      ...this.roomKnocks,
      [roomId]: cur.filter((k) => !sameMachine(k.from, from)),
    };
    // Every window queued the ask; the one that answered clears the rest.
    if (announce) {
      void emitRoomLocal({ token: this.windowToken, kind: "knock-done", room: roomId, from });
    }
  }

  // The rooms plane's local helpers.

  private appendRoomChat(roomId: string, line: RoomChatLine) {
    this.roomChat = {
      ...this.roomChat,
      [roomId]: [...(this.roomChat[roomId] ?? []), line].slice(-200),
    };
    // Unread unless the line landed where you're already reading: this
    // room's panel with the chat sidebar showing.
    if (this.roomOpenId !== roomId || !this.roomChatOpen) {
      this.roomUnread = { ...this.roomUnread, [roomId]: (this.roomUnread[roomId] ?? 0) + 1 };
    }
  }

  /** One room-call diagnostic line, mirrored to the backend log (see
   *  [`clientLog`]). The call plane is otherwise opaque from the outside:
   *  a toggle that wires nothing, a `join` that never lands, and a healthy
   *  muted call all read identically. The `[room-call]` tag makes the
   *  whole decision trail greppable in one `ALLMYSTUFF_GUI_LOG` capture. */
  private callLog(line: string) {
    clientLog(`[room-call] ${line}`);
  }

  private presenceAdd(roomId: string, member: string) {
    const cur = this.roomPresence[roomId] ?? [];
    if (!cur.includes(member)) {
      this.roomPresence = { ...this.roomPresence, [roomId]: [...cur, member] };
      this.callLog(
        `presence +${this.roomWho(member).who} in ${roomId} — ${(this.roomPresence[roomId] ?? []).length} present`,
      );
    }
  }

  private presenceDrop(roomId: string, member: string) {
    const cur = this.roomPresence[roomId] ?? [];
    const had = cur.includes(member);
    this.roomPresence = { ...this.roomPresence, [roomId]: cur.filter((m) => m !== member) };
    if (had) {
      this.callLog(
        `presence -${this.roomWho(member).who} from ${roomId} — ${(this.roomPresence[roomId] ?? []).length} present`,
      );
    }
  }

  /** Fan one room-plane message at every member but us. Fire-and-forget:
   *  the plane has no acks, and presence heals gaps. */
  private broadcastRoom(room: VirtualRoom, msg: RoomWireMessage) {
    this.broadcastTo(room.members, room, msg);
  }

  /** Like [`AppStore.broadcastRoom`] but to an explicit member list — the
   *  close and removal paths, where the audience is the roster *before*
   *  the change. */
  private broadcastTo(members: string[], _room: VirtualRoom, msg: RoomWireMessage) {
    if (!this.backendConnected) return;
    const others = members.filter((m) => !this.isMe(m));
    if (!others.length) return;
    void roomSend(others, msg).then((n) => {
      // The fan-out's reach, for the kinds presence rides on: a join
      // delivered to 0 peers is a roster that will never fill — and unlike
      // chat (sent later, once links are warm) a join fires at the instant
      // of joining, when a link may still be mid-handshake.
      if (msg.kind === "join" || msg.kind === "leave" || msg.kind === "invite") {
        this.callLog(`sent "${msg.kind}" → ${n}/${others.length} member(s) of ${msg.room}`);
      }
    });
  }

  /** This machine's audio source for a room leg: the loopback for
   *  `system`, otherwise the best non-system capture (the default mic
   *  first). The split is the whole point — the mic toggle must never
   *  quietly fall back to the loopback, or "talk" becomes "broadcast
   *  everything this machine plays". */
  private localAudioSource(kind: "mic" | "system"): Capability | undefined {
    const sources = this.capsOf(this.localId).filter(
      (c) => c.media === "audio" && canSource(c.flow),
    );
    if (kind === "system") return sources.find((c) => c.origin === "system");
    return sources
      .filter((c) => c.origin !== "system")
      .sort((a, b) => Number(b.default ?? false) - Number(a.default ?? false))[0];
  }

  /** Wire one room leg. Room sharing is **scoped to the room**: being a
   *  member is the consent, so the leg skips the share-grant gate — and
   *  minting a standing grant is exactly what it must never do. What
   *  happens in a room changes nothing about what its members may do to
   *  each other outside it. The route still validates structurally and
   *  still rides the real backend offer. */
  private roomConnect(from: string, to: string): { id: string; created: boolean } | null {
    const res = proposeRoomRoute(this.catalog, from, to);
    if (!res.ok) return null;
    const id = res.route.id;
    const existedBefore = this.catalog.routes.some((r) => r.id === id);
    if (!existedBefore) {
      this.addRoute(res.route.from, res.route.to);
      this.fireBackendConnect(res.route.from, res.route.to, res.route.media);
    }
    return { id, created: !existedBefore };
  }

  /** Wire one toggle's leg to every eligible member: `from` (a local
   *  source) into each member's matching sink. Returns how many members
   *  got a leg (created ones are owned by `roomId` for teardown). */
  private wireRoomLegs(
    roomId: string,
    channel: RoomChannel,
    from: Capability,
    media: MediaKind,
  ): number {
    let wired = 0;
    const members = this.roomMemberNodes;
    this.callLog(
      `wire "${channel}" (${media}) from ${from.label} — ${members.length} member(s) on the roster`,
    );
    for (const { id, node } of members) {
      const who = node?.label ?? shortId(id);
      // Each gate below is a place media silently went nowhere while chat
      // sailed through (chat fans out to the roster regardless of these).
      if (!node) {
        this.callLog(`  ${who}: skip — never seen on the graph (no presence advert yet)`);
        continue;
      }
      if (!isAppNode(node)) {
        this.callLog(`  ${who}: skip — not running AllMyStuff`);
        continue;
      }
      if (!node.online) {
        this.callLog(`  ${who}: skip — reads offline (node.online=false — the gate chat ignores)`);
        continue;
      }
      if (node.relationship.kind === "unclaimed") {
        this.callLog(`  ${who}: skip — unclaimed (claim or share it before media can route there)`);
        continue;
      }
      const sink = matchEndpoint(this.catalog, node.id, media, "consume");
      if (!sink) {
        this.callLog(`  ${who}: skip — advertises no ${media} sink to receive on`);
        continue;
      }
      const leg = this.roomConnect(from.id, sink.id);
      if (!leg) {
        this.callLog(`  ${who}: skip — route ${from.id} → ${sink.id} failed validateRoute`);
        continue;
      }
      if (leg.created) this.legsOf(roomId)[channel].push(leg.id);
      this.callLog(
        `  ${who}: wired → ${sink.id} (${leg.created ? "new route — offer fired to the daemon" : "route already live"})`,
      );
      wired += 1;
    }
    this.callLog(`wire "${channel}": ${wired}/${members.length} member(s) wired`);
    return wired;
  }

  /** Tear down the routes one room's toggle created (and only those). */
  private dropRoomLegs(roomId: string, channel: RoomChannel) {
    const legs = this.roomRoutes[roomId];
    if (!legs) return;
    const n = legs[channel].length;
    for (const id of legs[channel]) void this.disconnect(id);
    legs[channel] = [];
    if (n) this.callLog(`drop "${channel}" — tore down ${n} leg(s) in ${roomId}`);
  }

  private toastLegs(what: string, n: number) {
    this.toast("ok", `${what} — ${n} member${n === 1 ? "" : "s"}`);
  }

  /** Rooms persist on this device (like the graph's relationships, the
   *  mesh holds no central copy — every member keeps their own). Every
   *  save is announced on the local bus so this app's other windows (the
   *  main graph, an open room window) reload the same list. */
  private saveRooms() {
    try {
      localStorage.setItem(ROOMS_STORE_KEY, JSON.stringify(this.rooms));
    } catch {
      /* storage unavailable (private mode) — rooms last the session */
    }
    void emitRoomLocal({ token: this.windowToken, kind: "sync" });
  }

  private loadRooms() {
    try {
      const raw = localStorage.getItem(ROOMS_STORE_KEY);
      if (!raw) return;
      const rooms = JSON.parse(raw) as VirtualRoom[];
      if (Array.isArray(rooms)) {
        this.rooms = rooms.filter((r) => r && r.id && Array.isArray(r.members));
      }
    } catch {
      /* a corrupt store just means no rooms */
    }
  }

  // ---- networks / identity / roster -------------------------------

  async loadIdentity() {
    if (!isTauri()) return;
    try {
      this.identity = await meshIdentity();
      this.applyLocalLabel();
    } catch {
      /* no daemon yet — the graph still works from the demo/scan */
    }
  }

  /** Re-apply the naming rule to the local node after identity changes. */
  private applyLocalLabel() {
    const me = this.node(this.localId);
    if (!me) return;
    const host = me.hostname ?? me.label;
    me.label = this.identity?.label?.trim() || host;
  }

  async refreshNetworks() {
    if (!isTauri()) return;
    try {
      this.networks = (await meshNetworks()) ?? [];
      if (this.rosterNetwork) await this.refreshRoster(this.rosterNetwork);
    } catch (e) {
      this.toast("warn", `Couldn't load networks: ${errMsg(e)}`);
    }
  }

  /** Set this device's display-name override (empty resets to the hostname). */
  async setIdentityLabel(label: string) {
    try {
      await meshIdentitySetLabel(label);
      this.identity = { device_id: this.identity?.device_id ?? "", label };
      this.applyLocalLabel();
      this.toast("ok", label.trim() ? "Updated this device's name" : "Reset to the machine name");
    } catch (e) {
      this.toast("warn", `Couldn't set name: ${errMsg(e)}`);
    }
  }

  /** Get onto a network by name. A blank name generates a memorable 5-word
   *  one. There's no separate "create": a network is just a name two devices
   *  agree on (the signaling handle is a hash of it), so joining a name nobody
   *  else is on *is* creating it. Typed names are canonicalized (lowercased,
   *  spaces → hyphens) so "Beach House" and "beach-house" meet on the same one. */
  async joinNetwork(rawName: string, venueIds: string[] = [PUBLIC_VENUE_ID]) {
    const typed = rawName.trim();
    const id = typed ? canonicalNetworkId(typed) : generateNetworkPhrase();
    if (id.length < 3 || id.length > 64) {
      this.toast("warn", "A mesh name needs 3–64 letters, digits or hyphens");
      return;
    }
    // Compile the chosen venue(s) into the mesh's servers (union); remember
    // the choice so it shows in the Venue pane and survives edits.
    const venues = venueIds.map((v) => this.venueById(v)).filter((v): v is Venue => !!v);
    const s = unionServers(venues);
    try {
      await meshNetworkAdd(
        buildNetworkConfig({ networkId: id, signaling: s.signaling, stun: s.stun, turn: s.turn }),
      );
      this.networkVenues[id] = venues.map((v) => v.id);
      this.persistNetworkVenues();
      this.toast("ok", typed ? `Joined ${id}` : `Created ${id}`);
      await this.refreshNetworks();
    } catch (e) {
      this.toast("warn", `Couldn't ${typed ? "join" : "set up"} the network: ${errMsg(e)}`);
    }
  }

  /** Save a network's full settings (handle + signaling/STUN/TURN) to a JSON
   *  file you can hand to another device — the no-typing twin of "Copy id".
   *  Works for live or parked networks; pulls the full config if it isn't
   *  already loaded. */
  async exportNetwork(configId: string) {
    if (!this.backendConnected) {
      this.toast("info", "Exporting a network needs the desktop app");
      return;
    }
    let cfg =
      this.networkConfig(configId) ??
      this.disabledNets.find((c) => c.id === configId || c.network_id === configId) ??
      null;
    if (!cfg) {
      await this.loadNetworkConfigs();
      cfg = this.networkConfig(configId) ?? null;
    }
    if (!cfg) {
      this.toast("warn", "Couldn't find that network's settings to export");
      return;
    }
    try {
      // Bundle the mesh's custom venues so importing the file brings them too;
      // the built-in Public one is on every device already, so it's left out.
      const venues = this.venuesForNetwork(cfg.network_id).filter((v) => !v.builtin);
      const env = exportNetworkSettings(cfg, venues);
      const base = (env.label || env.network_id || "network").replace(/[^\w.-]+/g, "_").slice(0, 48);
      const saved = await exportNetworkFile(`${base}.network-settings.json`, env);
      if (saved) this.toast("ok", `Exported ${env.label || env.network_id}`);
    } catch (e) {
      this.toast("warn", `Couldn't export the network: ${errMsg(e)}`);
    }
  }

  /** Add a network from a network-settings file's contents — the third, and
   *  easiest, way onto a network (no handle to paste, no servers to re-enter).
   *  Tolerant: a file that isn't one of ours just warns. Skips a network
   *  you're already on rather than making a confusing duplicate. */
  async importNetworkSettings(text: string) {
    if (!this.backendConnected) {
      this.toast("info", "Importing a network needs the desktop app");
      return;
    }
    const env = tryParseNetworkSettings(text);
    if (!env) {
      this.toast("warn", "That file isn't an AllMyStuff network-settings export");
      return;
    }
    if (this.networks.some((n) => n.network_id === env.network_id)) {
      this.toast("info", `Already on ${env.label || env.network_id}`);
      return;
    }
    try {
      await meshNetworkAdd(networkAddPayloadFromEnvelope(env));
      this.toast("ok", `Imported ${env.label || env.network_id}`);
      await this.refreshNetworks();
      await this.loadNetworkConfigs();
      // Recreate the venues the mesh travelled with, map the mesh to them, and
      // refresh any remote ones from their host. The flat servers in the file
      // already seeded the right config, so this is for future edits/updates.
      const brought = venuesFromEnvelope(env);
      if (brought.length) {
        this.venues = [...this.venues, ...brought];
        this.persistVenues();
        this.networkVenues[env.network_id] = brought.map((v) => v.id);
        this.persistNetworkVenues();
        for (const v of brought) if (v.url) void this.refreshVenue(v.id);
      }
    } catch (e) {
      this.toast("warn", `Couldn't import the network: ${errMsg(e)}`);
    }
  }

  async leaveNetwork(configId: string) {
    try {
      await meshNetworkRemove(configId);
      if (this.rosterNetwork === configId) {
        this.rosterNetwork = null;
        this.roster = [];
        this.livePeers = [];
      }
      this.toast("info", "Left the network");
      await this.refreshNetworks();
    } catch (e) {
      this.toast("warn", `Couldn't leave: ${errMsg(e)}`);
    }
  }

  /** Pull the parked (disabled) network configs from the backend. */
  async loadDisabledNetworks() {
    if (!isTauri()) return;
    try {
      this.disabledNets = await disabledNetworks();
    } catch {
      this.disabledNets = [];
    }
  }

  /** Switch a network off or back on **without deleting it** — the pill
   *  menu's toggle. Off = leave the daemon but park the full config (the
   *  roster file survives on disk); on = re-join from the parked config.
   *  `key` may be a config id or network id. */
  async toggleNetworkEnabled(key: string, on: boolean) {
    if (!this.backendConnected) {
      this.demoToggleNetwork(key, on);
      return;
    }
    try {
      await setNetworkEnabled(key, on);
      this.toast(
        on ? "ok" : "info",
        on ? "Network enabled — reconnecting" : "Network disabled — kept for when you want it back",
      );
      await this.refreshNetworks();
      await this.loadDisabledNetworks();
      await this.syncMeshGraph();
    } catch (e) {
      this.toast("warn", `Couldn't ${on ? "enable" : "disable"} the network: ${errMsg(e)}`);
    }
  }

  /** Restart the live network(s) — leave and immediately re-join from the
   *  parked config, tearing down each transport and reconnecting without
   *  touching settings. The top bar's refresh control: a clean reconnect for
   *  when a network goes quiet (stuck handshaking, peers fallen silent). It
   *  acts on every currently-joined network, since the control is global. */
  async restartNetwork() {
    if (!this.backendConnected) {
      this.toast("info", "Nothing live to restart — connect to a network first");
      return;
    }
    const joined = (Array.isArray(this.networks) ? this.networks : []).slice();
    if (joined.length === 0) {
      this.toast("warn", "No live network to restart");
      return;
    }
    const many = joined.length > 1;
    this.toast("info", many ? "Restarting networks — reconnecting…" : "Restarting the network — reconnecting…");
    let failed = 0;
    for (const n of joined) {
      // `config_id` is the stable local key the off→on round-trip parks and
      // takes the config back under; fall back to the wire id just in case.
      const key = n.config_id || n.network_id;
      try {
        await setNetworkEnabled(key, false);
        await setNetworkEnabled(key, true);
      } catch (e) {
        failed++;
        this.toast("warn", `Couldn't restart ${networkDisplayName(n)}: ${errMsg(e)}`);
      }
    }
    // Re-sync regardless of partial failure so the pills/graph match reality
    // (a failed re-join leaves that network parked, recoverable from the menu).
    await this.refreshNetworks();
    await this.loadDisabledNetworks();
    await this.syncMeshGraph();
    if (failed === 0) this.toast("ok", many ? "Networks restarted" : "Network restarted");
  }

  /** Demo/web twin of the toggle: move the network between the live and
   *  parked lists, and quiet the devices that were only reachable on it. */
  private demoToggleNetwork(key: string, on: boolean) {
    if (on) {
      const cfg = this.disabledNets.find((c) => c.id === key || c.network_id === key);
      if (!cfg) return;
      this.disabledNets = this.disabledNets.filter((c) => c !== cfg);
      this.networks = [
        ...this.networks,
        {
          config_id: cfg.id,
          network_id: cfg.network_id,
          label: (cfg.label ?? "") as string,
          phase: "joined",
        },
      ];
    } else {
      const net = this.networks.find((n) => n.config_id === key || n.network_id === key);
      if (!net) return;
      this.networks = this.networks.filter((n) => n !== net);
      const cfg = this.networkConfig(net.config_id) ?? {
        id: net.config_id,
        network_id: net.network_id,
        label: net.label,
      };
      this.disabledNets = [...this.disabledNets, cfg];
    }
    // A device only reachable over disabled networks reads offline, and
    // this machine's own chips track what's actually joined.
    const enabledNames = new Set(this.networks.map((n) => networkDisplayName(n)));
    for (const n of this.catalog.nodes) {
      if (n.kind === "this") {
        n.networks = [...enabledNames].sort();
        continue;
      }
      if (n.networks?.length) n.online = n.networks.some((name) => enabledNames.has(name));
    }
    this.toast(on ? "ok" : "info", on ? "Network enabled (demo)" : "Network disabled (demo)");
  }

  // ---- per-network transport config (signaling · STUN · TURN) -----

  /** Pull every network's full config (servers) from the daemon for the
   *  Servers pane. Safe to call often; no-op in web mode. */
  async loadNetworkConfigs() {
    if (!isTauri()) return;
    try {
      this.networkConfigs = (await meshConfigShow()) ?? [];
      if (!this.serversNetwork && this.networkConfigs.length > 0) {
        this.serversNetwork = this.networkConfigs[0].id;
      }
    } catch (e) {
      this.toast("warn", `Couldn't load network settings: ${errMsg(e)}`);
    }
  }

  networkConfig(configId: string): NetworkConfigFull | undefined {
    return this.networkConfigs.find((n) => n.id === configId);
  }

  /** Replace one network's signaling/STUN/TURN servers. Round-trips the full
   *  config so unrelated fields (topology, auto-approve, roster path) survive,
   *  then asks the daemon to apply it — which restarts that network's
   *  transport and reconnects. */
  async updateNetworkServers(
    configId: string,
    servers: { signaling: string[]; stun: string[]; turn: TurnEntry[] },
  ) {
    const cfg = this.networkConfig(configId);
    if (!cfg) {
      this.toast("warn", "That network isn't loaded — reopen Settings");
      return;
    }
    const next: NetworkConfigFull = {
      ...cfg,
      signaling: {
        ...(cfg.signaling ?? {}),
        servers: servers.signaling.map((s) => s.trim()).filter(Boolean),
      },
      stun_servers: servers.stun.map((s) => s.trim()).filter(Boolean).map((u) => ({ urls: [u] })),
      turn_servers: servers.turn
        .filter((t) => t.url.trim())
        .map((t) => ({
          urls: [t.url.trim()],
          username: t.username.trim() || null,
          credential: t.credential.trim() || null,
        })),
    };
    try {
      await meshNetworkUpdate(next);
      this.toast("ok", "Saved — reconnecting with the new servers");
      await this.loadNetworkConfigs();
      await this.refreshNetworks();
    } catch (e) {
      this.toast("warn", `Couldn't save servers: ${errMsg(e)}`);
    }
  }

  // ---- venues (the named "where a mesh calls out" sets; app-side) --

  venueById(id: string): Venue | undefined {
    return this.venues.find((v) => v.id === id);
  }

  /** The venues a mesh uses, by its wire id. Defaults to Public when unmapped
   *  (matching the daemon defaults a fresh mesh already gets). */
  venuesForNetwork(networkId: string): Venue[] {
    const ids = this.networkVenues[networkId];
    if (!ids || ids.length === 0) {
      const pub = this.venueById(PUBLIC_VENUE_ID);
      return pub ? [pub] : [];
    }
    return ids.map((id) => this.venueById(id)).filter((v): v is Venue => !!v);
  }

  private persistVenues() {
    saveVenues(this.venues);
  }
  private persistNetworkVenues() {
    saveNetworkVenues(this.networkVenues);
  }

  /** Create or replace a venue (matched by id), persist it, and re-apply it to
   *  every live mesh that uses it so an edit propagates. */
  async saveVenue(v: Venue) {
    const i = this.venues.findIndex((x) => x.id === v.id);
    if (i >= 0) this.venues[i] = v;
    else this.venues = [...this.venues, v];
    this.persistVenues();
    await this.reapplyVenue(v.id);
  }

  /** Delete a venue (never the built-in Public one). Meshes that used it fall
   *  back to Public, re-applied. */
  async deleteVenue(id: string) {
    const v = this.venueById(id);
    if (!v || v.builtin || id === PUBLIC_VENUE_ID) return;
    this.venues = this.venues.filter((x) => x.id !== id);
    this.persistVenues();
    const affected: string[] = [];
    for (const [nid, ids] of Object.entries(this.networkVenues)) {
      if (ids.includes(id)) {
        const next = ids.filter((x) => x !== id);
        if (next.length) this.networkVenues[nid] = next;
        else delete this.networkVenues[nid];
        affected.push(nid);
      }
    }
    this.persistNetworkVenues();
    for (const nid of affected) await this.applyNetworkVenuesByWireId(nid);
    if (this.venueDraft?.id === id) this.venueDraft = null;
    this.toast("info", `Removed ${v.label}`);
  }

  /** Re-fetch a remote venue's servers from its url, cache them, and re-apply
   *  to any mesh using it. */
  async refreshVenue(id: string) {
    const v = this.venueById(id);
    if (!v?.url) return;
    try {
      const s = await fetchVenueServers(v.url);
      await this.saveVenue({ ...v, signaling: s.signaling, stun: s.stun, turn: s.turn, fetchedAt: Date.now() });
      this.toast("ok", `Refreshed ${v.label}`);
    } catch (e) {
      this.toast("warn", `Couldn't reach ${v.label}: ${errMsg(e)}`);
    }
  }

  /** Point a mesh at a set of venues: write the union of their servers to the
   *  daemon (reconnecting it) and remember the choice. */
  async setNetworkVenues(configId: string, venueIds: string[]) {
    const cfg = this.networkConfig(configId);
    if (!cfg) return;
    const venues = venueIds.map((id) => this.venueById(id)).filter((v): v is Venue => !!v);
    this.networkVenues[cfg.network_id] = venues.map((v) => v.id);
    this.persistNetworkVenues();
    await this.updateNetworkServers(configId, unionServers(venues));
  }

  /** Re-apply a venue to every live mesh that uses it (after an edit). */
  private async reapplyVenue(venueId: string) {
    for (const [nid, ids] of Object.entries(this.networkVenues)) {
      if (ids.includes(venueId)) await this.applyNetworkVenuesByWireId(nid);
    }
  }

  /** Recompute + write a mesh's union, found by wire id. */
  private async applyNetworkVenuesByWireId(networkId: string) {
    const cfg = this.networkConfigs.find((c) => c.network_id === networkId);
    if (!cfg) return;
    await this.updateNetworkServers(cfg.id, unionServers(this.venuesForNetwork(networkId)));
  }

  /** Save a mesh's current inline servers as a new named venue and switch the
   *  mesh onto it — the escape hatch from editing raw servers. */
  saveServersAsVenue(configId: string, label: string): Venue | undefined {
    const cfg = this.networkConfig(configId);
    if (!cfg) return;
    const v: Venue = {
      id: newVenueId(),
      label: label.trim() || "New venue",
      signaling: cfg.signaling?.servers ?? [],
      stun: (cfg.stun_servers ?? []).flatMap((s) => s.urls),
      turn: (cfg.turn_servers ?? []).map((t) => ({
        url: t.urls[0] ?? "",
        username: t.username ?? "",
        credential: t.credential ?? "",
      })),
    };
    this.venues = [...this.venues, v];
    this.persistVenues();
    this.networkVenues[cfg.network_id] = [v.id];
    this.persistNetworkVenues();
    return v;
  }

  /** Load the roster + live peers for one network (the approvals view). */
  async refreshRoster(configId: string) {
    this.rosterNetwork = configId;
    try {
      this.roster = (await meshRosterList(configId)) ?? [];
    } catch {
      this.roster = [];
    }
    try {
      this.livePeers = (await meshPeers(configId)) ?? [];
    } catch {
      this.livePeers = [];
    }
  }

  async approveDevice(configId: string, deviceId: string, label?: string) {
    try {
      await meshRosterApprove(configId, deviceId, label);
      this.toast("ok", "Approved — it can join now");
      await this.refreshRoster(configId);
    } catch (e) {
      this.toast("warn", `Couldn't approve: ${errMsg(e)}`);
    }
  }

  async removeDevice(configId: string, deviceId: string) {
    try {
      await meshRosterRemove(configId, deviceId);
      this.toast("info", "Removed from the network");
      await this.refreshRoster(configId);
    } catch (e) {
      this.toast("warn", `Couldn't remove: ${errMsg(e)}`);
    }
  }

  // ---- settings · approvals · fleet · updates ---------------------

  /** Open the unified settings panel on a given pane. The top-bar gear opens
   *  it on Networks; the Networks button deep-links here too. Refreshes the
   *  pane data so it's never stale on open. */
  openSettings(tab: SettingsTab = "networks") {
    this.settingsTab = tab;
    this.settingsOpen = true;
    void this.refreshNetworks().then(() => {
      const net = this.rosterNetwork ?? this.activeNetwork?.config_id ?? null;
      if (net) void this.refreshRoster(net);
    });
    void this.loadOwnedFleet();
    void this.loadNetworkConfigs();
    if (tab === "updates") void this.loadUpdateStatus();
  }

  /** Open the "a new device wants to join" approval popup (the code grid). */
  openApprovals() {
    if (this.freshJoins.length === 0) return;
    this.approvalsOpen = true;
  }

  /** Open the "claim a device" sheet (the adoption nudge's target). */
  openClaim() {
    if (this.claimables.length === 0) return;
    this.claimOpen = true;
  }

  /** Approve a pending join straight from the popup. */
  async approveJoin(j: PendingJoin) {
    await this.approveDevice(j.networkId, j.peer.device_id, j.peer.label);
    // Drop it locally so the popup updates at once (the next poll confirms).
    this.pendingJoins = this.pendingJoins.filter(
      (x) => !(x.networkId === j.networkId && x.peer.device_id === j.peer.device_id),
    );
    if (this.freshJoins.length === 0) this.approvalsOpen = false;
  }

  /** Decline a join — a *cancel*, not a deny. It stops the nudge but leaves
   *  the device approvable later under Settings → Networks. (A real block is a
   *  separate control, coming later.) */
  dismissJoin(deviceId: string) {
    const canon = canonicalNodeId(deviceId);
    if (!this.dismissedJoins.includes(canon)) {
      this.dismissedJoins = [...this.dismissedJoins, canon];
    }
    if (this.freshJoins.length === 0) this.approvalsOpen = false;
  }

  async loadOwnedFleet() {
    if (!isTauri()) return;
    try {
      const r = await ownedRoster();
      if (r) {
        this.ownedFleet = r;
        this.reconcileFleetRelationships();
      }
    } catch {
      /* no daemon yet — claim still simulates a fleet in demo mode */
    }
  }

  /** Leave the fleet this device is in. The backend broadcasts the bumped
   *  roster (the others drop us), releases our owner, and pushes the now-
   *  empty roster back via `allmystuff://owned`. */
  async leaveFleet() {
    if (this.backendConnected) {
      try {
        await fleetLeave();
        this.toast("ok", "Left the fleet");
      } catch (e) {
        this.toast("warn", `Couldn't leave the fleet: ${String(e)}`);
      }
      return;
    }
    // Demo/web: drop ourselves from the simulated roster, then re-derive
    // relationships so the devices we no longer co-own revert to unclaimed
    // (and the graph regroups them out of "yours") — the same convergence
    // the backend's roster push triggers.
    if (!this.ownedFleet) return;
    const members = this.ownedFleet.members.filter((m) => !this.isMe(m.device));
    this.ownedFleet = members.length
      ? { ...this.ownedFleet, version: this.ownedFleet.version + 1, members }
      : null;
    this.reconcileFleetRelationships();
    this.toast("ok", "Left the fleet");
  }

  /** Kick a member out of the fleet — allowed only while we're a member
   *  ourselves (the backend enforces it; the demo mirrors the rule). */
  async kickFleetMember(device: string) {
    if (this.isMe(device)) {
      void this.leaveFleet();
      return;
    }
    const label =
      this.ownedFleet?.members.find((m) => sameMachine(m.device, device))?.label || "that device";
    if (this.backendConnected) {
      try {
        await fleetKick(device);
        this.toast("ok", `Kicked ${label} from the fleet`);
      } catch (e) {
        this.toast("warn", `Couldn't kick ${label}: ${String(e)}`);
      }
      return;
    }
    // Demo/web: mirror the membership rule, then drop them.
    if (!this.ownedFleet || !this.isFleetMember(this.localId)) {
      this.toast("warn", "You can't kick devices from a fleet you aren't in");
      return;
    }
    this.ownedFleet = {
      ...this.ownedFleet,
      version: this.ownedFleet.version + 1,
      members: this.ownedFleet.members.filter((m) => !sameMachine(m.device, device)),
    };
    this.toast("ok", `Kicked ${label} from the fleet`);
  }

  async loadUpdateStatus() {
    if (!isTauri()) return;
    try {
      this.updateInfo = await updateStatus();
    } catch (e) {
      this.toast("warn", `Couldn't read update status: ${errMsg(e)}`);
    }
  }

  /** Check the release feed now and stage anything permitted. */
  async checkUpdates() {
    if (!isTauri()) {
      this.toast("info", "Updates need the desktop app");
      return;
    }
    this.updateBusy = true;
    this.updateOutcome = null;
    try {
      this.updateOutcome = await updateCheck();
      this.updateInfo = (await updateStatus()) ?? this.updateInfo;
      this.describeCheckOutcome(this.updateOutcome);
    } catch (e) {
      this.toast("warn", `Update check failed: ${errMsg(e)}`);
    } finally {
      this.updateBusy = false;
    }
  }

  /** Apply a staged update to disk (it takes effect on next launch). */
  async applyUpdate() {
    if (!isTauri()) return;
    this.updateBusy = true;
    try {
      const r = await updateApply();
      if (r?.applied) this.toast("ok", `Update ${r.applied} staged — it applies on next launch`);
      else this.toast("info", "Nothing staged to apply");
      this.updateInfo = (await updateStatus()) ?? this.updateInfo;
    } catch (e) {
      this.toast("warn", `Couldn't apply update: ${errMsg(e)}`);
    } finally {
      this.updateBusy = false;
    }
  }

  async setUpdatePrefs(prefs: UpdatePrefs) {
    if (!isTauri()) return;
    try {
      const next = await updateSetPrefs(prefs);
      if (next) this.updateInfo = next;
    } catch (e) {
      this.toast("warn", `Couldn't save update settings: ${errMsg(e)}`);
    }
  }

  /** Learn the channel's latest release version (once, read-only). Called
   *  lazily when a remote AllMyStuff machine is opened, so we only reach the
   *  release feed when there's a reason to. Best-effort: a failure just
   *  leaves `latestRelease` unset and the upgrade affordance hidden. */
  async loadLatestRelease(force = false) {
    if (!isTauri()) return;
    if (this.latestReleaseLoading) return;
    if (this.latestRelease && !force) return;
    this.latestReleaseLoading = true;
    try {
      const v = await updateLatestVersion();
      if (v) this.latestRelease = v;
    } catch {
      /* offline / no feed — the upgrade button just stays hidden */
    } finally {
      this.latestReleaseLoading = false;
    }
  }

  /** Whether `node` is a remote AllMyStuff machine running a version older
   *  than the channel's latest release — i.e. we can offer to upgrade it.
   *  Needs both the remote's advertised version and the latest release known;
   *  the drawer additionally gates on the machine being yours (owner/fleet),
   *  the same rule the far side enforces before acting. */
  upgradeAvailable(node: MeshNode | null | undefined): boolean {
    if (!node || node.kind === "this" || !isAppNode(node)) return false;
    return isOlderVersion(node.version, this.latestRelease ?? undefined);
  }

  /** Ask a fleet machine to update itself to the channel's latest release and
   *  restart. The far side enforces owner/fleet and decides if there's
   *  anything to do; its next presence advert (the new version) is the
   *  confirmation — the button disappears when the upgrade lands. */
  upgradeRemote(nodeId: string) {
    const n = this.node(nodeId);
    if (!n) return;
    if (!this.backendConnected) {
      this.toast("info", "Upgrading a machine needs the desktop app");
      return;
    }
    upgradeNode(nodeId).catch((e) => {
      this.toast("warn", `Couldn't ask ${n.label} to upgrade: ${String(e)}`);
    });
    this.toast("info", `Asking ${n.label} to upgrade and restart…`);
  }

  private describeCheckOutcome(o: CheckOutcome | null) {
    if (!o) return;
    switch (o.outcome) {
      case "staged":
        this.toast("ok", `Update ${o.version} downloaded — applies on next launch`);
        break;
      case "up_to_date":
        this.toast("ok", "You're on the latest version");
        break;
      case "policy_blocked":
        this.toast("info", `${o.latest} is available but held by your auto-apply setting`);
        break;
      case "package_manager":
        this.toast("info", "Installed via a package manager — update through it");
        break;
      case "disabled":
        this.toast("info", "Auto-update is off");
        break;
      case "not_due":
        this.toast("info", "Checked recently — try again shortly");
        break;
    }
  }

  // ---- relationships ----------------------------------------------

  /** Flip a node between "mine" and a fresh share, or vice versa. */
  setRelationship(nodeId: string, relationship: Relationship) {
    const n = this.node(nodeId);
    if (!n) return;
    n.relationship = relationship;
    this.reauthorize();
  }

  /** The person a share with this node is *really* with: its owner. The
   *  person is keyed by the owner's canonical pubkey (the node's own when
   *  it advertises no owner — an owner machine doesn't), so every node
   *  one owner brings resolves to the same person. An existing share with
   *  that person lends its identity, so names stay stable. */
  personFor(node: MeshNode): Person {
    const ownerKey = canonicalNodeId(node.owner ?? node.id);
    const id = `person:${ownerKey}`;
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind === "shared" && n.relationship.person.id === id) {
        return n.relationship.person;
      }
    }
    const ownerNode = this.catalog.nodes.find((n) => sameMachine(n.id, ownerKey));
    return { id, name: ownerNode?.label ?? node.label };
  }

  /** Mark a node shared — a connection with its *owner*, not just this
   *  machine: every node that owner brings joins the same share, so what
   *  you grant them works to any of their devices. */
  markShared(nodeId: string) {
    const n = this.node(nodeId);
    if (!n) return;
    const person = this.personFor(n);
    n.relationship = { kind: "shared", person, grants: [] };
    this.reconcileShares();
    this.reauthorize();
  }

  /** Sharing follows the person across their fleet: any unclaimed node
   *  whose owner is already a share partner joins that share (so a second
   *  machine of theirs appearing later is covered without re-asking). A
   *  relationship the user set is never touched. */
  private reconcileShares() {
    const partners = new Map<string, Person>();
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind === "shared") {
        partners.set(n.relationship.person.id, n.relationship.person);
      }
    }
    if (partners.size === 0) return;
    for (const n of this.catalog.nodes) {
      if (n.kind === "this" || this.isMe(n.id)) continue;
      if (n.relationship.kind !== "unclaimed") continue;
      const person = partners.get(`person:${canonicalNodeId(n.owner ?? n.id)}`);
      if (person) n.relationship = { kind: "shared", person, grants: [] };
    }
  }

  /** Everyone you're sharing with, one entry per person/fleet: their
   *  nodes and every grant you've given them (with the node each grant is
   *  recorded on). Drives the Sharing settings pane. */
  sharePartners = $derived.by((): SharePartner[] => {
    const map = new Map<string, SharePartner>();
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind !== "shared") continue;
      const share = n.relationship;
      const p = map.get(share.person.id) ?? { person: share.person, nodes: [], grants: [] };
      p.nodes.push(n);
      for (const g of share.grants) p.grants.push({ node: n, grant: g });
      map.set(share.person.id, p);
    }
    return [...map.values()].sort((a, b) => a.person.name.localeCompare(b.person.name));
  });

  /** Rescind the whole connection with a person: every node of theirs
   *  goes back to unclaimed, every grant (and any route riding one) goes
   *  with it. */
  stopSharingWith(personId: string) {
    let name = "";
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind === "shared" && n.relationship.person.id === personId) {
        name = n.relationship.person.name;
        n.relationship = { kind: "unclaimed" };
      }
    }
    this.reauthorize();
    if (name) this.toast("info", `Stopped sharing with ${name}`);
  }

  grant(nodeId: string, grant: Grant) {
    const n = this.node(nodeId);
    if (!n || n.relationship.kind !== "shared") return;
    const pid = n.relationship.person.id;
    // De-dupe by (media, role, capability) across the *person* — a grant
    // authorizes them wherever it happens to be recorded.
    const exists = this.catalog.nodes.some(
      (x) =>
        x.relationship.kind === "shared" &&
        x.relationship.person.id === pid &&
        x.relationship.grants.some(
          (g) =>
            g.media === grant.media && g.role === grant.role && g.capability === grant.capability,
        ),
    );
    if (!exists) n.relationship.grants.push(grant);
  }

  revokeGrant(nodeId: string, grantId: string) {
    const n = this.node(nodeId);
    if (!n || n.relationship.kind !== "shared") return;
    n.relationship.grants = n.relationship.grants.filter((g) => g.id !== grantId);
    this.reauthorize();
    this.toast("info", "Permission removed");
  }

  /** After any authorization change, drop routes that are no longer
   *  allowed. Security can't lag behind the grants. Routes the open room
   *  wired are exempt: room sharing is scoped to the room session
   *  (membership is the consent), so it never depended on a grant — and
   *  leaving the room is what tears it down. */
  private reauthorize() {
    const roomWired = new Set(
      Object.values(this.roomRoutes).flatMap((legs) => Object.values(legs).flat()),
    );
    const before = this.catalog.routes.length;
    this.catalog.routes = this.catalog.routes.filter(
      (r) => roomWired.has(r.id) || requiredGrants(this.catalog, r.from, r.to).length === 0,
    );
    const dropped = before - this.catalog.routes.length;
    if (dropped > 0) this.toast("warn", `${dropped} connection${dropped > 1 ? "s" : ""} stopped`);
  }

  // ---- toasts ------------------------------------------------------
  toast(kind: Toast["kind"], text: string) {
    const id = ++seq;
    this.toasts.push({ id, kind, text });
    setTimeout(() => {
      this.toasts = this.toasts.filter((t) => t.id !== id);
    }, 3200);
  }

  // ===================================================================
  // CEC service — the optional Critical Error Computing account and the
  // two services it unlocks: Ask-for-Help (Concierge) and the Private
  // Line. The HTTP side lives in the Tauri backend (`cec.rs` →
  // `allmystuff-cec`); this orchestrates the mesh side on top of the
  // existing room / network / venue verbs. Everything degrades to a
  // believable demo off the desktop backend (web preview).
  // ===================================================================

  cec = $state<CecSnapshot>({
    backend_url: "https://api.allmystuff.works",
    signed_in: false,
    account: null,
    entitlements: { hardware: false, private_line: false, concierge: null },
    provision: null,
  });
  /** A sign-in code has been requested and we're waiting for it. */
  cecCodeSent = $state(false);
  cecBusy = $state(false);
  cecPrivateLines = $state<CecPrivateLine[]>([]);
  /** The help session the user is currently in (or just opened), if any. */
  activeHelp = $state<CecHelpSession | null>(null);
  private helpPoll: ReturnType<typeof setInterval> | null = null;

  get cecSignedIn(): boolean {
    return this.cec.signed_in;
  }
  /** Ask-for-Help is live only with an account *and* an active Concierge plan. */
  get cecCanAskForHelp(): boolean {
    return this.cec.signed_in && cecApi.canAskForHelp(this.cec.entitlements);
  }
  get cecConciergeTier(): string | null {
    return cecApi.conciergeLabel(this.cec.entitlements.concierge);
  }
  /** The canonical id of the single CEC Service node, once a mesh exists. */
  get cecServiceNodeId(): string | null {
    const id = this.cec.provision?.cec_service_node_id;
    return id ? canonicalNodeId(id) : null;
  }
  isCecNetworkId(networkId: string | null | undefined): boolean {
    return cecApi.isCecNetwork(networkId ?? undefined);
  }
  /** "CEC Service" for the one service node, else null — so the graph shows a
   *  single CEC node rather than our plumbing. */
  cecNodeLabel(nodeId: string): string | null {
    const svc = this.cecServiceNodeId;
    return svc && sameMachine(nodeId, svc) ? cecApi.CEC_SERVICE_LABEL : null;
  }

  async loadCec() {
    try {
      const s = await cecApi.cecState();
      if (s) this.cec = s;
    } catch {
      /* a missing/old backend leaves the signed-out default in place */
    }
    if (this.cec.signed_in) void this.cecLoadLines();
  }

  async cecSetBackend(url: string) {
    const s = await cecApi.cecSetBackendUrl(url);
    this.cec = s ?? {
      ...this.cec,
      backend_url: url.trim().replace(/\/+$/, ""),
      signed_in: false,
      account: null,
      provision: null,
    };
    this.toast("ok", "Saved the CEC service address");
  }

  async cecBeginSignIn(email: string) {
    const e = email.trim();
    if (!e.includes("@")) {
      this.toast("warn", "Enter a valid email address");
      return;
    }
    this.cecBusy = true;
    try {
      const r = await cecApi.cecStartSignIn(e);
      this.cecCodeSent = true;
      this.toast("ok", r ? `Code sent to ${r.masked_email}` : "Demo mode — enter any 6-digit code");
    } catch (err) {
      this.toast("warn", `Couldn't send a code: ${errMsg(err)}`);
    } finally {
      this.cecBusy = false;
    }
  }

  async cecVerifyCode(email: string, code: string) {
    this.cecBusy = true;
    try {
      const me = isTauri() ? canonicalNodeId(this.localId) : "demo-device";
      const s = await cecApi.cecVerifySignIn(email.trim(), code.trim(), me, undefined);
      this.cec = s ?? {
        ...this.cec,
        signed_in: true,
        account: {
          id: "acct_demo",
          email: email.trim(),
          display_name: email.trim().split("@")[0],
          roles: ["customer"],
          device_ids: [me],
        },
        entitlements: { hardware: false, private_line: false, concierge: "pay_as_you_go" },
      };
      this.cecCodeSent = false;
      this.toast("ok", `Signed in to CEC as ${this.cec.account?.email ?? email}`);
      if (cecApi.wantsCecMesh(this.cec.entitlements)) await this.cecProvision();
      void this.cecLoadLines();
    } catch (err) {
      this.toast("warn", `Sign-in failed: ${errMsg(err)}`);
    } finally {
      this.cecBusy = false;
    }
  }

  async cecRefreshAccount() {
    try {
      const s = await cecApi.cecRefresh();
      if (s) {
        this.cec = s;
        void this.cecLoadLines();
      }
    } catch (err) {
      this.toast("warn", `Couldn't refresh your account: ${errMsg(err)}`);
    }
  }

  async cecLogOut() {
    const s = await cecApi.cecSignOut();
    this.cec = s ?? {
      backend_url: this.cec.backend_url,
      signed_in: false,
      account: null,
      entitlements: { hardware: false, private_line: false, concierge: null },
      provision: null,
    };
    this.cecPrivateLines = [];
    this.cecStopHelpPoll();
    this.activeHelp = null;
    this.toast("ok", "Signed out of CEC");
  }

  /** Stand up (or refresh) the isolated `cec-customer-<hash>` mesh: join the
   *  network with auto-approve, add its venue, and pre-trust the single CEC
   *  Service node — the only non-customer peer that ever appears on it. */
  async cecProvision() {
    const me = canonicalNodeId(this.localId);
    try {
      const prov = await cecApi.cecProvisionMesh(me);
      if (!prov) {
        // Demo: a stand-in descriptor so the UI can show the CEC node.
        this.cec.provision = {
          network_id: "cec-customer-demo",
          label: cecApi.CEC_NETWORK_LABEL,
          venue: { url: null, signaling: [], stun: [], turn: [] },
          cec_service_node_id: "cecservice-demo",
          auto_approve: true,
        };
        return;
      }
      this.cec.provision = prov;
      const venueId = this.cecEnsureVenue(prov.label || cecApi.CEC_NETWORK_LABEL, prov.venue);
      // Join the network (ignore "already joined" on a re-provision).
      try {
        await meshNetworkAdd(
          buildNetworkConfig({
            networkId: prov.network_id,
            label: cecApi.CEC_NETWORK_LABEL,
            autoApprove: true,
            signaling: prov.venue.signaling,
            stun: prov.venue.stun,
            turn: prov.venue.turn,
          }),
        );
      } catch {
        /* already on this network — fine, keep going */
      }
      this.networkVenues[prov.network_id] = [venueId];
      this.persistNetworkVenues();
      try {
        await meshRosterApprove(
          prov.network_id,
          canonicalNodeId(prov.cec_service_node_id),
          cecApi.CEC_SERVICE_LABEL,
        );
      } catch {
        /* the auto-approve network will admit it regardless */
      }
      await this.refreshNetworks();
    } catch (err) {
      this.toast("warn", `Couldn't set up your CEC connection: ${errMsg(err)}`);
    }
  }

  /** Find or create the local venue record for a CEC venue (so it shows in the
   *  Venues pane and can be re-fetched), returning its id. */
  private cecEnsureVenue(
    label: string,
    v: { url?: string | null; signaling: string[]; stun: string[]; turn: { url: string; username: string; credential: string }[] },
  ): string {
    const url = v.url ?? undefined;
    const existing = this.venues.find((x) => (url && x.url === url) || (!url && x.label === label));
    if (existing) return existing.id;
    const venue: Venue = {
      id: newVenueId(),
      label,
      ...(url ? { url } : {}),
      signaling: [...v.signaling],
      stun: [...v.stun],
      turn: v.turn.map((t) => ({ url: t.url, username: t.username, credential: t.credential })),
    };
    this.venues.push(venue);
    saveVenues(this.venues);
    return venue.id;
  }

  async cecRentLine(label?: string) {
    this.cecBusy = true;
    try {
      const pl = await cecApi.cecRentPrivateLine(label);
      if (pl) {
        this.cecEnsureVenue(pl.label, pl.venue);
        this.cec.entitlements.private_line = true;
        await this.cecLoadLines();
        this.toast(
          "ok",
          `Your Private Line “${pl.label}” is ready — assign it to a network in Venues`,
        );
      } else {
        this.toast("info", "Renting a Private Line needs the desktop app and a CEC account");
      }
    } catch (err) {
      this.toast("warn", `Couldn't rent a Private Line: ${errMsg(err)}`);
    } finally {
      this.cecBusy = false;
    }
  }

  async cecLoadLines() {
    try {
      const lines = await cecApi.cecListPrivateLines();
      if (lines) this.cecPrivateLines = lines;
    } catch {
      /* signed out / offline — keep whatever we have */
    }
  }

  async cecCancelLine(id: string) {
    const s = await cecApi.cecCancelPrivateLine(id);
    if (s) this.cec = s;
    await this.cecLoadLines();
    this.toast("ok", "Private Line cancelled");
  }

  /** The Ask-for-Help button. Opens a host-side help room holding just this
   *  device and the single CEC Service node, tells the backend (which exposes
   *  it to online agents), and pops the call window. */
  async cecAskHelp(topic?: string) {
    if (!this.cecSignedIn) {
      this.toast("info", "Create a free CEC account to ask for help");
      this.openSettings("account");
      return;
    }
    if (!cecApi.canAskForHelp(this.cec.entitlements)) {
      this.toast("info", "Ask-for-Help needs an active Concierge plan");
      this.openSettings("account");
      return;
    }
    this.cecBusy = true;
    try {
      if (!this.cec.provision) await this.cecProvision();
      const prov = this.cec.provision;
      if (!prov) {
        this.toast("warn", "Couldn't reach your CEC connection");
        return;
      }
      const me = canonicalNodeId(this.localId);
      const cecNode = canonicalNodeId(prov.cec_service_node_id);
      const roomId = cecApi.helpRoomId(me);
      // Host-side help room: just you and the one CEC Service node.
      const room: VirtualRoom = {
        id: roomId,
        name: cecApi.CEC_SERVICE_LABEL,
        members: [me, cecNode],
        owner: me,
        access: "invite",
      };
      this.rooms.push(room);
      this.saveRooms();
      this.broadcastRoom(room, this.inviteMessage(room));
      const session = await cecApi.cecAskForHelp(prov.network_id, roomId, me, topic);
      this.activeHelp = session ?? {
        id: "help_demo",
        status: "assigned",
        network_id: prov.network_id,
        room_id: roomId,
        cec_service_node_id: prov.cec_service_node_id,
        customer_device_id: me,
        customer_label: this.cec.account?.display_name ?? "you",
        topic: topic ?? null,
        agent_label: "Sam @ CEC",
        created_at: Date.now() / 1000,
      };
      this.toast("ok", "Connecting you to CEC…");
      void openRoomWindow(roomId);
      if (session) this.cecStartHelpPoll(session.id);
    } catch (err) {
      this.toast("warn", `Couldn't ask for help: ${errMsg(err)}`);
    } finally {
      this.cecBusy = false;
    }
  }

  private cecStartHelpPoll(id: string) {
    this.cecStopHelpPoll();
    if (!isTauri()) return;
    this.helpPoll = setInterval(() => {
      void cecApi
        .cecHelpStatus(id)
        .then((s) => {
          if (!s) return;
          this.activeHelp = s;
          if (s.status === "ended" || s.status === "cancelled") this.cecStopHelpPoll();
        })
        .catch(() => {
          /* transient — try again next tick */
        });
    }, 3000);
  }

  private cecStopHelpPoll() {
    if (this.helpPoll) {
      clearInterval(this.helpPoll);
      this.helpPoll = null;
    }
  }

  async cecEndHelp() {
    const h = this.activeHelp;
    this.cecStopHelpPoll();
    this.activeHelp = null;
    if (h && !h.id.startsWith("help_demo")) await cecApi.cecCancelHelp(h.id);
  }
}

function requestToGrant(req: GrantRequest): Grant {
  return {
    id: newId("grant"),
    media: req.media,
    role: req.role,
    capability: req.capability,
    label: req.description,
  };
}

export const app = new AppStore();

/** Re-export so components can build typed values without reaching into
 *  catalog.ts directly. */
export type { MediaKind };
