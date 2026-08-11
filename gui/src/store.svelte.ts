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
  scopedGrantId,
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
  loadInactiveVenues,
  loadNetworkVenues,
  loadVenues,
  newVenueId,
  PUBLIC_VENUE_ID,
  saveInactiveVenues,
  saveNetworkVenues,
  saveVenues,
  unionServers,
  type Venue,
} from "./venues";
import {
  decodeInviteVenues,
  encodeInviteVenues,
  fetchVenueServers,
  venueFromExport,
} from "./venue-settings";
import { canonicalNetworkId, generateNetworkPhrase } from "./network-phrase";
import {
  buildNetworkConfig,
  claimNode,
  shareGrant,
  shareRevoke,
  shareStop,
  clientLog,
  closeThisWindow,
  connectRoute,
  labsSet,
  tuneRoute,
  type NativeDecoderPreference,
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
  resetNetworking,
  factoryReset,
  fleetSetName,
  fleetGrantRole,
  fleetRevokeRole,
  fleetSetHubs,
  fleetMfaStatus,
  isMobile,
  isTauri,
  hostWifiScan,
  kvmApiCall,
  kvmAttach,
  kvmDetach,
  kvmMeshAdd,
  kvmMeshRemove,
  linkStatus,
  mapNativeDrive,
  mapNativeDriveFrom,
  pickKvmMediaImage,
  stageKvmMedia as stageKvmMediaNative,
  unmountKvmMedia as unmountKvmMediaNative,
  onClockSkew,
  onControlRefused,
  onDeviceRestart,
  onDriveMount,
  onFileProgress,
  onFileSaved,
  onKvmMedia,
  onMeshEvent,
  openFilesWindow,
  pickFilesToShare,
  pickDriveFolder,
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
  onUpdateChecked,
  onRoom,
  onRoomLocal,
  onSession,
  onShare,
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
  terminalSessions,
  onTerminalSessions,
  filesWindowTarget,
  scanSelf,
  autostartGet,
  autostartSet,
  clipboardPaste,
  clipboardDrop,
  clipboardPull,
  shareFolderFrom,
  sendInput,
  debugLoggingGet,
  debugLoggingSet,
  serviceInstall,
  serviceRestart,
  serviceStart,
  serviceStatus,
  serviceStop,
  serviceUninstall,
  sessionSnapshot,
  setClaimable,
  setPublicClaims,
  claimViaCode,
  setNetworkEnabled,
  networkReconnect,
  updateApply,
  updateCheck,
  requestNodeRefresh,
  restartApp,
  restartDevice,
  restartNode,
  updateLatestVersion,
  updateRelaunch,
  updateSetPrefs,
  updateStatus,
  componentStatus,
  componentRepair,
  upgradeNode,
  windowBehaviorGet,
  windowBehaviorSet,
  cecStatus,
  cecDial,
  cecDialNode,
  cecPending,
  cecApprove,
  cecDeny,
  cecRevoke,
  cecGrants,
  cecCancelDial,
  cecDialed,
  cecHelpList,
  cecHelpWatch,
  onCecHelp,
  type CecHelpSeeker,
  cecChatSend,
  cecChatHistory,
  onCecChat,
  openChatWindow,
  type CecChatMsg,
  forgetNode,
  onCecRequest,
  onCecPeer,
  onCecSession,
  onCecGrants,
  type CecStatus,
  type CecPending,
  type CecGrant,
  type CecPeer,
  type CecScope,
  type ServiceActionResult,
  type ServiceStatus,
  type SessionSnapshot,
  type WindowBehavior,
} from "./tauri";
import {
  CAP_TAG_ALLMYSTUFF,
  displayName,
  FEATURE_CAMERA,
  FEATURE_FILES,
  FEATURE_KVM,
  FEATURE_ROOMS,
  FEATURE_SITES,
  FEATURE_TERMINAL,
  isAppNode,
  isOlderVersion,
  LOCAL_CLAIM_NETWORK_ID,
  networkDisplayName,
  siteIsWeb,
  sitePort,
  type Capability,
  type Catalog,
  type CheckOutcome,
  type ComponentVersionRow,
  type ExposedSite,
  type Grant,
  type GrantRole,
  type IdentityInfo,
  type ListeningService,
  type SiteAdvert,
  type SiteMapping,
  type InputAction,
  type InventorySummary,
  type HostWifi,
  type KvmApiCallResult,
  type KvmApiRsp,
  type KvmVersion,
  type KvmWifiNetwork,
  type KvmWifiStatus,
  type KvmWifiStatusRaw,
  type MediaKind,
  type MeshNode,
  type Standing,
  type NetworkConfigFull,
  type NetworkSummary,
  type OwnedRoster,
  type OwnedMember,
  type PeerInfo,
  type Person,
  type Relationship,
  type RoomAccess,
  type RoomChatLine,
  type RoomWireMessage,
  type RosterPeer,
  type Route,
  type Share,
  type SharedEntry,
  type SharedFileMeta,
  type TerminalSessionInfo,
  type RouteLiveState,
  type VirtualRoom,
  type TurnEntry,
  type UpdatePrefs,
  type UpdateStatus,
} from "./types";

/** The one shared CEC Support area every node lives on (the node's
 *  `HELP_NETWORK_ID`). Node-managed — hidden from the Meshes list, since it's
 *  driven entirely from the CEC tab. This single well-known id replaced the
 *  old per-customer `cec-<number>` rooms: sessions ride the area now. */
const CEC_AREA_NETWORK_ID = "cecsupport-clients";

/** The CEC asking room (the node's `ASK_NETWORK_ID`) — the help queue itself.
 *  Membership is the raised hand: a customer sits in it only while asking, a
 *  watching technician reads it listen-only. Node-managed and transient, so
 *  the Meshes list hides it like the area. */
const CEC_ASK_NETWORK_ID = "cecsupport-asking";

/** A stored customer unused for this long reads as "stale" — the cleanup nudge
 *  for a directory that grows as customers cycle out. Shared by the CEC tab's
 *  per-row badge and the "Remove stale" bulk curate action. */
export const CEC_STALE_AFTER_S = 7 * 24 * 60 * 60;

/** Which pane the settings panel is showing. */
export type SettingsTab =
  | "networks"
  | "venues"
  | "devices"
  | "fleet"
  | "sharing"
  | "always_on"
  | "updates"
  | "danger"
  // The secret CEC Support tab — shown only when a technician reveals it with
  // the hidden keyboard gesture (see `App.cecRevealed`).
  | "cec";

/** Sub-pane within the Networks settings tab. The all-devices roster used to
 *  live here as a third "Devices" sub-tab; it's now a top-level Devices tab of
 *  its own, so a mesh's sub-panes are just its status and its venue (the latter
 *  reached by clicking a mesh's Venue button). */
export type NetworksSubtab = "status" | "servers";

/** A capability the Share Flow builder can switch on between two devices.
 *  Audio / Video (the screen feed) / Control all ride the remote-control
 *  console; Control needs Video to map focus. Terminal / Files / Sites are
 *  their own console shares. */
/** What a share can hand over, as a *person* thinks about it.
 *
 *  `console` is the remote-control console as one thing — screen, sound,
 *  keyboard/mouse and clipboard. Those are four separate grants underneath
 *  and stay that way on the wire (each is scoped to its own capability and
 *  revocable on its own), but nobody shares "display without input" on
 *  purpose: picking them apart only ever produced a console that came up
 *  half-dead. One toggle, four grants. */
export type ShareCap = "console" | "terminal" | "files" | "sites";

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

interface KvmApiOutcome<T> {
  rsp: KvmApiRsp<T> | null;
  timedOut: boolean;
  reason: string | null;
}

function normalizeWifi(data: KvmWifiStatusRaw): KvmWifiStatus {
  const ssid = (data.ssid ?? data.wifi?.ssid ?? "").trim();
  return {
    supported: !!data.supported,
    apMode: !!data.apMode,
    connected: !!data.connected,
    ssid: ssid || null,
  };
}

function sortNetworks(list: KvmWifiNetwork[]): KvmWifiNetwork[] {
  return list
    .filter((network) => !!network?.ssid)
    .slice()
    .sort((a, b) => (b.signal ?? -999) - (a.signal ?? -999));
}

function demoWifiNetworks(): KvmWifiNetwork[] {
  return [
    { ssid: "Office Wi-Fi", signal: -48, security: "WPA2" },
    { ssid: "Guest", signal: -67, security: "WPA2" },
  ];
}

/** A short, readable device id for labels when no friendly name is known. */
/** Order-insensitive equality of two server lists — how an invite's venue
 *  is matched against one already here, so re-joining doesn't mint dupes. */
function sameServerSet(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  const sa = [...a].sort();
  const sb = [...b].sort();
  return sa.every((x, i) => x === sb[i]);
}

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
const PRESENCE_GRACE_MS = 45_000;

/** How long a node refresh may spin before we stop waiting. A reachable node's
 *  re-learn round-trip lands in a second or two (then the spinner clears the
 *  moment its details change); this is the fallback for a node that's gone or
 *  simply never answers, so the ring doesn't spin forever. */
const REFRESH_TIMEOUT_MS = 12_000;

/** The app features the running binary always supports — the GUI's mirror of
 *  the node's `build_profile` feature list (node/src/mesh.rs). A peer learns
 *  these from our presence/capability advert, but the *local* node never
 *  receives its own advert (presence arrives from peers), so it must stamp
 *  them itself — otherwise sharing this machine's Terminal/Files/Sites is
 *  greyed out (their availability gates read `node.features`), while
 *  Screen/Audio/Control work because they resolve through scanned
 *  capabilities. Sites additionally needs at least one exposed site (wired
 *  from `exposedSites`), so it isn't claimed until something is exposed. */
const LOCAL_FEATURES = [
  FEATURE_TERMINAL,
  FEATURE_FILES,
  FEATURE_ROOMS,
  FEATURE_CAMERA,
  FEATURE_SITES,
];

/** Which of a machine's consoles/legs are available to open *right now* — the
 *  per-capability availability the graph cards, the drawer, and the console
 *  itself all read so they agree on what to show (and what to hide). */
export type ConsoleAccess = {
  remote: boolean;
  files: boolean;
  terminal: boolean;
  sites: boolean;
  audio: boolean;
  control: boolean;
  clipboard: boolean;
};

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
  persistent?: boolean;
  tag?: "update";
  actions?: Array<{
    label: string;
    action: "apply_update" | "relaunch_update" | "open_updates";
    primary?: boolean;
  }>;
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

/** The console quality surface a previous window left selected (slider or
 *  pills), shared across windows via localStorage. Defaults to the simpler
 *  slider; falls back to it where storage isn't available. */
function loadConsoleControlMode(): "slider" | "pills" {
  try {
    return localStorage.getItem("ams.consoleControlMode") === "pills" ? "pills" : "slider";
  } catch {
    return "slider";
  }
}

/** Whether the secret CEC Support tab has been unlocked on this device
 *  (persisted, toggled by the secret gesture in App.svelte). */
function loadCecEnabled(): boolean {
  try {
    return localStorage.getItem("ams.cec.enabled") === "1";
  } catch {
    return false;
  }
}

/** The persisted technician Agent Name — the name a customer sees in the
 *  connect prompt. */
function loadCecAgentName(): string {
  try {
    return localStorage.getItem("ams.cec.agentName") ?? "";
  } catch {
    return "";
  }
}

/** localStorage key for the technician's private customer labels. */
const CEC_ALIAS_STORE_KEY = "ams.cec.aliases";

/** The technician's private customer labels (customer **number** → alias). Kept
 *  GUI-side and keyed by the number (stable per customer) rather than the graph
 *  node id, so a label survives a restart even though the dialed list — an
 *  ephemeral, re-dialed-on-launch thing — does not. */
function loadCecAliases(): Record<string, string> {
  try {
    const raw = localStorage.getItem(CEC_ALIAS_STORE_KEY);
    const v = raw ? JSON.parse(raw) : null;
    return v && typeof v === "object" ? (v as Record<string, string>) : {};
  } catch {
    return {};
  }
}

type UiMode = "normal" | "advanced";
const UI_MODE_STORE_KEY = "allmystuff.uiMode.v1";

/** The native app opens in the deliberately quiet, graph-first experience.
 *  Once someone opts into Advanced we remember that choice on this machine. */
function loadUiMode(): UiMode {
  try {
    return localStorage.getItem(UI_MODE_STORE_KEY) === "advanced" ? "advanced" : "normal";
  } catch {
    return "normal";
  }
}

class AppStore {
  // Under the real app the graph is built entirely from the live scan + mesh
  // presence, so it starts empty and fills with *your* stuff. The demo
  // catalog is only a stand-in for the browser/preview build (no Tauri
  // backend) so the marketing page is never blank.
  catalog = $state<Catalog>(isTauri() ? emptyCatalog() : demoCatalog());

  // ---- interaction state ------------------------------------------
  uiMode = $state<UiMode>(loadUiMode());
  selectedNodeId = $state<string | null>(null);
  /** A "centre the graph on this node" request. Bumped by [`focusNode`]; the
   *  graph watches the `seq` and pans to the node once per new request (so a
   *  repeat focus of the same node still re-centres). Null = nothing pending. */
  focusRequest = $state<{ id: string; seq: number } | null>(null);
  /** Capability the user is dragging a wire from, if any. */
  dragFrom = $state<string | null>(null);
  pendingShare = $state<PendingShare | null>(null);
  /** The unified Settings panel (networks · updates · fleet) and which pane
   *  it's showing. The top-bar gear opens it; the Networks button deep-links
   *  to the networks pane. */
  settingsOpen = $state(false);
  settingsTab = $state<SettingsTab>("networks");
  // ---- CEC Support (technician-side remote help desk) --------------
  //
  // The secret CEC tab and its state. The tab is a purely manual show/hide,
  // toggled by the hidden keyboard gesture (Ctrl+Alt+Shift+C in App.svelte) and
  // persisted — the node's CEC role/number never auto-reveals it. The Agent Name
  // persists locally so it survives a restart and is sent with every dial.
  cecEnabled = $state(loadCecEnabled());
  cecStatusInfo = $state<CecStatus | null>(null);
  /** The technician's Agent Name — what a customer sees in the connect prompt.
   *  Persisted in localStorage. */
  cecAgentName = $state(loadCecAgentName());
  /** Customer number the technician is about to dial (the tab's input). */
  cecNumberDraft = $state("");
  /** Whether a dial is in flight (disables the Connect button). */
  cecDialing = $state(false);

  /** The number currently being dialed, while the dial RPC is in flight — the
   *  CEC tab renders it as a live "Dialing…" row so a connect attempt is
   *  visible from the first click (discovery alone can take up to ~45s). */
  cecDialingNumber = $state<string | null>(null);
  /** The NODE currently being dialed (Answer / Open / Chat go by node, which
   *  never sets `cecDialingNumber`) — what lets the help sidebar pin its
   *  inline "Connecting…" status to the row that was actually clicked. */
  cecDialingNode = $state<string | null>(null);
  /** Live CEC sessions this app is party to, newest first. */
  cecSessions = $state<{ sessionId: string; state: string; node?: string }[]>([]);
  /** The customer's live consent grants (populated when hosting). */
  cecGrantList = $state<CecGrant[]>([]);
  /** Inbound technician requests awaiting this customer's choice. */
  cecRequests = $state<CecPending[]>([]);
  /** Customers this technician has dialed — the CEC tab's "Active connections"
   *  list. Read from CEC state (`cec_dialed`), not from the graph: a dialed
   *  customer is an ordinary peer with no special grouping (the CEC mesh is
   *  Silent, so there is no roster to build a "fleet" from). */
  cecCustomers = $state<CecPeer[]>([]);
  /** The technician's private customer labels (customer number → alias),
   *  persisted GUI-side. Lets a tech name a customer something they'll remember
   *  ("Dr Kim's front desk") independent of the machine's own hostname. */
  cecAliases = $state<Record<string, string>>(loadCecAliases());
  /** The dialed customers sorted most-recently-used first — the order the CEC
   *  tab lists them in, so active connections stay at the top and stale ones
   *  sink to where they're easy to prune. */
  cecCustomersByRecent = $derived.by(() =>
    [...this.cecCustomers].sort((a, b) => (b.last_used ?? 0) - (a.last_used ?? 0)),
  );
  /** A customer just dialed, waiting for their approval: when their CEC session
   *  goes active, the technician's remote-control console opens for them (the
   *  AnyDesk-style "connect and you're in"). Cleared once opened. */
  cecAutoOpenNode = $state<string | null>(null);
  /** When the pending auto-open was armed by the **Chat** button, open only the
   *  chat window on approval, not the remote-control console — so a technician
   *  can reach a customer by chat *before* (or without) taking their screen. A
   *  Remote-Control / Answer dial leaves this false and opens both. */
  private cecAutoOpenChatOnly = false;
  /** The customers currently asking for help on the global help room —
   *  longest-waiting first (the node keeps queue order). Drives the CEC tab's
   *  "Asking for help" section; fed by `cec_help_list` + the `cec://help`
   *  event. */
  cecHelpWaiting = $state<CecHelpSeeker[]>([]);
  /** Chat threads with CEC customers, keyed by the customer's node id, each
   *  oldest-first. Reassigned (never mutated in place) on every change so the
   *  pop-out chat window stays reactive — the same discipline as `cecAliases`.
   *  Every window that runs `onCecChat` fills this; the chat window reads it. */
  chatThreads = $state<Record<string, CecChatMsg[]>>({});
  /** Unread inbound (customer) lines per customer since their thread was last
   *  read — the little badge on the drawer's Chat affordance. Cleared by
   *  {@link markChatRead} when the chat window takes focus. */
  chatUnread = $state<Record<string, number>>({});
  /** The "Watch the help queue" opt-in, mirrored from the node's room
   *  membership (`cec_status.help_watching`) — the saved state lives in the
   *  daemon, so it survives restarts without a GUI-side flag. Off means this
   *  install is not on the global help room at all. */
  cecHelpWatching = $derived(this.cecStatusInfo?.help_watching === true);
  /** Whether the secret CEC tab shows in Settings. Purely the manual reveal —
   *  the persisted keyboard-gesture toggle (Ctrl+Alt+Shift+C in App.svelte).
   *  Node CEC status/role deliberately does *not* auto-reveal it. */
  cecRevealed = $derived(this.cecEnabled);
  /** The "a new device wants to join" approval popup (the code-grid nudge). */
  approvalsOpen = $state(false);
  /** The "claim a device" sheet — the forefront adoption surface, opened from
   *  the top-bar nudge or a device's drawer. Claiming is the step right after
   *  joining a network, so it gets the same prominence the join nudge has. */
  claimOpen = $state(false);
  manageShareNodeId = $state<string | null>(null);
  /** The KVM node whose attach-target dropdown is currently dropped out
   *  (opened from the link button in its card's button bar) — the same
   *  one-at-a-time reveal model as the claim drawer. Null = none showing. */
  kvmRevealed = $state<string | null>(null);
  // KVM Wi-Fi modal state. The mapped console port is reused for the modal's
  // status, scan and write calls so one open does not create several tunnels.
  wifiFor = $state<string | null>(null);
  wifiStatus = $state<KvmWifiStatus | null>(null);
  wifiScan = $state<KvmWifiNetwork[] | null>(null);
  wifiScanSource = $state<"kvm" | "host" | null>(null);
  wifiHostCurrent = $state<string | null>(null);
  wifiHostNote = $state<string | null>(null);
  wifiLoading = $state(false);
  wifiScanning = $state(false);
  wifiBusy = $state(false);
  wifiError = $state<string | null>(null);
  private wifiPort: number | null = null;
  private kvmUpdating = $state<Record<string, boolean>>({});
  /** The latched clock-skew warning: this machine's wall clock is well out
   *  of line with its peers' (estimated passively — no extra calls to any
   *  node). Null = in sync (or nothing measurable). Drives the topbar pill;
   *  set/cleared by the `allmystuff://clock-skew` transitions. */
  clockSkew = $state<{ skewMs: number | null; peers: number | null; message: string } | null>(
    null,
  );
  /** The node's *mesh* status ("live" / "no_network" / "disconnected" /
   *  "unknown") — distinct from `backendConnected`, which only says the node
   *  socket answers. The node can be perfectly alive while its myownmesh
   *  daemon is down (it reconnects on its own now); during that window the
   *  header says "mesh reconnecting…" and the per-call error toasts stay
   *  quiet (they'd all share this one cause). */
  meshStatus = $state<string>("unknown");
  /** Per-key latch for mesh-diagnostic toasts (the no-TURN hint), so a
   *  watchdog that re-fires per ladder cycle warns once per peer, not on
   *  every cycle. */
  private meshAlerted = new Set<string>();
  toasts = $state<Toast[]>([]);
  /** Nodes with a refresh in flight — the card's refresh ring spins and its
   *  button is disabled until the data lands (the node's fingerprint changes on
   *  a later sync) or the request times out. Keyed by canonical pubkey; the
   *  value carries the fingerprint captured when the refresh started and its
   *  timeout handle. */
  private refreshInFlight = $state<
    Record<string, { fp: string; timer: ReturnType<typeof setTimeout> }>
  >({});
  backendConnected = $state(false);

  // ---- share flow builder (Sender → Receiver) ---------------------
  /** The side-by-side share builder, when open. `sender` makes things
   *  available; `receiver` gets them. Opened from "New Share" or by dragging
   *  one device onto another on the graph. */
  shareFlowOpen = $state(false);
  shareFlowSender = $state<string | null>(null);
  // A node id of *any* device in the receiving fleet — the builder presents it
  // as that fleet (its owner/person), and the share fans across all their
  // devices. Kept a node id (not a person id) so the existing grant path works.
  shareFlowReceiver = $state<string | null>(null);
  /** Consoles to pre-toggle when the builder opens — used by "Manage share" to
   *  show what's already granted to that fleet. */
  shareFlowInitialCaps = $state<ShareCap[]>([]);
  /** The folder the builder's Files toggle is about, once one is picked — the
   *  minted id is what its grant pins to, so this is what turns "Files" from
   *  "the whole disk" into "this folder". Null until picked, which is why the
   *  toggle can't be completed without choosing one. */
  shareFlowFolder = $state<{ id: string; label: string; path: string } | null>(null);
  /** Set while the pick is being minted on the device that holds the folder,
   *  so the builder can say it's working rather than look stuck. */
  shareFlowFolderPending = $state(false);

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
  /** Console legs the user has explicitly switched OFF this session. The
   *  auto-legs — both the one-shot at open and the re-assert on every session
   *  snapshot — only ever bring a leg *up*, and by the on/off flag alone they
   *  can't tell "not wired yet" from "the user just turned it off": both read
   *  as `false`. Tearing a leg down itself pushes a fresh snapshot, so without
   *  this the re-assert would flip a just-silenced leg straight back on — the
   *  "I can't turn off audio" bug (audio is the leg people actually switch off
   *  mid-session). Reset on each console open; cleared for a leg the moment the
   *  user turns it back on. */
  private consoleUserOff = new Set<"audio" | "control" | "clipboard">();
  /** The *live* display route the console renders frames for — also set when
   *  the route pre-existed (owned-for-teardown is tracked separately). */
  consoleVideoLive = $state<string | null>(null);
  /** Per-source video controls, keyed by the source capability id (screen,
   *  an extra monitor, a camera). Each source keeps its own codec + quality,
   *  so switching sources restores that source's picks rather than carrying
   *  one shared setting across all of them. The node already tunes per
   *  route-id; this is the GUI remembering which pick belongs to which. */
  private consoleCodecBySource = $state<Record<string, "auto" | "h264" | "mjpeg">>({});
  private consoleTuneBySource = $state<Record<string, StreamTune>>({});
  /** The selected source's codec (which transport to *offer*). "auto" and
   *  "h264" both offer H.264; "mjpeg" forces the fallback. */
  get consoleCodec(): "auto" | "h264" | "mjpeg" {
    const s = this.consoleInput;
    return (s ? this.consoleCodecBySource[s] : undefined) ?? "auto";
  }
  /** The selected source's quality picks — absent fields are Automatic. */
  get consoleTune(): StreamTune {
    const s = this.consoleInput;
    return (s ? this.consoleTuneBySource[s] : undefined) ?? {};
  }
  /** Which quality surface the console shows — the single Speed↔Quality
   *  slider or the four granular pills. The "…" button flips it, and it's
   *  remembered across windows, so a freshly opened console opens the way
   *  you last left it. */
  consoleControlMode = $state<"slider" | "pills">(loadConsoleControlMode());
  /** The Experimental (Labs) tier — the Mode dropdown's toggle. Persisted
   *  so it survives restarts and is pushed to the node on boot; the node's
   *  labs gate is what every future field-trial feature reads. Off by
   *  default: a stock session behaves exactly like today. */
  labsTier = $state<boolean>(localStorage.getItem("ams.labs") === "1");
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
  driveMounts = $state<Record<string, { label: string; mount: string }>>({});
  /** The resolved host-side terminal session id per terminal route id, from
   *  the snapshot (the host echoes it on `Accept` for a shared shell). A
   *  terminal tab reads it to label which shell it's on and to re-query the
   *  host for the live attacher count ("shared with N"). */
  routeSessions = $state<Record<string, string>>({});
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
  sidebarTab = $state<"rooms" | "sites" | "help">("sites");
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
  /** The venues pill's dropdown — the master on/off for each venue, the sibling
   *  of the meshes pill. */
  venueMenuOpen = $state(false);
  /** Venues the user has switched **off** (by id). An off-list, so venues are
   *  on by default; only the user adds to it, while driving a mesh can only
   *  remove from it. */
  inactiveVenues = $state<string[]>(loadInactiveVenues());
  /** Briefly true right after driving a mesh turned a venue back on, so the
   *  venues pill can shimmer to say "something here changed". */
  venuePillShimmer = $state(false);
  /** The refresh control's 3-step progress, shown floating over the graph while
   *  a restart runs. Each step's status drives a red→yellow→green dot:
   *  `wait` (red) → `go` (yellow) → `ok` (green). Null when idle. */
  restartFlow = $state<{ label: string; status: "wait" | "go" | "ok" }[] | null>(null);
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

  // ---- owned fleet (the closed network's signed roster) -----------
  /** The shared key + members linking the devices you've claimed. Members are
   *  the fleet's closed-network signed roster — authenticated, not gossip. */
  ownedFleet = $state<OwnedRoster | null>(null);

  /** The fleet's display name ("Casey"), empty when unnamed. */
  fleetName = $derived.by(() => this.ownedFleet?.name?.trim() ?? "");

  /** Whether **this** device is in a fleet at all — the single self-membership
   *  truth the whole UI reads, taken straight from the backend's one `in_fleet`
   *  flag (true when you hold the key *or* have been claimed). A fleet of one
   *  counts; an owned-but-keyless device (claimed, awaiting its key) counts.
   *  The drawer, the settings Fleet pane, the graph label and the leave control
   *  all read this, so none of them can claim you're in a fleet while another
   *  insists you're not. */
  inFleet = $derived.by(() => this.ownedFleet?.in_fleet === true);

  /** This device's *own* role in the fleet ("owner" | "manager" | "member" |
   *  null when not in one). Gates which governance controls are shown, matching
   *  the backend's flat-tier authority: an owner can act on anyone, a manager on
   *  managers and members, a member on no one. */
  myFleetRole = $derived.by(() => this.fleetRoleOf(this.localId));

  /** Whether this device is the fleet owner (founder / key-holder). Only the
   *  owner can rename the fleet, grant/withdraw roles, or evict a device — the
   *  backend enforces it; the UI gates to match so members aren't shown
   *  actions that would fail. */
  isFleetOwner = $derived.by(() => this.ownedFleet?.is_owner ?? false);

  /** The fleet's closed-network id (the word-salad mesh name), if any. The
   *  meshes list uses it to lock the fleet mesh. */
  fleetNetworkId = $derived.by(() => this.ownedFleet?.network_id?.trim() || null);

  /** Whether a mesh in the list is *the fleet mesh* — the closed network that
   *  backs your fleet. It's joined and left via the fleet, not as a mesh. */
  isFleetMesh(net: { network_id?: string } | null | undefined): boolean {
    const id = this.fleetNetworkId;
    return !!id && !!net?.network_id && net.network_id === id;
  }

  /** Whether a mesh in the list is the node-owned **local claiming** network
   *  — the fixed mDNS passthrough for claiming and local pairing. It can't be
   *  left or configured (no venue, no invites), only switched on and off; the
   *  node re-joins it whenever it isn't deliberately disabled. */
  isLocalClaimMesh(net: { network_id?: string } | null | undefined): boolean {
    return net?.network_id === LOCAL_CLAIM_NETWORK_ID;
  }

  /** Whether a mesh is managed by CEC Support: its public discovery/asking
   *  directories or a private `cec-<Support ID>` session room. These are
   *  lifecycle-owned by the support workflow, never user meshes. */
  isManagedCecMesh(net: { network_id?: string } | null | undefined): boolean {
    return (
      net?.network_id === CEC_AREA_NETWORK_ID ||
      net?.network_id === CEC_ASK_NETWORK_ID ||
      /^cec-\d{9}$/.test(net?.network_id ?? "")
    );
  }

  /** Your meshes minus the CEC Support area — what the Meshes list actually
   *  shows. Client support lives in the (secret) CEC tab instead. */
  normalNetworks = $derived.by(() =>
    this.networks.filter((n) => !this.isManagedCecMesh(n)),
  );

  /** Parked user meshes, excluding CEC's private/public workflow rooms. */
  normalDisabledNets = $derived.by(() =>
    this.disabledNets.filter((n) => !this.isManagedCecMesh(n)),
  );

  /** The fleet owner's display name, read from the signed roster (the member
   *  whose role is "owner"). Lets every device — member, manager, owner — label
   *  the fleet mesh by its owner even without an explicit fleet name. Empty when
   *  the owner isn't in the roster yet. */
  fleetOwnerName(): string {
    const owner = this.ownedFleet?.members.find((m) => this.fleetRoleOf(m.device) === "owner");
    return owner?.label?.trim() ?? "";
  }

  /** The display name for a mesh. The fleet mesh reads "<owner>'s Fleet" — the
   *  explicit fleet name if set, else the owner's name from the roster — so
   *  members and managers show the same label the owner does, sourced from the
   *  roster that always converges, not a backend network label that may not have
   *  reached them. Every other mesh is just its own label/id. */
  meshLabel(net: { network_id?: string; label?: string | null; config_id?: string } | null | undefined): string {
    if (!net) return "";
    if (this.isFleetMesh(net)) {
      const name = this.fleetName || this.fleetOwnerName();
      if (name) return `${name}'s Fleet`;
    }
    if (this.isLocalClaimMesh(net)) return "Local";
    return networkDisplayName(net);
  }

  /** A fleet member's governance role ("member" | "manager" | "owner"), for
   *  the drawer's grant/withdraw controls. "controller" is surfaced as the
   *  friendlier "manager". Null when the device isn't a fleet member. */
  fleetRoleOf(deviceId: string): "member" | "manager" | "owner" | null {
    const m = this.ownedFleet?.members.find((x) => sameMachine(x.device, deviceId));
    if (!m) return null;
    if (m.role === "owner") return "owner";
    if (m.role === "controller") return "manager";
    // The founding owner's member entry often isn't stamped with a role — the
    // roster only carries `is_owner` for *this* device. Honour it so the owner
    // machine reads as "owner", not a plain member, on its own screen.
    if (this.isMe(deviceId) && this.isFleetOwner) return "owner";
    return "member";
  }

  /** The single, derived **standing** of a node relative to you — what the
   *  graph, the drawer and every claim/fleet button read so they always agree.
   *  Computed live from the authoritative reactive state (your fleet roster,
   *  the device's advertised owner + claimable flag, its share). Nothing here
   *  is stored: change the fleet or a device's advert and this recomputes, so
   *  the UI can't drift into a contradictory "unclaimed but in your fleet"
   *  state the way the old stored `relationship.kind` did. */
  standing(node: MeshNode): Standing {
    const self = node.kind === "this" || this.isMe(node.id);
    const app = isAppNode(node);
    const shared = node.relationship.kind === "shared" ? node.relationship.person : null;
    // Roster-authoritative ownership. The signed fleet roster — what the
    // settings Fleet pane and the graph's fleet grouping both read — is the
    // authority for "is this device in my fleet". A device's *own* presence
    // advert of "owner = you" is a weaker, staler hint: it lingers after the
    // device leaves or is evicted (it may be offline, or have missed the
    // Release), so it must not keep a device marked mine once the roster has
    // dropped it — that's the graph-vs-settings divergence. Fall back to the
    // advert only when you hold no fleet at all (nothing authoritative to
    // contradict it).
    const advertisedMine = !!node.owner && this.isMe(node.owner);
    const ownedByOther = !!node.owner && !this.isMe(node.owner);
    // A KVM we claimed announces us as its owner and — via our fleet-key
    // handoff — joins our fleet's CLOSED mesh (it can only be there because we
    // admitted it). A constrained KVM can lag converging into the signed roster
    // membership is normally read from, so `isFleetMember` misses it for a while
    // — leaving it stuck "unclaimed / unknown fleet" with its own claimed-state
    // controls, the exact contradiction this resolves. Trust the KVM's own
    // advert when it is BOTH owned-by-us AND live on our fleet network. Unlike a
    // bare owner-advert this can't pin a stale claim: eviction drops the device
    // from the fleet mesh, so `onMyFleetMesh` clears the moment it's really gone.
    // (The backend self-heals the signed roster too; this makes the graph agree
    // immediately, before that convergence lands.)
    const onMyFleetMesh =
      !!this.fleetNetworkId && (node.kvm?.meshes ?? []).includes(this.fleetNetworkId);
    const kvmClaimedByMe = !self && this.isKvm(node) && advertisedMine && onMyFleetMesh;
    // "In a fleet": for *this* device, whether it's claimed at all — either it
    // holds a fleet credential (a solo fleet you founded still counts) *or* it's
    // been claimed by an owner whose fleet-key handoff hasn't landed yet (owned
    // but keyless). Both mean it isn't free: the make-claimable button gives way
    // to "Leave the fleet", matching the backend's no-claiming-while-owned rule
    // so the UI can't say "not in a fleet" while the backend refuses adoption.
    // For a remote device, whether it's a member of your fleet roster — plus the
    // just-claimed-KVM self-heal above, so it reads as ours the instant it lands
    // on our fleet mesh, not only once the signed roster catches up.
    const inFleet = self ? this.inFleet : this.isFleetMember(node.id) || kvmClaimedByMe;
    const role = this.fleetRoleOf(node.id);
    const ownedByMe = inFleet || (advertisedMine && !this.inFleet);
    const offering = node.claimable === true;
    const mine = ownedByMe;
    // Claiming needs the device reachable *right now*: a machine that has gone
    // offline (left the mesh, dropped off a disabled network, or is past its
    // presence grace) is no longer available for adoption, so it must not read
    // as claimable even while its last advert still says it was offering.
    // `offering` stays raw (it's this device's own advertised claim-mode flag,
    // which the drawer's Make-claimable toggle reflects); `claimable` is the
    // derived "you can take it now", and that's what every claim affordance and
    // the graph's claimable badge read. `online` is the same live-reachability
    // signal the graph filter and every presence dot use.
    const claimable = !self && app && offering && !mine && !ownedByOther && node.online === true;
    let kind: Standing["kind"];
    if (self) kind = "self";
    else if (!app) kind = "mesh";
    else if (shared) kind = "shared";
    else if (inFleet) kind = "fleet";
    else if (ownedByMe) kind = "mine";
    else if (claimable) kind = "claimable";
    else if (ownedByOther) kind = "theirs";
    else kind = "free";
    return {
      self,
      app,
      mine,
      inFleet,
      role,
      iAmFleetOwner: this.isFleetOwner,
      ownedByMe,
      ownedByOther,
      claimable,
      offering,
      shared,
      kind,
    };
  }

  /** Every node's standing, as a **derived** map keyed by node id — so the
   *  graph and drawer read a reactive value (which Svelte re-tracks the moment
   *  the fleet roster or any device's advert changes) rather than calling a
   *  method per render and hoping the dependency is tracked. This is the single
   *  reactive source the whole claim/fleet UI consumes. */
  standings = $derived.by(() => {
    const m = new Map<string, Standing>();
    for (const n of this.catalog.nodes) m.set(n.id, this.standing(n));
    return m;
  });

  /** The standing for one node — the reactive map entry, with a live fallback
   *  for a node that isn't in the catalog yet. */
  standingOf(node: MeshNode): Standing {
    return this.standings.get(node.id) ?? this.standing(node);
  }

  /** Whether this device has a fleet custody authenticator enrolled. When it
   *  is, owner governance acts (evict, grant/withdraw role) need a fresh code,
   *  so the UI prompts for one. Refreshed on the mesh poll. */
  fleetMfaEnrolled = $state(false);

  /** A pending owner-governance action waiting on a custody code. When set, a
   *  small modal collects the code and calls `run(code)`. Null = no prompt. */
  fleetCodePrompt = $state<{ title: string; run: (code: string) => Promise<void> } | null>(null);

  async loadFleetMfa() {
    if (!isTauri()) return;
    try {
      this.fleetMfaEnrolled = (await fleetMfaStatus()).enrolled;
    } catch {
      this.fleetMfaEnrolled = false;
    }
  }

  /** Run an owner-authority governance action. When fleet MFA is enrolled the
   *  daemon needs a fresh custody code, so open the prompt (the modal calls
   *  the action with the entered code); otherwise run it straight. */
  private async runFleetGov(title: string, action: (code?: string) => Promise<void>) {
    if (this.fleetMfaEnrolled) {
      this.fleetCodePrompt = { title, run: (code: string) => action(code) };
      return;
    }
    try {
      await action(undefined);
      // No toast on success — the fleet roster's role tags update in place.
    } catch (e) {
      this.toast("warn", `${title} failed: ${String(e)}`);
    }
  }

  /** Grant a fleet member a role (owner-only; backend enforces + may need MFA). */
  async grantFleetRole(device: string, role: "manager" | "owner") {
    if (this.isKvm(this.machineByAnyId(device))) {
      this.toast("warn", "KVMs are fixed-role fleet members and cannot be promoted");
      return;
    }
    await this.runFleetGov(role === "owner" ? "Make owner" : "Make manager", (code) =>
      fleetGrantRole(device, role, code),
    );
  }

  /** Withdraw a fleet member's role, back to a plain member (owner-only). */
  async withdrawFleetRole(device: string) {
    if (this.isKvm(this.machineByAnyId(device))) {
      this.toast("warn", "KVMs are fixed-role fleet members and cannot be demoted");
      return;
    }
    await this.runFleetGov("Withdraw role", (code) => fleetRevokeRole(device, code));
  }

  /** Designate the fleet's infra hubs (owner-only; daemon ≥ 0.2.36 — the
   *  backend enforces authority and may need the MFA code). Pass the FULL
   *  hub set; an empty set signs the fleet back to full mesh. */
  async setFleetHubs(hubs: string[], redundancy?: number) {
    await this.runFleetGov(
      hubs.length ? "Set infra hubs" : "Reset to full mesh",
      (code) => fleetSetHubs(hubs, redundancy, code),
    );
  }

  /** Name (or rename) the fleet — members only (the backend enforces it;
   *  the demo mirrors the rule). The renamed roster gossips out and every
   *  member converges, exactly like a kick. */
  async setFleetName(name: string) {
    const clean = name.trim();
    if (this.backendConnected) {
      try {
        await fleetSetName(clean);
        // No toast — the fleet name shows its new value in the pane immediately.
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
    // No toast — the fleet name shows its new value in the pane immediately.
  }

  // ---- self-update -------------------------------------------------
  updateInfo = $state<UpdateStatus | null>(null);
  updateBusy = $state(false);
  /** Result of the last manual "check now", for the Updates pane. */
  updateOutcome = $state<CheckOutcome | null>(null);
  /** Version just applied to disk this session (via "Apply now") and now
   *  awaiting a relaunch to actually run. Drives the Updates pane's
   *  "Relaunch now" prompt. */
  updateApplied = $state<string | null>(null);
  componentVersions = $state<ComponentVersionRow[]>([]);
  componentBusy = $state<string | null>(null);
  /** The channel's latest release version, learned once (read-only) so the
   *  drawer can tell which of your fleet machines are behind it. Null until
   *  loaded; stays null in web mode / if the feed can't be reached. */
  latestRelease = $state<string | null>(null);
  private latestReleaseLoading = false;

  // ---- "Always On": background service + window behaviour ----------
  /** OS background-service status, for the Always On tab. Null until loaded
   *  (or in web mode). */
  serviceInfo = $state<ServiceStatus | null>(null);
  /** A service install/start/stop/… is in flight (buttons disabled). */
  serviceBusy = $state(false);
  /** Whether closing / minimizing keeps the app in the tray. Null until read
   *  from the backend (the source of truth). */
  windowBehavior = $state<WindowBehavior | null>(null);
  /** Whether "Start with computer" (the OS login item) is registered. Null
   *  until read from the backend. */
  autostartEnabled = $state<boolean | null>(null);
  /** Opt-in verbose node/GUI logging. Null until the local preference loads. */
  debugLoggingEnabled = $state<boolean | null>(null);

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

  /** This machine's own node — the "this device" the drawer falls back to when
   *  nothing is selected, so the panel always has something to show rather than
   *  vanishing. Matched by the definitive `kind === "this"` marker, with the
   *  local-id match as a backstop before the first scan re-homes it. */
  localNode = $derived(
    this.catalog.nodes.find((n) => n.kind === "this") ??
      this.catalog.nodes.find((n) => this.isLocalMachine(n.id)) ??
      null,
  );

  /** The machine a console session is currently open on, if any. Resolved
   *  canonically (by bare pubkey), not by exact id: the id is captured when the
   *  console opens, but a node's display id can change form afterwards (a
   *  presence snapshot re-homing it onto its `pubkey-SUFFIX` display id). An
   *  exact `n.id === consoleNodeId` then misses it, leaving `<Console>`'s
   *  `{#if node}` false and the whole remote-console window blank. */
  consoleNode = $derived(
    this.consoleNodeId ? this.machineByAnyId(this.consoleNodeId) ?? null : null,
  );

  /** Routes running between this machine and the console's remote — the
   *  live session the console's footer chips show. */
  consoleSessionRoutes = $derived.by(() => {
    const remote = this.consoleNodeId;
    if (!remote) return [] as Route[];
    const customer = this.machineByAnyId(remote);
    const kvm = customer && this.isCecCustomer(customer.id) ? this.kvmAttachedTo(customer.id) : null;
    const remoteEnds = [remote, ...(kvm ? [kvm.id] : [])];
    return this.catalog.routes.filter((r) => {
      const f = this.capability(r.from);
      const t = this.capability(r.to);
      if (!f || !t) return false;
      const ends = [f.node, t.node];
      return ends.some((end) => remoteEnds.some((peer) => sameMachine(end, peer))) &&
        ends.some((end) => this.isMe(end));
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
  /** Header-facing active mesh. CEC's managed rooms must never replace the
   *  user's Meshes pill label merely because one became the session primary. */
  activeNormalNetwork = $derived.by(() =>
    this.normalNetworks.find((n) => n.config_id === this.sessionNetwork) ??
    this.normalNetworks[0] ??
    null,
  );
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
   *  running AllMyStuff, reachable right now, still unclaimed, in claim mode,
   *  and not already owned by someone else. The mirror of `freshJoins` for the
   *  claim step: what the top-bar "ready to claim" nudge counts and the Claim
   *  sheet lists. The `online` gate is what keeps a device that's no longer
   *  available — it left the mesh, dropped off a network you've disabled, or is
   *  riding out its last presence grace before being pruned — from lingering
   *  here as "ready to claim" on the strength of a stale advert; a claim can't
   *  land on a machine we can't reach, so it must not read as available. This
   *  matches the graph, which already hides offline devices you don't own. */
  claimables = $derived(
    this.catalog.nodes.filter(
      (n) =>
        isAppNode(n) &&
        n.online === true &&
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

  /** Whether a node is part of your owned fleet (linked by the shared key).
   *  A fleet of one is a real fleet: when you hold a fleet credential the
   *  roster always lists at least yourself, and when you don't it's empty —
   *  so membership is just "is this device on the roster," with no size floor
   *  that would otherwise read a solo fleet as no fleet at all. */
  isFleetMember(nodeId: string): boolean {
    return this.fleetMemberIds.has(canonicalNodeId(nodeId));
  }

  /** Whether an id refers to this very machine (any suffix form). */
  isMe(id: string): boolean {
    return sameMachine(id, this.localId);
  }

  /** Whether two ids name the same machine (canonical pubkey match) — the
   *  public form of the module's `sameMachine`, used to match an inbound
   *  event's `from` (a bare pubkey) against a host's display id. */
  isSameMachine(a: string, b: string): boolean {
    return sameMachine(a, b);
  }

  capsOf(nodeId: string): Capability[] {
    // Canonical (bare-pubkey) match, not exact: the caller's id and a
    // capability's `node` can be different display-id forms of the same machine
    // (`pubkey-SUFFIX` vs the bare pubkey). An exact compare misses them — which
    // left consoleVideoInputs empty, so the console opened with no screen to
    // auto-start.
    const want = canonicalNodeId(nodeId);
    return this.catalog.capabilities.filter((c) => canonicalNodeId(c.node) === want);
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
    // The CEC dialed directory, FIRST — everything below is a prerequisite for
    // nothing here, and a dedicated console window cannot open its target
    // until this lands. `ConsoleHost`'s readiness gate exempts a dialed CEC
    // customer from the fleet-relationship stage (they approve you, you never
    // claim them), and `isCecCustomer` reads `cecCustomers`, which only this
    // fills. Anywhere further down the chain, that window sits on "resolving
    // whether it's yours (fleet roster loading)…" — the one screen whose text
    // is actively wrong for a CEC customer — until a hardware scan, a local
    // service scan and a per-network mesh sync have all finished. On Windows
    // those take long enough to read as broken; on macOS they merely hid the
    // ordering. Fire-and-forget, so starting it here overlaps it with the rest
    // of bring-up instead of queueing it behind ~29 awaits.
    void this.loadCec();
    // Update discovery starts before mesh hydration and hardware scans. Those
    // can take seconds on a cold Windows launch; version checking must not wait
    // behind them or depend on the later background ticker.
    await onUpdateChecked((o) => this.applyUpdateChecked(o));
    if (isTauri() && !isMobile()) void this.checkUpdates();
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
    // This machine refused someone's input/clipboard (route live, sender not
    // authorized): say so here too — the person at this desk is often the
    // same person wondering why their other device's console stopped typing.
    await onControlRefused((e) => {
      const who = this.node(e.from)?.label ?? shortId(e.from);
      this.toast("warn", `Refused ${e.plane} from ${who} — not on this machine's fleet roster`);
    });
    // A fleet peer asked this machine to reboot: tell whoever is sitting
    // here why the OS is about to go down.
    await onDeviceRestart((e) => {
      const who = this.node(e.from)?.label ?? shortId(e.from);
      this.toast("warn", `Restarting this device — asked by ${who}`);
    });
    await onDriveMount((e) => {
      const who = this.nodeByCanonical(e.from)?.label ?? shortId(e.from);
      if (e.error) {
        delete this.driveMounts[e.route];
        this.toast("warn", `Couldn't map the drive from ${who}: ${e.error}`);
      } else {
        this.driveMounts[e.route] = {
          label: e.label || "Remote drive",
          mount: e.mount!,
        };
        this.toast("ok", `${e.label || "Remote drive"} mounted as ${e.mount}`);
      }
    });
    await onKvmMedia((e) => {
      const kvm = this.nodeByCanonical(e.kvm)?.label ?? "the KVM";
      if (e.error)
        this.toast("warn", `Couldn't stage ${e.label || "virtual media"} on ${kvm}: ${e.error}`);
      else this.toast("ok", `${e.label || "Virtual media"} is mounted on ${kvm}`);
    });
    // The passive clock-skew verdict (this machine's clock vs its peers',
    // measured off traffic that was already flowing): keep the topbar pill
    // in step and toast the transitions — a wrong clock quietly breaks
    // fleet-roster convergence and cross-device timestamps.
    await onClockSkew((e) => {
      if (e.state === "warn") {
        this.clockSkew = {
          skewMs: e.skew_ms,
          peers: e.peers,
          message: e.message ?? "This device's clock is out of sync with the network.",
        };
        this.toast("warn", this.clockSkew.message);
      } else {
        this.clockSkew = null;
        this.toast("ok", "This device's clock is back in sync with the network");
      }
    });
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
    // The listener and forced launch check were started before hydration; now
    // read the resulting staged/install state for Settings and node cards.
    await this.loadUpdateStatus();
    // Card-level update affordances need the channel version even when the
    // details drawer never mounts (Normal mode's default).
    void this.loadLatestRelease();
    await this.loadDisabledNetworks();
    this.startMeshPolling();
    await onSubscription((s) => {
      const live = s.status === "live";
      this.meshStatus = s.status;
      // When the mesh comes up, re-scan + reload networks/identity: the first
      // pass at mount can run before the session is ready.
      if (live) {
        void this.hydrateFromBackend();
        this.pullLiveState();
      }
      this.backendConnected = live;
    });
    // The subscription event is one-shot per transition and this window may
    // have subscribed after it fired — read the node's *current* verdict
    // once, so `meshStatus` starts truthful instead of "unknown".
    void linkStatus().then((s) => {
      if (s?.status) this.meshStatus = s.status;
    });
    // The daemon's own diagnostics (forwarded verbatim by the node): map the
    // load-bearing ones onto the graph instead of dropping them — the
    // no-TURN verdict used to be invisible, a peer that was just… quiet.
    await onMeshEvent((e) => this.handleMeshDiag(e));
    await onSession((snap) => this.applySessionSnapshot(snap));
    // CEC Support: keep the tab's state live. A technician trying to connect
    // (customer side) queues a request + toasts; a dialed customer's node
    // updating (technician side) refreshes the graph + the "Active connections"
    // list; a session/grant change keeps the tab in step. (The tab's visibility
    // is the manual keyboard-gesture reveal — it isn't driven by these events.)
    await onCecRequest((r) => {
      this.cecRequests = [r, ...this.cecRequests.filter((p) => p.tech !== r.tech)];
      this.toast("info", `${r.agent_name || "A technician"} is trying to connect (code ${r.verification_code})`);
    });
    await onCecPeer(() => {
      void this.pullSessionSnapshot();
      void this.loadCec();
    });
    await onCecSession((s) => {
      this.cecSessions = [
        { sessionId: s.session_id, state: s.state },
        ...this.cecSessions.filter((x) => x.sessionId !== s.session_id),
      ];
      // The technician just got approval on a customer they dialed: open the
      // remote-control console for them, as soon as the peer has actually
      // landed in the catalog. The approval can beat the peer sweep by a
      // breath — the old one-shot 400ms timer lost that race silently and the
      // console "just never opened".
      if (s.state === "active" && this.cecAutoOpenNode) {
        const node = this.cecAutoOpenNode;
        const chatOnly = this.cecAutoOpenChatOnly;
        this.cecAutoOpenNode = null;
        this.cecAutoOpenChatOnly = false;
        // A Chat-button dial opens only the chat (chat before remote control);
        // a Remote-Control / Answer dial also opens the console.
        if (!chatOnly) this.openCecConsoleWhenFound(node);
        // Pop the customer chat alongside the console — the technician can
        // message them ("close the browser and reopen it") right beside the
        // live session. Needs the `open_chat_window` backend command (see the
        // FLAG in the change report); a no-op in web mode.
        this.openChat(node);
      } else if ((s.state === "denied" || s.state === "ended") && this.cecAutoOpenNode) {
        // A decline (or the session ending before approval) must disarm the
        // auto-open — leaving it armed both hides the outcome from the
        // technician and would pop a console on the wrong, later dial.
        this.cecAutoOpenNode = null;
        this.cecAutoOpenChatOnly = false;
        if (s.state === "denied") {
          this.toast("warn", "The customer declined the connection");
        }
      }
    });
    await onCecGrants((grants) => {
      this.cecGrantList = grants;
    });
    await onCecHelp((e) => {
      // Technician side of `cec://help`: the node pushes the whole fresh
      // queue on every membership change. (The `asking` field is the
      // customer app's side of the event — irrelevant here.)
      if (e.waiting) this.cecHelpWaiting = e.waiting;
    });
    // The customer chat plane: every window listens so any of them (the main
    // window, the console window, the chat pop-out) keeps threads live. The
    // append dedupes by id, so the sender's own echoed line shows once.
    await onCecChat((e) => this.appendChat(e.peer, e.message));
    // (The CEC directory — `loadCec`, which every window needs and not just
    // the one where the Support tab mounted — is kicked off at the very top of
    // init instead of here. It gates a console window's readiness, so it must
    // not queue behind this chain; see the comment there.)
    // Re-assert the Experimental tier the user last chose: the node's gate
    // is process-local (a fresh serve boots it off), so every window pushes
    // its persisted setting — harmless when off (the default), and the one
    // that streams carries the real intent.
    void this.pushLabsTier();
    // (The rooms plane — invites, join/leave presence, chat, knocks — and
    // its same-device sibling bus are subscribed at the top of init, before
    // the identity pull, so a room window can't join before onRoom listens.)
    // The video-popout sibling: which streams live in their own windows,
    // and the "Return video here" ask that puts one back.
    await onVideoLocal((e) => this.handleVideoLocal(e));
    // The fleet roster converges live — a claim, or gossip catching up, pushes
    // a fresh copy. This is what makes a claim visibly *do* something.
    await onOwned((r) => {
      const renamed = (this.ownedFleet?.name ?? "") !== (r.name ?? "");
      // Joining a fleet adds its closed network; leaving, being released or
      // a dissolve REMOVES it (the node tears the mesh out before it emits
      // this event). Either way the network SET changed, not just the
      // roster — and the meshes list only re-reads network configs when
      // asked, so without this the dead fleet mesh sat in the list until
      // some unrelated refresh ("left the fleet but still on the fleet
      // mesh"). Rename is the same story for the label only.
      const fleetMeshChanged =
        (this.ownedFleet?.network_id ?? null) !== (r.network_id ?? null);
      this.ownedFleet = r;
      this.reconcileFleetRelationships();
      // A rename changes the fleet *mesh* label, which the graph's network
      // chips (and the meshes list) read from the network config — not the
      // roster. The 3 s mesh poll never re-reads network labels, so pull them
      // now and re-derive the graph, so a fleet rename (or the fleet mesh
      // appearing/vanishing) actually shows up on the mesh pills, not just
      // the "<name>'s fleet" grouping.
      if (renamed || fleetMeshChanged) {
        void this.refreshNetworks().then(() => this.syncMeshGraph());
      }
    });
    await onOwnership((o) => {
      const who = this.catalog.nodes.find((n) => sameMachine(n.id, o.from))?.label ?? "A device";
      if (o.message.kind === "claimed") this.toast("ok", `${who} joined your fleet`);
      else if (o.message.kind === "declined")
        this.toast("warn", `Couldn't claim ${who}: ${o.message.reason ?? "not claimable"}`);
    });
    // Share negotiation: the session snapshot (above) already merges the
    // resulting grants into the graph — this only surfaces the human nudge.
    await onShare((s) => {
      const who =
        s.person?.trim() ||
        this.catalog.nodes.find((n) => sameMachine(n.id, s.from))?.label ||
        "Someone";
      if (s.kind === "invite") this.toast("info", `${who} shared with you`);
      else if (s.kind === "accept") this.toast("ok", `${who} accepted your share`);
      else if (s.kind === "decline") this.toast("info", `${who} declined your share`);
      else if (s.kind === "revoke") this.toast("info", `${who} changed what they share`);
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
    this.meshPoll = setInterval(() => {
      void this.syncMeshGraph();
      // The dialed customers' `online` flags are reconciled by the node against
      // its live peer set on every fetch, but nothing re-fetches them off a
      // cec:// event — and a customer simply going offline fires none. Refresh
      // ONLY while a CEC surface is actually on screen (Help sidebar / console):
      // the freshness matters only to someone looking, so a technician who isn't
      // costs nothing. Local IPC either way — this reads the daemon's peer
      // table, never the mesh.
      if (this.cecSurfaceWatchers > 0) void this.refreshCecPresence();
      // Safety net for a missed `live` event. The node emits it exactly once,
      // fire-and-forget — so a GUI that subscribed *after* the node went live
      // (a cold first launch, common on Windows where the node cold-spawns
      // slowly) never receives it and stays stuck in "demo mode" with dead
      // header pills even though the mesh is up. Re-hydrate from the node until
      // it takes; the call no-ops while the socket is still unreachable.
      if (!this.backendConnected) void this.recoverBackendConnection();
    }, 3000);
  }

  /** One engine diagnostic off the daemon's event stream. Today the
   *  load-bearing mapping is the ICE watchdog's no-TURN verdict: mark the
   *  peer's node (the "needs relay" chip) and say so once — the actionable
   *  fix (add a TURN server to this mesh's venue) is nothing a user could
   *  ever have guessed from a silently-offline card. */
  private handleMeshDiag(e: Record<string, unknown>) {
    if (e?.event_kind !== "diag") return;
    const detail = (e.detail ?? {}) as Record<string, unknown>;
    if (e.category === "ice" && detail.hint === "add_turn_server") {
      const peer = typeof detail.peer === "string" ? detail.peer : null;
      const node = peer ? this.nodeByCanonical(peer) : undefined;
      if (node) node.needsTurn = true;
      const key = `turn:${peer ?? "?"}`;
      if (!this.meshAlerted.has(key)) {
        this.meshAlerted.add(key);
        const who = node?.label ?? (peer ? shortId(peer) : "a device");
        this.toast(
          "warn",
          `Can't reach ${who} directly — this mesh needs a TURN relay (mesh settings → Servers)`,
        );
      }
    }
  }

  /** Re-pull everything that depends on a live mesh. Shared by the `live`
   *  subscription event and the poll-based recovery so the two can't drift. */
  private pullLiveState() {
    void this.loadIdentity();
    void this.refreshNetworks().then(() => this.syncMeshGraph());
    void this.pullSessionSnapshot();
    void this.loadOwnedFleet();
    void this.loadSites();
    void this.loadDisabledNetworks();
  }

  /** Poll-driven recovery when the one-shot `live` event was missed: bring the
   *  connected flag (and the live state behind it) current. Idempotent — bails
   *  until the node socket answers (hydrateFromBackend no-ops on a null scan),
   *  then flips the flag and pulls the live state once. */
  private async recoverBackendConnection() {
    await this.hydrateFromBackend();
    // Bring the mesh verdict current too: the node answering its socket
    // (backendConnected) says nothing about the daemon behind it, and the
    // header's "mesh reconnecting…" state reads from this.
    const s = await linkStatus();
    if (s?.status) this.meshStatus = s.status;
    if (this.backendConnected) this.pullLiveState();
  }

  /** Build the graph's machine nodes from the daemon's *actual* mesh
   *  membership — the roster of known devices plus currently-live peers,
   *  across every joined network. This is what makes "others on the mesh"
   *  appear; the bespoke presence channel only layers device detail on top
   *  when a peer also runs AllMyStuff. Mirrors how MyOwnMesh builds its map.
   *  Two deliberate exceptions keep normal churn from accreting here: the
   *  CEC Support area contributes only links this daemon holds (never its
   *  customer roster), and each poll ends with a sweep that drops the
   *  capabilities, routes and grace stamps of machines that left the
   *  catalog — a support box runs for days while customers come and go. */
  async syncMeshGraph() {
    if (!isTauri()) return;
    // Live peers are keyed by the full device id (`{pubkey}-{suffix}`) — the
    // same id presence + capabilities use, so they merge into one node.
    const live = new Map<
      string,
      {
        label: string;
        online: boolean;
        app: boolean;
        features: string[];
        version?: string;
        summary?: InventorySummary;
        endpoints?: Capability[];
        needsTurn?: boolean;
      }
    >();
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
    // A machine can appear in several networks' peer lists with independent
    // transport state (a link on one mesh rebuilding while another is steady).
    // The presence grace is machine-wide ("connected on ANY network recently"),
    // so accumulate the best status across all networks first and only settle
    // the grace marker once per machine — otherwise an `offline` row on one
    // mesh deletes the grace an `active` row on another just earned, and the
    // machine flaps offline on the next poll.
    const activeCanons = new Set<string>();
    const offlineCanons = new Set<string>();
    for (const net of nets) {
      const netName = this.meshLabel(net);
      // The CEC Support area never contributes its ROSTER: hundreds of
      // customers hold standing residence there and churn daily, and every
      // one of them would otherwise sit on the graph (and in Devices) as a
      // permanent offline card. Customers are CEC-tab material — the area
      // only ever contributes links this daemon actually HOLDS right now
      // (the infra hubs, a customer while we're dialed in), gated below.
      const cecArea = this.isManagedCecMesh(net);
      let peers: PeerInfo[] = [];
      let roster: RosterPeer[] = [];
      try {
        peers = await meshPeers(net.config_id);
      } catch {
        /* network still settling */
      }
      if (!cecArea) {
        try {
          roster = await meshRosterList(net.config_id);
        } catch {
          /* roster optional */
        }
      }
      for (const p of peers) {
        if (cecArea) {
          // Held links only: connected now, or a link that was connected
          // moments ago riding the same grace every mesh gets (an ICE
          // restart mid-support-session must not drop the customer's card).
          // Everything else — announce-sighted residents we never dialed,
          // rows gone terminal — never enters the catalog; a terminal row
          // still settles the machine-wide grace so a hung-up customer
          // leaves the graph on this poll, not 45 s later.
          const canon = canonicalNodeId(p.device_id);
          const terminal = p.status === "offline" || p.status === "error";
          if (!CONNECTED_STATUSES.has(p.status) && (terminal || !this.withinPresenceGrace(canon))) {
            if (terminal) offlineCanons.add(canon);
            continue;
          }
        }
        if (p.status === "pending_approval") {
          // Surfaced as a "new device wants to join" nudge + popup, not on the
          // graph. Gathered across every network so the nudge catches them all.
          joins.push({ networkId: net.config_id, networkName: netName, peer: p });
          continue;
        }
        addNet(p.device_id, netName);
        const e = live.get(p.device_id) ?? {
          label: p.label?.trim() || shortId(p.device_id),
          online: false,
          app: false,
          features: [] as string[],
          version: undefined as string | undefined,
        };
        if (p.label?.trim()) e.label = p.label.trim();
        // The daemon's ICE watchdog verdict: this link keeps failing with no
        // relay in play. Any network reporting it marks the machine — the
        // card's "needs relay" chip is how the block becomes visible.
        if (p.needs_turn) e.needsTurn = true;
        // The reliable "on AllMyStuff" signal: a peer advertising the
        // `allmystuff` capability tag on the mesh is an app node, and its
        // remaining tags are the features it offers. This rides the handshake +
        // daemon peer list, so a connected peer flips on without depending on
        // the bespoke presence advert landing.
        const tags = p.capabilities?.tags ?? [];
        if (tags.includes(CAP_TAG_ALLMYSTUFF)) {
          e.app = true;
          e.features = tags.filter((t) => t !== CAP_TAG_ALLMYSTUFF);
          if (p.capabilities?.app_version) e.version = p.capabilities.app_version;
          // The daemon's CapabilityAdvert only forwards app-specific data under
          // `extra` (its typed struct drops unknown top-level fields), so the
          // device stats and the wireable endpoints both ride there — reliably,
          // unlike the bespoke presence advert that used to be their only source.
          const extra = p.capabilities?.extra;
          if (extra?.summary) e.summary = extra.summary;
          if (extra?.endpoints?.length) e.endpoints = extra.endpoints;
        }
        const canon = canonicalNodeId(p.device_id);
        if (CONNECTED_STATUSES.has(p.status)) {
          e.online = true;
          activeCanons.add(canon);
        } else if (p.status === "offline" || p.status === "error") {
          // The daemon's explicit verdict on THIS network — recorded, but only
          // allowed to clear the machine-wide grace if no other network reports
          // it connected this poll (settled after the loop).
          offlineCanons.add(canon);
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
    // Settle the machine-wide grace once all networks are folded: a connected
    // reading on any mesh refreshes it; an explicit offline clears it only when
    // NO mesh reported the machine connected this poll.
    for (const canon of activeCanons) this.lastConnectedAt.set(canon, Date.now());
    for (const canon of offlineCanons) {
      if (!activeCanons.has(canon)) this.lastConnectedAt.delete(canon);
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
      known.set(r.device_id, {
        label: r.label?.trim() || shortId(r.device_id),
        online: false,
        app: false,
        features: [],
        version: undefined,
      });
    }
    // Upsert a node per known device (never the local machine). Discovered
    // devices start *unclaimed* — they're on the mesh but not yet yours; you
    // claim them (only if they offer it) or mark them shared from their
    // drawer. "On AllMyStuff" (`app`) now comes from the reliable mesh
    // capability marker carried in the peer list (CAP_TAG_ALLMYSTUFF), with
    // the bespoke presence advert still enriching the rest — so a device that
    // is a bare daemon (no marker) stays `app: false`, and we never downgrade
    // a node presence already enriched.
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
          app: info.app,
          features: info.features,
          version: info.version,
          summary: info.summary,
          networks: nodeNets,
          needsTurn: info.needsTurn ?? false,
        });
      } else {
        node.online = info.online;
        node.networks = nodeNets;
        // Refreshed every sweep (not only widened) so the chip clears once
        // the daemon's watchdog stands down after the link recovers.
        node.needsTurn = info.needsTurn ?? false;
        if (!node.hostname && info.label) node.label = info.label;
        // The mesh marker can flip a node *on* (app node), but it never
        // downgrades one: presence may have already enriched it with richer
        // detail (owner, sites), so only fill what's still missing.
        if (info.app) {
          node.app = true;
          if (!node.features?.length) node.features = info.features;
          if (!node.version && info.version) node.version = info.version;
          // The stats now ride the peer list, so keep them fresh from there —
          // this is the reliable source the missed-presence case fell back to
          // nothing on. Only overwrite with a real summary, never blank it.
          if (info.summary) node.summary = info.summary;
        }
      }
      // Carry the wireable endpoints from the reliable peer list onto the
      // catalog so `matchEndpoint` (which filters capabilities by node) finds
      // them — the missed-presence case that left rooms/remote-control with "no
      // audio/control/video path". Store them AS-IS, not re-keyed: an endpoint
      // id is the *remote's* own id (`<remote-node>:<slot>`), which is what the
      // remote resolves a route by — re-prefixing it with a local id makes the
      // remote reject the route ("path does not exist on the remote"). The
      // remote's node id is the same id this peer's node carries, so a plain
      // store still matches locally. Mirrors the presence path
      // (`applySessionSnapshot`), which also stores caps unchanged.
      if (info.app && info.endpoints?.length) {
        const epNode = info.endpoints[0].node;
        this.catalog.capabilities = [
          ...this.catalog.capabilities.filter((c) => c.node !== epNode),
          ...info.endpoints,
        ];
      }
    }
    // The local machine is on every network we've joined — minus the
    // node-managed CEC area: that's client-support plumbing, and letting it
    // onto this card would split the fleet's mesh sub-bands on a chip no
    // other fleet device carries. (A dialed customer's card keeps the
    // "CEC Support" chip — there it says something.)
    const me = this.node(this.localId) ?? this.catalog.nodes.find((n) => n.kind === "this");
    if (me) {
      me.networks = nets
        .filter((n) => !this.isManagedCecMesh(n))
        .map((n) => this.meshLabel(n))
        .sort();
    }
    // A machine that's no longer in any roster/peer set has dropped offline.
    // Compare by canonical pubkey so a presence node (display id) isn't wrongly
    // marked offline just because the daemon lists it under the bare pubkey.
    // Recently-connected machines get the same grace as a transient status:
    // a daemon restarting mid-poll reports *nobody* for a few seconds, and
    // without the grace that blanks the whole graph offline and back.
    // Refresh the fleet roster every poll so fleet status — a new member, a
    // role change, a device that left, one re-claimed out from under us —
    // converges on the graph within a poll, not only when the backend happens
    // to emit an `allmystuff://owned` event.
    await this.loadOwnedFleet();
    void this.loadFleetMfa();

    const knownCanon = new Set([...known.keys()].map(canonicalNodeId));
    // Devices no longer on any active mesh fall off the graph. We used to only
    // flip them offline and never remove them, so a node from a mesh you've
    // since disabled or left lingered (and "randomly reappeared"). Now: keep
    // your own machine, your fleet, and anything you've claimed or share;
    // anything else that isn't on an active mesh this poll (past its presence
    // grace) is pruned.
    this.catalog.nodes = this.catalog.nodes.filter((n) => {
      const canon = canonicalNodeId(n.id);
      if (n.kind === "this" || this.isLocalMachine(n.id)) return true;
      if (knownCanon.has(canon)) return true; // seen this poll; online already set
      // "mine" is roster-authoritative — a device is kept because it's an
      // actual fleet member (isFleetMember, below), NOT because its
      // relationship still reads "mine". A device you left/evicted keeps
      // advertising "owner = you" for a while (it's offline, or missed the
      // Release), which marks it "mine" via the stale-advert fallback in
      // `standing()`; keying the prune off that pinned departed devices on the
      // graph forever ("fails to update the graph"). Roster + live-this-poll +
      // shares + presence grace are the real keep signals.
      const keep =
        n.relationship.kind === "shared" ||
        this.isFleetMember(n.id) ||
        this.withinPresenceGrace(canon);
      if (keep) n.online = this.withinPresenceGrace(canon);
      return keep;
    });
    // What pruned machines leave behind, swept the same poll that dropped
    // them (forgetNode does the identical cleanup on the manual path):
    // their capabilities and routes — dead sessions can't be re-wired, and a
    // support workstation would otherwise bank a capability set per customer
    // it ever dialed — and any presence-grace stamp past its window, so the
    // map stops remembering every machine that ever connected. Routes are
    // matched by the NODE a leg belongs to (not by capability id: a live
    // terminal leg's endpoints aren't catalog capabilities).
    const survivorCanons = new Set(this.catalog.nodes.map((n) => canonicalNodeId(n.id)));
    if (this.catalog.capabilities.some((c) => !survivorCanons.has(canonicalNodeId(c.node)))) {
      this.catalog.capabilities = this.catalog.capabilities.filter((c) =>
        survivorCanons.has(canonicalNodeId(c.node)),
      );
    }
    if (
      this.catalog.routes.some(
        (r) =>
          !survivorCanons.has(this.capNodeOf(r.from)) || !survivorCanons.has(this.capNodeOf(r.to)),
      )
    ) {
      this.catalog.routes = this.catalog.routes.filter(
        (r) =>
          survivorCanons.has(this.capNodeOf(r.from)) && survivorCanons.has(this.capNodeOf(r.to)),
      );
    }
    for (const [canon, at] of [...this.lastConnectedAt]) {
      if (Date.now() - at > PRESENCE_GRACE_MS) this.lastConnectedAt.delete(canon);
    }
    // A freshly-discovered device may belong to someone we already share
    // with — fold it into that share.
    this.reconcileShares();
    // Now the graph reflects this poll, stop the spinner on any peer whose
    // refreshed details just landed.
    this.settleRefreshing();
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
      features: [...LOCAL_FEATURES],
    };
    me.id = newId;
    me.kind = "this";
    me.label = label;
    me.hostname = host;
    me.summary = scan.summary;
    me.online = true;
    me.app = true;
    // This binary's own feature set — the local node never hears its own
    // advert, so without this its Terminal/Files/Sites stay un-shareable.
    me.features = [...LOCAL_FEATURES];
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
    // The webview is where an inbound remote desktop renders, so viewing a
    // screen is a capability of the running app rather than a side effect of
    // the hardware scanner finding a physical monitor. New node builds expose
    // this synthetic endpoint themselves; keep the GUI fallback for an older
    // local service, and for any platform scan that omits display inventory.
    // Without it the console returns before sending a route offer while its
    // terminal/control legs still work — the one-way, no-picture failure.
    if (!matchEndpoint(this.catalog, newId, "display", "consume")) {
      this.catalog.capabilities.push({
        id: `${newId}:display-view`,
        node: newId,
        label: "Remote desktop",
        media: "display",
        flow: "sink",
        origin: "remote-desktop",
      });
    }
    // A console window scans too (it needs the local sinks to wire routes).
    // No toast — the refresh panel (and the repopulated graph) is the feedback.
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
          // Presence carries DETAIL, not liveness — a peer's NodeProfile
          // lingers in the session map after it goes offline (it's only
          // dropped on an explicit Leave), so being in a snapshot is no proof
          // of reachability. Derive online from the same grace memory the
          // mesh poll owns, so the 1s snapshot pull (Terminal/Files windows)
          // can't resurrect an offline node as "online for a second".
          online: this.withinPresenceGrace(canonicalNodeId(p.node)),
        };
        this.catalog.nodes.push(node);
      } else {
        // Adopt the presence display id so this peer's capabilities (keyed by
        // `p.node`) resolve to this node.
        if (node.id !== p.node) node.id = p.node;
        node.label = p.label;
        node.hostname = p.hostname;
        // See above — online is the poll's call (lastConnectedAt + grace), not
        // the snapshot's; the snapshot only ever merges presence detail.
        node.online = this.withinPresenceGrace(canonicalNodeId(p.node));
      }
      node.summary = p.summary;
      // Presence means it's running AllMyStuff — it has wireable stuff.
      node.app = true;
      // Ownership the device advertises about itself (Task 4).
      node.owner = p.owner ?? null;
      node.claimable = p.claimable ?? false;
      // App features it supports ("terminal", …). Only overwrite when the
      // frame carried them: features ALSO arrive on the reliable peer-list/tags
      // path, so a sparse/missed presence frame must not blank that (an empty
      // list is omitted on the wire, so "absent" reads as keep-what-we-have).
      if (p.features) node.features = p.features;
      // Sites it exposes. Presence is the SOLE source for a peer's sites (no
      // peer-list fallback), and empty is omitted on the wire — so this must be
      // presence-authoritative, not guarded: a node that withdraws its last
      // exposed site (Stop) advertises no `sites`, and that has to CLEAR the
      // peer's list, not pin the stale one. (Guarding it broke withdrawal of a
      // node's final site.) A briefly-missed frame self-heals on the next
      // advert, which presence re-sends on every change.
      node.sites = p.sites ?? [];
      // KVM-appliance binding (present only on a node advertising FEATURE_KVM)
      // — what the KVM controls, which site is its web UI, its joining mesh,
      // and its current mesh memberships. The wire is snake_case; map it onto
      // the camelCase shape. Absent on an ordinary peer leaves it undefined
      // (no KVM drawer).
      node.kvm = p.kvm
        ? {
            attachedTo: p.kvm.attached_to || undefined,
            web: p.kvm.web || undefined,
            joiningMesh: p.kvm.joining_mesh || undefined,
            meshes: p.kvm.meshes?.length ? p.kvm.meshes : undefined,
            virtualMedia: p.kvm.virtual_media
              ? {
                  source: p.kvm.virtual_media.source,
                  label: p.kvm.virtual_media.label,
                  file: p.kvm.virtual_media.file,
                }
              : undefined,
          }
        : undefined;
      // The AllMyStuff version it's running — let it tell when the machine
      // is behind the channel and offer an upgrade. Absent (older peer) =
      // unknown, and the upgrade button stays hidden.
      node.version = p.version;
      // Fleet metadata the device advertises — its fleet's name and its
      // owner's *person* name — so the graph groups + labels its fleet
      // ("Casey's fleet") straight from presence, even for fleets we don't
      // own. Absent (an older peer, or not in a fleet) leaves them undefined.
      node.fleetName = p.fleet_name || undefined;
      node.fleetOwner = p.fleet_owner || undefined;
      // A device that says *we* own it is ours; one owned by someone else
      // stays a guest/unclaimed (you can't flat-claim it). Never auto-flip a
      // relationship the user already set, and never auto-adopt.
      // (Relationship is no longer set here — `reconcileFleetRelationships`
      // at the end of this method is the single owner of mine/unclaimed,
      // projecting it from the node's live standing.)
      // Collapse any other view of this same machine into the one node we just
      // settled on (id `p.node`) — heals an already-split graph. Match by id,
      // not reference: a freshly-pushed node is proxied by `$state`, so
      // `n === node` would be false and would delete the peer we just added.
      this.catalog.nodes = this.catalog.nodes.filter(
        (n) => n.id === p.node || !sameMachine(n.id, p.node),
      );
      // Refresh this peer's capabilities — but only when presence actually
      // carried some. An empty/missed presence frame must not blank the
      // endpoints we already have from the reliable peer list (the
      // control/audio/video sinks rooms + remote-control wire to); otherwise a
      // session event with no caps wipes them and reads as "no path".
      if (p.capabilities?.length) {
        this.catalog.capabilities = [
          ...this.catalog.capabilities.filter((c) => c.node !== p.node),
          ...p.capabilities,
        ];
      }

      // A console that opened before THIS peer's capabilities converged wired
      // to nothing — the CEC case, where the console pops the instant a
      // customer approves, ahead of their screen/input endpoints landing in
      // the catalog. So `applyConsoleVideo` found no input to watch (blank
      // stage until you reclick) and the auto-legs found no control sink to
      // reach (dead until you toggle control off→on). Now that this snapshot
      // (re)carried the peer's caps, re-assert the wire the way that manual
      // reclick / toggle does — pick the screen if we never got one, and bring
      // up any granted leg that isn't live yet. Idempotent: each guard is
      // "only if not already on", so a steady-state snapshot no-ops and a
      // fleet machine (caps present at open) never enters here. Runs after the
      // caps update above, so the endpoints these calls resolve are the fresh
      // ones — the whole point.
      if (this.consoleNodeId && sameMachine(this.consoleNodeId, p.node)) {
        if (!this.consoleInput) {
          const first = this.consoleVideoInputs(this.consoleNodeId)[0]?.id ?? null;
          if (first) {
            this.consoleInput = first;
            void this.applyConsoleVideo();
          }
        }
        const access = this.consoleAccess(this.consoleNode ?? undefined);
        // Skip a leg the user deliberately switched off — otherwise this
        // re-assert (which runs on every snapshot, including the one the
        // teardown itself pushes) would flip it right back on.
        if (!this.consoleAudio && access.audio && !this.consoleUserOff.has("audio"))
          this.toggleConsoleAudio();
        if (!this.consoleControl && access.control && !this.consoleUserOff.has("control"))
          this.toggleConsoleControl();
        if (!this.consoleClipboard && access.clipboard && !this.consoleUserOff.has("clipboard"))
          this.toggleConsoleClipboard();
      }
    }

    // Reflect live routes (active ones become catalog routes), and keep
    // the per-route negotiation states for whoever watches one (a
    // terminal tab telling "connecting" from "rejected" by its reason).
    const states: Record<string, RouteLiveState> = {};
    const sessions: Record<string, string> = {};
    for (const lr of snap.routes ?? []) {
      states[lr.route.id] = lr.state;
      // The resolved terminal session id (multi-attach) the host bound this
      // route to, when it sent one — the tab labels and re-queries by it.
      if (lr.term_session) sessions[lr.route.id] = lr.term_session;
      const active = lr.state.state === "active";
      const id = lr.route.id;
      const exists = this.catalog.routes.some((r) => r.id === id);
      if (active && !exists) {
        this.catalog.routes.push({ ...lr.route, drive: lr.drive ?? null });
      } else if (active && exists) {
        const route = this.catalog.routes.find((route) => route.id === id);
        if (route) route.drive = lr.drive ?? route.drive ?? null;
      } else if (!active && exists) {
        this.catalog.routes = this.catalog.routes.filter((r) => r.id !== id);
      }
    }
    this.routeStates = states;
    this.routeSessions = sessions;
    for (const routeId of Object.keys(this.driveMounts)) {
      if (!states[routeId] || states[routeId].state !== "active") {
        delete this.driveMounts[routeId];
      }
    }

    // A console leg the far side REFUSED: the controlled machine rejects the
    // control/clipboard route when our events fail its authorization gate
    // (e.g. this device fell off its fleet roster). Flip the toggle off and
    // say why — the old behaviour was a live-looking toggle typing into the
    // void, which read as "controls just stopped working".
    const refusal = (live: string | null): string | null => {
      if (!live) return null;
      const st = states[live];
      return st?.state === "rejected" ? st.reason || "the far side refused it" : null;
    };
    const controlRefused = refusal(this.consoleControlLive);
    if (controlRefused) {
      this.toast("warn", `Keyboard & mouse ended: ${controlRefused}`);
      if (this.consoleControlRouteId) void this.disconnect(this.consoleControlRouteId);
      this.consoleControlRouteId = null;
      this.consoleControlLive = null;
      this.consoleControl = false;
    }
    const clipboardRefused = refusal(this.consoleClipboardLive);
    if (clipboardRefused) {
      this.toast("warn", `Clipboard passthrough ended: ${clipboardRefused}`);
      if (this.consoleClipboardRouteId) void this.disconnect(this.consoleClipboardRouteId);
      this.consoleClipboardRouteId = null;
      this.consoleClipboardLive = null;
      this.consoleClipboard = false;
    }

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

    this.applyDurableShares(snap.shares ?? []);
    this.reconcileFleetRelationships();
    this.reconcileShares();
  }

  /** Project each node's **standing** onto the stored `relationship.kind`, the
   *  single owner of mine/unclaimed. `standing()` is the live truth the UI
   *  reads; this keeps the stored field — used by the older list/count/group
   *  consumers — a faithful projection of it, so nothing can drift into the
   *  contradictory "unclaimed but in your fleet" states the racing writers
   *  used to produce. An explicit `shared` relationship (user intent + grants)
   *  is never touched. Idempotent — safe to run after every state change. */
  private reconcileFleetRelationships() {
    for (const n of this.catalog.nodes) {
      if (n.kind === "this" || this.isMe(n.id)) continue;
      if (n.relationship.kind === "shared") continue;
      const want = this.standing(n).mine ? "mine" : "unclaimed";
      if (n.relationship.kind !== want) n.relationship = { kind: want };
    }
  }

  // ---- selection ---------------------------------------------------
  setUiMode(mode: UiMode) {
    this.uiMode = mode;
    if (mode === "normal") {
      // Normal mode has no details drawer or card drop-outs. Clear their
      // latent state so switching back never acts on a stale selection.
      this.selectedNodeId = null;
      this.kvmRevealed = null;
    }
    try {
      localStorage.setItem(UI_MODE_STORE_KEY, mode);
    } catch {
      /* private mode — keep the choice for this session */
    }
  }

  selectNode(id: string | null) {
    this.selectedNodeId = id;
  }

  /** "Show me this device": the one entry point the settings lists use to take
   *  you to a node on the graph. Resolves the id canonically first — a roster or
   *  presence id can be a different *form* of the same machine's id than the one
   *  the graph lays out under, so selecting the raw id would highlight nothing —
   *  then selects it, asks the graph to centre on it, and leaves Settings so it's
   *  actually visible. A no-op selection (id resolves to no live node) still
   *  closes settings and selects the id, so the drawer can show what it knows. */
  focusNode(idOrDevice: string) {
    const n = this.machineByAnyId(idOrDevice);
    const id = n?.id ?? idOrDevice;
    this.selectedNodeId = id;
    this.focusRequest = { id, seq: (this.focusRequest?.seq ?? 0) + 1 };
    this.settingsOpen = false;
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
      // No success toast — the graph route, the console pills, and the screen
      // are the visual confirmation a connection is up.
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
      // No toast — the new wire on the graph (and the console that pops) is the
      // confirmation. Toasts are reserved for failures.
      const ends = [this.capability(p.from)?.node, this.capability(p.to)?.node];
      const remote = ends.find((n) => n && !this.isMe(n));
      if (remote) this.popConsoleFor(remote, res.route.media);
    }
    this.pendingShare = null;
  }

  dismissPendingShare() {
    this.pendingShare = null;
  }

  // ---- share flow builder ------------------------------------------
  //
  // The Sender → Receiver composer. It doesn't invent any new wiring: each
  // media capability resolves the sender's source endpoint and the receiver's
  // sink through `matchEndpoint`, then rides the ordinary `connect()` path
  // (grants and all); the Files / Terminal / Sites toggles pop the existing
  // host views for the sender.

  /** Whether a device is one of *yours* — the only kind you may put on the
   *  sending side of a share. You can't share someone else's stuff. */
  isMyDevice(id: string | null): boolean {
    if (!id) return false;
    const n = this.node(id);
    if (!n) return false;
    const st = this.standingOf(n);
    return st.mine || st.self;
  }

  /** KVM appliances are headless passthroughs, never share recipients. */
  canReceiveShare(id: string | null): boolean {
    if (!id) return false;
    const node = this.node(id);
    return !!node && isAppNode(node) && !this.isKvm(node);
  }

  /** Open the builder, optionally pre-filling the sender (one of your devices),
   *  the receiver (a node id of any device in the receiving fleet), and the
   *  consoles to pre-toggle (for "Manage share"). A non-owned device offered as
   *  the sender is dropped — you only ever share your own stuff. The sender
   *  always lands on one of your devices (this one by default) so the capability
   *  switches are live the moment the builder opens — otherwise they'd all be
   *  greyed and there'd be nothing to add. */
  openShareFlow(sender?: string | null, receiver?: string | null, caps?: ShareCap[]) {
    if (sender !== undefined) this.shareFlowSender = this.isMyDevice(sender) ? sender : null;
    if (!this.isMyDevice(this.shareFlowSender)) this.shareFlowSender = this.localId;
    if (receiver !== undefined) {
      this.shareFlowReceiver = this.canReceiveShare(receiver) ? receiver : null;
    }
    this.shareFlowInitialCaps = caps ?? [];
    this.shareFlowOpen = true;
  }
  closeShareFlow() {
    this.shareFlowOpen = false;
  }

  /** The fleets you can share TO — everyone you already share with, plus the
   *  owner of any other fleet's device on the graph. Each carries a node id of
   *  one of that fleet's devices (what the builder's receiver state holds). */
  shareFleetOptions(): { personId: string; name: string; nodeId: string; devices: number }[] {
    const byPerson = new Map<string, { personId: string; name: string; nodeId: string; devices: number }>();
    for (const n of this.catalog.nodes) {
      if (!isAppNode(n) || this.isKvm(n) || this.isMyDevice(n.id) || !n.owner) continue;
      const p = this.personFor(n);
      const existing = byPerson.get(p.id);
      if (existing) existing.devices += 1;
      else byPerson.set(p.id, { personId: p.id, name: p.name, nodeId: n.id, devices: 1 });
    }
    return [...byPerson.values()].sort((a, b) => a.name.localeCompare(b.name));
  }

  /** The fleet/person a receiver node belongs to (for the builder's card). */
  receiverFleet(nodeId: string | null): { name: string; devices: number } | null {
    if (!nodeId) return null;
    const n = this.node(nodeId);
    if (!n) return null;
    const p = this.personFor(n);
    const devices = this.catalog.nodes.filter(
      (x) =>
        isAppNode(x) &&
        !this.isKvm(x) &&
        !this.isMyDevice(x.id) &&
        this.personFor(x).id === p.id,
    ).length;
    return { name: p.name, devices };
  }

  /** A capability's device id (the bit before the first ":"). */
  private capNodeOf(cap: string): string {
    const i = cap.indexOf(":");
    return canonicalNodeId(i >= 0 ? cap.slice(0, i) : cap);
  }

  /** Whether a grant is a *share-out* — something one of MY devices lets the
   *  fleet do (capability on my device, or a legacy media-wide grant) — vs a
   *  share-in (a console of THEIRS I may open). The drawer's "What X can do"
   *  shows only share-out. */
  isShareOutGrant(g: Grant): boolean {
    if (!g.capability) return true;
    return this.isMyDevice(this.capNodeOf(g.capability));
  }

  /** The consoles already granted to a fleet from a given sender device — to
   *  pre-toggle the builder. The reverse of shareCapGrants. */
  existingShareCaps(senderId: string | null, receiverNodeId: string | null): ShareCap[] {
    if (!senderId || !receiverNodeId) return [];
    const recv = this.node(receiverNodeId);
    if (!recv || recv.relationship.kind !== "shared") return [];
    const pid = recv.relationship.person.id;
    const grants: Grant[] = [];
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind === "shared" && n.relationship.person.id === pid) grants.push(...n.relationship.grants);
    }
    const sc = canonicalNodeId(senderId);
    const forSender = (g: Grant) => !!g.capability && this.capNodeOf(g.capability) === sc;
    const out: ShareCap[] = [];
    // The console reads back from its screen grant — the one piece it can't
    // work without, and the one every older split share is guaranteed to have
    // if it granted anything console-ish at all.
    if (grants.some((g) => forSender(g) && g.media === "display")) out.push("console");
    if (grants.some((g) => forSender(g) && g.media === "storage")) out.push("files");
    // Re-opening a share whose folder grant is already in place shows the
    // folder it names, so "Manage share" reads as what was actually shared
    // rather than a bare toggle with no folder behind it.
    const folderGrant = grants.find(
      (g) => forSender(g) && g.media === "storage" && !!g.capability?.includes(":folder:"),
    );
    if (folderGrant?.capability) {
      const id = folderGrant.capability.split(":folder:")[1] ?? "";
      if (id && !id.includes(":")) {
        this.shareFlowFolder = { id, label: folderGrant.label || "Shared folder", path: "" };
      }
    }
    if (grants.some((g) => forSender(g) && g.media === "generic" && !!g.capability?.endsWith(":terminal"))) out.push("terminal");
    if (grants.some((g) => forSender(g) && g.media === "generic" && !!g.capability?.endsWith(":sites"))) out.push("sites");
    return out;
  }

  /** Whether the sender can actually offer a given capability — drives which
   *  toggles are live (you can't share what isn't there). Audio / Video (the
   *  screen feed) / Control ride the remote-control console; Terminal / Files /
   *  Sites are their own console shares. */
  shareFlowCapAvailable(node: string | null, cap: ShareCap): boolean {
    if (!node) return false;
    const n = this.node(node);
    if (!n) return false;
    // The console needs a screen to show and a control sink to drive. Audio
    // and clipboard ride along without gating it — a machine with no sound
    // card still has a perfectly good console.
    if (cap === "console") {
      return (
        !!matchEndpoint(this.catalog, node, "display", "provide") &&
        !!matchEndpoint(this.catalog, node, "input", "consume")
      );
    }
    const feats = n.features ?? [];
    if (cap === "files") return isAppNode(n) && feats.includes(FEATURE_FILES);
    if (cap === "terminal") return isAppNode(n) && feats.includes(FEATURE_TERMINAL);
    if (cap === "sites") return (n.sites?.length ?? 0) > 0;
    return false;
  }

  /** Why a capability toggle is greyed for this sender — so the builder can
   *  say *why* it can't share Terminal/Files/Sites instead of a vague "not
   *  offered". Terminal & Files need the device to advertise that console
   *  (a current AllMyStuff with it enabled); Sites needs at least one exposed
   *  service. Returns null when the capability *is* available. */
  shareCapReason(node: string | null, cap: ShareCap): string | null {
    if (this.shareFlowCapAvailable(node, cap)) return null;
    if (!node) return "Pick one of your devices to share first";
    const n = this.node(node);
    const who = n?.label ?? "this device";
    if (!n || !isAppNode(n)) return `${who} isn't running AllMyStuff`;
    switch (cap) {
      case "console":
        return matchEndpoint(this.catalog, node, "display", "provide")
          ? `${who} doesn't accept remote control`
          : `${who} has no screen to share`;
      case "terminal":
        return `${who} isn't offering its terminal (older AllMyStuff, or it's turned off)`;
      case "files":
        return `${who} isn't offering file browsing (older AllMyStuff, or it's turned off)`;
      case "sites":
        return `${who} isn't exposing any sites — host a service on it first`;
      default:
        return `${who} can't share that`;
    }
  }

  /** Pick a folder on the sender device to share.
   *
   *  The path goes *inward* — to my own machine, naming what to share — and
   *  comes back as a minted id. That id is what the grant pins to and the only
   *  name for the folder that ever reaches the other fleet, which is what keeps
   *  a folder share to one folder instead of the disk it sits on.
   *
   *  Browsing to find it needs nothing special: `RemoteFolderPicker` already
   *  walks any of my devices over the ordinary files plane. */
  async pickShareFolder(path: string, label: string): Promise<boolean> {
    const sender = this.shareFlowSender;
    if (!sender) return false;
    this.shareFlowFolderPending = true;
    try {
      const minted = await shareFolderFrom(sender, path, label);
      if (!minted?.id) {
        this.toast("warn", "That device couldn't share the folder");
        return false;
      }
      this.shareFlowFolder = { id: minted.id, label: minted.label || label, path };
      return true;
    } catch (error) {
      this.toast("warn", `Couldn't share that folder: ${error}`);
      return false;
    } finally {
      this.shareFlowFolderPending = false;
    }
  }

  /** Forget the picked folder — the builder's Files toggle going off, or the
   *  sender changing under it (a folder id belongs to one machine). */
  clearShareFolder() {
    this.shareFlowFolder = null;
  }

  /** The grants one chosen capability mints — a persistent permission for the
   *  receiving fleet to open my sender device's console, NOT a live route.
   *  Each is scoped to the sender device so it only ever unlocks *that* device.
   *  Directions are what the receiver needs to OPEN the console: the device
   *  PROVIDES its screen/audio (and CONSUMES the receiver's input for control);
   *  Terminal/Files/Sites ride a generic grant carrying a `<device>:<kind>`
   *  capability. */
  private shareCapGrants(person: string, cap: ShareCap, sender: string, senderLabel: string): Grant[] {
    const mk = (media: MediaKind, role: GrantRole, suffix: string, what: string): Grant => {
      const capability = `${sender}:${suffix}`;
      return {
        id: scopedGrantId(person, media, role, capability),
        media,
        role,
        capability,
        label: `${senderLabel} — ${what}`,
      };
    };
    switch (cap) {
      case "console":
        // One visible share, four real grants — the console is useless
        // without all of them, and each stays scoped to its own capability so
        // the wire contract and per-grant revoke are unchanged.
        //
        // Each capability suffix must be the id `matchEndpoint` actually
        // resolves for that media, or the grant authorizes nothing and the
        // route is denied: the machine's audio source is the synthetic
        // `<node>:system-audio`, never a bare `:audio`.
        //
        // The role names the end being authorized, not who benefits: `screen`
        // and `system-audio` are sources on this machine (`provide`), while
        // `control` is an input *sink* on it (`consume`). Getting that
        // backwards is what silently denied every screen share.
        return [
          mk("display", "provide", "screen", "see its screen"),
          mk("audio", "provide", "system-audio", "hear its audio"),
          mk("input", "consume", "control", "control it"),
          mk("clipboard", "both", "clipboard", "share its clipboard"),
        ];
      case "files": {
        // A folder, never the whole-machine files console. The console
        // (`<sender>:files`) stays owner/fleet-only; what a share hands over
        // is one folder, named by the id its own machine minted, and the
        // sharer's node resolves that id back to a path locally. No path is
        // in the grant, so a grant can't be read as "where their files are".
        const folder = this.shareFlowFolder;
        if (!folder) return [];
        return [mk("storage", "both", `folder:${folder.id}`, `share ${folder.label}`)];
      }
      case "terminal":
        return [mk("generic", "provide", "terminal", "use its terminal")];
      case "sites":
        return [mk("generic", "provide", "sites", "reach its sites")];
    }
  }

  /** Start the share: grant the receiving FLEET access to my sender device's
   *  consoles. This is a persistent grant (it rides shareGrant to the daemon),
   *  not a live connection — the receiver's machines then see the console
   *  buttons on my device and open them on demand. Returns how many consoles
   *  were granted. */
  startShareFlow(caps: ShareCap[]): number {
    const sender = this.shareFlowSender;
    const receiver = this.shareFlowReceiver;
    if (!sender || !receiver) {
      this.toast("warn", "Pick a device of yours to share, and a fleet to share it with");
      return 0;
    }
    // You can only share your *own* devices' stuff — never someone else's.
    if (!this.isMyDevice(sender)) {
      this.toast("warn", "You can only share your own devices");
      return 0;
    }
    // The receiver is another fleet — not one of your own machines.
    if (this.isMyDevice(receiver)) {
      this.toast("warn", "Share with another fleet — the receiver can't be your own device");
      return 0;
    }
    const recv = this.node(receiver);
    if (!recv || !isAppNode(recv)) {
      this.toast("warn", "That device can't receive a share");
      return 0;
    }
    if (this.isKvm(recv)) {
      this.toast("warn", "KVMs are passthrough devices and can't receive shares");
      return 0;
    }
    // Establish the share with the receiver's fleet/person, then record the
    // console grants on it (a grant authorizes the whole fleet).
    if (recv.relationship.kind !== "shared") this.markShared(receiver);
    const rel = this.node(receiver)?.relationship;
    if (!rel || rel.kind !== "shared") {
      this.toast("warn", "Couldn't establish the share");
      return 0;
    }
    const person = rel.person;
    const senderLabel = this.node(sender)?.label ?? "device";

    // Reconcile against what's chosen: grant the selected consoles, revoke the
    // ones turned off — so the builder *manages* the share, not just adds to
    // it. The console's own pieces no longer need reconciling against each
    // other (they are one cap now), so nothing implies anything here.
    const want = new Set(caps);
    // `shareCapGrants` mints nothing for Files without a folder, which would
    // make the toggle look accepted and hand over precisely nothing. Say so
    // instead — a share nobody can open is worse than a refused one.
    if (want.has("files") && !this.shareFlowFolder) {
      this.toast("warn", "Pick the folder to share first");
      return 0;
    }
    const ALL: ShareCap[] = ["console", "files", "terminal", "sites"];
    for (const cap of ALL) {
      const grants = this.shareCapGrants(person.id, cap, sender, senderLabel);
      if (want.has(cap)) {
        for (const g of grants) this.grant(receiver, g);
      } else {
        for (const g of grants) this.revokeGrant(receiver, g.id);
      }
    }
    // No toast — on success the builder closes and the share shows up inline:
    // the grant rows in the device drawer's "What X can do", the Sharing pane,
    // and the console buttons on the fleet's cards.
    return want.size;
  }

  /** Stop the share: revoke every console grant my sender device gave the
   *  receiving fleet (the persistent permission, not a live route). Returns how
   *  many grants were pulled so the builder can close on success (its own
   *  disappearance — and the now-empty grant list behind it — is the feedback);
   *  only the nothing-to-stop case keeps a toast, since it's a soft failure. */
  stopShareFlow(): number {
    const sender = this.shareFlowSender;
    const receiver = this.shareFlowReceiver;
    if (!receiver) return 0;
    const recv = this.node(receiver);
    if (!recv || recv.relationship.kind !== "shared") {
      this.toast("warn", "Nothing to stop");
      return 0;
    }
    const senderCanon = sender ? canonicalNodeId(sender) : null;
    const toRevoke = recv.relationship.grants.filter((g) => {
      if (!g.capability) return false;
      const gNode = canonicalNodeId(g.capability.slice(0, g.capability.indexOf(":")));
      return senderCanon ? gNode === senderCanon : true;
    });
    for (const g of toRevoke) this.revokeGrant(receiver, g.id);
    if (!toRevoke.length) this.toast("warn", "Nothing to stop");
    return toRevoke.length;
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
    const inputs = this.capsOf(nodeId)
      .filter((c) => (c.media === "display" || c.media === "video") && canSource(c.flow))
      .map((c) => ({ ...c }));
    const customer = this.machineByAnyId(nodeId);
    if (customer && this.isCecCustomer(customer.id)) {
      const kvm = this.kvmAttachedTo(customer.id);
      if (kvm && this.kvmPassthroughDoors(kvm, customer)) {
        for (const cap of this.capsOf(kvm.id)) {
          if ((cap.media === "display" || cap.media === "video") && canSource(cap.flow)) {
            inputs.push({ ...cap, label: `${kvm.label} KVM` });
          }
        }
      }
    }
    return inputs.sort((a, b) => {
      const aKvm = this.isKvm(this.machineByAnyId(a.node));
      const bKvm = this.isKvm(this.machineByAnyId(b.node));
      const rank = (c: Capability, kvm: boolean) =>
        kvm ? 1 : c.origin === "screen" ? 0 : c.default ? 2 : 3;
      return rank(a, aKvm) - rank(b, bKvm) || a.id.localeCompare(b.id);
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
    // Resolve under any id form (display id vs bare pubkey): callers hand us
    // whatever id they have. An exact lookup here used to miss, fail
    // `consoleAllowed`, and return without a word — "the console just never
    // opens".
    const node = this.machineByAnyId(nodeId);
    if (node) nodeId = node.id;
    // A CEC customer's console NEVER opens straight off the graph, the
    // drawer, or the card buttons: every entry runs the Support tab's dial →
    // approval flow instead. Opening directly would ride whatever
    // authorization is left on the customer's node (e.g. an "Approve Once"
    // from the last session) — around the normal checks, with no fresh
    // approval. The tab opens showing the dial/waiting state, and the console
    // still pops by itself the moment they approve (or instantly on a
    // standing grant).
    if (node && this.isCecCustomer(node.id)) {
      this.openCecSupportFlowFor(node.id);
      return;
    }
    this.openConsoleResolved(nodeId, node);
  }

  /** The one console-open implementation — a dedicated OS window on
   *  desktop, an in-place session on the phone/web. BOTH the normal
   *  console (above) and the CEC support console (below) funnel through
   *  here, so the "support console" is a literal clone of the main one:
   *  it renders the same `ConsoleHost` → `Console.svelte`, wires video
   *  the same way (`openConsoleHere` → `applyConsoleVideo`), and cannot
   *  drift. The ONLY thing that differs between them is authorization —
   *  a CEC customer reaches this after their approval instead of a fleet
   *  relationship — which is upstream of this method, not inside it. */
  private openConsoleResolved(nodeId: string, node: MeshNode | undefined) {
    if (!this.consoleAllowed(node, nodeId)) return;
    if (isTauri() && !isMobile()) {
      void openConsoleWindow(nodeId);
      return;
    }
    this.openConsoleHere(nodeId);
  }

  /** The post-approval console open — the ONLY direct path onto a CEC
   *  customer's console, reached exclusively through a session the customer
   *  just approved (see [`openCecConsoleWhenFound`]). Ordinary machines never
   *  come through here. It is the identical open as a normal console
   *  (`openConsoleResolved`); the CEC-ness was the authorization to get
   *  here, nothing about the console itself. */
  private openCecConsoleDirect(nodeId: string) {
    const node = this.machineByAnyId(nodeId);
    if (node) nodeId = node.id;
    this.openConsoleResolved(nodeId, node);
  }

  /** Route a CEC customer's console ask through the Support tab: open the
   *  tab (where the dial / waiting-for-approval system lives) and re-open the
   *  session with that device on the support area. */
  openCecSupportFlowFor(nodeId: string) {
    this.openSettings("cec");
    const c = this.cecCustomers.find((x) => x.node && sameMachine(x.node, nodeId));
    if (c?.node && !this.cecDialing) void this.reconnectCec(c.node);
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
    // Same any-form resolve as `openConsole` — this is the web/window body.
    const node = this.machineByAnyId(nodeId);
    if (node) nodeId = node.id;
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
    // A new session starts with no leg suppressed — the auto-legs bring up
    // everything shared, and only a later user toggle-off re-populates this.
    this.consoleUserOff.clear();
    this.consoleInput = this.consoleVideoInputs(nodeId)[0]?.id ?? null;
    this.consoleCodecBySource = {};
    this.consoleTuneBySource = {};
    if (isTauri() && !isMobile()) {
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
      // The web preview and the phone shell are single-window (popouts
      // can't exist — `popOutConsoleInput` refuses there), so there's no
      // census to wait on: wire the picture now.
      void this.wireConsoleFirstVideo();
    }
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
    // Only bring up the legs the remote actually shared — auto-enabling an
    // ungranted one used to call connect() -> proposeRoute() and pop a
    // "share more access" approval sheet just for opening the console. With
    // this, a screen-only share opens to the screen and whatever else was
    // granted, and nothing prompts; the toggles for the rest stay hidden
    // (Console.svelte gates them on the same access).
    const access = this.consoleAccess(this.consoleNode ?? undefined);
    // Respect a leg the user already switched off (the fallback timer can land
    // after an early toggle) — same "only turn on what wasn't turned off" rule
    // the snapshot re-assert follows.
    if (!this.consoleAudio && access.audio && !this.consoleUserOff.has("audio"))
      this.toggleConsoleAudio();
    if (!this.consoleControl && access.control && !this.consoleUserOff.has("control"))
      this.toggleConsoleControl();
    if (!this.consoleClipboard && access.clipboard && !this.consoleUserOff.has("clipboard"))
      this.toggleConsoleClipboard();
  }

  /** The gate both console entries share: a known remote machine that runs
   *  AllMyStuff and is yours (or shared with you). */
  private consoleAllowed(node: MeshNode | undefined, nodeId: string): node is MeshNode {
    if (!node) {
      // Say so — a silent refusal here reads as "the button does nothing".
      this.toast("warn", "That machine isn't on the graph yet — give it a beat and try again");
      return false;
    }
    if (nodeId === this.localId) {
      this.toast("warn", "That's this device");
      return false;
    }
    if (!isAppNode(node)) {
      this.toast("warn", `${node.label} isn't running AllMyStuff`);
      return false;
    }
    if (node.relationship.kind === "unclaimed" && !this.isCecCustomer(node.id)) {
      // A dialed CEC customer is never "claimed" by the technician — the
      // customer's consent grant is the authorization — so the console opens on
      // it directly; every other unclaimed node still needs a claim/share first.
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
    if (this.consoleInput === capId) return;
    this.consoleInput = capId;
    void this.applyConsoleVideo();
    if (this.consoleControl) this.rewireConsoleControl();
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
    if (!inp) {
      // No input to wire — the CEC support-console failure the field saw:
      // the window pops the instant a customer approves, before their
      // display capability lands in this fresh window's catalog, so there
      // is nothing to stream. `ensureConsoleInput` (the reactive re-drive
      // in ConsoleHost) re-runs this the moment the cap arrives by any
      // path; name it here so a stuck console explains itself in the log.
      if (this.consoleNodeId) {
        const n = this.consoleVideoInputs(this.consoleNodeId).length;
        clientLog(
          `[console] no video input to wire for ${shortId(this.consoleNodeId)} yet ` +
            `(${n} candidate${n === 1 ? "" : "s"} in catalog) — waiting for the capability`,
        );
      }
      return;
    }
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
    if (!sink) {
      clientLog(
        `[console] no local ${inp.media} sink to receive ${shortId(inp.node)}'s stream — ` +
          `this machine can't consume it`,
      );
      return;
    }
    const leg = this.ownedConnect(inp.id, sink.id, this.consoleCodec);
    // Render whatever's live; only own the route for teardown if this call
    // created it.
    this.consoleVideoLive = leg?.id ?? null;
    this.consoleVideoRouteId = leg?.created ? leg.id : null;
    // Carry the quality pills onto the fresh route (the sender restarts
    // its capture with them; harmless no-op when everything is Auto).
    if (leg && this.hasTune()) void tuneRoute(leg.id, this.consoleTune);
  }

  /** Re-drive the console's video wire once a video input for the open
   *  target becomes available — the path-agnostic fix for the CEC
   *  support console, whose customer capabilities can land after the
   *  window already opened and gave up (the snapshot re-pick only fires
   *  for caps that arrive *in a session snapshot*; a cap arriving via
   *  presence/catalog otherwise stranded the window at a blank stage).
   *  Idempotent: does nothing once an input is picked, so a healthy
   *  console (caps present at open) never enters here. Driven reactively
   *  by [`ConsoleHost`]'s effect over `consoleVideoInputs`. */
  ensureConsoleInput() {
    if (!this.consoleNodeId || this.consoleInput) return;
    const first = this.consoleVideoInputs(this.consoleNodeId)[0]?.id ?? null;
    if (first) {
      this.consoleInput = first;
      void this.applyConsoleVideo();
    }
  }

  private hasTune(): boolean {
    const t = this.consoleTune;
    // Mode is a first-class tune too. Omitting it here meant a mode-only
    // Studio/Game selection was remembered in the GUI but not re-applied to
    // the fresh route after a monitor switch or codec re-offer, so the sender
    // silently came back Balanced.
    return (
      t.maxEdge != null ||
      t.bitrate != null ||
      t.fps != null ||
      t.mode != null ||
      t.game != null
    );
  }

  /** A quality pick changed (a pill or the slider): remember it against the
   *  current source and re-tune the live stream. */
  setConsoleTune(patch: StreamTune) {
    const s = this.consoleInput;
    if (!s) return;
    this.consoleTuneBySource = {
      ...this.consoleTuneBySource,
      [s]: { ...(this.consoleTuneBySource[s] ?? {}), ...patch },
    };
    if (this.consoleVideoLive) void tuneRoute(this.consoleVideoLive, this.consoleTune);
  }

  /** The codec pick changed: remember it against the current source and
   *  re-offer the video route on that transport. */
  setConsoleCodec(codec: "auto" | "h264" | "mjpeg") {
    const s = this.consoleInput;
    if (!s || this.consoleCodec === codec) return;
    this.consoleCodecBySource = { ...this.consoleCodecBySource, [s]: codec };
    void this.applyConsoleVideo();
  }

  /** Flip the quality surface (slider ⇄ pills) and remember it across
   *  windows, so the next console opens the same way. */
  toggleConsoleControlMode() {
    this.consoleControlMode = this.consoleControlMode === "slider" ? "pills" : "slider";
    try {
      localStorage.setItem("ams.consoleControlMode", this.consoleControlMode);
    } catch {
      // No storage (private mode / web preview) — in-memory for this session.
    }
  }

  /** Flip the Experimental (Labs) tier — the Mode dropdown's toggle.
   *  Persists the choice, pushes it to the node's labs gate (which every
   *  future field-trial feature reads), and it re-pushes on boot via
   *  [`pushLabsTier`] so a restart honors the last setting. `on` is
   *  optional — omit to flip. */
  setLabsTier(on?: boolean) {
    this.labsTier = on ?? !this.labsTier;
    try {
      localStorage.setItem("ams.labs", this.labsTier ? "1" : "0");
    } catch {
      // No storage — in-memory for this session.
    }
    void this.pushLabsTier();
  }

  /** Push the persisted Labs tier to this node's gate — on boot and on
   *  every toggle. A no-op in web mode (labsSet tryInvokes). */
  private pushLabsTier() {
    return labsSet(this.labsTier);
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
      // Remember the user chose off, so the auto-legs stop bringing it back.
      this.consoleUserOff.add("audio");
      return;
    }
    // Turning it (back) on is intent to hear — the auto-legs may resume.
    this.consoleUserOff.delete("audio");
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

  /** Send this machine's keyboard & mouse to the remote. The far side injects
   *  it once authorized — its owner/fleet, or a person it granted control to
   *  (the backend honours the share grant, so a shared "Control" actually
   *  drives the machine rather than lighting an inert route). */
  toggleConsoleControl() {
    if (!this.consoleNodeId) return;
    if (this.consoleControl) {
      if (this.consoleControlRouteId) this.disconnect(this.consoleControlRouteId);
      this.consoleControlRouteId = null;
      this.consoleControlLive = null;
      this.consoleControl = false;
      this.consoleUserOff.add("control");
      return;
    }
    this.consoleUserOff.delete("control");
    this.consoleControl = true;
    if (!this.rewireConsoleControl()) {
      this.consoleControl = false;
      this.toast("warn", "No control path to that machine");
    }
  }

  /** Route input to the machine behind the selected picture. The customer's
   *  own screen/cameras use its software input sink; its attached KVM screen
   *  uses the KVM HID sink under the same renewable support delegation. */
  private consoleControlTarget(): MeshNode | null {
    const customer = this.consoleNode;
    if (!customer) return null;
    const source = this.consoleInput ? this.capability(this.consoleInput) : null;
    const sourceNode = source ? this.machineByAnyId(source.node) : undefined;
    if (sourceNode && this.kvmPassthroughDoors(sourceNode, customer)) return sourceNode;
    return customer;
  }

  /** Re-home an enabled keyboard/mouse route when the selected picture moves
   *  between the customer computer and its attached KVM. */
  private rewireConsoleControl(): boolean {
    if (this.consoleControlRouteId) void this.disconnect(this.consoleControlRouteId);
    this.consoleControlRouteId = null;
    this.consoleControlLive = null;
    if (!this.consoleControl) return false;
    const remote = this.consoleControlTarget();
    if (!remote) return false;
    const mySrc = matchEndpoint(this.catalog, this.localId, "input", "provide");
    const remoteSink = matchEndpoint(this.catalog, remote.id, "input", "consume");
    const leg = mySrc && remoteSink ? this.ownedConnect(mySrc.id, remoteSink.id) : null;
    if (leg) {
      this.consoleControlRouteId = leg.created ? leg.id : null;
      this.consoleControlLive = leg.id;
      return true;
    }
    return false;
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
      this.consoleUserOff.add("clipboard");
      return;
    }
    this.consoleUserOff.delete("clipboard");
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

  /** Copy/cut *from* the remote — the mirror of paste. Forwards the copy/cut
   *  chord down the control route (so the remote copies its selection into
   *  its own clipboard), then pulls that clipboard back so it lands here.
   *  Awaits the keystroke before the pull so the two ride the same ordered
   *  channel to the peer in order (copy, then read); the remote waits a beat
   *  for its app to land the copy before replying. No-op unless both control
   *  and clipboard passthrough are live. `heldMeta` is whether the user's
   *  chord used Cmd (vs Ctrl) — see [`forwardClipboardChord`] for why. */
  async copyConsoleClipboard(
    key: string,
    code: string | undefined,
    heldMeta: boolean,
  ): Promise<void> {
    if (!this.consoleControlLive || !this.consoleClipboardLive) return;
    await this.forwardClipboardChord(key, code, heldMeta);
    await clipboardPull(this.consoleClipboardLive);
  }

  /** Paste *into* the remote: push this machine's clipboard down the live
   *  route, then forward the paste chord so the remote pastes our content.
   *  Clipboard first, keystroke second — the order the remote needs (write
   *  clipboard, then inject paste), both on the same ordered channel. */
  async pasteConsoleClipboard(
    key: string,
    code: string | undefined,
    heldMeta: boolean,
  ): Promise<void> {
    if (!this.consoleControlLive || !this.consoleClipboardLive) return;
    await this.sendConsoleClipboard();
    await this.forwardClipboardChord(key, code, heldMeta);
  }

  /** Drop native local files onto the remote desktop. Publish a real file-list
   * clipboard on the far OS, place and click the remote pointer where the user
   * released the drag, then press that OS's paste chord. The clipboard write
   * completes before the chord is sent, so slow image/file formats cannot
   * race the paste. */
  async dropConsoleFiles(
    paths: string[],
    point: { x: number; y: number; screen?: number },
  ): Promise<void> {
    const control = this.consoleControlLive;
    const clipboard = this.consoleClipboardLive;
    if (!control || !clipboard) {
      throw new Error("Keyboard & mouse and Clipboard must both be on");
    }
    await clipboardDrop(clipboard, paths);
    await sendInput(control, { kind: "mouse_move", ...point });
    await sendInput(control, { kind: "mouse_button", button: 0, down: true });
    await sendInput(control, { kind: "mouse_button", button: 0, down: false });
    await this.sendRemotePasteChord();
  }

  /** A complete paste chord when no physical modifier is already held (the
   * native file-drop path). `forwardClipboardChord` serves keyboard events and
   * intentionally assumes the user's Ctrl/Cmd is already down remotely. */
  private async sendRemotePasteChord(): Promise<void> {
    const control = this.consoleControlLive;
    if (!control) return;
    const remoteMeta = (this.consoleNode?.summary?.os ?? "").toLowerCase().includes("mac");
    const modifier = remoteMeta
      ? { key: "Meta", code: "MetaLeft" }
      : { key: "Control", code: "ControlLeft" };
    await sendInput(control, { kind: "key", ...modifier, down: true });
    await sendInput(control, { kind: "key", key: "v", code: "KeyV", down: true });
    await sendInput(control, { kind: "key", key: "v", code: "KeyV", down: false });
    await sendInput(control, { kind: "key", ...modifier, down: false });
  }

  /** Forward a copy/cut/paste chord to the remote, translating the modifier
   *  to the *remote's* convention — copy/paste is Cmd+key on macOS but
   *  Ctrl+key on Windows/Linux, and input injection forwards modifiers
   *  literally, so a Ctrl+C sent to a Mac never copies. `heldMeta` is the
   *  modifier the user actually pressed (Cmd if true, else Ctrl), already
   *  held down on the remote. When it matches what the remote needs, we just
   *  complete the chord with the letter; when it differs we lift the held
   *  one on the remote, swap in the right one for the letter, then restore
   *  the held one — so a following chord on the same modifier (Ctrl+C then
   *  Ctrl+V) still lands. The remote OS comes from its presence advert;
   *  absent (an older peer) we assume Ctrl, the no-op for same-OS pairs. */
  private async forwardClipboardChord(
    key: string,
    code: string | undefined,
    heldMeta: boolean,
  ): Promise<void> {
    const control = this.consoleControlLive;
    if (!control) return;
    const CONTROL = { key: "Control", code: "ControlLeft" };
    const META = { key: "Meta", code: "MetaLeft" };
    const remoteMeta = (this.consoleNode?.summary?.os ?? "").toLowerCase().includes("mac");
    const mod = (m: { key: string; code: string }, down: boolean) =>
      sendInput(control, { kind: "key", key: m.key, code: m.code, down });
    const letter = async () => {
      await sendInput(control, { kind: "key", key, code, down: true });
      await sendInput(control, { kind: "key", key, code, down: false });
    };
    if (remoteMeta === heldMeta) {
      await letter();
      return;
    }
    const held = heldMeta ? META : CONTROL;
    const want = remoteMeta ? META : CONTROL;
    await mod(held, false);
    await mod(want, true);
    await letter();
    await mod(want, false);
    await mod(held, true);
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
    // A dialed CEC customer never becomes "mine" or "shared" — the customer's
    // live consent grant is the authorization, and *their* node enforces it
    // (per offer for the screen, per frame for control). Routing these legs
    // through `proposeRoute` would refuse them structurally ("isn't yours yet
    // — claim it first" / a share prompt), because the client-side rules only
    // know owner/fleet/share. So wire a CEC leg directly, the same way a
    // popout continues an already-authorized stream: if the customer's grant
    // doesn't actually cover it, their node rejects the route and the console
    // surfaces that reason.
    const cecLeg = [from, to].some((capId) => {
      const cap = this.capability(capId);
      return !!cap && !this.isMe(cap.node) && this.isCecCustomer(cap.node);
    });
    // An attached KVM is a short-lived extension of the approved customer
    // session, not a device claimed/shared into the technician's fleet. Offer
    // its screen/input directly and let the KVM enforce the customer's lease.
    const supportKvmLeg = [from, to].some((capId) => {
      const cap = this.capability(capId);
      const kvm = cap ? this.machineByAnyId(cap.node) : undefined;
      return !!kvm && !!this.consoleNode && this.kvmPassthroughDoors(kvm, this.consoleNode);
    });
    if (cecLeg || supportKvmLeg) {
      if (!this.capability(from) || !this.capability(to)) return null;
      const id = `route:${from}→${to}`;
      const existedBefore = this.catalog.routes.some((r) => r.id === id);
      if (!existedBefore) {
        this.addRoute(from, to);
        this.fireBackendConnect(from, to, this.capability(from)!.media, codec);
      }
      return { id, created: !existedBefore };
    }
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
  async popOutConsoleInput(
    capId: string,
    decoder: NativeDecoderPreference = "automatic",
  ) {
    if (!isTauri() || isMobile()) return;
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
    void openVideoWindow(
      key,
      `${cap.label} · ${machine?.label ?? "AllMyStuff"}`,
      decoder,
    );
  }

  /** Lift a room share's tile out into its own OS window. The popout only
   *  *watches* the route (the sender owns it), so nothing re-negotiates —
   *  the frames simply land in the new window instead of the tile. */
  popOutRoomShare(route: Route, member: MeshNode) {
    if (!isTauri() || isMobile()) return;
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
   *  presence advertises the feature (an older build simply doesn't). The
   *  machine we're sitting at is always capable — the running binary *is* the
   *  terminal host — and its own presence features aren't self-populated
   *  (features arrive from peers), so check identity, not the advertised
   *  list, for self. */
  terminalSupported(node: MeshNode | undefined): boolean {
    if (!node) return false;
    if (this.isMe(node.id)) return true;
    return isAppNode(node) && (node.features ?? []).includes(FEATURE_TERMINAL);
  }

  /** The gate for "Open Terminal" — a mirror of the host's own rule
   *  (`sender_may_control`): only the node's recorded owner or a co-owned
   *  fleet member gets a shell. Deliberately *not* `relationship.kind`,
   *  which is a local label the user can set freely; this checks the same
   *  facts the far side will enforce, so the button never promises what
   *  the host would refuse. The machine we're sitting at is always allowed:
   *  it's our own shell, opened over a loopback route (no peer). */
  terminalAllowed(node: MeshNode | undefined): boolean {
    if (!node) return false;
    if (this.isMe(node.id)) return this.localTerminalAllowed;
    if (!this.terminalSupported(node)) return false;
    const ownerIsMe = !!node.owner && this.isMe(node.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node.id);
    return ownerIsMe || coFleet || this.hasShareGrant(node, "terminal");
  }

  /** Whether a terminal to *this* machine is offerable. The running binary
   *  is the host, so the feature is always present and you always control
   *  your own shell — the only requirement is a live backend (no backend,
   *  nothing can flow). */
  get localTerminalAllowed(): boolean {
    return this.backendConnected;
  }

  /** Open a terminal on a machine. A remote machine gets a shell over the
   *  mesh; *this* machine gets a local shell over a loopback route (no peer
   *  — the same terminal UI, the same engine, a PTY right here). On the
   *  desktop this opens (or focuses) the machine's dedicated terminal window
   *  — tabs inside it are separate shells; the web preview keeps an in-page
   *  popover. */
  openTerminal(nodeId: string) {
    const node = this.node(nodeId);
    if (!node) return;
    if (this.isMe(nodeId)) {
      // The local machine: it's our own shell, no support/ownership gate to
      // mirror — just a live backend to carry it.
      if (!this.localTerminalAllowed) {
        this.toast("warn", "The local terminal needs the desktop app's backend");
        return;
      }
    } else {
      if (!this.terminalSupported(node)) {
        this.toast("warn", `${node.label} doesn't support terminals (older AllMyStuff?)`);
        return;
      }
      if (!this.terminalAllowed(node)) {
        this.toast("warn", `Terminals are owner/fleet only — ${node.label} isn't yours`);
        return;
      }
    }
    if (isTauri() && !isMobile()) {
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

  /** Pop a terminal tab out into its own window — the terminal twin of a
   *  video feed's pop-out. The shell lives on the host (tmux-style
   *  multi-attach), so we just open a fresh terminal window for `hostNodeId`
   *  whose first tab *attaches* to that same `session`; the caller closes the
   *  originating tab, which detaches this viewer while the shell (and its
   *  scrollback) carries straight on in the new window. Desktop only — a
   *  popped-out window is an OS window, which the web preview can't open. */
  popOutTerminal(hostNodeId: string, session: string) {
    if (!isTauri() || isMobile()) return;
    void openTerminalWindow(hostNodeId, session);
  }

  /** Open one terminal *session* (a tab) to `hostNodeId`: a generic route
   *  from the host's `…:terminal` endpoint to a viewer endpoint minted for
   *  this tab — unique endpoint, unique route id, so tabs to one machine
   *  never collide. Deliberately not `connect()`/`proposeRoute`: terminal
   *  endpoints aren't catalog capabilities (see `capabilityForDisplay`),
   *  and the binding authorization runs host-side against the owner/fleet
   *  rule. Returns the route id the tab watches, or null in web mode
   *  (no backend — nothing can flow). */
  terminalConnect(hostNodeId: string, session?: string | null): string | null {
    if (!this.backendConnected) return null;
    const from = `${hostNodeId}:terminal`;
    const n = ++this.termViewSeq;
    const to = `${this.localId}:term-view:${Date.now().toString(36)}-${n}`;
    // `session` is the multi-attach hook: a non-null id makes the Offer name
    // an already-running host shell to join (shared, tmux-style); null/absent
    // mints a fresh shell — exactly what "New terminal" does.
    void connectRoute(from, to, "generic", undefined, session ?? null);
    return `route:${from}→${to}`;
  }

  /** Discover a host's open terminal sessions for the multi-attach picker.
   *  The **local** machine answers at once (its own shells); a **remote**
   *  host answers asynchronously over the mesh, arriving as a
   *  `allmystuff://terminal-sessions` event — so this returns the immediate
   *  list (or an empty list while a remote reply is in flight) and the
   *  caller subscribes to {@link onTerminalSessions} for the remote answer.
   *  Empty in web mode (no backend, nothing to list). */
  async listTerminalSessions(hostNodeId: string): Promise<TerminalSessionInfo[]> {
    if (!this.backendConnected) return [];
    const immediate = await terminalSessions(hostNodeId);
    return immediate ?? [];
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
    return ownerIsMe || coFleet || this.hasShareGrant(node, "files");
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
    if (isTauri() && !isMobile()) {
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

  /** App machines that can receive a mapped drive. KVMs stay excluded: they
   *  are headless passthroughs, not storage destinations. */
  get driveTargets(): MeshNode[] {
    return this.catalog.nodes.filter(
      (n) => !this.isMe(n.id) && n.online && isAppNode(n) && !this.isKvm(n),
    );
  }

  /** Remote machines whose filesystem this user is authorized to browse and
   * therefore choose as the source of an inbound native mapping. */
  get driveSources(): MeshNode[] {
    return this.catalog.nodes.filter(
      (node) => !this.isMe(node.id) && node.online && !this.isKvm(node) && this.filesAllowed(node),
    );
  }

  get driveMappings(): Array<{
    route: Route;
    direction: "in" | "out";
    drive: string;
    machine: string;
    host: string;
    target: string;
    mount: string;
    status: "connecting" | "mounted" | "shared";
  }> {
    const out = [] as Array<{
      route: Route;
      direction: "in" | "out";
      drive: string;
      machine: string;
      host: string;
      target: string;
      mount: string;
      status: "connecting" | "mounted" | "shared";
    }>;
    for (const route of this.catalog.routes) {
      if (route.media !== "storage" || !route.to.endsWith(":storage-in")) continue;
      const from = this.capability(route.from);
      const to = this.capability(route.to);
      const fromNode = from?.node ?? this.capNodeOf(route.from);
      const toNode = to?.node ?? this.capNodeOf(route.to);
      const mounted = this.driveMounts[route.id];
      const driveLabel = mounted?.label ?? route.drive?.label ?? from?.label ?? "Mapped drive";
      const mount = mounted?.mount ?? route.drive?.mount ?? "";
      if (this.isMe(toNode)) {
        out.push({
          route,
          direction: "in",
          drive: driveLabel,
          machine: this.machineByAnyId(fromNode)?.label ?? shortId(fromNode),
          host: fromNode,
          target: toNode,
          mount,
          status: mounted ? "mounted" : "connecting",
        });
      } else if (this.isMe(fromNode)) {
        out.push({
          route,
          direction: "out",
          drive: driveLabel,
          machine: this.machineByAnyId(toNode)?.label ?? shortId(toNode),
          host: fromNode,
          target: toNode,
          mount,
          status: "shared",
        });
      }
    }
    return out;
  }

  pickDriveSource(): Promise<string | null> {
    return pickDriveFolder();
  }

  pickKvmMediaImage(): Promise<string | null> {
    return pickKvmMediaImage();
  }

  /** Eligible virtual-media sources for a KVM. Its attached computer is
   * deliberately excluded: that circular source disappears during the very
   * reboot into BIOS/Setup for which virtual media exists. */
  kvmMediaSources(kvmId: string): MeshNode[] {
    const kvm = this.machineByAnyId(kvmId);
    const attached = kvm?.kvm?.attachedTo;
    return this.catalog.nodes.filter(
      (node) =>
        node.online &&
        isAppNode(node) &&
        !this.isKvm(node) &&
        (!attached || !sameMachine(node.id, attached)) &&
        (this.isMe(node.id) || this.filesAllowed(node)),
    );
  }

  async stageKvmMedia(kvm: string, source: string, path: string, label: string): Promise<boolean> {
    const kvmNode = this.machineByAnyId(kvm);
    const sourceNode = this.machineByAnyId(source);
    if (!this.isKvm(kvmNode) || !sourceNode) return false;
    if (kvmNode!.kvm?.attachedTo && sameMachine(kvmNode!.kvm!.attachedTo!, sourceNode.id)) {
      this.toast("warn", "The attached computer can't source this KVM's virtual media");
      return false;
    }
    try {
      await stageKvmMediaNative(sourceNode.id, kvmNode!.id, path, label);
      if (this.isMe(sourceNode.id))
        this.toast("ok", `${label || "Virtual media"} is mounted on ${kvmNode!.label}`);
      return true;
    } catch (error) {
      this.toast("warn", errMsg(error));
      return false;
    }
  }

  async unmountKvmMedia(kvm: string): Promise<void> {
    const node = this.machineByAnyId(kvm);
    if (!this.isKvm(node)) return;
    try {
      await unmountKvmMediaNative(node!.id);
      this.toast("ok", `Virtual media ejected from ${node!.label}`);
    } catch (error) {
      this.toast("warn", errMsg(error));
    }
  }

  async mapFolderToNode(
    targetNodeId: string,
    root: string,
    label: string,
    mount = "",
    supportSession = false,
  ): Promise<boolean> {
    if (!this.driveTargets.some((node) => sameMachine(node.id, targetNodeId))) {
      this.toast("warn", "That machine can't receive a mapped drive");
      return false;
    }
    if (!this.isFleetMember(targetNodeId) && !(supportSession && this.isCecCustomer(targetNodeId))) {
      this.toast("warn", "Mapping a drive onto another machine requires Fleet or an active support session");
      return false;
    }
    try {
      const route = await mapNativeDrive(targetNodeId, root, label, mount);
      if (!route) {
        this.toast("warn", "Native drive mapping requires the desktop app");
        return false;
      }
      const target = this.machineByAnyId(targetNodeId)?.label || "the other computer";
      this.toast("ok", `${label || "Drive"} is being mapped onto ${target}`);
      return true;
    } catch (error) {
      this.toast("warn", String(error));
      return false;
    }
  }

  async mapFolderFromNode(
    sourceNodeId: string,
    root: string,
    label: string,
    mount = "",
    supportSession = false,
  ): Promise<boolean> {
    if (
      !this.driveSources.some((node) => sameMachine(node.id, sourceNodeId)) &&
      !(supportSession && this.isCecCustomer(sourceNodeId))
    ) {
      this.toast("warn", "That machine hasn't shared Files access with you");
      return false;
    }
    try {
      await mapNativeDriveFrom(sourceNodeId, root, label, mount);
      this.toast("ok", `${label || "Drive"} is connecting to this computer`);
      return true;
    } catch (error) {
      this.toast("warn", String(error));
      return false;
    }
  }

  unmapDrive(routeId: string) {
    void this.disconnect(routeId);
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
    return ownerIsMe || coFleet || this.hasShareGrant(node, "sites");
  }

  // ---- cross-fleet console access (the share-enforcement the gates above
  //      anticipated) -------------------------------------------------------

  /** Every grant covering `node`, unioned across the whole person/fleet (a
   *  grant authorizes the person wherever it's recorded). */
  private shareGrantsFor(node: MeshNode): Grant[] {
    if (node.relationship.kind !== "shared") return [];
    const pid = node.relationship.person.id;
    const out: Grant[] = [];
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind === "shared" && n.relationship.person.id === pid) {
        out.push(...n.relationship.grants);
      }
    }
    return out;
  }

  /** Grants that authorize ME to use the other fleet's device. A share record
   *  contains both directions: scoped grants on my devices are what I shared
   *  OUT, while scoped grants on their devices are what they shared IN. Keep
   *  ambiguous legacy media-wide grants on the outbound side (the same rule
   *  [`isShareOutGrant`] uses in the Sharing UI) so an old grant can never
   *  manufacture Remote/Drives access to every computer in their fleet. */
  private shareInGrantsFor(node: MeshNode): Grant[] {
    return this.shareGrantsFor(node).filter((grant) => !this.isShareOutGrant(grant));
  }

  /** Whether a fleet that shared `node` with me granted me a given console on
   *  it. Direction matters: to *open* their console I need them to PROVIDE
   *  their screen/audio (and CONSUME my input for control) — the opposite of a
   *  grant where *I* let *them* see *my* screen, so sharing out never unlocks
   *  the same button on the way back. Terminal/Sites ride a generic grant
   *  carrying the synthetic `<node>:terminal` / `<node>:sites` capability. */
  hasShareGrant(node: MeshNode | undefined, kind: "remote" | "audio" | "control" | "clipboard" | "files" | "terminal" | "sites"): boolean {
    if (!node || node.relationship.kind !== "shared") return false;
    const gs = this.shareInGrantsFor(node);
    const provide = (g: Grant) => g.role === "provide" || g.role === "both";
    const consume = (g: Grant) => g.role === "consume" || g.role === "both";
    // A grant only unlocks the device it names — a grant scoped to my MacBook's
    // screen mustn't light up a different device's card. A capability-less grant
    // (media-wide) is deliberately absent here: without a capability it has
    // no direction or device identity, so it is share-out-only above.
    const canon = canonicalNodeId(node.id);
    const forNode = (g: Grant) =>
      !!g.capability &&
      canonicalNodeId(g.capability.slice(0, g.capability.indexOf(":"))) === canon;
    switch (kind) {
      case "remote":
        return gs.some((g) => g.media === "display" && provide(g) && forNode(g));
      case "audio":
        return gs.some((g) => g.media === "audio" && provide(g) && forNode(g));
      case "control":
        return gs.some((g) => g.media === "input" && consume(g) && forNode(g));
      case "files":
        return gs.some((g) => g.media === "storage" && forNode(g));
      case "clipboard":
        // The clipboard rides with the control grant (the share builder mints
        // both for "Control"), so a control grant is what unlocks it.
        return gs.some((g) => g.media === "clipboard" && forNode(g));
      case "terminal":
        return gs.some((g) => g.media === "generic" && !!g.capability?.endsWith(":terminal") && forNode(g));
      case "sites":
        return gs.some((g) => g.media === "generic" && !!g.capability?.endsWith(":sites") && forNode(g));
    }
  }

  /** The consoles you may open on a node *right now* — your own fleet gets
   *  every console it supports; a device a fleet shared with you gets exactly
   *  the ones their grant covers. One source of truth for the graph-card
   *  buttons and the drawer's buttons so they can't disagree. */
  /** Whether `nodeId` is a CEC customer this technician has dialed. Its
   *  authorization for the remote console is the customer's live consent grant
   *  (they tapped Approve in the CEC Support app), not owner/fleet or an
   *  AllMyStuff share — so the console legs and the open/reopen buttons key off
   *  this, and the customer's node still enforces the grant per frame. */
  isCecCustomer(nodeId: string | undefined): boolean {
    if (!nodeId) return false;
    return this.cecCustomers.some((c) => sameMachine(c.node, nodeId));
  }

  /** The dialed CEC customer row for `nodeId`, if this node is one — the
   *  graph/drawer twin of the Help sidebar's known-customer entry. Carries
   *  the canonical CEC node id (what `reconnectCec` dials) and the live
   *  online dot, so the node drawer can offer the same one-tap "Open" the
   *  sidebar does instead of bouncing through the CEC settings tab. */
  cecPeerFor(nodeId: string | undefined): CecPeer | null {
    if (!nodeId) return null;
    return this.cecCustomers.find((c) => c.node && sameMachine(c.node, nodeId)) ?? null;
  }

  /** Live CEC standing toward a machine: we dialed it (it's in the customer
   *  directory) or its hand is up in the watched help queue right now. This is
   *  the technician's authority arm for a KVM's session doors — the same
   *  lifetime as the appliance's own approved-tech set (admitted while asking
   *  or on dial, dropped on hangup), which is what actually authorizes every
   *  tunneled request. This gate only decides what the GUI offers. */
  cecStanding(nodeId: string | undefined): boolean {
    if (!nodeId) return false;
    return (
      this.isCecCustomer(nodeId) ||
      this.cecHelpWaiting.some((w) => w.node && sameMachine(w.node, nodeId))
    );
  }

  consoleAccess(node: MeshNode | undefined): ConsoleAccess {
    const none: ConsoleAccess = {
      remote: false,
      files: false,
      terminal: false,
      sites: false,
      audio: false,
      control: false,
      clipboard: false,
    };
    if (!node || !isAppNode(node)) return none;
    const self = this.isMe(node.id);
    const ownerIsMe = !!node.owner && this.isMe(node.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node.id);
    const mineOrFleet = node.relationship.kind === "mine" || ownerIsMe || coFleet;
    // A dialed CEC customer is authorized for the remote-control legs by its own
    // consent grant, not by ownership/fleet — so screen view + control + audio +
    // clipboard become available on it (the customer's node still gates each
    // frame on the grant). Files/terminal/sites stay owner/fleet-only: the CEC
    // consent model only covers screen + control, so those would just be denied.
    const isCec = this.isCecCustomer(node.id);
    // A KVM appliance advertises a native Display/Source "screen" capability
    // (and an Input/Sink "control" one) as well as its own web UI. So it now
    // gets the generic Remote Control console — rendered over the mesh's native
    // video/control lanes, gated on that screen cap via `kvmHasNativeScreen` —
    // AND the standard Sites/globe button, which opens its web UI through
    // `openKVM`. A web-only KVM (no screen cap) keeps the old behavior: no
    // native console, just its web UI behind the globe.
    const kvm = this.isKvm(node);
    // A capability is *available on the console* only when the remote actually
    // exposes the endpoint AND you're authorized for it — so the console
    // activates with whatever subset was shared and hides the rest, instead of
    // forcing every leg on (which used to pop a grant prompt for a screen-only
    // share). Endpoints resolve canonically, so a fleet machine that exposes
    // audio/control/clipboard lights those up while one that doesn't leaves them
    // hidden.
    const exposes = (media: MediaKind, flow: "provide" | "consume") =>
      !!matchEndpoint(this.catalog, node.id, media, flow);
    return {
      // You don't remote into yourself; otherwise your fleet, or a granted
      // share. A KVM only offers the native console when it advertises a
      // screen capability (`kvmHasNativeScreen`); a web-only one stays hidden.
      remote:
        !self &&
        (!kvm || this.kvmHasNativeScreen(node)) &&
        (mineOrFleet || isCec || this.hasShareGrant(node, "remote")),
      files: this.filesSupported(node) && !self && (mineOrFleet || this.hasShareGrant(node, "files")),
      terminal:
        this.terminalSupported(node) &&
        (self ? this.localTerminalAllowed : mineOrFleet || this.hasShareGrant(node, "terminal")),
      sites:
        // A KVM may advertise only FEATURE_KVM (not FEATURE_SITES), yet its
        // web UI still rides the sites proxy (see `mapSite`'s kvmDoors
        // bypass) — so `|| kvm` lets the standard globe show for it too.
        // A KVM's globe also opens for live CEC standing (the help tech's
        // arm, matching the appliance's own approved-tech admission);
        // an ordinary customer's sites stay owner/fleet-only — the CEC
        // consent model covers screen + control, so a person's exposed
        // sites would just be denied.
        (this.sitesSupported(node) || kvm) &&
        (node.sites?.length ?? 0) > 0 &&
        !self &&
        (mineOrFleet ||
          this.hasShareGrant(node, "sites") ||
          (kvm && this.cecStanding(node.id))),
      // Audio passthrough: only when the machine has audio to send and you may
      // hear it.
      audio:
        !self &&
        exposes("audio", "provide") &&
        (mineOrFleet || isCec || this.hasShareGrant(node, "audio")),
      // Control (keyboard & mouse): the machine must accept control and you must
      // be its fleet, hold a control grant, or (CEC) have a live consent grant.
      control:
        !self &&
        exposes("input", "consume") &&
        (mineOrFleet || isCec || this.hasShareGrant(node, "control")),
      // The clipboard rides with control (same grant), gated on the endpoint.
      clipboard:
        !self &&
        exposes("clipboard", "consume") &&
        (mineOrFleet || isCec || this.hasShareGrant(node, "clipboard") || this.hasShareGrant(node, "control")),
    };
  }

  /** Open the right console for a graph-card button click. */
  openConsoleKind(nodeId: string, kind: "remote" | "files" | "terminal" | "sites") {
    if (kind === "remote") this.openConsole(nodeId);
    else if (kind === "files") this.openFiles(nodeId);
    else if (kind === "terminal") this.openTerminal(nodeId);
    else if (kind === "sites") {
      const n = this.node(nodeId);
      // A KVM's globe opens its own web UI through the KVM front door, which
      // picks the right web site (`kvmWebSite`) rather than just sites[0].
      if (this.isKvm(n)) {
        void this.openKVM(nodeId);
        return;
      }
      const site = n?.sites?.[0];
      if (site) void this.mapSite(nodeId, site);
    }
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

  /** This machine's exposed sites — the persisted exposure set joined with
   *  the live scan, so each entry carries its current online status. An
   *  entry stays here after its server stops listening (it just flips
   *  `online` false); that's what lets the Sites tab keep an exposed-but-
   *  offline site visible — with a red dot and a Stop control — instead of
   *  leaving the "N exposed" count pointing at nothing. Sorted by port for
   *  a stable order. */
  myExposedSites = $derived.by<ExposedSite[]>(() => {
    const live = new Map(this.myListening.map((s) => [s.id, s]));
    return Object.entries(this.exposedSites)
      .map(([id, name]): ExposedSite => {
        const svc = live.get(id);
        const port = svc?.port ?? sitePort(id);
        return {
          id,
          name: name.trim() || (svc ? this.defaultSiteName(svc) : `Port ${port}`),
          port,
          scheme: svc?.scheme ?? "",
          loopback: svc?.loopback ?? false,
          process: svc?.process ?? "",
          online: !!svc,
        };
      })
      .sort((a, b) => a.port - b.port);
  });

  /** Open one of this machine's exposed sites in the system browser at its
   *  local address. A web scheme opens as-is; anything else — or an unknown
   *  scheme on a service that's gone offline — defaults to http so the
   *  button always does something. */
  openLocalSite(site: { scheme: string; port: number }) {
    const scheme = siteIsWeb(site) ? site.scheme : "http";
    void openExternal(`${scheme}://localhost:${site.port}`);
  }

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
    this.syncLocalSites();
  }

  /** Mirror this machine's exposed services onto the local node's `sites`, so
   *  sharing Sites from your own machine lights up (its availability gate reads
   *  `node.sites`, which only ever arrives from a peer's advert for remote
   *  nodes — the local node must stamp its own). */
  private syncLocalSites() {
    const me = this.node(this.localId) ?? this.catalog.nodes.find((n) => n.kind === "this");
    if (!me) return;
    me.sites = this.myExposedSites.map((s) => ({
      id: s.id,
      label: s.name,
      port: s.port,
      scheme: s.scheme,
    }));
  }

  /** Seed the Sites tab with demo data in web mode (no scan to run). */
  private seedDemoSites() {
    this.myListening = [
      { id: "tcp:5173", name: "HTTP", port: 5173, kind: "http", scheme: "http", loopback: true, process: "vite", title: "My Project — Dev" },
      { id: "tcp:8000", name: "HTTP", port: 8000, kind: "http", scheme: "http", loopback: true, process: "python", title: "" },
      { id: "tcp:22", name: "SSH", port: 22, kind: "ssh", scheme: "ssh", loopback: false, process: "sshd", title: "" },
    ];
    // tcp:5173 is live (a row in `myListening`); tcp:3000 is exposed but no
    // longer listening — it shows in "Exposed from this machine" with a red
    // dot, demoing the offline case.
    this.exposedSites = { "tcp:5173": "My Project — Dev", "tcp:3000": "Grafana" };
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
    this.syncLocalSites();
  }

  /** Map a peer's site to a local port — sets up the reverse-proxy and binds
   *  a local listener, then records the mapping. Re-mapping an already-mapped
   *  site just returns its mapping. */
  async mapSite(nodeId: string, site: SiteAdvert) {
    const node = this.node(nodeId);
    if (!node) return;
    // A KVM's own web UI rides this same proxy path — gated by its session
    // doors (`kvmDoors`: yours to curate, OR live CEC standing as the help
    // tech), so accept it even if the appliance advertises only FEATURE_KVM
    // and not the FEATURE_SITES tag. The door showing but this gate refusing
    // was the "Sites are owner/fleet only" a standing tech used to hit: the
    // KVM itself admits the session's tech, the GUI must not refuse first.
    if (!this.sitesAllowed(node) && !this.kvmDoors(node)) {
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
      // No toast — the row now shows its localhost:<port> address inline.
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
    // No toast — the row now shows its localhost:<port> address inline.
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
   *  pasting into whatever client speaks it (a DB tool, an ssh command).
   *  Resolves true on success so the caller can flash an inline "Copied ✓" on
   *  the button itself; only a clipboard *failure* falls back to a toast. */
  async copySite(m: SiteMapping): Promise<boolean> {
    const url = this.siteUrl(m);
    try {
      await navigator.clipboard?.writeText(url);
      return true;
    } catch {
      this.toast("warn", `Reach it at ${url}`);
      return false;
    }
  }

  // ---- KVM appliance (the out-of-band screen/keyboard plane) ---------
  //
  // A KVM is a NanoKVM-class device: it carries its own web UI (a SiteAdvert)
  // reachable through the mesh proxy, and it's bound (attached) to the one
  // machine it physically controls. "Open KVM" maps + opens that web UI; the
  // Power/Reset feature buttons drive the KVM's GPIO endpoint through the same
  // tunnel (auth is bypassed over the mesh, so no token); Attach/Detach curate
  // the binding via a gated control message the KVM confirms by re-advertising.

  /** Whether `node` is a KVM appliance — it runs AllMyStuff and its presence
   *  advertises `FEATURE_KVM` (an older build never does). */
  isKvm(node: MeshNode | undefined): boolean {
    return !!node && isAppNode(node) && (node.features ?? []).includes(FEATURE_KVM);
  }

  /** Whether a KVM exposes the *native* remote-control console — it advertises
   *  a Display/Source "screen" graph capability (the firmware publishes one with
   *  an id ending `:screen`, media `display`, flow `source`, alongside its
   *  `:control` input sink). That capability is the console's video input, so
   *  its presence is the signal a KVM supports the mesh-native console. A
   *  web-only KVM has none, so its generic Remote Control stays hidden. */
  kvmHasNativeScreen(node: MeshNode | undefined): boolean {
    return (
      this.isKvm(node) &&
      this.capsOf(node!.id).some(
        (c) => c.id.endsWith(":screen") || (c.media === "display" && canSource(c.flow)),
      )
    );
  }

  /** The site serving a KVM's own web UI: the one whose id matches
   *  `node.kvm.web`, else the first web-scheme (http/https) site it exposes.
   *  Undefined when the KVM advertises no usable web site. */
  kvmWebSite(node: MeshNode | undefined): SiteAdvert | undefined {
    if (!node) return undefined;
    const sites = node.sites ?? [];
    const named = node.kvm?.web ? sites.find((s) => s.id === node.kvm!.web) : undefined;
    return named ?? sites.find((s) => siteIsWeb(s)) ?? undefined;
  }

  /** Whether the KVM is YOURS to curate — owner / co-fleet / shared, the rule
   *  for attach/detach and the drawer's fleet tools. The session doors (web
   *  Site, Power/Reset) are the wider `kvmDoors`: a help technician gets
   *  those, never this. */
  kvmAllowed(node: MeshNode | undefined): boolean {
    if (!this.isKvm(node) || this.isMe(node!.id)) return false;
    const ownerIsMe = !!node!.owner && this.isMe(node!.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node!.id);
    return ownerIsMe || coFleet || this.hasShareGrant(node, "sites");
  }

  /** Whether the KVM's session doors — its manufacturer web Site and the
   *  Power/Reset that ride the same tunnel — are yours to open: a KVM you
   *  curate (`kvmAllowed`), or one you hold live CEC standing toward (you
   *  dialed it, or its hand is up in your watched queue). The appliance is the
   *  real authority: it accepts a tunnel only from its owner, its fleet, or a
   *  tech it admitted for the live support session (admitted while asking or
   *  on dial, dropped on hangup) — so widening this gate can't grant access
   *  the KVM itself would refuse; it just stops the GUI refusing first. */
  kvmDoors(node: MeshNode | undefined): boolean {
    if (!this.isKvm(node)) return false;
    return this.kvmAllowed(node) || this.cecStanding(node!.id);
  }

  /** Session doors for a KVM reached *through* the ordinary customer computer
   *  it controls. Owners/fleet use their normal KVM authority; a support tech
   *  uses the live CEC standing toward the attached computer, which that
   *  computer renews as a short appliance-side grant only while control is
   *  actually active. */
  kvmPassthroughDoors(kvm: MeshNode | undefined, attached: MeshNode | undefined): boolean {
    if (!this.isKvm(kvm) || !attached || !kvm!.kvm?.attachedTo) return false;
    if (!sameMachine(kvm!.kvm!.attachedTo!, attached.id)) return false;
    return this.kvmAllowed(kvm) || this.cecStanding(attached.id);
  }

  /** The fleet-graph twin of a CEC help/directory row, when the machine that
   *  raised its hand (or was dialed) is a KVM we can also reach on the normal
   *  mesh. The CEC plane's beacon carries no site adverts, but the same
   *  appliance's graph presence does — this is the bridge that lets a
   *  raised-hand row offer the KVM's web Site (and anything else the graph
   *  knows) alongside the remote-control console. Undefined for an ordinary
   *  customer, a KVM outside our graph, or one whose KVM affordances aren't
   *  ours to use.
   *
   *  Authorization is the KVM's session doors (`kvmDoors`): ours to curate,
   *  or live CEC standing — a dialed customer, or a hand up in the watched
   *  queue right now (the hand-raise IS the ask, a beacon the customer
   *  deliberately lit). One gate for showing the door AND opening it, so the
   *  button can never render only for the tunnel to refuse; the KVM itself
   *  still authorizes every request by its own approved-tech set. */
  kvmTwin(cecNode: string | undefined): MeshNode | undefined {
    if (!cecNode) return undefined;
    const node = this.nodeByCanonical(cecNode);
    return this.kvmDoors(node) ? node : undefined;
  }

  /** Open a KVM's own web UI through the mesh — map its web site to a local
   *  port if it isn't already, then open `http(s)://localhost:<port>` in the
   *  system browser. In web mode (no backend) there's nothing to map, so the
   *  flow is just demoed via a toast. */
  async openKVM(nodeId: string) {
    const node = this.node(nodeId);
    const site = this.kvmWebSite(node);
    if (!node || !site) {
      this.toast("warn", `${node?.label ?? "This KVM"} hasn't published a web UI yet`);
      return;
    }
    if (!this.backendConnected) {
      this.toast("info", `Opening ${node.label}'s KVM console…`);
      return;
    }
    await this.mapSite(nodeId, site);
    const m = this.siteMappingFor(nodeId, site.id);
    if (!m) return; // mapSite already toasted the failure
    this.openSite(m);
  }

  /** Resolve and map a KVM's published web console to one local tunnel port. */
  private async kvmConsolePort(nodeId: string): Promise<number | null> {
    const node = this.node(nodeId);
    const site = this.kvmWebSite(node);
    if (!node || !site) return null;
    await this.mapSite(nodeId, site);
    return this.siteMappingFor(nodeId, site.id)?.localPort ?? null;
  }

  /** Call one KVM JSON endpoint outside the webview's CORS boundary. */
  private async kvmApi<T>(
    port: number,
    path: string,
    init?: { method?: string; body?: unknown; timeoutMs?: number },
  ): Promise<KvmApiOutcome<T>> {
    let out: KvmApiCallResult;
    try {
      out = await kvmApiCall(port, path, init);
    } catch (error) {
      return { rsp: null, timedOut: false, reason: errMsg(error) };
    }
    if (out.error) {
      return { rsp: null, timedOut: out.error.kind === "timeout", reason: out.error.message };
    }
    const body = out.body as { code?: unknown; msg?: unknown } | null;
    const envelope = !!body && typeof body === "object" && typeof body.code === "number";
    if (out.status === 401 || out.status === 403) {
      return { rsp: null, timedOut: false, reason: "The KVM refused mesh authorisation." };
    }
    if (out.status === 404) {
      return { rsp: null, timedOut: false, reason: "This KVM doesn't offer that." };
    }
    if (out.status < 200 || out.status >= 300) {
      const detail = envelope && typeof body!.msg === "string" ? `: ${body!.msg}` : "";
      return { rsp: null, timedOut: false, reason: `The KVM answered HTTP ${out.status}${detail}.` };
    }
    if (!envelope) {
      return { rsp: null, timedOut: false, reason: "The KVM sent an unexpected reply." };
    }
    return { rsp: body as KvmApiRsp<T>, timedOut: false, reason: null };
  }

  private kvmMsg(rsp: KvmApiRsp, fallback: string): string {
    return typeof rsp.msg === "string" && rsp.msg.trim() ? rsp.msg.trim() : fallback;
  }

  /** Wake or operate the physical power/reset lines on the machine connected
   *  to a KVM. Wake is deliberately a keyboard event, not a disguised short
   *  power press: the KVM's native mesh input sink turns the Shift tap into a
   *  real USB-HID report at the attached machine. */
  async kvmFeature(nodeId: string, action: "wake" | "short" | "long" | "reset") {
    const node = this.node(nodeId);
    if (!node) return;
    const label =
      action === "wake"
        ? "Wake"
        : action === "short"
          ? "Short press"
          : action === "long"
            ? "Long press"
            : "Reset";
    if (!this.backendConnected) {
      this.toast("info", `${label} sent via ${node.label}`);
      return;
    }

    if (action === "wake") {
      const source = matchEndpoint(this.catalog, this.localId, "input", "provide");
      const sink = matchEndpoint(this.catalog, nodeId, "input", "consume");
      if (!source || !sink) {
        this.toast("warn", `${node.label} hasn't published its keyboard path yet`);
        return;
      }

      const routeId = `route:${source.id}→${sink.id}`;
      const previous = this.routeStates[routeId]?.state;
      const alreadyWired = previous === "active" || previous === "offered" || previous === "accepted";
      let created = false;
      let shiftDown = false;
      const shift = (down: boolean): InputAction => ({
        kind: "key",
        key: "Shift",
        code: "ShiftLeft",
        down,
      });
      try {
        if (previous !== "active") {
          const offered = await connectRoute(source.id, sink.id, "input");
          if (!offered) throw new Error("the keyboard route could not be offered");
          created = !alreadyWired;

          const deadline = Date.now() + 8_000;
          while (this.routeStates[routeId]?.state !== "active" && Date.now() < deadline) {
            await this.refreshSession();
            const state = this.routeStates[routeId];
            if (state?.state === "rejected" || state?.state === "torn_down") {
              throw new Error(state.reason || "the KVM refused the keyboard route");
            }
            await new Promise<void>((resolve) => setTimeout(resolve, 100));
          }
          if (this.routeStates[routeId]?.state !== "active") {
            throw new Error("the keyboard route did not become active");
          }
        }

        await sendInput(routeId, shift(true));
        shiftDown = true;
        await new Promise<void>((resolve) => setTimeout(resolve, 80));
        await sendInput(routeId, shift(false));
        shiftDown = false;
        // Input and route control use separate mesh lanes. Give the HID release
        // time to land before tearing down a one-shot route so Shift can never
        // be left held on the attached machine.
        if (created) await new Promise<void>((resolve) => setTimeout(resolve, 200));
        this.toast("info", `Wake sent via ${node.label}`);
      } catch (error) {
        // If transport failed between key-down and key-up, make one last
        // release attempt before the route is removed. The KVM also clears its
        // HID state when the route closes, but the explicit release is faster.
        if (shiftDown) {
          try {
            await sendInput(routeId, shift(false));
            await new Promise<void>((resolve) => setTimeout(resolve, 200));
          } catch {
            // The teardown below is the final release path.
          }
        }
        this.toast("warn", `Couldn't wake via ${node.label}: ${errMsg(error)}`);
      } finally {
        if (created) await this.disconnect(routeId).catch(() => undefined);
      }
      return;
    }

    const port = await this.kvmConsolePort(nodeId);
    if (port === null) {
      this.toast("warn", `${node.label} hasn't published a web UI yet`);
      return;
    }
    const type = action === "reset" ? "reset" : "power";
    const duration = action === "long" ? 12_000 : 800;
    const { rsp, reason } = await this.kvmApi(port, "/api/vm/gpio", {
      method: "POST",
      body: { type, duration },
    });
    if (!rsp || rsp.code !== 0) {
      const detail = rsp ? this.kvmMsg(rsp, "the KVM declined") : reason;
      this.toast("warn", `Couldn't send ${label.toLowerCase()} via ${node.label}: ${detail}`);
      return;
    }
    this.toast("info", `${label} sent via ${node.label}`);
  }

  isKvmUpdating(nodeId: string): boolean {
    return !!this.kvmUpdating[canonicalNodeId(nodeId)];
  }

  /** Update KVM firmware when available; otherwise reboot the appliance. */
  async updateKvm(nodeId: string): Promise<void> {
    const key = canonicalNodeId(nodeId);
    if (this.kvmUpdating[key]) return;
    const node = this.node(nodeId);
    if (!node) return;
    if (!this.backendConnected) {
      this.toast("info", "Update sent to the KVM");
      return;
    }
    this.kvmUpdating = { ...this.kvmUpdating, [key]: true };
    try {
      const port = await this.kvmConsolePort(nodeId);
      if (port === null) {
        this.toast("warn", `${node.label} hasn't published a console yet`);
        return;
      }
      const version = await this.kvmApi<KvmVersion>(port, "/api/application/version");
      if (!version.rsp || version.rsp.code !== 0) {
        const detail = version.rsp
          ? this.kvmMsg(version.rsp, "the KVM declined")
          : version.reason;
        this.toast("warn", `Couldn't check the KVM firmware: ${detail}`);
        return;
      }
      const current = (version.rsp.data?.current ?? "").trim();
      const latest = (version.rsp.data?.latest ?? "").trim();
      if (latest && latest !== current) {
        this.toast("info", `Installing ${latest} on the KVM — this takes a few minutes`);
        const update = await this.kvmApi(port, "/api/application/update", {
          method: "POST",
          body: {},
          timeoutMs: 300_000,
        });
        if (update.timedOut) {
          this.toast("info", `The KVM is installing ${latest} and will restart when done`);
        } else if (!update.rsp || update.rsp.code !== 0) {
          const detail = update.rsp
            ? this.kvmMsg(update.rsp, "the KVM declined")
            : update.reason;
          this.toast("warn", `Couldn't update the KVM: ${detail}`);
        } else {
          this.toast("ok", `KVM updated to ${latest}; it is restarting`);
        }
        return;
      }
      const reboot = await this.kvmApi(port, "/api/vm/system/reboot", {
        method: "POST",
        body: {},
      });
      if (reboot.timedOut || reboot.rsp?.code === 0) {
        this.toast("info", `KVM is current${current ? ` (${current})` : ""}; rebooting it now`);
      } else {
        const detail = reboot.rsp
          ? this.kvmMsg(reboot.rsp, "the KVM declined")
          : reboot.reason;
        this.toast("warn", `KVM is current, but reboot failed: ${detail}`);
      }
    } finally {
      const { [key]: removed, ...rest } = this.kvmUpdating;
      void removed;
      this.kvmUpdating = rest;
    }
  }

  get wifiKvmLabel(): string {
    return this.node(this.wifiFor ?? "")?.label ?? "KVM";
  }

  async openKvmWifi(nodeId: string): Promise<void> {
    this.wifiFor = nodeId;
    this.wifiStatus = null;
    this.wifiScan = null;
    this.wifiScanSource = null;
    this.wifiHostCurrent = null;
    this.wifiHostNote = null;
    this.wifiError = null;
    this.wifiPort = null;
    await this.loadKvmWifi(nodeId);
  }

  closeKvmWifi(): void {
    this.wifiFor = null;
    this.wifiStatus = null;
    this.wifiScan = null;
    this.wifiScanSource = null;
    this.wifiHostCurrent = null;
    this.wifiHostNote = null;
    this.wifiError = null;
    this.wifiPort = null;
    this.wifiLoading = false;
    this.wifiScanning = false;
    this.wifiBusy = false;
  }

  async loadKvmWifi(nodeId: string): Promise<void> {
    if (!this.backendConnected) {
      this.wifiStatus = { supported: true, apMode: false, connected: false, ssid: null };
      this.wifiScan = demoWifiNetworks();
      this.wifiScanSource = "host";
      this.wifiHostCurrent = this.wifiScan[0]?.ssid ?? null;
      return;
    }
    this.wifiLoading = true;
    this.wifiError = null;
    try {
      const port = this.wifiPort ?? (await this.kvmConsolePort(nodeId));
      if (!port) {
        this.wifiError = "Couldn't reach this KVM's console.";
        return;
      }
      this.wifiPort = port;
      const { rsp, reason } = await this.kvmApi<KvmWifiStatusRaw>(port, "/api/network/wifi");
      if (!rsp || rsp.code !== 0 || !rsp.data) {
        this.wifiPort = null;
        this.wifiError = rsp
          ? this.kvmMsg(rsp, "The KVM couldn't report its Wi-Fi settings.")
          : (reason ?? "Couldn't read the KVM's Wi-Fi settings.");
        return;
      }
      this.wifiStatus = normalizeWifi(rsp.data);
      if (this.wifiStatus.supported) await this.scanKvmWifi(nodeId);
      await this.loadHostWifi();
    } finally {
      this.wifiLoading = false;
    }
  }

  async scanKvmWifi(nodeId: string): Promise<void> {
    if (!this.backendConnected) {
      this.wifiScan = demoWifiNetworks();
      return;
    }
    const port = this.wifiPort ?? (await this.kvmConsolePort(nodeId));
    if (!port) return;
    this.wifiPort = port;
    this.wifiScanning = true;
    try {
      const { rsp } = await this.kvmApi<{ wifiList?: KvmWifiNetwork[] }>(
        port,
        "/api/network/wifi/scan",
        { timeoutMs: 20_000 },
      );
      const list = rsp?.data?.wifiList;
      if (rsp?.code === 0 && Array.isArray(list)) {
        this.wifiScan = sortNetworks(list);
        this.wifiScanSource = "kvm";
      }
    } finally {
      this.wifiScanning = false;
    }
  }

  async rescanWifi(nodeId: string): Promise<void> {
    if (this.wifiBusy) return;
    if (this.wifiStatus?.supported) await this.scanKvmWifi(nodeId);
    if (this.wifiScanSource === "kvm") return;
    this.wifiScan = null;
    this.wifiScanSource = null;
    this.wifiScanning = true;
    try {
      await this.loadHostWifi();
    } finally {
      this.wifiScanning = false;
    }
  }

  private async loadHostWifi(): Promise<void> {
    let host: HostWifi;
    try {
      host = await hostWifiScan();
    } catch {
      return;
    }
    this.wifiHostCurrent = host.current;
    if (this.wifiScan === null && host.networks.length > 0) {
      this.wifiScan = sortNetworks(host.networks);
      this.wifiScanSource = "host";
    }
    this.wifiHostNote = this.wifiScan === null ? host.note : null;
  }

  async connectKvmWifi(nodeId: string, ssid: string, password: string): Promise<void> {
    const name = ssid.trim();
    if (!name || this.wifiBusy) return;
    this.wifiError = null;
    if (!this.backendConnected) {
      this.wifiStatus = { supported: true, apMode: false, connected: true, ssid: name };
      this.toast("ok", `Connected the KVM to ${name}`);
      return;
    }
    const port = this.wifiPort ?? (await this.kvmConsolePort(nodeId));
    if (!port) {
      this.wifiError = "Couldn't reach this KVM's console.";
      return;
    }
    this.wifiPort = port;
    this.wifiBusy = true;
    try {
      const { rsp, timedOut, reason } = await this.kvmApi(port, "/api/network/wifi/connect", {
        method: "POST",
        body: { ssid: name, password },
        timeoutMs: 40_000,
      });
      if (rsp?.code === 0) {
        this.toast("ok", `Connected the KVM to ${name}`);
        await this.loadKvmWifi(nodeId);
        void this.refreshNode(nodeId);
      } else if (rsp) {
        this.wifiError = password
          ? this.kvmMsg(rsp, "Couldn't connect. Check the network name and password.")
          : "This KVM won't accept an empty password, so it can't join an open network.";
      } else if (timedOut) {
        this.wifiError = "Sent the Wi-Fi details. The KVM may be reconnecting on its new network.";
        void this.refreshNode(nodeId);
      } else {
        this.wifiError = reason ?? "Couldn't send the Wi-Fi details.";
      }
    } finally {
      this.wifiBusy = false;
    }
  }

  promptDisconnectKvmWifi(nodeId: string, ssid: string | null): void {
    const network = ssid ? `\"${ssid}\"` : "the current network";
    if (window.confirm(`Disconnect this KVM from ${network}? It may go offline if Wi-Fi is its only connection.`)) {
      void this.disconnectKvmWifi(nodeId);
    }
  }

  async disconnectKvmWifi(nodeId: string): Promise<void> {
    if (this.wifiBusy) return;
    this.wifiError = null;
    if (!this.backendConnected) {
      this.wifiStatus = { supported: true, apMode: false, connected: false, ssid: null };
      return;
    }
    const port = this.wifiPort ?? (await this.kvmConsolePort(nodeId));
    if (!port) {
      this.wifiError = "Couldn't reach this KVM's console.";
      return;
    }
    this.wifiPort = port;
    this.wifiBusy = true;
    try {
      const { rsp, timedOut, reason } = await this.kvmApi(port, "/api/network/wifi/disconnect", {
        method: "POST",
        timeoutMs: 20_000,
      });
      if (rsp?.code === 0) {
        await this.loadKvmWifi(nodeId);
      } else if (rsp) {
        this.wifiError = this.kvmMsg(rsp, "Couldn't disconnect the Wi-Fi.");
      } else if (timedOut) {
        this.wifiError = "Sent the disconnect request. The KVM may be reconnecting.";
      } else {
        this.wifiError = reason ?? "Couldn't disconnect the Wi-Fi.";
      }
    } finally {
      this.wifiBusy = false;
    }
  }

  /** Point a KVM at the machine it controls — bind `nodeId` (the KVM) to
   *  `target`. The KVM confirms by re-advertising its new binding; a delivery
   *  failure surfaces so the ask never silently hangs. In web mode it's
   *  simulated on the demo graph so the flow is demoable. */
  async attachKVM(nodeId: string, target: string) {
    const node = this.node(nodeId);
    if (!node || !this.isKvm(node)) return;
    if (!this.kvmAllowed(node)) {
      this.toast("warn", `KVM controls are owner/fleet only — ${node.label} isn't yours`);
      return;
    }
    if (this.backendConnected) {
      kvmAttach(nodeId, target).catch((e) => {
        this.toast("warn", `Couldn't point ${node.label}: ${String(e)}`);
      });
      const tlabel = this.node(target)?.label ?? "that machine";
      this.toast("info", `Pointing ${node.label} at ${tlabel}…`);
    } else {
      // Demo/web: mirror BOTH the binding and the KVM-<label> rename (in live
      // mode the device does the rename and the GUI learns it from the
      // re-advertised presence — a mechanism the demo lacks), so requirement 7
      // is visible in the preview.
      const tlabel = this.node(target)?.label;
      node.kvm = { ...(node.kvm ?? {}), attachedTo: target };
      if (tlabel) node.label = `KVM-${tlabel}`;
    }
  }

  /** Clear a KVM's binding — the deliberately-buried, confirm-gated action
   *  (detaching strips a machine of its out-of-band screen/keyboard). The KVM
   *  confirms by re-advertising the cleared binding. Simulated in web mode. */
  async detachKVM(nodeId: string) {
    const node = this.node(nodeId);
    if (!node || !this.isKvm(node)) return;
    if (!this.kvmAllowed(node)) {
      this.toast("warn", `KVM controls are owner/fleet only — ${node.label} isn't yours`);
      return;
    }
    if (this.backendConnected) {
      kvmDetach(nodeId).catch((e) => {
        this.toast("warn", `Couldn't detach ${node.label}: ${String(e)}`);
      });
      this.toast("info", `Detaching ${node.label}…`);
    } else if (node.kvm) {
      // Demo/web: clear the binding and revert the KVM-<label> rename back to
      // the appliance's own name, matching the device's detach behavior.
      node.kvm = { ...node.kvm, attachedTo: undefined };
      if (node.label.startsWith("KVM-")) node.label = "CEC-KVM";
    }
  }

  /** The machines a KVM can be pointed at — your own devices and fleet
   *  members (the binding only makes sense for a machine you control), minus
   *  the KVM itself. The picker lists these. */
  kvmAttachTargets(kvmId: string): MeshNode[] {
    return this.catalog.nodes.filter(
      (n) => isAppNode(n) && !sameMachine(n.id, kvmId) && !this.isKvm(n) && this.isMyDevice(n.id),
    );
  }

  /** The target the attach picker defaults to — the KVM's owner when it's one
   *  of your machines, else the first candidate. Undefined when there are no
   *  candidates. */
  kvmDefaultTarget(kvmId: string): string | undefined {
    const targets = this.kvmAttachTargets(kvmId);
    if (targets.length === 0) return undefined;
    const kvm = this.node(kvmId);
    const owner = kvm?.owner ? targets.find((t) => sameMachine(t.id, kvm.owner!)) : undefined;
    return (owner ?? targets[0]).id;
  }

  /** The KVM (if any) currently attached to `nodeId` — scan the catalog for a
   *  KVM node whose binding points here, so a controlled machine can show
   *  "Controlled by KVM <label>" with its own Open-KVM affordance. */
  kvmAttachedTo(nodeId: string): MeshNode | undefined {
    return this.catalog.nodes.find(
      (n) => this.isKvm(n) && !!n.kvm?.attachedTo && sameMachine(n.kvm.attachedTo, nodeId),
    );
  }

  /** The catalog node a KVM's binding points at, resolved canonically —
   *  `attachedTo` may be a bare pubkey while the catalog keys display ids,
   *  so an exact `node()` lookup can miss. Drives the tether wire and the
   *  drawer's "Controls X" line. */
  kvmTargetNode(kvm: MeshNode | undefined): MeshNode | undefined {
    const target = kvm?.kvm?.attachedTo;
    if (!target) return undefined;
    return this.catalog.nodes.find((n) => sameMachine(n.id, target));
  }

  /** Whether you may curate THIS KVM's mesh memberships / unclaim it — an
   *  owner-authority act on the KVM's own fleet. The device itself obeys any
   *  fleet co-member, but membership and adoption are the owner's calls, so
   *  the UI offers them only when you actually own this device: either it's
   *  directly yours (`owner` is me) or it's in your fleet and you're that
   *  fleet's owner. A KVM merely *shared* with you never qualifies — offering
   *  a shelf the device would refuse is just a confusing dead end. */
  kvmOwnerControls(node: MeshNode | undefined): boolean {
    if (!this.isKvm(node) || this.isMe(node!.id)) return false;
    const ownerIsMe = !!node!.owner && this.isMe(node!.owner);
    const coFleetAndIOwn =
      this.isFleetMember(this.localId) && this.isFleetMember(node!.id) && this.isFleetOwner;
    return ownerIsMe || coFleetAndIOwn;
  }

  /** Walk a KVM onto another mesh. The KVM validates, refuses its own fleet
   *  mesh, joins, and confirms by re-advertising `kvm.meshes` — same model as
   *  attach. Simulated on the demo graph in web mode. */
  async kvmAddMesh(nodeId: string, networkId: string) {
    const node = this.node(nodeId);
    if (!node || !this.isKvm(node)) return;
    if (!this.kvmOwnerControls(node)) {
      this.toast("warn", `Mesh membership is fleet-owner only — ${node.label} isn't yours to move`);
      return;
    }
    const id = networkId.trim().toLowerCase();
    if (id.length < 3 || id.length > 64 || !/^[a-z0-9-_]+$/.test(id)) {
      this.toast("warn", "A mesh name is 3–64 letters, digits, hyphens or underscores");
      return;
    }
    if (node.kvm?.meshes?.includes(id)) {
      this.toast("info", `${node.label} is already on ${id}`);
      return;
    }
    if (this.backendConnected) {
      kvmMeshAdd(nodeId, id).catch((e) => {
        this.toast("warn", `Couldn't add ${node.label} to ${id}: ${String(e)}`);
      });
      this.toast("info", `Asking ${node.label} to join ${id}…`);
    } else {
      node.kvm = { ...(node.kvm ?? {}), meshes: [...(node.kvm?.meshes ?? []), id].sort() };
    }
  }

  /** Take a KVM off a mesh. The fleet mesh is refused here and on the device
   *  (that membership is governed by the fleet key — unclaim is the way out). */
  async kvmRemoveMesh(nodeId: string, networkId: string) {
    const node = this.node(nodeId);
    if (!node || !this.isKvm(node)) return;
    if (!this.kvmOwnerControls(node)) {
      this.toast("warn", `Mesh membership is fleet-owner only — ${node.label} isn't yours to move`);
      return;
    }
    if (this.kvmMeshIsFleet(networkId)) {
      this.toast("warn", "The fleet mesh can't be removed — unclaim the device instead");
      return;
    }
    if (this.backendConnected) {
      kvmMeshRemove(nodeId, networkId).catch((e) => {
        this.toast("warn", `Couldn't remove ${node.label} from ${networkId}: ${String(e)}`);
      });
      this.toast("info", `Asking ${node.label} to leave ${networkId}…`);
    } else if (node.kvm?.meshes) {
      node.kvm = { ...node.kvm, meshes: node.kvm.meshes.filter((m) => m !== networkId) };
    }
  }

  /** Whether a mesh id (from a KVM's advertised membership list) is your
   *  fleet's own mesh — the one row that gets the "fleet" badge and no ✕. */
  kvmMeshIsFleet(networkId: string): boolean {
    return !!this.fleetNetworkId && this.fleetNetworkId === networkId;
  }

  /** Unclaim a KVM — the owner's factory reset of its mesh identity. Runs the
   *  fleet eviction (signed governance + a direct Release), so the device
   *  forgets its owner and fleet, returns to its own joining mesh
   *  (`kvm.joiningMesh`, the name on its screen), and offers itself for
   *  adoption again. Routed through the governance helper so an MFA-enrolled
   *  fleet prompts for the custody code. */
  async unclaimKVM(nodeId: string) {
    const node = this.node(nodeId);
    if (!node || !this.isKvm(node)) return;
    if (!this.kvmOwnerControls(node)) {
      this.toast("warn", `Unclaiming is fleet-owner only — ${node.label} isn't yours`);
      return;
    }
    const where = node.kvm?.joiningMesh;
    if (this.backendConnected) {
      // The success toast rides INSIDE the governance action so it fires only
      // when the eviction actually ran — not when runFleetGov merely opened
      // the MFA prompt (user may cancel) or when fleetKick threw (runFleetGov
      // warns on that). Otherwise "is resetting" would lie.
      await this.runFleetGov(`Unclaim ${displayName(node)}`, async (code) => {
        await fleetKick(nodeId, code);
        this.toast(
          "info",
          where
            ? `${node.label} is resetting — it'll reappear claimable on ${where}`
            : `${node.label} is resetting to its joining mesh`,
        );
      });
      return;
    }
    // Demo/web: mirror the reset on the demo graph — including dropping it
    // from the fleet roster (via the same helper the fleet-kick demo uses), so
    // it actually reads as claimable afterward instead of staying "in fleet".
    node.owner = null;
    node.claimable = true;
    node.kvm = {
      ...(node.kvm ?? {}),
      attachedTo: undefined,
      meshes: where ? [where] : undefined,
    };
    if (this.ownedFleet) {
      this.ownedFleet = {
        ...this.ownedFleet,
        version: this.ownedFleet.version + 1,
        members: this.ownedFleet.members.filter((m) => !sameMachine(m.device, nodeId)),
      };
      this.reconcileFleetRelationships();
    }
    this.toast("info", `${node.label} unclaimed — it's offering itself for adoption again`);
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
      // No toast — the claimable card gives way and the device joins your fleet
      // band on the graph (and the Fleet roster) in front of you.
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
    // Preserve the roster's identity fields (name / is_owner / in_fleet /
    // network_id) — rebuilding as a bare {key,version,members} would drop
    // is_owner and network_id, silently killing the fleet-mesh lock and the
    // Meshes/Unclaim shelves in web-demo mode after the first claim.
    this.ownedFleet = {
      ...(this.ownedFleet ?? { is_owner: true, in_fleet: true }),
      key,
      version: (this.ownedFleet?.version ?? 0) + 1,
      members,
    };
  }

  /** Demo/web only: seed the fleet from the machines already marked yours, so
   *  the Fleet view isn't empty before you claim anything in the preview. A
   *  believable named fleet with this device as owner and one promoted
   *  co-owner, so the owner/promote/evict controls are all alive to try. */
  private seedDemoFleet() {
    const mine = this.catalog.nodes.filter((n) => n.relationship.kind === "mine");
    if (mine.length <= 1) return;
    const members: OwnedMember[] = mine.map((n) => ({
      device: n.id,
      label: n.label,
      // This device founded the fleet; one other is a promoted co-owner — the
      // rest are plain members, so Promote has somewhere to go.
      // This device's owner role is left unstamped on purpose — it reads as
      // owner via the roster's is_owner flag (the realistic backend shape).
      role: this.isMe(n.id) ? undefined : n.id === "desk" ? "owner" : n.id === "tv" ? "controller" : "member",
    }));
    this.ownedFleet = {
      key: "demo-fleet-key-7f3a91c2",
      name: "Nathan Paul",
      version: 1,
      members,
      is_owner: true,
      in_fleet: true,
      // Matches the demo KVM's advertised membership so its Meshes shelf
      // shows a locked "fleet" row, exactly like a real fleet would.
      network_id: "amber-turing-x3k9q",
    };
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

  /** This device's claims-over-the-public-mesh setting, straight from the
   *  owned payload. Device-local — never synced from a fleet. */
  publicClaims = $derived.by(() => this.ownedFleet?.public_claims === true);

  /** Flip this device's public-claims setting (the Fleet pane toggle).
   *  Optimistic — the backend confirms via the owned payload it re-emits. */
  async setPublicClaims(on: boolean) {
    if (this.backendConnected) {
      try {
        const now = (await setPublicClaims(on)) ?? on;
        if (this.ownedFleet) {
          this.ownedFleet = { ...this.ownedFleet, public_claims: now };
        }
        this.toast(
          now ? "info" : "ok",
          now
            ? "Claims over the public mesh are ON for this device"
            : "Claiming is local-network only on this device again",
        );
      } catch (e) {
        this.toast("warn", `Couldn't change the public-claims setting: ${errMsg(e)}`);
      }
      return;
    }
    // Demo/web: flip locally so the toggle behaves.
    if (this.ownedFleet) {
      this.ownedFleet = { ...this.ownedFleet, public_claims: on };
    }
  }

  /** Claim a remote device by the claim code shown on it. Slow by nature
   *  (joins a rendezvous network, waits for the device to appear) — the
   *  caller shows its own progress state; this resolves or throws with an
   *  actionable message. */
  async claimRemoteByCode(code: string): Promise<void> {
    if (!this.backendConnected) {
      this.toast("warn", "Remote claiming needs the desktop app");
      return;
    }
    await claimViaCode(code);
    this.toast("ok", "Device claimed — it's joining your fleet now");
  }

  /** Put *this* device into (or out of) claim mode so another of your
   *  machines can adopt it. */
  async setLocalClaimable(on: boolean) {
    // Resolve the *actual* local node (by its definitive marker, not just an id
    // match) so the optimistic flip lands on the node the graph is showing.
    const me =
      this.catalog.nodes.find((n) => n.kind === "this") ??
      this.catalog.nodes.find((n) => this.isLocalMachine(n.id));
    if (this.backendConnected) {
      try {
        // The backend is the authority: it refuses claim mode for a device
        // already in a fleet, so `now` is what actually took — report *that*,
        // not what was asked, so the toast can't claim something untrue.
        const now = (await setClaimable(on)) ?? on;
        if (me) me.claimable = now;
        if (on && !now) {
          this.toast("warn", "This device is in a fleet — leave it first to offer it for adoption.");
        } else {
          this.toast(
            now ? "info" : "ok",
            now ? "This device can now be adopted by another of your machines" : "Adoption turned off",
          );
        }
      } catch (e) {
        this.toast("warn", `Couldn't change claim mode: ${errMsg(e)}`);
      }
    } else {
      if (me) me.claimable = on;
      // No toast — the claim-mode toggle in the drawer reflects the new state.
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

  /** Whether `room` is exactly a private two-device room between this
   *  machine and `peer` — no third member can ever be folded into a DM. */
  private isDirectRoomWith(room: VirtualRoom, peer: string): boolean {
    const members = [...new Set(room.members.map(canonicalNodeId))];
    return (
      members.length === 2 &&
      members.some((m) => this.isMe(m)) &&
      members.some((m) => sameMachine(m, peer))
    );
  }

  /** Drop a duplicate DM locally without sending `close`: either participant
   *  may discover the duplicate, while Close remains the host's authority. */
  private removeMergedRoom(roomId: string) {
    if (this.isJoined(roomId)) this.unjoinRoom(roomId);
    else this.clearRoomShares(roomId);
    this.rooms = this.rooms.filter((r) => r.id !== roomId);
    const { [roomId]: _chat, ...chat } = this.roomChat;
    const { [roomId]: _unread, ...unread } = this.roomUnread;
    const { [roomId]: _presence, ...presence } = this.roomPresence;
    const { [roomId]: _knocks, ...knocks } = this.roomKnocks;
    this.roomChat = chat;
    this.roomUnread = unread;
    this.roomPresence = presence;
    this.roomKnocks = knocks;
  }

  /** Open the DM-like room for one external machine. Concurrent creation can
   *  leave each side hosting a copy; choose the same lowest room id on both
   *  ends, remove every duplicate locally, and send a merge statement so the
   *  peer repairs its own saved list too. */
  openDirectRoom(peerId: string) {
    const node = this.machineByAnyId(peerId);
    if (!node || !isAppNode(node) || this.isKvm(node) || this.isMe(node.id)) return;
    if (this.isCecCustomer(node.id)) {
      this.toast("warn", "CEC Support sessions stay separate from your personal rooms");
      return;
    }
    if (!this.roomsSupported(node)) {
      this.toast("warn", `${node.label} needs a newer AllMyStuff before it can use Rooms`);
      return;
    }

    const me = canonicalNodeId(this.localId);
    const peer = canonicalNodeId(node.id);
    let direct = this.rooms.filter((room) => this.isDirectRoomWith(room, peer));
    if (direct.length === 0) {
      const mine = this.node(this.localId)?.label?.trim() || "This device";
      const theirs = node.label.trim() || node.hostname?.trim() || shortId(peer);
      const room: VirtualRoom = {
        id: newRoomId(me),
        name: [mine, theirs].sort((a, b) => a.localeCompare(b)).join(" + "),
        members: [me, peer],
        owner: me,
        access: "invite",
      };
      this.rooms.push(room);
      this.saveRooms();
      this.broadcastRoom(room, this.inviteMessage(room));
      direct = [room];
    }

    direct.sort((a, b) => a.id.localeCompare(b.id));
    const keep = direct[0];
    const duplicates = direct.slice(1);
    if (duplicates.length > 0) {
      for (const duplicate of duplicates) this.removeMergedRoom(duplicate.id);
      this.saveRooms();
      if (this.backendConnected) {
        for (const duplicate of duplicates) {
          void roomSend([peer], {
            room: duplicate.id,
            name: keep.name,
            kind: "merge",
            into: keep.id,
            members: keep.members,
            access: this.roomAccess(keep),
          });
        }
      }
      // If we host the winner, re-state it too; this heals an invite the peer
      // missed while asleep before the merge arrived.
      if (this.isRoomHost(keep)) this.broadcastRoom(keep, this.inviteMessage(keep));
      this.toast("info", `Merged ${duplicates.length + 1} private rooms with ${node.label}`);
    }
    this.joinRoom(keep.id);
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
    // No toast — the invited machines appear in the room's roster immediately.
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
    // No toast — the member disappears from the room's People panel.
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
    // No toast — the new name shows in the room tile and panel header.
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
   *  with an id" to knock — to the clipboard. Resolves true on success so the
   *  caller can flash an inline "Copied ✓" on the button; only a failure toasts. */
  async copyRoomId(roomId: string): Promise<boolean> {
    try {
      await navigator.clipboard.writeText(roomId);
      return true;
    } catch {
      this.toast("warn", "Couldn't copy the join ID");
      return false;
    }
  }

  /** Join a room (or bring an already-joined one back on screen). On the
   *  desktop this opens the room's *dedicated OS window* — the call lives
   *  there, movable and full-screenable like a console window; re-joining
   *  focuses it. The web preview (and the room windows themselves, via
   *  [`AppStore.joinRoomHere`]) keep the call in-page. */
  joinRoom(roomId: string) {
    if (!this.rooms.some((r) => r.id === roomId)) return;
    if (isTauri() && !isMobile() && !roomWindowTarget()) {
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
    // Success shows inline (the self tile's live-mic badge); only warn if nobody
    // could receive it.
    if (wired === 0) this.toast("warn", "Nobody in the room can receive audio right now");
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
    // Success shows inline (the self tile's "sharing sound" badge).
    if (wired === 0) this.toast("warn", "Nobody in the room can receive audio right now");
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
    // Success shows inline (the self tile's "sharing screen" badge + banner).
    if (wired === 0) this.toast("warn", "Nobody in the room can receive a screen right now");
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
    // Success shows inline (the self tile's "camera live" badge).
    if (wired === 0) this.toast("warn", "Nobody in the room can receive camera video right now");
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
    // Success shows inline (the self tile's "control open" badge).
    if (wired === 0) this.toast("warn", "No member can send control right now");
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
    // No toast — the files appear in the room's Shared Files list immediately.
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
      case "merge": {
        // A DM merge is deliberately symmetric: either authenticated member
        // may converge duplicates, but only for the exact two-device pair and
        // only onto the deterministic lowest id. It cannot rewrite a group.
        const members = [...new Set(msg.members.map(canonicalNodeId))];
        const validPair =
          members.length === 2 &&
          members.some((m) => this.isMe(m)) &&
          members.some((m) => sameMachine(m, sender));
        if (!validPair || msg.into === msg.room) return;
        const candidates = this.rooms.filter((room) => this.isDirectRoomWith(room, sender));
        const sourceKnown = candidates.some((room) => room.id === msg.room);
        const targetKnown = candidates.some((room) => room.id === msg.into);
        if (!sourceKnown && !targetKnown) return;
        const ids = [...new Set([...candidates.map((room) => room.id), msg.into])].sort();
        if (ids[0] !== msg.into) return;

        let target = this.rooms.find((room) => room.id === msg.into);
        // Never let a peer repurpose an unrelated room whose id it happens to
        // know. A pre-existing target must already be this exact DM pair.
        if (target && !this.isDirectRoomWith(target, sender)) return;
        if (!target) {
          const owner = roomHostFromId(msg.into);
          if (!owner || (!this.isMe(owner) && !sameMachine(owner, sender))) return;
          target = {
            id: msg.into,
            name: msg.name?.trim() || "Private room",
            members,
            owner,
            access: msg.access,
          };
          this.rooms.push(target);
        }
        for (const room of [...this.rooms]) {
          if (room.id !== target.id && this.isDirectRoomWith(room, sender)) {
            this.removeMergedRoom(room.id);
          }
        }
        target.name = msg.name?.trim() || target.name;
        target.members = members;
        target.access = msg.access ?? target.access;
        this.saveRooms();
        if (this.isRoomHost(target)) this.broadcastRoom(target, this.inviteMessage(target));
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
    // No toast — the admitted machine moves from "Asking to join" into the
    // room's roster in the People panel.
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
      // One daemon outage would otherwise toast this from every poll — the
      // header's "mesh reconnecting…" state already names the one cause.
      if (this.meshStatus !== "disconnected") {
        this.toast("warn", `Couldn't load networks: ${errMsg(e)}`);
      }
    }
  }

  /** Set this device's display-name override (empty resets to the hostname).
   *  Resolves true on success so the pane can flash an inline "Saved ✓" on the
   *  button; only a failure toasts. */
  async setIdentityLabel(label: string): Promise<boolean> {
    try {
      await meshIdentitySetLabel(label);
      this.identity = { device_id: this.identity?.device_id ?? "", label };
      this.applyLocalLabel();
      return true;
    } catch (e) {
      this.toast("warn", `Couldn't set name: ${errMsg(e)}`);
      return false;
    }
  }

  /** Get onto a network by name. A blank name generates a memorable 5-word
   *  one. There's no separate "create": a network is just a name two devices
   *  agree on (the signaling handle is a hash of it), so joining a name nobody
   *  else is on *is* creating it. Typed names are canonicalized (lowercased,
   *  spaces → hyphens) so "Beach House" and "beach-house" meet on the same one. */
  /** The shareable invite for a mesh: the bare handle when it calls out at
   *  the Public venue only, or `handle#<venues>` when private venue(s) are
   *  in play — rendezvous needs both sides on the same relays, and a bare
   *  handle joined against the wrong venue never meets its mesh, with no
   *  error anywhere. The venue part is base64url of the same envelopes the
   *  Export/Import files use (`decodeInviteVenues` on the way in). */
  meshInvite(networkId: string): string {
    const extra = this.venuesForNetwork(networkId).filter((v) => !v.builtin);
    if (extra.length === 0) return networkId;
    return `${networkId}#${encodeInviteVenues(extra)}`;
  }

  async joinNetwork(rawName: string, venueIds: string[] = [PUBLIC_VENUE_ID]) {
    let typed = rawName.trim();
    // A compound invite carries the venue(s) the mesh calls out at: adopt
    // them (deduped against venues already here), fetch any remote one so
    // its servers are real before the join, and use exactly that venue set —
    // the whole point is landing on the sender's relays, not the picker's.
    const hash = typed.indexOf("#");
    if (hash > 0) {
      const envs = decodeInviteVenues(typed.slice(hash + 1));
      if (envs) {
        typed = typed.slice(0, hash).trim();
        const ids: string[] = [];
        const added: Venue[] = [];
        for (const env of envs) {
          const existing = this.venues.find((v) =>
            env.url
              ? v.url === env.url
              : !v.url &&
                sameServerSet(v.signaling, env.signaling_servers ?? []) &&
                sameServerSet(v.stun, env.stun_servers ?? []) &&
                sameServerSet(
                  v.turn.map((t) => t.url),
                  (env.turn_servers ?? []).map((t) => t.url),
                ),
          );
          if (existing) {
            ids.push(existing.id);
            continue;
          }
          const v = venueFromExport(env);
          added.push(v);
          ids.push(v.id);
        }
        if (added.length) {
          this.venues = [...this.venues, ...added];
          this.persistVenues();
          // A remote venue arrives as a pointer; fetch its servers now so
          // the join below writes real relays into the daemon config.
          for (const v of added) {
            if (v.url) await this.refreshVenue(v.id);
          }
        }
        venueIds = ids;
      } else {
        this.toast("warn", "That invite's venue part didn't parse — joining by name alone");
        typed = typed.slice(0, hash).trim();
      }
    }
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
      // No toast — the new mesh appears as a row in the Meshes list.
      await this.refreshNetworks();
    } catch (e) {
      this.toast("warn", `Couldn't ${typed ? "join" : "set up"} the network: ${errMsg(e)}`);
    }
  }

  /** Save a network's full settings (handle + signaling/STUN/TURN) to a JSON
   *  file you can hand to another device — the no-typing twin of "Copy id".
   *  Works for live or parked networks; pulls the full config if it isn't
   *  already loaded. */
  async exportNetwork(configId: string): Promise<boolean> {
    if (!this.backendConnected) {
      this.toast("info", "Exporting a network needs the desktop app");
      return false;
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
      return false;
    }
    try {
      // Bundle the mesh's custom venues so importing the file brings them too;
      // the built-in Public one is on every device already, so it's left out.
      const venues = this.venuesForNetwork(cfg.network_id).filter((v) => !v.builtin);
      const env = exportNetworkSettings(cfg, venues);
      const base = (env.label || env.network_id || "network").replace(/[^\w.-]+/g, "_").slice(0, 48);
      // No toast on success — the caller flashes an inline "Exported ✓".
      return !!(await exportNetworkFile(`${base}.network-settings.json`, env));
    } catch (e) {
      this.toast("warn", `Couldn't export the network: ${errMsg(e)}`);
      return false;
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
      // No toast — the imported mesh appears as a row in the Meshes list.
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
    // The local claiming mesh can't be left, only switched off — the node
    // refuses the remove too (and would re-join on the next ownership check).
    if (this.isLocalClaimMesh({ network_id: configId })) {
      this.toast("warn", "Local can't be left — switch it off instead.");
      return;
    }
    try {
      await meshNetworkRemove(configId);
      if (this.rosterNetwork === configId) {
        this.rosterNetwork = null;
        this.roster = [];
        this.livePeers = [];
      }
      // No toast — the mesh's row leaves the Meshes list (and its nodes drop
      // from the graph).
      await this.refreshNetworks();
      // Re-derive the graph now so the left network's nodes drop immediately,
      // rather than lingering until the next 3 s poll happens to run (matches
      // toggleNetworkEnabled / restartNetwork, which both re-sync on change).
      await this.syncMeshGraph();
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

  /** Refresh the live and parked mesh lists *together*, assigning both within
   *  one synchronous tick so a render can never catch them out of step. A
   *  toggle flips a mesh between the two lists; refreshing them as two separate
   *  awaited steps lets a render land in the gap — when enabling, the mesh is
   *  already in `networks` but not yet gone from `disabledNets`, so the keyed
   *  meshes menu sees it twice under the same network_id and throws
   *  `each_key_duplicate`. Fetch both, then assign back-to-back. */
  async refreshNetworkLists() {
    if (!isTauri()) return;
    try {
      const [nets, disabled] = await Promise.all([meshNetworks(), disabledNetworks()]);
      this.networks = nets ?? [];
      this.disabledNets = disabled ?? [];
    } catch (e) {
      if (this.meshStatus !== "disconnected") {
        this.toast("warn", `Couldn't load networks: ${errMsg(e)}`);
      }
    }
    if (this.rosterNetwork) await this.refreshRoster(this.rosterNetwork);
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
      // No toast — the mesh's switch flips and its row moves between the live
      // and disabled lists. Refresh both lists in one tick so the row is never
      // caught in both at once mid-swap (that duplicate key crashes the menu).
      await this.refreshNetworkLists();
      // Driving a mesh on turns its venues back on if any were off, and shimmers
      // the venues pill so the change is seen. Disabling never touches a venue —
      // other meshes may still ride it, and turning one off is the user's call.
      if (on) {
        const net = this.networks.find((n) => n.config_id === key || n.network_id === key);
        if (net && this.reactivateVenuesFor(net.network_id)) {
          await this.applyNetworkVenuesByWireId(net.network_id);
          this.shimmerVenuePill();
        }
      }
      await this.syncMeshGraph();
    } catch (e) {
      this.toast("warn", `Couldn't ${on ? "enable" : "disable"} the network: ${errMsg(e)}`);
    }
  }

  /** Reconnect the live network(s) **in place** — redial signaling and
   *  renegotiate ICE without leaving the room. The top bar's refresh control:
   *  a clean reconnect for when a network goes quiet (stuck handshaking, peers
   *  fallen silent). It acts on every currently-joined network, since the
   *  control is global.
   *
   *  This used to leave-and-rejoin each network (`setNetworkEnabled` off then
   *  on), which was needlessly destructive: a leave announces a departure and
   *  drops the local peer caches, so a refresh on *one* side stranded the
   *  *other* side on a stale session until it, too, refreshed (or rebooted).
   *  The in-place reconnect keeps every session and all app-level state, so
   *  the link comes back without taking the peer down with it. */
  async restartNetwork() {
    // Guard against a double-click while a reconnect is already playing out.
    if (this.restartFlow) return;
    const settle = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

    // The 3-step panel is the feedback now (it floats over the graph), so the
    // reconnect no longer narrates itself through toasts — only failures speak.
    const steps: { label: string; status: "wait" | "go" | "ok" }[] = [
      { label: "Refreshing", status: "wait" },
      { label: "Reconnecting", status: "wait" },
      { label: "Connected", status: "wait" },
    ];
    this.restartFlow = steps;
    const mark = (i: number, status: "wait" | "go" | "ok") => {
      steps[i].status = status;
      this.restartFlow = [...steps];
    };

    const joined = (Array.isArray(this.networks) ? this.networks : []).slice();
    let failed = 0;

    // Step 1 — kick an in-place reconnect on every live mesh: redial signaling
    // and renegotiate ICE with every peer, no leave, no teardown.
    mark(0, "go");
    await settle(420);
    if (this.backendConnected) {
      for (const n of joined) {
        try {
          await networkReconnect({ network: n.config_id || n.network_id });
        } catch (e) {
          failed++;
          this.toast("warn", `Couldn't reconnect ${networkDisplayName(n)}: ${errMsg(e)}`);
        }
      }
    }
    mark(0, "ok");

    // Step 2 — let the redial + renegotiation land, then re-sync the view.
    mark(1, "go");
    if (this.backendConnected) {
      await settle(700);
      await this.refreshNetworkLists();
      await this.syncMeshGraph();
    } else {
      // Demo/web: nothing live to cycle, but play the sequence so the panel is
      // a real, visible thing in the preview.
      await settle(700);
    }
    await settle(260);
    mark(1, "ok");

    // Step 3 — connected: the all-clear.
    mark(2, "go");
    await settle(280);
    mark(2, "ok");

    if (failed > 0) this.toast("warn", "Some meshes couldn't reconnect — open the meshes menu to retry");

    // Let the green "Connected" sit a beat, then fade the panel away.
    await settle(1000);
    this.restartFlow = null;
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
    const enabledNames = new Set(this.networks.map((n) => this.meshLabel(n)));
    for (const n of this.catalog.nodes) {
      if (n.kind === "this") {
        n.networks = [...enabledNames].sort();
        continue;
      }
      if (n.networks?.length) n.online = n.networks.some((name) => enabledNames.has(name));
    }
    // No toast — the mesh's switch flips and its row moves between lists.
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
      if (this.meshStatus !== "disconnected") {
        this.toast("warn", `Couldn't load network settings: ${errMsg(e)}`);
      }
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
  ): Promise<boolean> {
    const cfg = this.networkConfig(configId);
    if (!cfg) {
      this.toast("warn", "That network isn't loaded — reopen Settings");
      return false;
    }
    // The fleet's venue is owner-defined and owner-broadcast: members and
    // managers ride the owner's choice. Refuse here too, so no UI path (the
    // venues editor included) lets a non-owner change the fleet mesh's servers
    // — the owner's next broadcast would just overwrite it anyway.
    if (this.isFleetMesh(cfg) && !this.isFleetOwner) {
      this.toast("warn", "The fleet's venue is set by the fleet owner — you ride the owner's choice.");
      return false;
    }
    // The local claiming mesh has no servers to set — it's the LAN-only mDNS
    // passthrough for claiming and local pairing. The node refuses the write
    // too; refusing here keeps venue sweeps quiet instead of toasting errors.
    if (this.isLocalClaimMesh(cfg)) {
      this.toast("warn", "Local has no venue — it's LAN-only by design.");
      return false;
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
      // No toast — the Save button flashes "Saved ✓" inline.
      await this.loadNetworkConfigs();
      await this.refreshNetworks();
      return true;
    } catch (e) {
      this.toast("warn", `Couldn't save servers: ${errMsg(e)}`);
      return false;
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

  /** Whether a venue is currently on. On by default; only an explicit user
   *  switch-off (the off-list) turns it off. */
  isVenueActive(id: string): boolean {
    return !this.inactiveVenues.includes(id);
  }

  /** The merger of every live mesh's venues — the list the venues pill shows.
   *  Deduped by id, Public first (so the built-in anchors the list), the rest
   *  by label. This is "the venues across all your meshes" in one place. */
  meshVenues(): Venue[] {
    const seen = new Set<string>();
    const out: Venue[] = [];
    for (const n of Array.isArray(this.networks) ? this.networks : []) {
      // The local claiming mesh rides no venue (LAN-only) — don't let the
      // unmapped-defaults-to-Public rule count Public on its behalf.
      if (this.isLocalClaimMesh(n)) continue;
      for (const v of this.venuesForNetwork(n.network_id)) {
        if (!seen.has(v.id)) {
          seen.add(v.id);
          out.push(v);
        }
      }
    }
    return out.sort((a, b) => {
      if (a.id === PUBLIC_VENUE_ID) return -1;
      if (b.id === PUBLIC_VENUE_ID) return 1;
      return a.label.localeCompare(b.label);
    });
  }

  /** The on/off counts for the venues pill label. */
  venueCounts = $derived.by(() => {
    const all = this.meshVenues();
    const on = all.filter((v) => this.isVenueActive(v.id)).length;
    return { on, total: all.length };
  });

  /** Turn venues **on** — the single place the off-list shrinks. The pill, the
   *  venues library and assigning a venue to a mesh all route through it, so
   *  they're one system. Returns whether anything changed. */
  private activateVenues(ids: string[]): boolean {
    const set = new Set(ids);
    const next = this.inactiveVenues.filter((x) => !set.has(x));
    if (next.length === this.inactiveVenues.length) return false;
    this.inactiveVenues = next;
    saveInactiveVenues(this.inactiveVenues);
    return true;
  }

  /** Turn a venue **off** (add to the off-list) — only ever the user's action,
   *  never a side effect of driving a mesh. */
  private deactivateVenue(id: string) {
    if (this.inactiveVenues.includes(id)) return;
    this.inactiveVenues = [...this.inactiveVenues, id];
    saveInactiveVenues(this.inactiveVenues);
  }

  /** Flip a venue on or off across every mesh that uses it. Turning one off is
   *  the user's call (driving a mesh never does it); turning one on re-applies
   *  its servers. Re-deriving each affected mesh skips the fleet mesh unless
   *  you own it — its venue is owner-defined. */
  async toggleVenue(id: string, on: boolean) {
    if (on) this.activateVenues([id]);
    else this.deactivateVenue(id);
    // Re-apply to every live mesh that references this venue.
    for (const n of Array.isArray(this.networks) ? this.networks : []) {
      if (this.venuesForNetwork(n.network_id).some((v) => v.id === id)) {
        if (this.isFleetMesh(n) && !this.isFleetOwner) continue;
        await this.applyNetworkVenuesByWireId(n.network_id);
      }
    }
  }

  /** Driving a mesh on turns its venues back on if any were off (the user's
   *  off-switch is the only thing that turns one off). Returns whether anything
   *  was re-activated, so the caller can shimmer the venues pill. */
  private reactivateVenuesFor(networkId: string): boolean {
    return this.activateVenues(this.venuesForNetwork(networkId).map((v) => v.id));
  }

  /** Shimmer the venues pill briefly — used when driving a mesh re-activated a
   *  venue, so the change is visible without a toast. */
  private shimmerVenuePill() {
    this.venuePillShimmer = true;
    setTimeout(() => (this.venuePillShimmer = false), 1100);
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
    // No toast — the venue's row leaves the Venues list.
  }

  /** Re-fetch a remote venue's servers from its url, cache them, and re-apply
   *  to any mesh using it. Resolves true on success so the pane can flash an
   *  inline "Fetched ✓" on the button; only a failure toasts. */
  async refreshVenue(id: string): Promise<boolean> {
    const v = this.venueById(id);
    if (!v?.url) return false;
    try {
      const s = await fetchVenueServers(v.url);
      await this.saveVenue({ ...v, signaling: s.signaling, stun: s.stun, turn: s.turn, fetchedAt: Date.now() });
      return true;
    } catch (e) {
      this.toast("warn", `Couldn't reach ${v.label}: ${errMsg(e)}`);
      return false;
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
    // Assigning a venue to a mesh turns it on — the same off-list the pill
    // drives, so settings and the pill are one system — then apply through the
    // single active-filtered path (not a separate raw write) so both agree.
    this.activateVenues(venues.map((v) => v.id));
    await this.applyNetworkVenuesByWireId(cfg.network_id);
  }

  /** Re-apply a venue to every live mesh that uses it (after an edit). */
  private async reapplyVenue(venueId: string) {
    for (const [nid, ids] of Object.entries(this.networkVenues)) {
      if (ids.includes(venueId)) await this.applyNetworkVenuesByWireId(nid);
    }
  }

  /** Recompute + write a mesh's union, found by wire id. Only the **active**
   *  venues contribute — a venue the user switched off drops out of the union —
   *  but if that would leave the mesh with no servers at all, fall back to its
   *  full venue set rather than strand it offline. */
  private async applyNetworkVenuesByWireId(networkId: string) {
    // The local claiming mesh rides no venue — every venue sweep (toggles,
    // edits, drive-mesh reactivation) skips it silently. Without this, the
    // unmapped-defaults-to-Public rule would try to write Public's servers
    // onto it and the node would refuse with an error toast.
    if (this.isLocalClaimMesh({ network_id: networkId })) return;
    const cfg = this.networkConfigs.find((c) => c.network_id === networkId);
    if (!cfg) return;
    const all = this.venuesForNetwork(networkId);
    const active = all.filter((v) => this.isVenueActive(v.id));
    await this.updateNetworkServers(cfg.id, unionServers(active.length ? active : all));
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
      // No toast — the device moves from "Waiting for you" to "Approved".
      await this.refreshRoster(configId);
    } catch (e) {
      this.toast("warn", `Couldn't approve: ${errMsg(e)}`);
    }
  }

  async removeDevice(configId: string, deviceId: string) {
    try {
      await meshRosterRemove(configId, deviceId);
      // No toast — the device's row leaves the roster list.
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
    if (tab === "always_on") {
      void this.loadServiceStatus();
      void this.loadWindowBehavior();
      void this.loadAutostart();
      void this.loadDebugLogging();
    }
    if (tab === "cec") void this.loadCec();
  }

  // ---- CEC Support -------------------------------------------------
  //
  // The technician side lives here (this app is the technician): dial a
  // customer by number, list live sessions, disconnect. The customer-side
  // approve/deny/revoke flow is wired too so a build hosting can answer the
  // 3-choice prompt. All degrade to no-ops in web mode (the tauri helpers
  // guard on `isTauri`), so the tab stays interactive in the browser preview.

  /** Unlock (or re-lock) the secret CEC tab — the reveal gesture. Persists. */
  toggleCecTab() {
    this.cecEnabled = !this.cecEnabled;
    try {
      localStorage.setItem("ams.cec.enabled", this.cecEnabled ? "1" : "0");
    } catch {
      /* private mode — the unlock just doesn't persist */
    }
    if (this.cecEnabled) {
      this.settingsTab = "cec";
      this.settingsOpen = true;
      void this.loadCec();
    }
  }

  /** Persist the technician's Agent Name (sent with every dial). */
  setCecAgentName(name: string) {
    this.cecAgentName = name;
    try {
      localStorage.setItem("ams.cec.agentName", name);
    } catch {
      /* private mode — not persisted */
    }
  }

  /** The name to show for a dialed customer: the technician's private alias if
   *  they've set one, else the machine's own label, else a plain fallback. */
  cecCustomerName(c: CecPeer): string {
    const alias = this.cecAliases[c.number]?.trim();
    return alias || c.label?.trim() || "Customer";
  }

  /** Set (or, with an empty string, clear) the technician's private label for a
   *  customer, keyed by their number so it survives a restart. Persisted. */
  setCecAlias(number: string, alias: string) {
    const next = { ...this.cecAliases };
    const trimmed = alias.trim();
    if (trimmed) next[number] = trimmed;
    else delete next[number];
    this.cecAliases = next;
    try {
      localStorage.setItem(CEC_ALIAS_STORE_KEY, JSON.stringify(next));
    } catch {
      /* private mode — not persisted */
    }
  }

  /** The chat thread with one customer (their node id), oldest-first — empty
   *  until history or a live line lands. A plain getter so components can read
   *  `app.chatThread(peer)` reactively (it reads the `chatThreads` state). */
  chatThread(peer: string): CecChatMsg[] {
    return this.chatThreads[peer] ?? [];
  }

  /** Pull the stored history for a customer and fold it into the local thread,
   *  oldest-first. Null-tolerant like {@link loadCec}: a failed fetch keeps the
   *  last snapshot. Any live line that beat the history reply is preserved
   *  (merged + de-duplicated by id), so opening the window never drops a line
   *  that arrived a breath earlier. */
  async loadChatHistory(peer: string) {
    const msgs = await cecChatHistory(peer);
    if (!msgs) return;
    const seen = new Set(msgs.map((m) => m.id));
    const extra = (this.chatThreads[peer] ?? []).filter((m) => !seen.has(m.id));
    this.chatThreads = {
      ...this.chatThreads,
      [peer]: [...msgs, ...extra].sort((a, b) => a.ts - b.ts),
    };
  }

  /** Technician → customer: send a line. Appends it optimistically (so the
   *  thread updates the instant Enter is pressed), then calls the node and
   *  reconciles the temporary row to the node-assigned id/ts. The node also
   *  echoes the line back over `cec://chat`; {@link appendChat} dedupes that
   *  echo against the reconciled row so it shows once. */
  async sendChat(peer: string, text: string) {
    const body = text.trim();
    if (!body) return;
    const tempId = `local-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    this.appendChat(peer, {
      id: tempId,
      from: "technician",
      text: body,
      // UNIX **seconds**, to match the node's `ts` — the thread sorts by `ts`,
      // and a millisecond stamp here would misorder the optimistic bubble
      // against real (seconds) rows until the send reply reconciled it.
      ts: Math.floor(Date.now() / 1000),
    });
    const r = await cecChatSend(peer, body);
    if (!r?.id) return;
    const thread = this.chatThreads[peer] ?? [];
    // The echo can beat this reply and already have collapsed the optimistic
    // row (see appendChat) — only rewrite while our temp row is still pending,
    // and never mint a duplicate of the canonical id.
    if (thread.some((m) => m.id === tempId) && !thread.some((m) => m.id === r.id)) {
      this.chatThreads = {
        ...this.chatThreads,
        [peer]: thread.map((m) =>
          m.id === tempId ? { ...m, id: r.id, ts: r.ts } : m,
        ),
      };
    }
  }

  /** Append one line to a customer's thread, deduped by id (the sender gets its
   *  own line echoed back). A technician echo that arrives before its send call
   *  reconciled the optimistic copy collapses that still-pending copy in place
   *  rather than doubling it. An inbound customer line bumps the unread badge. */
  private appendChat(peer: string, msg: CecChatMsg) {
    let thread = this.chatThreads[peer] ?? [];
    if (thread.some((m) => m.id === msg.id)) return;
    if (msg.from === "technician") {
      const pending = thread.findIndex(
        (m) =>
          m.id.startsWith("local-") &&
          m.from === "technician" &&
          m.text === msg.text,
      );
      if (pending >= 0) {
        thread = thread.map((m, i) => (i === pending ? msg : m));
        this.chatThreads = { ...this.chatThreads, [peer]: thread };
        return;
      }
    }
    this.chatThreads = { ...this.chatThreads, [peer]: [...thread, msg] };
    if (msg.from === "client") {
      this.chatUnread = {
        ...this.chatUnread,
        [peer]: (this.chatUnread[peer] ?? 0) + 1,
      };
    }
  }

  /** Clear a customer's unread badge (their chat window is open/focused). */
  markChatRead(peer: string) {
    if (!this.chatUnread[peer]) return;
    const next = { ...this.chatUnread };
    delete next[peer];
    this.chatUnread = next;
  }

  /** Open (or focus) the pop-out chat window for a customer, priming its
   *  history first. Desktop pops a real OS window (`open_chat_window`); the web
   *  preview has no window to open, so this just loads the thread in place. */
  openChat(peer: string) {
    void this.loadChatHistory(peer);
    void openChatWindow(peer);
  }

  /** Chat with a customer, **ensuring they're connected first** — the same
   *  guarantee the Remote-Control button gives, so a technician can open a chat
   *  from any CEC surface (the sidebar, the Help queue, the CEC console) even
   *  before a session exists. Opens the chat window immediately (so it's on
   *  screen while the dial runs), then dials/reconnects the customer's device;
   *  an expired grant re-prompts, a live one auto-approves. On approval only the
   *  chat opens — the console is deliberately left closed (chat *before* remote
   *  control). If a session is already live this is just an open+focus, since
   *  the redial auto-approves instantly. */
  async chatWithCustomer(node: string) {
    this.openChat(node);
    await this.dialCecTarget({ node }, shortId(node), { chatOnly: true });
  }

  /** Pull the node's CEC status + grants + pending requests + dialed customers. */
  async loadCec() {
    // Each fetch can transiently fail while the node socket is busy (mid-dial,
    // console bring-up) — and loadCec re-runs on every cec://* event, exactly
    // those moments. A failed refresh keeps the last known snapshot: it must
    // never read as "everything's gone" and wipe the rendered lists (that's
    // what made a dialed customer's row vanish from the CEC tab mid-approval).
    const status = await cecStatus();
    if (status) this.cecStatusInfo = status;
    const grants = await cecGrants();
    if (grants) this.cecGrantList = grants;
    const pending = await cecPending();
    if (pending) this.cecRequests = pending;
    const dialed = await cecDialed();
    if (dialed) this.cecCustomers = dialed;
    else clientLog("[cec] dialed-list fetch failed — keeping the last snapshot");
    // The help queue: same failure discipline — null keeps the last snapshot.
    // The fetch itself is what first joins the global help room, so opening
    // the CEC tab is enough to start hearing the beacons.
    const waiting = await cecHelpList();
    if (waiting) this.cecHelpWaiting = waiting;
  }

  /** How many CEC surfaces (Help sidebar, CEC console) are on screen right now.
   *  The presence poll runs only while this is > 0 — see {@link watchCecPresence}. */
  private cecSurfaceWatchers = 0;

  /** A CEC surface is on screen and wants the dialed customers' live `online`
   *  state kept fresh while it's visible. Refcounted; returns an unwatch fn to
   *  call on destroy. Refreshes once immediately (so opening the surface reads
   *  live at once), then rides the existing 3s mesh poll — no extra timer, and
   *  nothing polls when no CEC surface is shown. The refresh is local IPC (it
   *  reads the daemon's already-maintained peer table); it puts nothing on the
   *  mesh. */
  watchCecPresence(): () => void {
    this.cecSurfaceWatchers += 1;
    if (this.cecSurfaceWatchers === 1) void this.refreshCecPresence();
    return () => {
      this.cecSurfaceWatchers = Math.max(0, this.cecSurfaceWatchers - 1);
    };
  }

  /** Lightweight poll refresh of the dialed customers' live `online` state,
   *  null-tolerant like {@link loadCec}. Separate from loadCec so the poll
   *  doesn't also re-fetch status/grants/pending — those change only on their
   *  own cec:// events, while `online` has no event and must be polled (else a
   *  customer that went offline stays "online" until an app restart). */
  private async refreshCecPresence() {
    const dialed = await cecDialed();
    if (dialed) this.cecCustomers = dialed;
  }

  /** Technician: answer a raised hand — dial the customer's device directly
   *  (the beacon carried its id). The headline path; no number typed. */
  async answerHelp(node: string, label?: string) {
    await this.dialCecTarget({ node }, label ?? "that customer");
  }

  /** Technician: dial a customer by the number they read out — the fallback
   *  for when the raised-hand list is too crowded to spot them. The node
   *  resolves the digits to a device on the support area (a raised hand
   *  first, else any area member whose number matches). On success the
   *  customer appears in the CEC tab's "Active connections" list. */
  async dialCec() {
    const number = this.cecNumberDraft.trim();
    if (!number) {
      this.toast("warn", "Enter the customer's number first");
      return;
    }
    if (await this.dialCecTarget({ number }, number)) this.cecNumberDraft = "";
  }

  /** Reconnect to a device already in the directory. Re-sends the connect
   *  request, so an **expired** grant re-prompts the customer (a still-valid
   *  one auto-approves) and the console opens on approval — the one-click
   *  reuse the durable directory is for. Targets the stored device id, so it
   *  never depends on the customer being reachable-by-number right now. */
  async reconnectCec(node: string) {
    await this.dialCecTarget({ node }, shortId(node));
  }

  /** Shared connect path for every CEC dial — a raised hand (`node`), the
   *  number fallback (`number`), or a directory reconnect (`node`). Arms the
   *  console to open the moment the customer approves (see the onCecSession
   *  handler) and refreshes the CEC lists. Returns whether the dial was
   *  accepted, so the caller can clear its input only on success. */
  private async dialCecTarget(
    target: { node: string } | { number: string },
    display: string,
    opts?: { chatOnly?: boolean },
  ): Promise<boolean> {
    if (!this.cecAgentName.trim()) {
      this.toast("warn", "Set your Agent Name first — the customer sees it");
      return false;
    }
    const byNode = "node" in target;
    this.cecDialing = true;
    this.cecDialingNumber = byNode ? null : target.number;
    // Which ROW is being dialed, for the help sidebar's inline status — a
    // by-node dial (Answer / Open / Chat) never sets `cecDialingNumber`, so
    // without this the sidebar had nothing to pin its "Connecting…" line to.
    this.cecDialingNode = byNode ? target.node : null;
    clientLog(`[cec] dialing ${display}…`);
    try {
      // Hard cap the wait so a wedged socket request can't leave cecDialing
      // stuck true forever (every Connect button then silently disabled until
      // restart). The UI must always get its state back.
      const agent = this.cecAgentName.trim();
      const r = await Promise.race([
        byNode ? cecDialNode(target.node, agent) : cecDial(target.number, agent),
        new Promise<never>((_, reject) =>
          setTimeout(() => reject(new Error("no answer from the node after 60s")), 60_000),
        ),
      ]);
      if (r?.node) {
        clientLog(`[cec] dial ok — customer node ${r.node}; waiting for approval`);
        this.toast("ok", `Connecting to ${display} — waiting for them to approve`);
        this.cecAutoOpenNode = r.node;
        this.cecAutoOpenChatOnly = opts?.chatOnly ?? false;
        void this.pullSessionSnapshot();
      } else {
        clientLog(`[cec] dial returned no node (web mode or empty reply)`);
        this.toast("ok", `Dialed ${display}`);
      }
      return true;
    } catch (e) {
      const msg = errMsg(e);
      clientLog(`[cec] dial FAILED — ${msg}`);
      if (msg.includes("cancelled") || msg.includes("canceled")) {
        this.toast("ok", "Stopped dialing");
      } else {
        this.toast("warn", `Couldn't reach ${display}: ${msg}`);
      }
      return false;
    } finally {
      this.cecDialing = false;
      this.cecDialingNumber = null;
      this.cecDialingNode = null;
      void this.loadCec();
    }
  }

  /** Stop the in-flight dial ("stop trying"): trips the node's cancel flag,
   *  which ends the discovery poll — the awaited dial then returns and the
   *  tab gets its state back. The attempt row stays in the directory. */
  async cancelCecDial() {
    clientLog("[cec] cancel requested");
    await cecCancelDial();
  }

  /** The inline wait status a CEC row should show for `node`: "dialing" while
   *  the connect RPC is in flight to that machine, "approval" once the dial
   *  landed and the ball is in the customer's court (their approve prompt is
   *  up), null when nothing is pending. The help sidebar renders this in the
   *  row itself — it has no status strip like the settings console panel. */
  cecWaitPhase(node: string | undefined): "dialing" | "approval" | null {
    if (!node) return null;
    if (this.cecAutoOpenNode && this.isSameMachine(this.cecAutoOpenNode, node)) {
      return "approval";
    }
    if (this.cecDialing && this.cecDialingNode && this.isSameMachine(this.cecDialingNode, node)) {
      return "dialing";
    }
    return null;
  }

  /** Flip the "Watch the help queue" opt-in, then re-read the node's status —
   *  membership is the saved state, so the toggle shows what actually took
   *  (and keeps showing it after a restart, no local flag needed). A failure
   *  says so out loud: a silent one leaves a checkbox that just snaps back,
   *  which reads as haunted rather than broken. */
  async setCecHelpWatch(on: boolean) {
    try {
      await cecHelpWatch(on);
    } catch (e) {
      this.toast(
        "warn",
        `Couldn't ${on ? "watch" : "stop watching"} the help queue: ${String(e)}`,
      );
    }
    const status = await cecStatus();
    if (status) this.cecStatusInfo = status;
    if (on) void this.loadCec();
    else this.cecHelpWaiting = [];
  }

  /** Open the console for a just-approved CEC customer the moment their
   *  machine is actually in the catalog. Retries the resolve for a few
   *  seconds (the peer sweep + presence can land a breath after the approval
   *  event), nudging a snapshot pull each beat; if they still haven't landed,
   *  say so instead of doing nothing. */
  private openCecConsoleWhenFound(anyId: string, tries = 12) {
    const found = this.machineByAnyId(anyId);
    if (found) {
      // Direct on purpose: this runs only off a session the customer just
      // approved — the one moment a CEC console may open without re-dialing.
      this.openCecConsoleDirect(found.id);
      return;
    }
    if (tries <= 0) {
      this.toast(
        "warn",
        "They approved, but their machine hasn't appeared on the graph yet — press Control again",
      );
      return;
    }
    void this.pullSessionSnapshot();
    setTimeout(() => this.openCecConsoleWhenFound(anyId, tries - 1), 500);
  }

  /** Stop waiting on an unanswered approval: quit the connect-request
   *  re-sends and disarm the console auto-open, so the row's badge clears. */
  async stopCecWait() {
    clientLog("[cec] stop-waiting requested");
    this.cecAutoOpenNode = null;
    await cecCancelDial();
    void this.loadCec();
  }

  /** Remove a directory row — drops the device from the graph/roster and the
   *  CEC directory. Every dialed row is device-keyed now, so this is just
   *  `forgetNode` on the customer's id. */
  async removeCecCustomer(node: string) {
    try {
      await forgetNode(node);
    } catch (e) {
      this.toast("warn", `Couldn't remove it: ${errMsg(e)}`);
    }
    void this.loadCec();
  }

  /** How many stored customers have gone stale (unused past
   *  {@link CEC_STALE_AFTER_S}) — the count on the "Remove stale" curate button. */
  get cecStaleCount(): number {
    const cutoff = Date.now() / 1000 - CEC_STALE_AFTER_S;
    return this.cecCustomers.filter((c) => c.last_used && c.last_used < cutoff).length;
  }

  /** Curate the directory in one action: forget every customer that's cycled
   *  out (gone stale), instead of picking them off one by one. Drops each from
   *  the graph, same as a single Remove — the shared area stays (it's home). */
  async removeStaleCec() {
    const cutoff = Date.now() / 1000 - CEC_STALE_AFTER_S;
    const stale = this.cecCustomers.filter((c) => c.last_used && c.last_used < cutoff);
    if (stale.length === 0) return;
    for (const c of stale) {
      if (!c.node) continue;
      try {
        await forgetNode(c.node);
      } catch {
        /* keep going — one failure shouldn't strand the rest */
      }
      this.catalog.nodes = this.catalog.nodes.filter((n) => !sameMachine(n.id, c.node));
      this.catalog.capabilities = this.catalog.capabilities.filter(
        (cap) => !sameMachine(cap.node, c.node),
      );
    }
    this.toast("ok", `Removed ${stale.length} stale ${stale.length === 1 ? "connection" : "connections"}`);
    void this.loadCec();
  }

  /** Customer: approve an inbound technician at a scope. */
  async approveCecRequest(req: CecPending, scope: CecScope) {
    try {
      await cecApprove(req.tech, scope, req.session_id, req.want_control);
      this.toast("ok", `Approved ${req.agent_name || "the technician"}`);
    } catch (e) {
      this.toast("warn", `Couldn't approve: ${errMsg(e)}`);
    }
    void this.loadCec();
  }

  /** Customer: decline an inbound technician. */
  async denyCecRequest(req: CecPending) {
    try {
      await cecDeny(req.tech, req.session_id);
    } catch (e) {
      this.toast("warn", `Couldn't decline: ${errMsg(e)}`);
    }
    void this.loadCec();
  }

  /** Customer: "Forget this technician" — revoke every grant and tear down. */
  async revokeCecTech(tech: string, agentName: string) {
    try {
      await cecRevoke(tech);
      this.toast("ok", `Forgot ${agentName || "the technician"}`);
    } catch (e) {
      this.toast("warn", `Couldn't revoke: ${errMsg(e)}`);
    }
    void this.loadCec();
  }

  /** The per-node gear "Forget this node": drop it from the graph + roster,
   *  tear its session down, end any CEC session. Removes it locally at once so
   *  the graph reacts immediately; the next snapshot confirms.
   *
   *  A FLEET MEMBER can't be merely forgotten: the signed roster is the
   *  authority, so a local-only forget gets gossiped straight back onto the
   *  graph — an eternity member. Forgetting one therefore PROPAGATES: it
   *  routes through the same owner/manager governance eviction the Fleet
   *  pane runs (MFA prompt included), and only a completed evict lets the
   *  local sweep proceed. If the evict is refused (not your fleet role) or
   *  cancelled at the prompt, nothing is half-forgotten. */
  async forgetNode(nodeId: string) {
    if (this.backendConnected && this.isFleetMember(nodeId) && !this.isMe(nodeId)) {
      const label = this.node(nodeId)?.label ?? "that device";
      await this.runFleetGov(`Forget ${label}`, async (code) => {
        await fleetKick(nodeId, code);
        await this.forgetNodeLocal(nodeId);
      });
      return;
    }
    await this.forgetNodeLocal(nodeId);
  }

  /** The local half of Forget — session/route teardown + catalog sweep.
   *  Fleet members reach this only after their eviction went through. */
  private async forgetNodeLocal(nodeId: string) {
    // Drop any live routes we hold to it, then the backend teardown.
    const routes = this.catalog.routes.filter(
      (r) => sameMachine(this.capNodeOf(r.from), nodeId) || sameMachine(this.capNodeOf(r.to), nodeId),
    );
    for (const r of routes) void this.disconnect(r.id);
    try {
      await forgetNode(nodeId);
    } catch (e) {
      this.toast("warn", `Couldn't forget it: ${errMsg(e)}`);
      return;
    }
    // Drop it (and any other view of the same machine) from the local graph.
    this.catalog.nodes = this.catalog.nodes.filter((n) => !sameMachine(n.id, nodeId));
    this.catalog.capabilities = this.catalog.capabilities.filter(
      (c) => !sameMachine(c.node, nodeId),
    );
    if (this.selectedNodeId && sameMachine(this.selectedNodeId, nodeId)) this.selectNode(null);
    this.toast("ok", "Forgot this node");
    void this.loadCec();
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
        // Reflect it now — clear the local fleet so the graph + drawer update
        // this instant, not on the next poll. The backend's empty roster lands
        // right after and confirms it. A freshly fleet-less device also stops
        // being claimable-blocked, so the make-claimable affordance returns.
        this.ownedFleet = null;
        this.reconcileFleetRelationships();
        // No toast — the Fleet pane drops to its "No fleet yet" state and the
        // graph regroups the devices you no longer co-own.
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
    // No toast — the Fleet pane drops to its "No fleet yet" state.
  }

  // ---- Danger Zone (Settings → Updates) ----
  // Each level reboots the whole stack after it clears state. The mesh daemon
  // is the real datastore, so a reset that only deletes state on one layer can
  // be undone by another layer's in-memory cache re-persisting it — the reboot
  // is what makes the wipe actually stick. `restartApp()` relaunches the app
  // (and the supervised node + daemon come back on clean state), so these calls
  // normally end by relaunching rather than returning.

  /** Leave the fleet, then reboot. */
  async dangerLeaveFleet() {
    await this.leaveFleet();
    await restartApp();
  }

  /** Leave the fleet and forget every network (keeps the device identity),
   *  then reboot. */
  async dangerResetNetworking() {
    await resetNetworking();
    await restartApp();
  }

  /** Factory reset — wipe identity, config, all networks and fleet ownership,
   *  then reboot into a brand-new device. */
  async dangerFactoryReset() {
    await factoryReset();
    await restartApp();
  }

  /** Evict a member from the fleet (owner-only). Routes through the governance
   *  helper, so it prompts for the custody code when fleet MFA is enrolled. */
  async kickFleetMember(device: string) {
    if (this.isMe(device)) {
      void this.leaveFleet();
      return;
    }
    const label =
      this.ownedFleet?.members.find((m) => sameMachine(m.device, device))?.label || "that device";
    if (this.backendConnected) {
      await this.runFleetGov(`Evict ${label}`, (code) => fleetKick(device, code));
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
    // No toast — the evicted device's row leaves the Fleet roster.
  }

  async loadUpdateStatus() {
    // The self-updater is desktop-only: on the phone/tablet the App Store
    // owns updates and the updater commands aren't registered, so never
    // invoke them there (the Settings pane shows a plain About instead).
    if (!isTauri() || isMobile()) return;
    try {
      this.updateInfo = await updateStatus();
      this.componentVersions = (await componentStatus())?.rows ?? [];
    } catch (e) {
      this.toast("warn", `Couldn't read update status: ${errMsg(e)}`);
    }
  }

  async repairComponent(component: string) {
    if (!isTauri()) return;
    this.componentBusy = component;
    try {
      await componentRepair(component);
      this.toast("info", "Repair finished. Rechecking installed versions…");
      // A backend/daemon repair may briefly restart the node socket.
      await new Promise((resolve) => setTimeout(resolve, 900));
      await this.loadUpdateStatus();
    } catch (e) {
      this.toast("warn", `Couldn't repair component: ${errMsg(e)}`);
    } finally {
      this.componentBusy = null;
    }
  }

  /** A background check reported in. Only the outcomes that mean "there is
   *  something newer than what you're running" are worth interrupting for —
   *  the routine up-to-date / not-due / disabled ticks refresh the Updates
   *  pane quietly and say nothing. */
  private applyUpdateChecked(o: CheckOutcome) {
    this.updateOutcome = o;
    void this.loadUpdateStatus();
    this.showUpdateNotice(o);
  }

  /** Keep an available update visible until the user acts or dismisses it. */
  private showUpdateNotice(o: CheckOutcome): boolean {
    if (o.outcome === "staged") {
      this.toast("info", `AllMyStuff ${o.version} is downloaded and ready.`, {
        persistent: true,
        tag: "update",
        actions: [
          { label: "Apply", action: "apply_update" },
          { label: "Restart & update", action: "relaunch_update", primary: true },
        ],
      });
      return true;
    }
    if (o.outcome === "manual_update_available") {
      this.toast("info", `${o.latest} is available. Update it the way AllMyStuff was installed.`, {
        persistent: true,
        tag: "update",
        actions: [{ label: "Update details", action: "open_updates", primary: true }],
      });
      return true;
    }
    if (o.outcome === "policy_blocked") {
      this.toast("info", `${o.latest} is available and waiting for your approval.`, {
        persistent: true,
        tag: "update",
        actions: [{ label: "Review update", action: "open_updates", primary: true }],
      });
      return true;
    }
    return false;
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
      const outcome = await updateCheck();
      if (outcome) this.applyUpdateChecked(outcome);
      this.updateInfo = (await updateStatus()) ?? this.updateInfo;
      // Routine outcomes stay in the Updates pane; an available release gets
      // the persistent inline-action notice in applyUpdateChecked.
    } catch (e) {
      this.toast("warn", `Update check failed: ${errMsg(e)}`);
    } finally {
      this.updateBusy = false;
    }
  }

  /** Apply a staged update to disk now. The swap lands immediately, but the
   *  running process keeps the old build until it relaunches — so we surface a
   *  "Relaunch now" prompt rather than claiming it's already live. */
  async applyUpdate() {
    if (!isTauri()) return;
    this.updateBusy = true;
    try {
      const r = await updateApply();
      if (r?.applied) this.updateApplied = r.applied;
      // No toast — the pane swaps to its "<version> is ready · Relaunch now"
      // block when applied; nothing staged simply leaves the block hidden.
      this.updateInfo = (await updateStatus()) ?? this.updateInfo;
    } catch (e) {
      this.toast("warn", `Couldn't apply update: ${errMsg(e)}`);
    } finally {
      this.updateBusy = false;
    }
  }

  /** Apply any staged update and relaunch straight into it — the one-click
   *  finish for both the "Relaunch & update" and "Relaunch now" buttons. The
   *  app restarts on success, so control only returns here on failure. */
  async relaunchUpdate() {
    if (!isTauri()) {
      this.toast("info", "Updates need the desktop app");
      return;
    }
    this.updateBusy = true;
    try {
      await updateRelaunch();
      // On success the process is already restarting; we won't reach here.
    } catch (e) {
      this.toast("warn", `Couldn't relaunch to update: ${errMsg(e)}`);
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

  // ---- "Always On": background service ------------------------------

  /** Read the OS background-service status for the Always On tab. */
  async loadServiceStatus() {
    if (!isTauri()) return;
    try {
      this.serviceInfo = await serviceStatus();
    } catch (e) {
      this.toast("warn", `Couldn't read service status: ${errMsg(e)}`);
    }
  }

  /** Run a service mutation (install/start/stop/restart/uninstall), then
   *  refresh status. Shared plumbing for the Always On buttons: it disables
   *  them while in flight, surfaces the CLI's output, and re-reads status so
   *  the pane reflects reality (important on Windows, where the elevated child
   *  reports only by exit code). */
  private async runServiceAction(
    label: string,
    action: () => Promise<ServiceActionResult>,
  ) {
    if (!isTauri()) {
      this.toast("info", "The background service needs the desktop app");
      return;
    }
    this.serviceBusy = true;
    try {
      const r = await action();
      if (!r.ok) {
        this.toast("warn", r.output || `Couldn't ${label} the service`);
      }
      // No toast on success — the Always On pane's status pill (Running/Stopped)
      // updates from loadServiceStatus() below.
    } catch (e) {
      this.toast("warn", `Couldn't ${label} the service: ${errMsg(e)}`);
    } finally {
      this.serviceBusy = false;
      await this.loadServiceStatus();
    }
  }

  installService() {
    return this.runServiceAction("install", serviceInstall);
  }
  startService() {
    return this.runServiceAction("start", serviceStart);
  }
  stopService() {
    return this.runServiceAction("stop", serviceStop);
  }
  restartService() {
    return this.runServiceAction("restart", serviceRestart);
  }
  uninstallService() {
    return this.runServiceAction("uninstall", serviceUninstall);
  }

  // ---- "Always On": development diagnostics -------------------------

  async loadDebugLogging() {
    if (!isTauri()) return;
    try {
      this.debugLoggingEnabled = await debugLoggingGet();
    } catch (e) {
      this.toast("warn", `Couldn't read debug logging setting: ${errMsg(e)}`);
    }
  }

  async setDebugLogging(enabled: boolean) {
    if (!isTauri()) return;
    const previous = this.debugLoggingEnabled;
    this.debugLoggingEnabled = enabled;
    try {
      this.debugLoggingEnabled = await debugLoggingSet(enabled);
      this.toast("info", "Debug logging setting saved; restart the app/backend to apply it.");
    } catch (e) {
      this.debugLoggingEnabled = previous;
      this.toast("warn", `Couldn't save debug logging setting: ${errMsg(e)}`);
    }
  }

  // ---- "Always On": window behaviour --------------------------------

  /** Read the persisted close/minimize-to-tray preference (backend-owned). */
  async loadWindowBehavior() {
    if (!isTauri()) return;
    try {
      this.windowBehavior = await windowBehaviorGet();
    } catch (e) {
      this.toast("warn", `Couldn't read window settings: ${errMsg(e)}`);
    }
  }

  /** Update one window-behaviour toggle and persist it via the backend. */
  async setWindowBehavior(patch: Partial<WindowBehavior>) {
    if (!isTauri()) return;
    const base = this.windowBehavior ?? {
      close_to_tray: true,
      minimize_to_tray: false,
      start_minimized: false,
    };
    const next = { ...base, ...patch };
    // Optimistic: reflect immediately, reconcile with the stored value.
    this.windowBehavior = next;
    try {
      const saved = await windowBehaviorSet(next);
      if (saved) this.windowBehavior = saved;
    } catch (e) {
      this.toast("warn", `Couldn't save window settings: ${errMsg(e)}`);
      void this.loadWindowBehavior();
    }
  }

  // ---- "Always On": start with computer -----------------------------

  /** Read whether the OS login item ("Start with computer") is registered. */
  async loadAutostart() {
    if (!isTauri()) return;
    try {
      this.autostartEnabled = await autostartGet();
    } catch (e) {
      this.toast("warn", `Couldn't read startup setting: ${errMsg(e)}`);
    }
  }

  /** Register / unregister the login item and reflect the resulting state. */
  async setAutostart(enabled: boolean) {
    if (!isTauri()) return;
    const prev = this.autostartEnabled;
    this.autostartEnabled = enabled; // optimistic
    try {
      this.autostartEnabled = await autostartSet(enabled);
    } catch (e) {
      this.autostartEnabled = prev;
      this.toast("warn", `Couldn't ${enabled ? "enable" : "disable"} Start with computer: ${errMsg(e)}`);
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
    if (!node || node.kind === "this" || !isAppNode(node) || this.isKvm(node)) return false;
    return isOlderVersion(node.version, this.latestRelease ?? undefined);
  }

  /** Whether a card can offer an actionable AllMyStuff update. Only this
   *  machine and our own fleet members may be updated; the receiver enforces
   *  the same ownership rule, but hiding unauthorized buttons keeps the graph
   *  honest before a request is sent. */
  canUpdateAllMyStuff(node: MeshNode | null | undefined): boolean {
    if (!node || !isAppNode(node) || this.isKvm(node)) return false;
    const standing = this.standingOf(node);
    if (!standing.self && !standing.mine) return false;
    const current = standing.self
      ? this.updateInfo?.current_version ?? node.version
      : node.version;
    const latest = this.latestRelease ?? this.updateOutcome?.latest ?? this.updateOutcome?.version;
    return isOlderVersion(current, latest);
  }

  /** The card's single Update action. Remote fleet machines take the existing
   *  signed upgrade path; this machine opens the updater's full status/policy
   *  pane so staged, blocked, and package-manager installs remain explicit. */
  updateAllMyStuff(nodeId: string) {
    const node = this.node(nodeId);
    if (!node) return;
    if (node.kind === "this" || this.isMe(nodeId)) {
      this.openSettings("updates");
      return;
    }
    this.upgradeRemote(nodeId);
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

  /** Refresh what we know about a node — re-learn its details, not the
   *  transport. For *this* device it re-scans the hardware (so newly
   *  plugged-in stuff, changed sites, etc. show up) and re-advertises the
   *  fresh picture to peers. For another node it nudges that machine to
   *  re-sync (ownership/fleet + its exposed sites) and re-pulls everything the
   *  daemon holds about it — capabilities, endpoints, summary, version,
   *  features, presence detail, fleet and shares — so its card, console
   *  options and shared/available stuff all reflect reality again. The refresh
   *  ring around the dot and the gear menu's Refresh both call this. */
  async refreshNode(nodeId: string) {
    const n = this.node(nodeId);
    if (!n) return;
    // Already spinning — the button is disabled, but guard re-entry from the
    // gear menu's "Refresh details" too.
    if (this.isRefreshing(nodeId)) return;
    if (!this.backendConnected) {
      // Demo/web: nothing live to re-pull; the catalog already reflects the
      // mock graph.
      this.toast("info", "Refreshing needs the desktop app");
      return;
    }
    const canon = canonicalNodeId(nodeId);
    this.beginRefresh(canon); // ring starts spinning, button disabled
    // The graph's "This device" card is authoritative even during the brief
    // startup window where the canonical local id is still converging. Its
    // Refresh button must always take the full local scan path, never fall
    // through to the peer/network path because `isMe` was temporarily stale.
    if (n.kind === "this" || this.isMe(nodeId)) {
      // Self: fully awaitable, so "done" is the rescan resolving.
      try {
        await requestNodeRefresh(); // backend: re-scan + re-advertise to peers
        await this.hydrateFromBackend(); // re-scan into our own catalog
        await this.loadSites();
        this.toast("info", "Rescanned this device");
      } finally {
        this.endRefresh(canon);
      }
      return;
    }
    // A peer: nudge it to re-sync, then re-pull the daemon's view of it. The
    // real round-trip is async — its fresh profile lands on a later poll — so
    // the spinner keeps going until `settleRefreshing` sees the details change
    // (or the timeout fires). Only a failed nudge stops it here.
    try {
      // Kick an in-place transport reconnect at this one node too — renegotiate
      // ICE on its link (and re-seed discovery if we'd lost it), the per-node
      // twin of the top bar's refresh. Non-destructive: no leave, no teardown.
      // Fire-and-forget so a quiet link doesn't hold up the detail re-pull.
      void networkReconnect({ peer: canon });
      await requestNodeRefresh(nodeId);
      await this.syncMeshGraph();
      await this.refreshSession();
      // Re-request its exposed sites (the backend gates this to managed peers;
      // a refusal is harmless).
      void siteRemoteList(nodeId);
      this.toast("info", `Refreshing ${n.label}…`);
    } catch {
      this.endRefresh(canon);
    }
  }

  /** Whether a refresh is in flight for `nodeId` — drives the spinning ring and
   *  the disabled button. Canonicalised so a display id and its bare pubkey
   *  resolve to the same in-flight entry. */
  isRefreshing(nodeId: string): boolean {
    return !!this.refreshInFlight[canonicalNodeId(nodeId)];
  }

  /** Start a node's refresh spinner: record the fingerprint we're refreshing
   *  away from and arm the give-up timer. */
  private beginRefresh(canon: string) {
    const prev = this.refreshInFlight[canon];
    if (prev) clearTimeout(prev.timer);
    const timer = setTimeout(() => this.endRefresh(canon), REFRESH_TIMEOUT_MS);
    this.refreshInFlight[canon] = { fp: this.nodeFingerprint(canon), timer };
  }

  /** Stop a node's refresh spinner and clear its timer. */
  private endRefresh(canon: string) {
    const entry = this.refreshInFlight[canon];
    if (!entry) return;
    clearTimeout(entry.timer);
    delete this.refreshInFlight[canon];
  }

  /** After a graph sync, stop the spinner on any peer whose details actually
   *  changed since the refresh started — the round-trip landed. Self refreshes
   *  resolve on their own await, so they're skipped here. */
  private settleRefreshing() {
    for (const canon of Object.keys(this.refreshInFlight)) {
      if (this.isMe(canon)) continue;
      const entry = this.refreshInFlight[canon];
      if (entry && this.nodeFingerprint(canon) !== entry.fp) this.endRefresh(canon);
    }
  }

  /** A compact signature of the facts a refresh re-learns about a node — its
   *  presence, app/version/features, hardware summary, owner, exposed sites and
   *  wireable capabilities. When this changes after a peer refresh, the
   *  round-trip has delivered fresh details and the spinner can stop. */
  private nodeFingerprint(nodeId: string): string {
    const n = this.nodeByCanonical(nodeId);
    if (!n) return "";
    const caps = this.capsOf(nodeId)
      .map((c) => c.id)
      .sort();
    return JSON.stringify({
      online: n.online,
      app: n.app ?? false,
      version: n.version ?? null,
      features: [...(n.features ?? [])].sort(),
      summary: n.summary ?? null,
      owner: n.owner ?? null,
      claimable: n.claimable ?? false,
      sites: n.sites ?? [],
      fleetName: n.fleetName ?? null,
      fleetOwner: n.fleetOwner ?? null,
      caps,
    });
  }

  /** Whether "Restart app" is offerable for `node`: your own machine (restart
   *  it locally) or one you may drive — owner/fleet — since the far side gates
   *  the restart on exactly that, the same rule as Upgrade. A guest's machine
   *  is never restartable from here. */
  canRestartApp(node: MeshNode | null | undefined): boolean {
    if (!node || !isAppNode(node)) return false;
    if (this.isMe(node.id)) return true;
    const ownerIsMe = !!node.owner && this.isMe(node.owner);
    const coFleet = this.isFleetMember(this.localId) && this.isFleetMember(node.id);
    return ownerIsMe || coFleet;
  }

  /** Restart a machine's AllMyStuff app. Your own machine relaunches locally;
   *  a fleet machine is asked over the mesh (owner/fleet enforced there), its
   *  next presence advert the confirmation. Heavier than a reconnect — the
   *  whole app comes down and back — so it's the gear menu's deeper recovery. */
  restartNodeApp(nodeId: string) {
    const n = this.node(nodeId);
    if (!n) return;
    if (!this.backendConnected) {
      this.toast("info", "Restarting the app needs the desktop app");
      return;
    }
    if (this.isMe(nodeId)) {
      this.toast("info", "Restarting AllMyStuff…");
      restartApp().catch((e) => this.toast("warn", `Couldn't restart: ${String(e)}`));
      return;
    }
    if (!this.canRestartApp(n)) {
      this.toast("warn", `${n.label} isn't yours to restart`);
      return;
    }
    restartNode(nodeId).catch((e) => {
      this.toast("warn", `Couldn't ask ${n.label} to restart: ${String(e)}`);
    });
    this.toast("info", `Asking ${n.label} to restart its app…`);
  }

  /** Whether "Restart device" is offerable for `node`: same owner/fleet rule
   *  as {@link canRestartApp} (the far side gates the reboot identically, then
   *  the OS's own privilege rules apply), plus the target must be a machine
   *  running AllMyStuff new enough to know the command — an older build
   *  ignores it, so offering the button would be a silent no-op there. */
  canRestartDevice(node: MeshNode | null | undefined): boolean {
    return this.canRestartApp(node);
  }

  /** Reboot a machine's whole OS — the gear menu's step past
   *  {@link restartNodeApp}, for the wedge an app relaunch can't clear. Your
   *  own device hands straight to the OS; a fleet machine is asked over the
   *  mesh. Its presence dropping off the graph and returning is the
   *  confirmation. */
  restartNodeDevice(nodeId: string) {
    const n = this.node(nodeId);
    if (!n) return;
    if (!this.backendConnected) {
      this.toast("info", "Restarting a device needs the desktop app");
      return;
    }
    if (!this.canRestartDevice(n)) {
      this.toast("warn", `${n.label} isn't yours to restart`);
      return;
    }
    restartDevice(nodeId).catch((e) => {
      this.toast("warn", `Couldn't ask ${n.label} to reboot: ${String(e)}`);
    });
    this.toast(
      "info",
      this.isMe(nodeId) ? "Restarting this device…" : `Asking ${n.label} to restart the device…`,
    );
  }

  /** A human one-liner for the last check's outcome, shown inline in the
   *  Updates pane (no toast). Null when there's nothing to add — the staged /
   *  ready blocks already cover the downloaded & applied cases, so this carries
   *  the outcomes that otherwise had no inline home. */
  checkOutcomeText(o: CheckOutcome | null): string | null {
    if (!o) return null;
    switch (o.outcome) {
      case "staged":
        return `Update ${o.version} downloaded — applies on next launch`;
      case "up_to_date":
        return "You're on the latest version";
      case "policy_blocked":
        return `${o.latest} is available but held by your auto-apply setting`;
      case "manual_update_available":
        return `${o.latest} is available, but this install can't update itself — update it the way you installed it`;
      case "package_manager":
        return "Installed via a package manager — update through it";
      case "disabled":
        return "Auto-update is off";
      case "not_due":
        return "Checked recently — try again shortly";
      default:
        return null;
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
    // Prefer the owner's *person* name straight from the device's presence
    // advert (the fleet-owner name every fleet device broadcasts). This labels
    // "Casey's fleet" by who actually owns it, without depending on the owner
    // device being present in our catalog — the old fallback below.
    const advertised = node.fleetOwner?.trim();
    if (advertised) return { id, name: advertised };
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

  /** Re-hydrate the durable shares the node persisted: any peer whose owner
   *  is a share partner on disk is reclassified *shared* with that person's
   *  grants, so a restart remembers what you shared instead of forgetting it
   *  and defaulting the peer to unclaimed. The node is the source of truth;
   *  this never overrides a device you own. */
  private applyDurableShares(shares: Share[]) {
    if (!shares.length) return;
    const byPerson = new Map(shares.map((s) => [s.person.id, s]));
    for (const n of this.catalog.nodes) {
      if (n.kind === "this" || this.isMe(n.id)) continue;
      if (n.relationship.kind === "mine") continue;
      // Match the node's *own* pubkey first, then its fleet/person. An inbound
      // share is keyed by the **sending device's** pubkey — the trust rule binds
      // a peer only to its own node — which is the device's own id, not its
      // fleet owner. Matching only by `personFor` (owner-based) meant a share
      // from a *non-owner* fleet device (a manager, say) never landed on that
      // device's node, so the receiver never reclassified it shared, never grew
      // the provide-grant the console buttons read, and never redrew. Checking
      // the node's own id first also gives it *its own* grants (device-scoped),
      // not a sibling's, when several of a fleet's devices share separately.
      const share =
        byPerson.get(`person:${canonicalNodeId(n.id)}`) ?? byPerson.get(this.personFor(n).id);
      if (share) {
        n.relationship = { kind: "shared", person: share.person, grants: [...share.grants] };
      }
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
      // Dedupe by the keys the Sharing list renders with (node id, grant id):
      // a grant is to the *person*, so the same grant id is recorded on each of
      // their devices — collecting them raw gives a duplicate key per grant and
      // crashes the keyed {#each} (`each_key_duplicate`). Show each device, and
      // each person-level grant, once.
      if (!p.nodes.some((x) => x.id === n.id)) p.nodes.push(n);
      for (const g of share.grants) {
        if (!p.grants.some((x) => x.grant.id === g.id)) p.grants.push({ node: n, grant: g });
      }
      map.set(share.person.id, p);
    }
    return [...map.values()].sort((a, b) => a.person.name.localeCompare(b.person.name));
  });

  /** Rescind the whole connection with a person: every node of theirs
   *  goes back to unclaimed, every grant (and any route riding one) goes
   *  with it. */
  stopSharingWith(personId: string) {
    for (const n of this.catalog.nodes) {
      if (n.relationship.kind === "shared" && n.relationship.person.id === personId) {
        n.relationship = { kind: "unclaimed" };
      }
    }
    void shareStop(personId).catch(() => {});
    this.reauthorize();
    // No toast — the partner drops off the Sharing list and their nodes leave
    // the shared band on the graph.
  }

  grant(nodeId: string, grant: Grant) {
    const n = this.node(nodeId);
    if (!n || n.relationship.kind !== "shared") return;
    const person = n.relationship.person;
    // De-dupe by (media, role, capability) across the *person* — a grant
    // authorizes them wherever it happens to be recorded.
    const exists = this.catalog.nodes.some(
      (x) =>
        x.relationship.kind === "shared" &&
        x.relationship.person.id === person.id &&
        x.relationship.grants.some(
          (g) =>
            g.media === grant.media && g.role === grant.role && g.capability === grant.capability,
        ),
    );
    if (!exists) {
      n.relationship.grants.push(grant);
      // Persist to the node — the durable source of truth, so the grant
      // survives a restart (no-op in web mode).
      void shareGrant(person, nodeId, grant).catch(() => {});
    }
  }

  revokeGrant(nodeId: string, grantId: string) {
    const n = this.node(nodeId);
    if (!n || n.relationship.kind !== "shared") return;
    const personId = n.relationship.person.id;
    // A grant is to the person, so it can be recorded on several of their nodes
    // (and the backend revoke is person-level). Pull it from every one of them,
    // not just the holder the row named — otherwise the Sharing list, which
    // shows person-level grants, would keep showing it from a sibling node until
    // the next backend snapshot reconciles.
    for (const x of this.catalog.nodes) {
      if (x.relationship.kind === "shared" && x.relationship.person.id === personId) {
        x.relationship.grants = x.relationship.grants.filter((g) => g.id !== grantId);
      }
    }
    void shareRevoke(personId, grantId).catch(() => {});
    this.reauthorize();
    // No toast — the grant row vanishes from the drawer / Sharing pane (and this
    // is called in a loop when reconciling a share, so a toast would flood).
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
  toast(
    kind: Toast["kind"],
    text: string,
    options: Pick<Toast, "persistent" | "tag" | "actions"> = {},
  ) {
    if (options.tag) this.toasts = this.toasts.filter((toast) => toast.tag !== options.tag);
    const id = ++seq;
    this.toasts.push({ id, kind, text, ...options });
    if (options.persistent) return;
    setTimeout(() => {
      this.toasts = this.toasts.filter((t) => t.id !== id);
    }, 3200);
  }

  dismissToast(id: number) {
    this.toasts = this.toasts.filter((toast) => toast.id !== id);
  }

  async runToastAction(
    id: number,
    action: "apply_update" | "relaunch_update" | "open_updates",
  ) {
    if (action === "open_updates") {
      this.openSettings("updates");
      this.dismissToast(id);
      return;
    }
    if (action === "apply_update") {
      await this.applyUpdate();
      if (this.updateApplied) {
        this.dismissToast(id);
        this.toast("info", `AllMyStuff ${this.updateApplied} is applied and ready to restart.`, {
          persistent: true,
          tag: "update",
          actions: [
            { label: "Restart now", action: "relaunch_update", primary: true },
          ],
        });
      }
      return;
    }
    await this.relaunchUpdate();
  }
}

function requestToGrant(req: GrantRequest): Grant {
  return {
    // Content-derived, stable id (mirrors Grant::scoped on the Rust side) so
    // the grant persists, de-dupes, and revokes by the same id on both ends.
    id: scopedGrantId(req.person, req.media, req.role, req.capability),
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
