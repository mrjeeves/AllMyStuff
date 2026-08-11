// Thin bridge to the Tauri backend. Everything here degrades gracefully
// when the app runs as a plain web page (no Tauri) — `pnpm dev` in a
// browser, this repo's CI build — so the graph is always interactive even
// without the Rust side or a running `myownmesh` daemon.

import type {
  Capability,
  CheckOutcome,
  ComponentStatus,
  FileEvent,
  Grant,
  RoomWireMessage,
  IdentityInfo,
  InputAction,
  InventorySummary,
  HostWifi,
  KvmApiCallResult,
  ListeningService,
  MediaKind,
  Person,
  Share,
  NetworkConfigFull,
  NetworkSummary,
  OwnedRoster,
  PeerInfo,
  RosterPeer,
  SharedFileMeta,
  SiteAdvert,
  SiteService,
  TermEvent,
  TerminalSessionInfo,
  UpdatePrefs,
  UpdateStatus,
  VideoFrameMsg,
} from "./types";

interface ScanResult {
  node_id: string;
  /** This machine's display name (hostname unless overridden). */
  label?: string;
  /** This machine's real hostname. */
  hostname?: string;
  summary: InventorySummary;
  capabilities: Capability[];
}

/** Live session snapshot from the backend: the peers presence has found
 *  and the routes currently negotiating/streaming. Mirrors the JSON the
 *  Rust `mesh::Mesh::snapshot` emits. */
export interface SessionSnapshot {
  ready: boolean;
  me?: string;
  network?: string | null;
  peers?: Array<{
    node: string;
    label: string;
    hostname?: string;
    summary: InventorySummary;
    capabilities: Capability[];
    /** From the peer's presence advert (Task 4): who owns it, and whether
     *  it's currently offering itself for adoption. */
    owner?: string | null;
    claimable?: boolean;
    /** App features the peer advertises ("terminal", …). Absent from an
     *  older peer — same as empty. */
    features?: string[];
    /** Sites the peer exposes for reverse-proxying (from its presence
     *  advert). Absent — an older peer, or one exposing nothing — is empty. */
    sites?: SiteAdvert[];
    /** KVM-appliance binding (from the peer's `NodeProfile.kvm`), present
     *  only when it advertises `FEATURE_KVM`. The Rust struct's fields are
     *  snake_case on the wire — `attached_to` / `web` / `joining_mesh` /
     *  `meshes` — so the store maps them onto the camelCase `MeshNode.kvm`.
     *  Absent on an ordinary peer. */
    kvm?: {
      attached_to?: string;
      web?: string;
      joining_mesh?: string;
      meshes?: string[];
      virtual_media?: { source: string; label: string; file: string };
    };
    /** The AllMyStuff version the peer is running, from its advert. Absent
     *  from an older peer — "unknown". */
    version?: string;
    /** The peer's fleet display name ("Casey"), from its presence advert —
     *  so the graph groups + labels its fleet without reconstructing the
     *  owner's name from the catalog. Absent (an older peer, or not in a
     *  fleet) — empty. */
    fleet_name?: string;
    /** The peer's fleet **owner** (person) name, from its presence advert —
     *  the human who owns the fleet, not the owner device's hostname. Absent
     *  (an older peer, or unknown) — empty. */
    fleet_owner?: string;
  }>;
  routes?: Array<{
    route: { id: string; from: string; to: string; media: MediaKind };
    peer: string;
    origin: "outbound" | "inbound";
    state: { state: string; reason?: string };
    /** For a terminal route: the resolved host-side shell session this route
     *  is attached to (multi-attach). Absent on non-terminal routes, and from
     *  an older peer. */
    term_session?: string | null;
    drive?: { label: string; mount: string } | null;
  }>;
  /** Durable share relationships (person + unioned grants) the node has on
   *  disk, so the GUI reclassifies a peer as *shared* with its grants across
   *  a restart. Absent — nothing shared yet — is empty. */
  shares?: Share[];
}

/** Networks the host computer can see. Used when a KVM cannot scan itself. */
export async function hostWifiScan(): Promise<HostWifi> {
  const out = await tryInvoke<HostWifi>("host_wifi_scan", {});
  return out ?? { supported: false, current: null, networks: [], note: null };
}

/** Call a KVM JSON endpoint through its mapped mesh site from Rust, avoiding
 * webview CORS and preflight restrictions. */
export async function kvmApiCall(
  port: number,
  path: string,
  opts?: { method?: string; body?: unknown; timeoutMs?: number },
): Promise<KvmApiCallResult> {
  if (!isTauri()) throw new Error("not available in the browser preview");
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<KvmApiCallResult>("kvm_api", {
    port,
    path,
    method: opts?.method,
    body: opts?.body,
    timeoutMs: opts?.timeoutMs,
  });
}

export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** Whether this build is the phone/tablet shell. The bundle is byte-identical
 *  across desktop and mobile, so the split is a runtime check: mobile drives
 *  one-window layouts (panels default closed, in-page views instead of
 *  popout windows) while desktop keeps its multi-window habits. iPadOS
 *  masquerades as a Mac in WKWebView user agents — the touch-point probe
 *  catches it. */
export function isMobile(): boolean {
  if (typeof navigator === "undefined") return false;
  const ua = navigator.userAgent;
  return (
    /Android|iPhone|iPad|iPod/i.test(ua) ||
    (/Macintosh/.test(ua) && navigator.maxTouchPoints > 2)
  );
}

// ---- app metadata -----------------------------------------------------
//
// The running build's version, for showing in the UI like MyOwnMesh /
// MyOwnLLM do. Tauri's source of truth is `gui/src-tauri/Cargo.toml`
// (kept in sync with `gui/package.json` by `scripts/bump-version.sh`), so
// this stays correct after every `just release`. Both helpers degrade to a
// no-op in web mode — the in-browser preview has no Tauri runtime.

/** The running app's version (e.g. "0.1.0"), or null in web mode. */
export async function appVersion(): Promise<string | null> {
  if (!isTauri()) return null;
  try {
    const { getVersion } = await import("@tauri-apps/api/app");
    return await getVersion();
  } catch (e) {
    console.warn("app version unavailable:", e);
    return null;
  }
}

/** Stamp the native window title so the running build is identifiable at a
 *  glance. No-op (and harmless) in web mode. */
export async function setWindowTitle(title: string): Promise<void> {
  if (!isTauri()) return;
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().setTitle(title);
  } catch (e) {
    console.warn("set window title failed:", e);
  }
}

async function tryInvoke<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T | null> {
  if (!isTauri()) return null;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    return (await invoke(cmd, args)) as T;
  } catch (e) {
    console.warn(`backend command ${cmd} failed:`, e);
    return null;
  }
}

/** Mirror one diagnostic line into the backend's `tracing` log so a
 *  desktop session's *frontend* decisions land in the same capturable
 *  stream (`ALLMYSTUFF_GUI_LOG`) as the Rust side — no webview devtools to
 *  juggle. Always echoes to the webview console too, so the in-browser
 *  preview (no Tauri) still shows it. Fire-and-forget: a diagnostic must
 *  never change behaviour or throw. */
export function clientLog(line: string): void {
  console.info(line);
  void tryInvoke("client_log", { line });
}

/** Scan this machine. Returns null in web mode; the caller keeps its demo
 *  data. `node_id` is the mesh device id once the session is up. */
export function scanSelf(): Promise<ScanResult | null> {
  return tryInvoke<ScanResult>("scan_self");
}

/** Offer a real connection over the mesh. Returns the route id, or null in
 *  web mode (the store falls back to a local route for the demo). A
 *  display or camera route advertises H.264 by default (the streaming
 *  side then uses the mesh's RTP track lane): decode is covered
 *  everywhere — WebCodecs where the webview has it, the backend's native
 *  openh264 decoder where it doesn't — and the backend still withholds
 *  the offer when the local daemon predates the track lane. MJPEG stays
 *  the floor both ends share, and `codec: "mjpeg"` forces it (the
 *  console's codec pill). */
export function connectRoute(
  from: string,
  to: string,
  media: MediaKind,
  codec?: "auto" | "h264" | "mjpeg",
  session?: string | null,
): Promise<string | null> {
  const video =
    (media === "display" || media === "video") && codec !== "mjpeg"
      ? ["h264"]
      : [];
  // `session` is the terminal multi-attach hook: a non-null id makes the
  // terminal Offer name an already-running host shell to attach to (shared,
  // tmux-style); null/undefined (and every non-terminal route) mints a fresh
  // one. Sent as null when absent so the backend's Option decodes cleanly.
  return tryInvoke<string>("connect_route", {
    from,
    to,
    media,
    video,
    session: session ?? null,
  });
}

/** The console's quality picks for a stream it's watching — each absent
 *  field means "automatic". The far end restarts its capture with these. */
export interface StreamTune {
  maxEdge?: number;
  bitrate?: number;
  fps?: number;
  /** Game mode: the latency-first posture (gradual intra-refresh instead
   *  of keyframe walls on capable streamers, 60 fps floor off-LAN, tight
   *  burst bounds). Absent/false = balanced. Kept for hosts that predate
   *  the named tri-state below. */
  game?: boolean;
  /** The stream posture by name. Balanced favors stability and quality;
   *  Game favors latency and instant recovery; Studio is the LAN
   *  fidelity mode — a high-bitrate quality-first encode for links with
   *  bandwidth to spend; Studio-Lossless is its top shelf — bit-exact
   *  HEVC on NVIDIA hardware, falling soft to Studio anywhere the rung
   *  can't run (old hosts read it as no named ask). */
  mode?: "balanced" | "game" | "studio" | "studio-lossless";
}

/** Ask the sender of `routeId` to stream with these picks. Best-effort:
 *  an old peer drops the ask and stays on automatic. */
export function tuneRoute(routeId: string, tune: StreamTune): Promise<null> {
  return tryInvoke("tune_route", {
    routeId,
    maxEdge: tune.maxEdge ?? null,
    bitrate: tune.bitrate ?? null,
    fps: tune.fps ?? null,
    game: tune.game ?? null,
    mode: tune.mode ?? null,
  });
}

/** The *effective* encode dials for a route — what the streamer is actually
 *  doing right now, beside the requested {@link StreamTune}. The AIMD loop
 *  (Game posture) moves `targetBitrateBps` at runtime; `outW`/`outH` are the
 *  real output geometry; `codec`/`encoderLabel` name the wire codec and the
 *  encoder rung. Bitrates are bits/s, `""` means "not reported yet". */
export interface RouteDials {
  posture: "balanced" | "game" | "studio" | "studio-lossless";
  encoderLabel: string;
  codec: string;
  targetBitrateBps: number;
  ceilingBps: number;
  fpsTarget: number;
  edgeCap: number;
  outW: number;
  outH: number;
}

/** Poll the effective encode dials for `routeId`. Returns null when this
 *  machine isn't the route's streamer (the ordinary remote-view case — the
 *  viewer surfaces its own measured actuals instead) or in web mode. Cheap
 *  and read-only; call ~1 Hz only while a quality panel is open. */
export function routeDials(routeId: string): Promise<RouteDials | null> {
  return tryInvoke<RouteDials>("route_dials", { routeId });
}

/** Flip the Experimental (Labs) tier on this node — the Mode dropdown's
 *  toggle. Omit `feature` for the whole tier; pass one (e.g. "damage") to
 *  flip a single experiment. GUI-internal, never wire-visible; a no-op in
 *  web mode. Every future Labs feature reads this gate backend-side, so
 *  no new control is ever needed. */
export function labsSet(on: boolean, feature?: string): Promise<null> {
  return tryInvoke("labs_set", { on, feature: feature ?? null });
}

/** Ask the sender of `routeId` for a clean decode entry (IDR) now — call
 *  from a decode-error handler. Rate-limited backend-side. */
export function refreshRoute(routeId: string): Promise<null> {
  return tryInvoke("video_refresh", { routeId });
}

/** Report this console's decode health for `routeId` back to its streamer
 *  (receiver → sender), so the streamer can adapt. Sent periodically;
 *  best-effort (an old streamer drops it). */
export function sendVideoFeedback(
  routeId: string,
  recvFps: number,
  decodeFails: number,
  queueDepth: number,
): Promise<null> {
  return tryInvoke("video_feedback", {
    routeId,
    recvFps,
    decodeFails,
    queueDepth,
  });
}

export function disconnectRoute(routeId: string): Promise<null> {
  return tryInvoke("disconnect_route", { routeId });
}

export async function mapNativeDrive(
  target: string,
  root: string,
  label: string,
  mount: string,
): Promise<string | null> {
  if (!isTauri()) return null;
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<string>("drive_map", { target, root, label, mount });
}

export async function mapNativeDriveFrom(
  source: string,
  root: string,
  label: string,
  mount: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("drive_map_from", { source, root, label, mount });
}

/** Share a folder on `source` — whichever machine of mine holds it — and get
 *  back the id it minted. The share builder pins its grant to that id; the
 *  path never leaves the machine that owns the disk. */
export async function shareFolderFrom(
  source: string,
  path: string,
  label: string,
): Promise<{ id: string; label: string } | null> {
  if (!isTauri()) return null;
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("folder_share_from", { source, path, label })) as {
    id: string;
    label: string;
  };
}

/** Open a folder someone shared with us as a native drive here. `mount` is
 *  ours to choose — it describes this desktop, not theirs. */
export async function openSharedFolder(
  source: string,
  folder: string,
  mount: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("folder_open", { source, folder, mount });
}

export async function stageKvmMedia(
  source: string,
  kvm: string,
  path: string,
  label: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("kvm_media_stage", { source, kvm, path, label });
}

export async function unmountKvmMedia(kvm: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("kvm_media_unmount", { kvm });
}

export async function pickKvmMediaImage(): Promise<string | null> {
  if (!isTauri()) return null;
  const { open } = await import("@tauri-apps/plugin-dialog");
  const picked = await open({
    multiple: false,
    directory: false,
    filters: [{ name: "Bootable disk images", extensions: ["iso", "img"] }],
  });
  return typeof picked === "string" ? picked : null;
}

/** Claim a device as yours. The claim only takes if that device is in claim
 *  mode; its next presence advert (owner = us) confirms it. Throws when the
 *  backend couldn't deliver the ask (device dropped offline, no shared
 *  network) so the UI can say so instead of waiting forever. No-op in web
 *  mode (the store simulates the claim on the demo graph). */
export async function claimNode(node: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("claim_node", { node });
}

/** Persist an outbound grant to a person so it survives a restart — what
 *  they may do with my stuff. The node is the durable source of truth; the
 *  next session snapshot reflects it. No-op in web mode (the store keeps the
 *  grant in its in-memory catalog). */
export async function shareGrant(
  person: Person,
  node: string,
  grant: Grant,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("share_grant", { person, node, grant });
}

/** Revoke a grant by its (content-derived) id from a person's durable share. */
export async function shareRevoke(
  person: string,
  grantId: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("share_revoke", { person, grantId });
}

/** Stop sharing with a person entirely — drop the whole durable record. */
export async function shareStop(person: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("share_stop", { person });
}

/** Ask one of your fleet machines to update its AllMyStuff to the channel's
 *  latest release and restart. The target enforces owner/fleet before acting;
 *  its next presence advert (the new version) confirms it landed. Throws when
 *  the backend couldn't deliver the ask (the machine dropped offline, no
 *  shared network). No-op in web mode. */
export async function upgradeNode(node: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("upgrade_node", { node });
}

/** Ask one of your fleet machines to restart its AllMyStuff app (relaunch onto
 *  the same build — no update). Owner/fleet enforced on the far side; its next
 *  presence advert confirms it came back. Throws when the ask couldn't be
 *  delivered (machine offline / no shared network). No-op in web mode. */
export async function restartNode(node: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("restart_node", { node });
}

/** Restart *this* machine's AllMyStuff app now (the local twin of
 *  {@link restartNode}). Never returns on the desktop — the app relaunches.
 *  No-op in web mode. */
export async function restartApp(): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("restart_app");
}

/** Reboot a machine's whole OS — the step past {@link restartNode} for the
 *  wedge an app relaunch can't clear. Your own device hands straight to the
 *  OS; a fleet machine is asked over the mesh (owner/fleet enforced there,
 *  the OS's own privilege rules after that). Its presence dropping and
 *  returning is the confirmation. No-op in web mode. */
export async function restartDevice(node: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("restart_device", { node });
}

/** Re-learn a node's details. `node` omitted = *this* device (re-scan its
 *  hardware + re-advertise); a peer id = nudge it to re-sync ownership/sites.
 *  The GUI follows up by re-pulling the daemon's view. No-op in web mode. */
export async function requestNodeRefresh(node?: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("refresh_node", node ? { node } : {});
}

/** Put this device into / out of claim mode so another of your machines can
 *  adopt it. Returns whether it's now claimable (null in web mode). */
export function setClaimable(claimable: boolean): Promise<boolean | null> {
  return tryInvoke<boolean>("set_claimable", { claimable });
}

/** Flip this device's claims-over-the-public-mesh setting. Strictly
 *  device-local (never fleet-synced; no remote peer can flip it). Returns
 *  the new value (null in web mode). */
export function setPublicClaims(on: boolean): Promise<boolean | null> {
  return tryInvoke<boolean>("set_public_claims", { on });
}

/** Claim a remote device by the claim code shown on it. Joins the code's
 *  randomized rendezvous network, claims the device, and leaves again —
 *  throws with an actionable message when nothing answered or the device
 *  declined. No-op in web mode. */
export async function claimViaCode(code: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("claim_via_code", { code });
}

/** Point a KVM appliance at the machine it controls — binds `node` (the KVM)
 *  to `target` (the graph node it's wired into). The KVM enforces owner/fleet
 *  before applying, then re-advertises presence with the new binding (the
 *  authoritative confirmation, exactly as a claim confirms). Throws when the
 *  backend couldn't deliver the ask so the UI can say so. No-op in web mode
 *  (the store simulates the binding on the demo graph). */
export async function kvmAttach(node: string, target: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("kvm_attach", { node, target });
}

/** Clear a KVM appliance's binding — it no longer represents any machine.
 *  Same delivery/confirmation model as {@link kvmAttach}. No-op in web mode. */
export async function kvmDetach(node: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("kvm_detach", { node });
}

/** Walk a KVM appliance onto another mesh — the fleet owner's membership
 *  tool. The KVM validates the id, refuses its own fleet mesh, joins, and
 *  re-advertises its membership list (`kvm.meshes`) — the authoritative
 *  confirmation, same model as {@link kvmAttach}. No-op in web mode. */
export async function kvmMeshAdd(
  node: string,
  networkId: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("kvm_mesh_add", { node, networkId });
}

/** Take a KVM appliance off a mesh (never its fleet mesh). Same
 *  delivery/confirmation model as {@link kvmMeshAdd}. No-op in web mode. */
export async function kvmMeshRemove(
  node: string,
  networkId: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("kvm_mesh_remove", { node, networkId });
}

/** Ownership feedback from the mesh — a `claimed` / `declined` reply to a
 *  claim we sent. No-op listener in web mode. */
export async function onOwnership(
  cb: (o: { from: string; message: { kind: string; reason?: string } }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ from: string; message: { kind: string; reason?: string } }>(
    "allmystuff://ownership",
    (e) => cb(e.payload),
  );
}

/** Share negotiation from a peer — someone invited us into a share, accepted
 *  one we sent, declined, or revoked a grant. The session snapshot carries the
 *  resulting grant set; this event is just the nudge to surface it (a toast).
 *  No-op listener in web mode. */
export async function onShare(
  cb: (s: { from: string; kind: string; person?: string }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ from: string; kind: string; person?: string }>(
    "allmystuff://share",
    (e) => cb(e.payload),
  );
}

export function sessionSnapshot(): Promise<SessionSnapshot | null> {
  return tryInvoke<SessionSnapshot>("session_snapshot");
}

// ---- remote console (per-machine windows + the media plane) ------------

/** Open (or focus) the dedicated console window for `node`. Desktop only —
 *  the web preview keeps its in-page console. */
export async function openConsoleWindow(node: string): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_console_window", { node });
}

/** Which machine this window is a console for, when the window was opened
 *  by `openConsoleWindow` (`?console=<node id>`). Null in the main window. */
export function consoleWindowTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("console");
}

/** Forward one keyboard/mouse event down an active outbound input route. */
export function sendInput(routeId: string, action: InputAction): Promise<null> {
  return tryInvoke("send_input", { routeId, action });
}

/** Read this machine's clipboard and push it down an active outbound
 *  clipboard route — the console calls this the instant it forwards a paste,
 *  so the far side writes our content (text, an image, or files) to its
 *  clipboard before the paste keystroke (right behind it on the same ordered
 *  channel) lands. The read happens in the backend: it's the only side that
 *  can see file references on the OS clipboard. */
export function clipboardPaste(routeId: string): Promise<null> {
  return tryInvoke("clipboard_paste", { routeId });
}

/** Stream native local file paths down the active clipboard route. Unlike a
 * browser `<input>`, Tauri's OS drag event supplies real paths, so the backend
 * can open and chunk the files without loading them into the webview. Errors
 * deliberately propagate: a drop needs a visible success/failure result. */
export async function clipboardDrop(routeId: string, paths: string[]): Promise<void> {
  if (!isTauri()) throw new Error("File drops need the desktop app");
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("clipboard_drop", { routeId, paths });
}

export type NativeFileDragEvent =
  | { type: "enter" | "drop"; paths: string[]; x: number; y: number }
  | { type: "over"; x: number; y: number }
  | { type: "leave" };

/** Follow OS-native file drags over this webview. Positions from Tauri are
 * physical pixels; DOM hit testing is logical pixels, so normalize them here
 * once and keep the console component platform-agnostic. */
export async function onNativeFileDrag(
  cb: (event: NativeFileDragEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const [{ getCurrentWebview }, { getCurrentWindow }] = await Promise.all([
    import("@tauri-apps/api/webview"),
    import("@tauri-apps/api/window"),
  ]);
  const scale = await getCurrentWindow().scaleFactor();
  return getCurrentWebview().onDragDropEvent((event) => {
    const payload = event.payload;
    if (payload.type === "leave") {
      cb({ type: "leave" });
      return;
    }
    const x = payload.position.x / scale;
    const y = payload.position.y / scale;
    if (payload.type === "over") cb({ type: "over", x, y });
    else cb({ type: payload.type, paths: payload.paths, x, y });
  });
}

/** Copy/cut *from* the remote: ask the far side to read its clipboard and send
 *  it back down this route, so the selection it just copied lands on this
 *  machine. The console calls this right after forwarding the copy/cut
 *  keystroke (so the remote has copied into its own clipboard first). The
 *  read happens on the remote; the backend gates the reply onto our clipboard
 *  to the window this pull opens. */
export function clipboardPull(routeId: string): Promise<null> {
  return tryInvoke("clipboard_pull", { routeId });
}

/** Decode one wire packet (28-byte little-endian header + payload) out
 *  of a poll batch. Returns null for shapes we don't recognize. */
function parseVideoPacket(
  buf: ArrayBuffer,
  offset: number,
  len: number,
): VideoFrameMsg | null {
  if (len < 28) return null;
  const head = new DataView(buf, offset, 28);
  const kindByte = head.getUint8(0);
  if (kindByte !== 1 && kindByte !== 2 && kindByte !== 3) return null;
  return {
    kind: kindByte === 3 ? "raw" : kindByte === 2 ? "h264" : "jpeg",
    key: (head.getUint8(1) & 1) === 1,
    width: head.getUint32(4, true),
    height: head.getUint32(8, true),
    sourceWidth: head.getUint32(12, true),
    sourceHeight: head.getUint32(16, true),
    seq: Number(head.getBigUint64(20, true)),
    data: new Uint8Array(buf.slice(offset + 28, offset + len)),
  };
}

/** Stream one route's inbound video into this window by *pulling*: the
 *  backend queues raw packets per route and this drains them every
 *  display tick (`video_poll` → `[u32 len][packet]…`). A failed poll
 *  costs one tick and the next one recovers — unlike a push channel,
 *  where ordered delivery means one lost message silently freezes the
 *  stream while the backend keeps sending. `opts.decode` asks the backend
 *  to decode H.264 natively and deliver ready-to-paint RGBA (`raw`)
 *  packets — for webviews without WebCodecs, and the bottom rung of the
 *  console's decode ladder. Returns an unwatch fn (a no-op in web mode,
 *  where no frames can arrive anyway). */
export type NativeDecoderPreference = "automatic" | "software";

export async function watchVideo(
  routeId: string,
  cb: (f: VideoFrameMsg) => void,
  opts?: { decode?: boolean; decoder?: NativeDecoderPreference },
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");
  const token = (await invoke("video_watch", {
    routeId,
    decode: opts?.decode ?? false,
    decoder: opts?.decoder ?? "automatic",
  })) as number;
  let stopped = false;
  let inFlight = false;
  const tick = async () => {
    if (stopped || inFlight) return;
    inFlight = true;
    try {
      const batch = (await invoke("video_poll", { routeId })) as ArrayBuffer;
      if (stopped || !(batch instanceof ArrayBuffer)) return;
      const view = new DataView(batch);
      let offset = 0;
      while (offset + 4 <= batch.byteLength) {
        const len = view.getUint32(offset, true);
        offset += 4;
        if (len === 0 || offset + len > batch.byteLength) break;
        const packet = parseVideoPacket(batch, offset, len);
        offset += len;
        if (packet) cb(packet);
      }
    } catch {
      // One missed poll; the next tick drains everything queued.
    } finally {
      inFlight = false;
    }
  };
  // Drain on the backend's "queue went non-empty" poke — event delivery
  // isn't timer-throttled, so an occluded (non-maximized) console keeps
  // painting at full rate, and arrival-driven pulls beat the interval's
  // worst-case 16 ms. The interval stays as the safety net.
  const unlisten = await listen<string>("allmystuff://video-ready", (e) => {
    if (e.payload === routeId) void tick();
  });
  const timer = setInterval(() => void tick(), 16);
  return () => {
    stopped = true;
    clearInterval(timer);
    unlisten();
    void invoke("video_unwatch", { routeId, token }).catch(() => {});
  };
}

/** A display-route host explaining its capture state in-band (`vstat`
 *  media frames): why frames aren't flowing — consent dialog pending on
 *  the host, display asleep, no monitor, grabs failing — or `ok` when
 *  they are (again). */
export type VideoHostStatus = {
  route: string;
  state:
    | "ok"
    | "waiting_consent"
    | "display_asleep"
    | "no_monitor"
    | "grab_failed"
    | "no_camera"
    | "camera_failed";
  detail?: string | null;
};

/** Listen for the host's capture-status reports on `routeId`, so the
 *  console can explain a black stage instead of just showing one. No-op
 *  in web mode. */
export async function watchVideoStatus(
  routeId: string,
  cb: (s: VideoHostStatus) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<VideoHostStatus>(
    "allmystuff://video-status",
    (e) => {
      if (e.payload.route === routeId) cb(e.payload);
    },
  );
  return () => unlisten();
}

// ---- terminal (the mesh-native shell) ----------------------------------

/** Open (or focus) the dedicated terminal window for `node` — one window
 *  per machine, holding its terminal tabs. With `attach` set, opens a
 *  *popped-out* window whose first tab joins that shared session (its own
 *  window, keyed by the session). Desktop only; the web preview keeps its
 *  in-page terminal. */
export async function openTerminalWindow(
  node: string,
  attach?: string,
): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_terminal_window", { node, attach: attach ?? null });
}

/** Which machine this window is a terminal for, when the window was opened
 *  by `openTerminalWindow` (`?terminal=<node id>`). Null in the main window. */
export function terminalWindowTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("terminal");
}

/** The shared session this terminal window should attach its first tab to,
 *  when it was opened as a popped-out tab (`?attach=<session id>`). Null for an
 *  ordinary terminal window (its first tab mints a fresh shell). */
export function terminalAttachTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("attach");
}

/** Send keystrokes or a resize down an active terminal route (this window
 *  is the viewer; the far end feeds the PTY). Throws when the backend
 *  refuses, so a dead tab is told apart from a slow one. */
export async function termSend(
  routeId: string,
  event: TermEvent,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("term_send", { routeId, event });
}

/** Stream one terminal route's output into this window by *pulling* —
 *  the exact shape `watchVideo` uses (the backend buffers from
 *  route-activation, pokes `allmystuff://term-ready` when the queue goes
 *  non-empty, and a safety interval catches lost pokes). The callback gets
 *  raw PTY bytes ready for `Terminal.write`. Returns an unwatch fn. */
export async function watchTerminal(
  routeId: string,
  cb: (bytes: Uint8Array) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");
  const token = (await invoke("term_watch", { routeId })) as number;
  let stopped = false;
  let inFlight = false;
  const tick = async () => {
    if (stopped || inFlight) return;
    inFlight = true;
    try {
      const batch = (await invoke("term_poll", { routeId })) as ArrayBuffer;
      if (stopped || !(batch instanceof ArrayBuffer)) return;
      const view = new DataView(batch);
      let offset = 0;
      while (offset + 4 <= batch.byteLength) {
        const len = view.getUint32(offset, true);
        offset += 4;
        if (offset + len > batch.byteLength) break;
        if (len > 0) cb(new Uint8Array(batch.slice(offset, offset + len)));
        offset += len;
      }
    } catch {
      // One missed poll; the next tick drains everything queued.
    } finally {
      inFlight = false;
    }
  };
  const unlisten = await listen<string>("allmystuff://term-ready", (e) => {
    if (e.payload === routeId) void tick();
  });
  const timer = setInterval(() => void tick(), 50);
  void tick(); // drain whatever buffered before this window subscribed
  return () => {
    stopped = true;
    clearInterval(timer);
    unlisten();
    void invoke("term_unwatch", { routeId, token }).catch(() => {});
  };
}

/** The far shell ended (`allmystuff://term-exit`): which route, and the
 *  exit code when there was one (null = killed / no status). */
export async function onTermExit(
  cb: (e: { route: string; code: number | null }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ route: string; code: number | null }>(
    "allmystuff://term-exit",
    (e) => cb(e.payload),
  );
}

/** The host's authoritative shared-PTY size (`allmystuff://term-resize`):
 *  which route, and the cols/rows every attacher renders at so a shared shell
 *  wraps identically for all of them (a bigger window letterboxes to it). */
export async function onTermResize(
  cb: (e: { route: string; cols: number; rows: number }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ route: string; cols: number; rows: number }>(
    "allmystuff://term-resize",
    (e) => cb(e.payload),
  );
}

/** Ask `node` for its open terminal sessions — the picker's "attach to an
 *  existing shell" list (multi-attach). The **local** machine answers
 *  synchronously, returning the list here; a **remote** host answers
 *  asynchronously, returning null while the reply arrives via
 *  {@link onTerminalSessions}. Owner/fleet gated both ends; empty/null in
 *  web mode (no backend). */
export async function terminalSessions(
  node: string,
): Promise<TerminalSessionInfo[] | null> {
  return tryInvoke<TerminalSessionInfo[] | null>("terminal_sessions", { node });
}

/** A host's answer to {@link terminalSessions} for a remote machine: its open
 *  terminal sessions, tagged with which host they came from. No-op in web
 *  mode. */
export async function onTerminalSessions(
  cb: (e: { from: string; sessions: TerminalSessionInfo[] }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ from: string; sessions: TerminalSessionInfo[] }>(
    "allmystuff://terminal-sessions",
    (e) => cb(e.payload),
  );
}

// ---- files (the mesh-native file manager) -------------------------------

/** Open (or focus) the dedicated files window for `node` — one window per
 *  machine, the finder-like view of its disk. Desktop only; the web
 *  preview keeps an in-page popover. */
export async function openFilesWindow(node: string): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_files_window", { node });
}

/** Which machine this window is a file manager for, when the window was
 *  opened by `openFilesWindow` (`?files=<node id>`). Null in the main
 *  window. */
export function filesWindowTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("files");
}

/** Send one file request down an active files route (this window is the
 *  viewer; the far end owns the disk). Throws when the backend refuses. */
export async function fileSend(
  routeId: string,
  event: FileEvent,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("file_send", { routeId, event });
}

/** Stream one files route's responses into this window by *pulling* —
 *  the exact shape `watchTerminal` uses (the backend buffers from
 *  route-activation, pokes `allmystuff://file-ready` when the queue goes
 *  non-empty, and a safety interval catches lost pokes). Each buffered
 *  chunk is one JSON `FileEvent` frame; the callback gets it parsed.
 *  Returns an unwatch fn. */
export async function watchFiles(
  routeId: string,
  cb: (event: FileEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");
  const token = (await invoke("file_watch", { routeId })) as number;
  const decoder = new TextDecoder();
  let stopped = false;
  let inFlight = false;
  const tick = async () => {
    if (stopped || inFlight) return;
    inFlight = true;
    try {
      const batch = (await invoke("file_poll", { routeId })) as ArrayBuffer;
      if (stopped || !(batch instanceof ArrayBuffer)) return;
      const view = new DataView(batch);
      let offset = 0;
      while (offset + 4 <= batch.byteLength) {
        const len = view.getUint32(offset, true);
        offset += 4;
        if (len === 0 || offset + len > batch.byteLength) break;
        try {
          cb(
            JSON.parse(
              decoder.decode(new Uint8Array(batch, offset, len)),
            ) as FileEvent,
          );
        } catch {
          // One unparseable frame; the stream's surviving frames stand.
        }
        offset += len;
      }
    } catch {
      // One missed poll; the next tick drains everything queued.
    } finally {
      inFlight = false;
    }
  };
  const unlisten = await listen<string>("allmystuff://file-ready", (e) => {
    if (e.payload === routeId) void tick();
  });
  const timer = setInterval(() => void tick(), 100);
  void tick(); // drain whatever buffered before this window subscribed
  return () => {
    stopped = true;
    clearInterval(timer);
    unlisten();
    void invoke("file_unwatch", { routeId, token }).catch(() => {});
  };
}

/** Route the coming `read` request's chunks straight into this machine's
 *  Downloads folder. Returns the destination path; completion lands as
 *  `allmystuff://file-saved`. Call *before* `fileSend`-ing the read so
 *  the first chunk can't race the registration. */
export async function fileDownload(
  routeId: string,
  req: number,
  name: string,
): Promise<string> {
  if (!isTauri()) throw new Error("Downloads need the desktop app");
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("file_download", { routeId, req, name })) as string;
}

/** A registered download finished (`allmystuff://file-saved`): where it
 *  landed, or the error that stopped it. */
export async function onFileSaved(
  cb: (e: {
    route: string;
    req: number;
    path: string | null;
    error: string | null;
  }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{
    route: string;
    req: number;
    path: string | null;
    error: string | null;
  }>("allmystuff://file-saved", (e) => cb(e.payload));
}

/** This machine refused an inbound input/clipboard event
 *  (`allmystuff://control-refused`): the route was live but the sender isn't
 *  an authorized controller (or the route wasn't live here at all). Rate
 *  limited backend-side; the store toasts it so the refusal is visible on
 *  the refusing machine too, not just in its log. */
export async function onControlRefused(
  cb: (e: {
    route: string;
    from: string;
    plane: string;
    reason: string;
  }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ route: string; from: string; plane: string; reason: string }>(
    "allmystuff://control-refused",
    (e) => cb(e.payload),
  );
}

/** The passive clock-skew verdict changed (`allmystuff://clock-skew`):
 *  this machine's wall clock drifted well out of line with its peers'
 *  (state "warn"), or came back (state "clear"). Estimated entirely from
 *  traffic that was already flowing — presence stamps node-side, heartbeat
 *  pings daemon-side — never from extra calls to other nodes. */
export interface ClockSkewEvent {
  state: "warn" | "clear";
  skew_ms: number | null;
  peers: number | null;
  message: string | null;
  source: string;
}
export async function onClockSkew(
  cb: (e: ClockSkewEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<ClockSkewEvent>("allmystuff://clock-skew", (e) =>
    cb(e.payload),
  );
}

/** A fleet peer asked this machine to reboot (`allmystuff://device-restart`)
 *  — the heads-up shown to whoever is sitting here just before the OS goes
 *  down. */
export async function onDeviceRestart(
  cb: (e: { from: string }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ from: string }>("allmystuff://device-restart", (e) =>
    cb(e.payload),
  );
}

export interface DriveMountEvent {
  route: string;
  from: string;
  mount?: string;
  label?: string;
  error?: string | null;
}
export async function onDriveMount(
  cb: (e: DriveMountEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<DriveMountEvent>("allmystuff://drive-mount", (e) =>
    cb(e.payload),
  );
}

export interface KvmMediaEvent {
  from: string;
  kvm: string;
  label: string;
  error?: string | null;
}
export async function onKvmMedia(
  cb: (e: KvmMediaEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<KvmMediaEvent>("allmystuff://kvm-media", (e) =>
    cb(e.payload),
  );
}

/** Progress of a registered download (`allmystuff://file-progress`),
 *  throttled backend-side. */
export async function onFileProgress(
  cb: (e: {
    route: string;
    req: number;
    written: number;
    total: number;
  }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ route: string; req: number; written: number; total: number }>(
    "allmystuff://file-progress",
    (e) => cb(e.payload),
  );
}

// ---- Shared Files (the call's "Shared Files" area) ----------------------
//
// A call's file sharing is deliberately *not* the file manager: you offer
// specific files into a room-scoped area members can download, never a
// window onto your disk. The bytes ride the files plane peer-to-peer
// (uploader → downloader, by opaque token); the room's host only carries
// the list. All of these no-op (empty) in web mode — sharing needs the
// desktop app on a live mesh.

/** Offer `paths` into a room's Shared Files area, allowing `members`
 *  (canonical node ids) to fetch them. Returns one `{ token, name, size }`
 *  per file that could be read — the caller hands these to the room's host
 *  for its shared list. Empty in web mode. */
export async function roomShareFiles(
  members: string[],
  paths: string[],
): Promise<SharedFileMeta[]> {
  const r = await tryInvoke<SharedFileMeta[]>("room_share_files", {
    members,
    paths,
  });
  return r ?? [];
}

/** Refresh which members may fetch a set of shared tokens (the room's
 *  roster changed while the files were on offer). No-op in web mode. */
export function roomSetSharePeers(
  tokens: string[],
  members: string[],
): Promise<null> {
  return tryInvoke("room_set_share_peers", { tokens, members });
}

/** Stop offering a set of shared files (the uploader removed them or left
 *  the room). No-op in web mode. */
export function roomUnshare(tokens: string[]): Promise<null> {
  return tryInvoke("room_unshare", { tokens });
}

/** Pick local files to share, via the OS open dialog (multi-select).
 *  Returns the chosen absolute paths (empty if cancelled, or in web mode
 *  where there's no native picker). */
export async function pickFilesToShare(): Promise<string[]> {
  if (!isTauri()) return [];
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: true,
      directory: false,
      title: "Share files with the room",
    });
    if (picked == null) return [];
    return Array.isArray(picked) ? picked : [picked];
  } catch (e) {
    console.warn("file picker failed:", e);
    return [];
  }
}

/** Pick one local drive or folder to expose as a native remote drive. */
export async function pickDriveFolder(): Promise<string | null> {
  if (!isTauri()) return null;
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: false,
      directory: true,
      title: "Choose a drive or folder to map",
    });
    return typeof picked === "string" ? picked : null;
  } catch (error) {
    console.warn("drive folder picker failed:", error);
    return null;
  }
}

/** Clipboard for the terminal: WebKitGTK's async clipboard API is flaky,
 *  so under Tauri these ride the clipboard-manager plugin; the web preview
 *  falls back to `navigator.clipboard`. */
export async function clipboardWrite(text: string): Promise<void> {
  if (isTauri()) {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
    return;
  }
  await navigator.clipboard.writeText(text);
}

export async function clipboardRead(): Promise<string> {
  if (isTauri()) {
    const { readText } = await import("@tauri-apps/plugin-clipboard-manager");
    return (await readText()) ?? "";
  }
  return navigator.clipboard.readText();
}

/** Open a link in the system browser (terminal web-links). Tauri routes it
 *  through the shell plugin; the web preview just opens a tab. */
export async function openExternal(url: string): Promise<void> {
  if (isTauri()) {
    const { open } = await import("@tauri-apps/plugin-shell");
    await open(url);
    return;
  }
  window.open(url, "_blank", "noopener");
}

/** Tear this window down (a console window's "End session"). Uses
 *  `destroy` rather than `close` so it never re-fires the close-requested
 *  handler — route teardown has already run by the time this is called. */
export async function closeThisWindow(): Promise<void> {
  if (!isTauri()) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().destroy();
}

/** Run `cb` when the user closes this window via the OS chrome. The close
 *  is *held* (preventDefault) so the console can tear its routes down
 *  first and then finish the job with `closeThisWindow`. Returns an
 *  unlisten fn. */
export async function onThisWindowClose(cb: () => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow().onCloseRequested((event) => {
    event.preventDefault();
    cb();
  });
}

// ---- owned fleet (the "Owned" roster) ---------------------------------

/** The current owned-fleet roster (shared key + members). Null in web mode —
 *  the store simulates a fleet on the demo graph. */
export function ownedRoster(): Promise<OwnedRoster | null> {
  return tryInvoke<OwnedRoster>("owned_roster");
}

/** Subscribe to live fleet-roster updates (after a claim, or when gossip
 *  converges). Returns an unlisten fn (no-op in web mode). */
export async function onOwned(
  cb: (r: OwnedRoster) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<OwnedRoster>("allmystuff://owned", (e) => cb(e.payload));
}

/** Leave the fleet this device belongs to (also releases its owner).
 *  Throws with the backend's reason when refused. No-op in web mode —
 *  the store simulates it on the demo roster. */
export async function fleetLeave(): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_leave");
}

/** Danger Zone: leave the fleet and forget every network the daemon holds,
 *  keeping the device identity. The daemon exits so it reloads clean — pair
 *  with `restartApp()`. No-op in web mode. */
export async function resetNetworking(): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("reset_networking");
}

/** Danger Zone: factory reset — wipe this device's entire state (identity,
 *  config, all networks, fleet ownership) for a brand-new start. The daemon
 *  wipes `~/.myownmesh` and exits — pair with `restartApp()`. No-op in web. */
export async function factoryReset(): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("factory_reset");
}

/** Evict a device from the fleet (owner-only; the backend enforces it).
 *  `code` is the owner's custody second factor when fleet MFA is enrolled.
 *  Throws with the reason when refused. */
export async function fleetKick(device: string, code?: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_kick", { device, code: code ?? null });
}

/** Name (or rename) the fleet (members only). Throws with the reason when
 *  refused; the renamed roster arrives back via `allmystuff://owned`. */
export async function fleetSetName(name: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_set_name", { name });
}

/** Grant a fleet member a role: "manager" (a controller — can admit members)
 *  or "owner" (full authority). Owner-only; the daemon enforces the quorum and
 *  throws with the reason when refused. `code` is the custody second factor
 *  when fleet MFA is enrolled. */
export async function fleetGrantRole(
  device: string,
  role: "manager" | "owner",
  code?: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_grant_role", { device, role, code: code ?? null });
}

/** Withdraw a fleet member's role — back to a plain member. Owner-only; throws
 *  with the reason when refused. `code` is the custody second factor when
 *  fleet MFA is enrolled. */
export async function fleetRevokeRole(
  device: string,
  code?: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_revoke_role", { device, code: code ?? null });
}

/** Designate the fleet's infra hubs — the owner-signed, network-wide shape
 *  every member's daemon converges onto (daemon ≥ 0.2.36). Pass the FULL hub
 *  set each call (it replaces, not merges); an empty set returns the fleet to
 *  full mesh. Owner-only; throws with the reason when refused — including the
 *  "daemon needs 0.2.36+" hint from members whose daemon predates the op.
 *  `code` is the custody second factor when fleet MFA is enrolled. */
export async function fleetSetHubs(
  hubs: string[],
  redundancy?: number,
  code?: string,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_set_hubs", {
    hubs,
    redundancy: redundancy ?? null,
    code: code ?? null,
  });
}

/** Whether this device has enrolled a custody authenticator for the fleet's
 *  closed network. `no_fleet` is true when there's no fleet to enroll yet. */
export interface FleetMfaStatus {
  enrolled: boolean;
  no_fleet?: boolean;
}
export async function fleetMfaStatus(): Promise<FleetMfaStatus> {
  if (!isTauri()) return { enrolled: false, no_fleet: true };
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("fleet_mfa_status")) as FleetMfaStatus;
}

/** Enroll a custody authenticator for the fleet — guards owner/role/kind
 *  changes on the fleet's closed network. Returns the secret, an `otpauth://`
 *  URI (for a QR), and one-time recovery codes; shown to the user once. */
export interface FleetMfaEnrolled {
  secret: string;
  otpauth_uri: string;
  recovery_codes: string[];
}
export async function fleetMfaEnroll(): Promise<FleetMfaEnrolled> {
  if (!isTauri()) throw new Error("desktop app only");
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("fleet_mfa_enroll")) as FleetMfaEnrolled;
}

/** Remove the fleet's custody authenticator. Requires a valid current code. */
export async function fleetMfaDisable(code: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("fleet_mfa_disable", { code });
}

// ---- CEC Support (the technician-side remote help desk) -----------------
//
// CEC Support is AnyDesk-like remote support riding the same mesh engine.
// This app is the *technician*: after revealing the secret settings tab an
// agent enters their name and a customer's number, dials them onto the graph
// (as an ordinary peer — the CEC mesh is Silent, so there is no fleet group),
// and drives the normal screen/control features — gated by the customer's live
// consent grant. All of
// these degrade to null/empty/no-op in web mode (no backend). The `cec://*`
// events flow through the ordinary Tauri event bus (the node's event pump
// forwards every emit by name).

/** This node's CEC snapshot — its own support number (a display label, from
 *  the device key), its role, and the support area it's on. Null in web mode. */
export interface CecStatus {
  number: string;
  /** Always the shared support area (`cecsupport-clients`) — the only mesh
   *  CEC uses now that per-number rooms are gone. */
  network_id: string;
  role: "client" | "technician";
  /** Whether the technician's help-queue view is armed — a node-held view
   *  state, so the toggle reflects it across a reload. Absent from an older
   *  node. */
  help_watching?: boolean;
}

/** One inbound technician connect-request awaiting the customer's 3-choice
 *  prompt (the shape of `cec_pending` rows and the `cec://request` event). */
export interface CecPending {
  tech: string;
  agent_name: string;
  want_control: boolean;
  session_id: string;
  verification_code: string;
}

/** A customer a technician has dialed onto the graph (the `cec_dial` result +
 *  the `cec://peer` event). */
export interface CecPeer {
  node: string;
  number: string;
  label: string;
  /** The machine's hostname — shown beside the name so the technician's card
   *  and the customer's own app spell the same identity. Absent from an older
   *  node. */
  hostname?: string;
  online: boolean;
  /** Epoch **seconds** of the last time this connection was actively used (a
   *  fresh dial, or the console session going active). The CEC tab renders it
   *  as time-since so a technician can prune stale connections. */
  last_used: number;
}

/** One standing consent grant a customer holds (the `cec_grants` rows + the
 *  `cec://grants` event). */
export interface CecGrant {
  technician: string;
  agent_name: string;
  scope: CecScope;
  granted_at: number;
  expires_at: number | null;
  control: boolean;
}

/** The customer's three choices in the "*so-and-so* is trying to connect"
 *  prompt. */
export type CecScope = "once" | "three_hours" | "forever";

/** This node's CEC status. Null in web mode. */
export function cecStatus(): Promise<CecStatus | null> {
  return tryInvoke<CecStatus>("cec_status");
}

/** Technician: dial a customer by the number they read out — the fallback for
 *  when the raised-hand list is too crowded to spot them. The node resolves
 *  the digits to a device on the support area (a raised hand first, else any
 *  area member whose key-derived number matches) and connects. Throws with the
 *  backend's reason when nobody with that number is on the area. Returns the
 *  customer's node id (null in web mode). */
export async function cecDial(
  number: string,
  agentName: string,
): Promise<{ node: string } | null> {
  if (!isTauri()) return null;
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("cec_dial", { number, agentName })) as { node: string };
}

/** Technician: dial a customer's device directly — the headline path, fed by
 *  a raised hand's beacon (its authenticated device id) or a directory
 *  reconnect. Returns the customer's node id (null in web mode). */
export async function cecDialNode(
  node: string,
  agentName: string,
): Promise<{ node: string } | null> {
  if (!isTauri()) return null;
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("cec_dial_node", { node, agentName })) as { node: string };
}

/** Customer: take up residence on the shared support area — called at bring-up
 *  so technicians can see and dial this device. Returns the support number to
 *  read out (a display label). Null in web mode. */
export function cecOnline(): Promise<{ number: string } | null> {
  return tryInvoke<{ number: string }>("cec_online");
}

/** Customer: the inbound technician connect-requests awaiting a choice.
 *  `null` = couldn't fetch (web mode, or a transient RPC failure while the
 *  node socket is busy) — callers keep their last snapshot rather than wiping. */
export async function cecPending(): Promise<CecPending[] | null> {
  const r = await tryInvoke<CecPending[]>("cec_pending");
  return Array.isArray(r) ? r : null;
}

/** Customer: approve a technician at a scope (once / three hours / forever),
 *  driving the session Active. Throws with the reason when refused (e.g. a
 *  durable grant that couldn't be saved). */
export async function cecApprove(
  tech: string,
  scope: CecScope,
  sessionId: string,
  wantControl: boolean,
): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("cec_approve", { tech, scope, sessionId, wantControl });
}

/** Customer: decline a pending connect-request. No-op in web mode. */
export async function cecDeny(tech: string, sessionId: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("cec_deny", { tech, sessionId });
}

/** Customer: "Forget this technician" — revoke every grant and tear down. The
 *  revoke bites the next privileged frame even if the wire End is lost. */
export async function cecRevoke(tech: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("cec_revoke", { tech });
}

/** Customer: the live consent grants. `null` = couldn't fetch — keep the
 *  last snapshot. */
export async function cecGrants(): Promise<CecGrant[] | null> {
  const r = await tryInvoke<CecGrant[]>("cec_grants");
  return Array.isArray(r) ? r : null;
}

/** Technician: the customers this node has dialed — the CEC tab's "Active
 *  connections" list. These are ordinary graph peers; the list comes from CEC
 *  state, not from any graph grouping. Empty in web mode. */
export async function cecDialed(): Promise<CecPeer[] | null> {
  const r = await tryInvoke<CecPeer[]>("cec_dialed");
  return Array.isArray(r) ? r : null;
}

/** One customer waiting on the global help room (`cec_help_list` rows + the
 *  `cec://help` event). `number` is derived node-side from the authenticated
 *  beacon sender, so it's dialable as-is. */
export interface CecHelpSeeker {
  node: string;
  number: string;
  label: string;
  /** The machine's hostname, from the beacon — the match-up key beside the
   *  name. Absent from an older node. */
  hostname?: string;
  /** Epoch seconds they first asked — the queue position. */
  asked_at: number;
  /** Epoch seconds of their latest beacon. */
  last_seen: number;
}

/** Technician: the customers currently asking for help, longest-waiting
 *  first. Read-only — empty unless the queue view is armed (`cecHelpWatch`).
 *  Null = fetch failed (keep the last snapshot); empty in web mode. */
export async function cecHelpList(): Promise<CecHelpSeeker[] | null> {
  const r = await tryInvoke<CecHelpSeeker[]>("cec_help_list");
  return Array.isArray(r) ? r : null;
}

/** Technician: arm (or disarm) the help-queue view — the "Watch the help
 *  queue" toggle. A view state, not a membership: the node stays on the
 *  support area either way (dialed customers' sessions ride it), so disarming
 *  never hangs up on anyone; it just stops surfacing raised hands. Throws on
 *  failure: this exact call once failed silently for days (the command wasn't
 *  registered) while the toggle looked merely stubborn. */
export async function cecHelpWatch(on: boolean): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("cec_help_watch", { on });
}

/** The help queue changed (`cec://help`) — a fresh asker, a withdrawal, or
 *  (customer-side) our own ask being cleared. No-op in web mode. */
export async function onCecHelp(
  cb: (e: { waiting?: CecHelpSeeker[]; asking?: boolean }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ waiting?: CecHelpSeeker[]; asking?: boolean }>(
    "cec://help",
    (e) => cb(e.payload),
  );
}

/** Technician: stop whatever the in-flight dial is trying — the connect-request
 *  re-sends quit at the node's cancel flag. No-op in web mode or when nothing
 *  is in flight. */
export async function cecCancelDial(): Promise<void> {
  await tryInvoke("cec_cancel_dial");
}

/** "Forget this node" — an app-wide action on every node's gear: drop it from
 *  the graph + roster and tear its session down (also ends a CEC session when
 *  the node happens to be a CEC peer). No-op in web mode. */
export async function forgetNode(node: string): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("forget_node", { node });
}

/** Customer: a technician is trying to connect (`cec://request`) — the nudge
 *  that drives the 3-choice prompt. No-op listener in web mode. */
export async function onCecRequest(
  cb: (r: CecPending) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<CecPending>("cec://request", (e) => cb(e.payload));
}

/** Technician: a dialed customer's graph node updated (`cec://peer`). No-op in
 *  web mode. */
export async function onCecPeer(cb: (p: CecPeer) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<CecPeer>("cec://peer", (e) => cb(e.payload));
}

/** A CEC session changed state (`cec://session`). No-op in web mode. */
export async function onCecSession(
  cb: (s: { session_id: string; state: string }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ session_id: string; state: string }>("cec://session", (e) =>
    cb(e.payload),
  );
}

/** Customer: the grant list changed (`cec://grants`). No-op in web mode. */
export async function onCecGrants(
  cb: (grants: CecGrant[]) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ grants: CecGrant[] }>("cec://grants", (e) =>
    cb(e.payload.grants),
  );
}

// ---- CEC customer chat (the technician ↔ customer thread) ---------------

/** One line in a CEC customer chat thread. `from` is who wrote it (the dialed
 *  customer vs this technician); `ts` is the node-assigned epoch-ms timestamp;
 *  `id` is the node-assigned line id the whole plane dedupes on. */
export interface CecChatMsg {
  id: string;
  from: "client" | "technician";
  text: string;
  ts: number;
}

/** Technician → customer: send a chat line to a dialed customer (`peer` is
 *  their node id). The node persists it, returns the assigned `{ id, ts }`,
 *  and *also* echoes it back over `cec://chat` — so the sender dedupes by id
 *  rather than double-showing its own line. Null in web mode. */
export async function cecChatSend(
  peer: string,
  text: string,
): Promise<{ id: string; ts: number } | null> {
  return tryInvoke<{ id: string; ts: number }>("cec_chat_send", { peer, text });
}

/** The stored chat history with one customer, oldest-first. `null` = couldn't
 *  fetch (web mode, or a transient RPC failure) — callers keep their last
 *  snapshot rather than blanking the thread, the same discipline as
 *  {@link cecDialed}. */
export async function cecChatHistory(
  peer: string,
): Promise<CecChatMsg[] | null> {
  const r = await tryInvoke<{ messages: CecChatMsg[] }>("cec_chat_history", {
    peer,
  });
  return r && Array.isArray(r.messages) ? r.messages : null;
}

/** A chat line landed for some customer (`cec://chat`) — an inbound line from
 *  the customer, or this technician's own line echoed back. Carries the
 *  customer's node id so a listener can file it under the right thread. No-op
 *  listener in web mode. */
export async function onCecChat(
  cb: (e: { peer: string; message: CecChatMsg }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ peer: string; message: CecChatMsg }>("cec://chat", (e) =>
    cb(e.payload),
  );
}

// ---- virtual rooms (the rooms plane) ------------------------------------

/** Fan one room-plane message out to `members` (canonical or display node
 *  ids; self is skipped backend-side). Resolves to how many members the
 *  daemon actually dispatched to — 0 in web mode, where nothing can flow
 *  and the room is local-only. */
export async function roomSend(
  members: string[],
  message: RoomWireMessage,
): Promise<number> {
  const n = await tryInvoke<number>("room_send", { members, message });
  return n ?? 0;
}

/** Inbound room-plane traffic (an invite, a join/leave, a chat line).
 *  No-op listener in web mode. */
export async function onRoom(
  cb: (e: { from: string; message: RoomWireMessage }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ from: string; message: RoomWireMessage }>(
    "allmystuff://room",
    (e) => cb(e.payload),
  );
}

/** Open (or focus) the dedicated window for one room — the call in its
 *  own OS window, full-screenable like a console. Desktop only; the web
 *  preview keeps the in-page panel. */
export async function openRoomWindow(roomId: string): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_room_window", { room: roomId });
}

/** Which room this window is the call for, when the window was opened by
 *  `openRoomWindow` (`?room=<room id>`). Null in the main window. */
export function roomWindowTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("room");
}

/** The same-device chatter between this app's windows about rooms — a
 *  room window telling the main window "I joined / left", the main window
 *  asking a room window to hang up, any window announcing the saved rooms
 *  list changed. Mesh room messages never echo back to their sender, so
 *  windows of one device need this local lane to stay in step. Carried on
 *  the Tauri event bus (which reaches every window, the sender included —
 *  receivers drop their own `token`). No-op in web mode: one window. */
export interface RoomLocalEvent {
  /** The emitting window's store token — receivers ignore their own. */
  token: string;
  /** What happened: `join`/`leave` mirror self-presence, `leave-ask`
   *  requests whichever window holds the room joined to hang up, `sync`
   *  says the persisted rooms list changed (reload it), `knock-done`
   *  says an ask-to-join was answered (clear it everywhere). */
  kind: "join" | "leave" | "leave-ask" | "sync" | "knock-done";
  room?: string;
  /** `knock-done`: whose ask was answered. */
  from?: string;
}

export async function emitRoomLocal(event: RoomLocalEvent): Promise<void> {
  if (!isTauri()) return;
  const { emit } = await import("@tauri-apps/api/event");
  await emit("allmystuff://room-local", event);
}

export async function onRoomLocal(
  cb: (e: RoomLocalEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<RoomLocalEvent>("allmystuff://room-local", (e) =>
    cb(e.payload),
  );
}

/** Claim OS focus for this window — the KVM rule for control surfaces:
 *  with remote control active, the window under the mouse is the one
 *  your keyboard should reach, with no click in between (a click would
 *  go to the *remote* anyway). Callers gate this on control being live;
 *  web mode is a no-op (one window). */
export async function focusThisWindow(): Promise<void> {
  if (!isTauri()) return;
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().setFocus();
  } catch (e) {
    console.warn("window focus failed:", e);
  }
}

/** Flip this OS window in or out of fullscreen (a room window's
 *  fullscreen control). Resolves to the new state; web mode is a no-op
 *  (the browser owns fullscreen there). */
export async function toggleWindowFullscreen(): Promise<boolean> {
  if (!isTauri()) return false;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  const win = getCurrentWindow();
  const next = !(await win.isFullscreen());
  await win.setFullscreen(next);
  return next;
}

/** The window's REAL fullscreen state — the OS can flip it without our
 *  ⛶ (Win+Up, taskbar, a shell gesture), and pointer-lock/fit logic
 *  keys off the flag, so callers re-query on resize instead of trusting
 *  their last click. Web mode reports the Fullscreen API instead. */
export async function isWindowFullscreen(): Promise<boolean> {
  if (!isTauri()) return document.fullscreenElement != null;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow().isFullscreen();
}

// ---- video popouts (one stream in its own OS window) -------------------

/** Open (or focus) the dedicated popout window for one video stream.
 *  `key` names the stream (`cap:<capability id>` for a console input the
 *  popout wires itself, `share:<route id>` for a room share it merely
 *  watches); `title` seeds the OS window title. Desktop only — the web
 *  preview has no windows to pop into. */
export async function openVideoWindow(
  key: string,
  title: string,
  decoder: NativeDecoderPreference = "automatic",
): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_video_window", { key, title, decoder });
}

/** Which stream this window is a popout for, when it was opened by
 *  `openVideoWindow` (`?video=<key>`). Null everywhere else. */
export function videoWindowTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("video");
}

/** Native decoder choice carried into a dedicated video popout. This query
 *  belongs to the local window only and never enters route negotiation. */
export function videoWindowDecoderPreference(): NativeDecoderPreference {
  if (typeof window === "undefined") return "automatic";
  return new URLSearchParams(window.location.search).get("decoder") === "software"
    ? "software"
    : "automatic";
}

/** Pop the CEC Support console out into its own window. One fixed window —
 *  re-invoking focuses it rather than stacking a second console. */
export async function openCecWindow(): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_cec_window", {});
}

/** True when THIS webview is the CEC console popout (`?cec=1`), so the main
 *  App renders just the console and CecSection hides its own pop-out button. */
export function isCecWindow(): boolean {
  if (typeof window === "undefined") return false;
  return new URLSearchParams(window.location.search).get("cec") === "1";
}

/** Pop a customer chat out into its own OS window (`?chat=<peer node id>`),
 *  the sibling of the per-machine console window. One window per customer
 *  (`chat-<peer>` label) — re-opening focuses the existing thread rather than
 *  stacking a second one. Desktop only; the web preview has no windows to pop
 *  into. Rides `open_chat_window`, the chat twin of `open_console_window`. */
export async function openChatWindow(peer: string): Promise<void> {
  if (!isTauri()) return;
  await tryInvoke("open_chat_window", { peer });
}

/** Which customer this window is a chat for, when it was opened by
 *  {@link openChatWindow} (`?chat=<peer node id>`). Null in the main window
 *  and every other popout. */
export function chatWindowTarget(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("chat");
}

/** The same-device chatter between this app's windows about video
 *  popouts — the popout-presence twin of [`RoomLocalEvent`]. A popout
 *  announces itself (`opened` at boot, and again in answer to a `hello`
 *  ping, so a console/room window that opens later still learns of it)
 *  and its end (`closed` — including the OS ✕); `return-ask` is a tab's
 *  "Return video here" asking whichever window holds that stream to put
 *  it back. No-op in web mode: one window. */
export interface VideoLocalEvent {
  /** The emitting window's store token — receivers ignore their own. */
  token: string;
  kind: "opened" | "closed" | "return-ask" | "hello";
  /** The popout key (`cap:<capability id>` / `share:<route id>`).
   *  Absent on `hello` (a who's-out-there ping). */
  key?: string;
}

export async function emitVideoLocal(event: VideoLocalEvent): Promise<void> {
  if (!isTauri()) return;
  const { emit } = await import("@tauri-apps/api/event");
  await emit("allmystuff://video-local", event);
}

export async function onVideoLocal(
  cb: (e: VideoLocalEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<VideoLocalEvent>("allmystuff://video-local", (e) =>
    cb(e.payload),
  );
}

// ---- sites (the reverse-proxy plane) -----------------------------------
//
// All of these degrade in web mode (no backend): the scan is empty, the
// exposed set is empty, and a map/unmap is a no-op — so the Sites sidebar
// falls back to its demo data and stays interactive.

/** This machine's discovered listening TCP services (the full set, so the
 *  owner can choose which to expose). Empty in web mode. */
export async function siteScan(): Promise<ListeningService[]> {
  const r = await tryInvoke<ListeningService[]>("site_scan");
  return Array.isArray(r) ? r : [];
}

/** The services this machine currently *advertises* (exposes to the mesh),
 *  as id → display name (empty = the classified default). Persisted
 *  backend-side and reflected in presence. */
export async function siteExposed(): Promise<Record<string, string>> {
  const r = await tryInvoke<Record<string, string>>("site_exposed");
  return r && typeof r === "object" ? r : {};
}

/** Set which listening services this machine advertises (id → display name).
 *  Returns the new exposed map (the backend re-broadcasts presence so peers
 *  see the change). */
export async function siteSetExposed(
  exposed: Record<string, string>,
): Promise<Record<string, string>> {
  const r = await tryInvoke<Record<string, string>>("site_set_exposed", {
    exposed,
  });
  return r && typeof r === "object" ? r : exposed;
}

/** A live local mapping of a remote site: the host port and the local port
 *  this machine bound the tunnel on. */
export interface SiteMappingInfo {
  node: string;
  port: number;
  localPort: number;
}

/** Map a peer's site to a local port — sets up the reverse-proxy route and
 *  binds a local listener. Returns the bound local port (the same number
 *  when free, else a remapped one), or null in web mode. */
export function siteMap(
  node: string,
  port: number,
): Promise<{ localPort: number } | null> {
  return tryInvoke<{ localPort: number }>("site_map", { node, port });
}

/** Tear a site mapping down (unbinds the local listener, drops the route). */
export function siteUnmap(node: string, port: number): Promise<null> {
  return tryInvoke("site_unmap", { node, port });
}

/** Every site this machine currently has mapped. Empty in web mode. */
export async function siteMappings(): Promise<SiteMappingInfo[]> {
  const r = await tryInvoke<SiteMappingInfo[]>("site_mappings");
  return Array.isArray(r) ? r : [];
}

/** Ask a co-owned fleet machine for its full site list (to manage its
 *  exposure from its drawer). The reply arrives via {@link onNodeSites}. */
export function siteRemoteList(node: string): Promise<null> {
  return tryInvoke("site_remote_list", { node });
}

/** Tell a co-owned fleet machine to advertise exactly `exposed` (id → name). */
export function siteRemoteSetExposed(
  node: string,
  exposed: Record<string, string>,
): Promise<null> {
  return tryInvoke("site_remote_set_exposed", { node, exposed });
}

/** A managed machine's answer to {@link siteRemoteList}: its full discovered
 *  services + its current exposed map. */
export interface NodeSitesEvent {
  from: string;
  services: SiteService[];
  exposed: Record<string, string>;
}

/** Subscribe to fleet machines' site-list replies. No-op in web mode. */
export async function onNodeSites(
  cb: (e: NodeSitesEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<NodeSitesEvent>("allmystuff://node-sites", (e) =>
    cb(e.payload),
  );
}

// ---- self-update -------------------------------------------------------
//
// These degrade to null in web mode (no backend), so the Updates settings
// section can render a friendly "desktop only" note instead of throwing.

export function updateStatus(): Promise<UpdateStatus | null> {
  return tryInvoke<UpdateStatus>("update_status");
}

/** Versions of the independently replaceable local components. */
export function componentStatus(): Promise<ComponentStatus | null> {
  return tryInvoke<ComponentStatus>("component_status");
}

/** Repair/update one component row. Uses a raw invoke so failures reach UI. */
export async function componentRepair(component: string): Promise<unknown> {
  if (!isTauri()) throw new Error("component repair needs the desktop app");
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke("component_repair", { component });
}

/** The background auto-update ticker reporting what a check decided — the
 *  launch check (~30s after start) and then every `check_interval_hours`.
 *  Without this the ticker was mute: it staged updates that nobody was told
 *  about, so auto-update looked like it wasn't running at all. No-op listener
 *  in web mode. */
export async function onUpdateChecked(
  cb: (o: CheckOutcome) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<CheckOutcome>("update://checked", (e) => cb(e.payload));
}

export function updateCheck(): Promise<CheckOutcome | null> {
  return tryInvoke<CheckOutcome>("update_check");
}

export function updateApply(): Promise<{ applied: string | null } | null> {
  return tryInvoke<{ applied: string | null }>("update_apply");
}

/** Apply any staged update to disk and relaunch into the new version. The
 *  process restarts on success, so this never resolves then — it only returns
 *  (throwing) if the apply failed and we stayed on the old build. No-op in web
 *  mode. Uses a raw invoke so the failure surfaces instead of being swallowed
 *  to null like the graceful-degradation helpers above. */
export async function updateRelaunch(): Promise<void> {
  if (!isTauri()) return;
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("update_relaunch");
}

export function updateSetPrefs(
  prefs: UpdatePrefs,
): Promise<UpdateStatus | null> {
  return tryInvoke<UpdateStatus>("update_set_prefs", { prefs });
}

/** The latest release version on the configured channel (read-only — it
 *  doesn't stage or apply anything). Null in web mode, or if the feed had no
 *  usable tag. Used to tell whether a remote machine is behind the channel. */
export function updateLatestVersion(): Promise<string | null> {
  return tryInvoke<string>("update_latest_version");
}

// ---- "Always On": background service + window behaviour ----------------
//
// The service half drives the OS background service (systemd / launchd /
// Windows SCM) through the `allmystuff` CLI; the window half persists whether
// closing / minimizing keeps AllMyStuff alive in the tray. Both degrade to
// null in web mode so the settings tab can render a "desktop only" note.

/** Status of the OS background ("Always On") service, computed in-process.
 *  `supported` reflects the platform (Linux/macOS/Windows all have a service
 *  layer); `installed`/`running`/`enabled` drive the buttons (null when not
 *  installed / indeterminate). */
export interface ServiceStatus {
  platform: string;
  supported: boolean;
  manager?: string;
  scope?: string;
  installed?: boolean;
  running?: boolean | null;
  enabled?: boolean | null;
  needs_privilege?: boolean;
}

/** Result of a service mutation (install/start/stop/restart/uninstall). */
export interface ServiceActionResult {
  ok: boolean;
  output: string;
}

/** Read the background service's status. Null in web mode. Needs no privilege
 *  on any platform (a plain status query), so it never prompts. */
export function serviceStatus(): Promise<ServiceStatus | null> {
  return tryInvoke<ServiceStatus>("service_status");
}

/** Run a service mutation. Unlike the graceful-degradation helpers, these use
 *  a raw invoke so a backend failure (a missing CLI, a declined UAC prompt, an
 *  `sc`/systemctl error) *throws* with its message instead of being swallowed
 *  to null — the caller surfaces it. No-op stub in web mode. */
async function serviceAction(cmd: string): Promise<ServiceActionResult> {
  if (!isTauri())
    return {
      ok: false,
      output: "The background service needs the desktop app",
    };
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke(cmd)) as ServiceActionResult;
}

export function serviceInstall(): Promise<ServiceActionResult> {
  return serviceAction("service_install");
}
export function serviceStart(): Promise<ServiceActionResult> {
  return serviceAction("service_start");
}
export function serviceStop(): Promise<ServiceActionResult> {
  return serviceAction("service_stop");
}
export function serviceRestart(): Promise<ServiceActionResult> {
  return serviceAction("service_restart");
}
export function serviceUninstall(): Promise<ServiceActionResult> {
  return serviceAction("service_uninstall");
}

/** Opt-in verbose diagnostics. The backend reads this local preference when it
 * starts; changing it never changes mesh traffic and takes effect after the
 * GUI/node backend is restarted. */
export function debugLoggingGet(): Promise<boolean | null> {
  return tryInvoke<boolean>("debug_logging_get");
}

export async function debugLoggingSet(enabled: boolean): Promise<boolean> {
  if (!isTauri()) return false;
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("debug_logging_set", { enabled })) as boolean;
}

/** Window/startup behaviour: whether closing / minimizing keeps AllMyStuff in
 *  the tray, and whether a login-item launch starts hidden. */
export interface WindowBehavior {
  close_to_tray: boolean;
  minimize_to_tray: boolean;
  start_minimized: boolean;
}

export function windowBehaviorGet(): Promise<WindowBehavior | null> {
  return tryInvoke<WindowBehavior>("window_behavior_get");
}

export function windowBehaviorSet(
  b: WindowBehavior,
): Promise<WindowBehavior | null> {
  return tryInvoke<WindowBehavior>("window_behavior_set", {
    closeToTray: b.close_to_tray,
    minimizeToTray: b.minimize_to_tray,
    startMinimized: b.start_minimized,
  });
}

/** Whether "Start with computer" (the OS login item) is registered. Null in
 *  web mode. */
export function autostartGet(): Promise<boolean | null> {
  return tryInvoke<boolean>("autostart_get");
}

/** Register / unregister the login item. Uses a raw invoke so a failure throws
 *  with its message rather than being swallowed to null. Returns the resulting
 *  state. Throws in web mode is avoided by the isTauri guard. */
export async function autostartSet(enabled: boolean): Promise<boolean> {
  if (!isTauri()) return false;
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke("autostart_set", { enabled })) as boolean;
}

/** Subscribe to live session snapshots. Returns an unlisten fn (or a no-op
 *  in web mode). */
export async function onSession(
  cb: (snap: SessionSnapshot) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<SessionSnapshot>("allmystuff://session", (e) => cb(e.payload));
}

export async function onSubscription(
  cb: (s: { status: string }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<{ status: string }>("allmystuff://subscription", (e) =>
    cb(e.payload),
  );
}

// ---- networks · identity · roster -------------------------------------
//
// Unlike the graph commands above (which degrade to null in web mode), these
// require a real daemon — the Networks panel only renders under Tauri. They
// throw on error so the UI can surface "couldn't create network", etc.

async function invokeReq<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (!isTauri())
    throw new Error(
      "Networks need the desktop app (no backend in the browser).",
    );
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke(cmd, args)) as T;
}

export const meshIdentity = () => invokeReq<IdentityInfo>("mesh_identity");

/** Set this device's display-name override (empty resets to the hostname). */
export const meshIdentitySetLabel = (label: string) =>
  invokeReq<unknown>("mesh_identity_set_label", { label });

// The daemon wraps these lists in an object (`{ networks }`, `{ peers }`,
// `{ roster }`) — matching MyOwnMesh's client, we unwrap to the inner array
// and never hand a non-array back (a non-array would crash the graph).
export async function meshNetworks(): Promise<NetworkSummary[]> {
  const r = await invokeReq<{ networks?: NetworkSummary[] }>("mesh_networks");
  return Array.isArray(r?.networks) ? r.networks : [];
}

export async function meshNetworkIdGenerate(): Promise<string> {
  const r = await invokeReq<{ network_id: string }>("mesh_network_id_generate");
  return r.network_id;
}

export const meshNetworkAdd = (config: unknown) =>
  invokeReq<unknown>("mesh_network_add", { config });

export const meshNetworkUpdate = (config: unknown) =>
  invokeReq<unknown>("mesh_network_update", { config });

export const meshNetworkRemove = (network: string) =>
  invokeReq<unknown>("mesh_network_remove", { network });

/** The networks currently switched off — their full parked configs (we
 *  only read the summary fields; the rest round-trips untouched). */
export async function disabledNetworks(): Promise<NetworkConfigFull[]> {
  const r = await invokeReq<NetworkConfigFull[]>("disabled_networks");
  return Array.isArray(r) ? r : [];
}

/** Switch a network off (leave the daemon, park the config) or back on
 *  (re-join from the parked config). Rosters survive on disk in between. */
export const setNetworkEnabled = (network: string, enabled: boolean) =>
  invokeReq<unknown>("network_set_enabled", { network, enabled });

/** Reconnect mesh transport *in place* — redial signaling and renegotiate ICE
 *  without leaving the room. The non-destructive twin of a leave+rejoin: peers
 *  keep their sessions and app-level state, so the connection comes back
 *  without stranding the other side. Resolution mirrors the daemon:
 *    - `network` set → every peer on that mesh (the global refresh).
 *    - `peer` only   → that one node, on the mesh it's reachable on (the
 *                      per-node refresh — the daemon resolves the network).
 *    - neither       → every joined mesh.
 *  `network` may be the config id or network id. No-op in web mode. */
export const networkReconnect = (
  opts: { network?: string; peer?: string } = {},
) =>
  invokeReq<unknown>("network_reconnect", {
    network: opts.network ?? null,
    peer: opts.peer ?? null,
  });

/** The whole daemon config (every network with its full signaling/STUN/TURN).
 *  The Servers settings pane reads this to populate its editor. */
export async function meshConfigShow(): Promise<NetworkConfigFull[]> {
  const r = await invokeReq<{ config?: { networks?: NetworkConfigFull[] } }>(
    "mesh_config_show",
  );
  return Array.isArray(r?.config?.networks) ? r.config!.networks! : [];
}

export async function meshRosterList(network: string): Promise<RosterPeer[]> {
  const r = await invokeReq<{ roster?: RosterPeer[] }>("mesh_roster_list", {
    network,
  });
  return Array.isArray(r?.roster) ? r.roster : [];
}

export const meshRosterApprove = (
  network: string,
  deviceId: string,
  label?: string,
) => invokeReq<unknown>("mesh_roster_approve", { network, deviceId, label });

export const meshRosterRemove = (network: string, deviceId: string) =>
  invokeReq<unknown>("mesh_roster_remove", { network, deviceId });

export async function meshPeers(network: string): Promise<PeerInfo[]> {
  const r = await invokeReq<{ peers?: PeerInfo[] }>("mesh_peers", { network });
  return Array.isArray(r?.peers) ? r.peers : [];
}

/** The node's daemon-link status as last emitted on
 *  `allmystuff://subscription` — poll-safe, for a window that subscribed
 *  after the one-shot event fired. Distinguishes "the node socket answers"
 *  (backend is up) from "the mesh behind it is live". */
export async function linkStatus(): Promise<{
  status: string;
  error?: string | null;
} | null> {
  if (!isTauri()) return null;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    return (await invoke("link_status")) as {
      status: string;
      error?: string | null;
    };
  } catch {
    return null; // node socket itself unreachable
  }
}

/** Raw daemon engine events (`allmystuff://event`) — the diagnostics the
 *  mesh already produces (the no-TURN hint after repeated ICE failures,
 *  relay/offline transitions). Forwarded verbatim by the node; the store
 *  maps the load-bearing ones onto the graph instead of letting them fall
 *  on the floor. */
export async function onMeshEvent(
  cb: (e: Record<string, unknown>) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<Record<string, unknown>>("allmystuff://event", (e) =>
    cb(e.payload),
  );
}

// MyOwnMesh's semi-public reference servers — the defaults a new network
// uses so two devices rendezvous on the *same* signaling relay (the usual
// reason "nothing connects": peers scattered across different public relays)
// and can traverse NAT via the shared STUN/TURN. All three are editable per
// network from Settings → Networks → Servers.
export const MYOWNMESH_SIGNALING = "wss://myownmesh.com";
export const MYOWNMESH_STUN = "stun:stun.myownmesh.com:3478";
export const MYOWNMESH_TURN_URL = "turn:turn.myownmesh.com:3478";
export const MYOWNMESH_TURN_USER = "guest";
export const MYOWNMESH_TURN_PASS = "theguestpassword";

function newNetworkInternalId(): string {
  return `net_${Math.random().toString(36).slice(2, 10)}_${Date.now().toString(36)}`;
}

/** Build the NetworkConfig payload `mesh_network_add` expects. Defaults the
 *  signaling relay + STUN + TURN to MyOwnMesh's reference servers so a freshly
 *  created/joined network connects out of the box; the Servers pane can change
 *  any of them later. Importing a network-settings file passes the file's own
 *  servers in instead — an empty list there is sent as "none" (the daemon
 *  resolves empty signaling to its built-in relay either way).
 *
 *  Auto-approve defaults **on**: every ordinary mesh in AllMyStuff is fully
 *  open — there's no per-mesh approval gate, so any node that joins is admitted
 *  automatically. Meshing is controlled by private venues, the Fleet, and
 *  Sharing, not by approving devices one by one. (The fleet's own closed mesh
 *  is created by the node, never through this builder, so it's never affected.)
 *  The node also enforces this on every non-fleet mesh it already holds, so an
 *  older mesh that predates the open default is migrated on the next launch. */
export function buildNetworkConfig(args: {
  networkId: string;
  label?: string;
  autoApprove?: boolean;
  signaling?: string[];
  stun?: string[];
  turn?: { url: string; username?: string; credential?: string }[];
}): Record<string, unknown> {
  const signaling = args.signaling ?? [MYOWNMESH_SIGNALING];
  const stun = args.stun ?? [MYOWNMESH_STUN];
  const turn = args.turn ?? [
    {
      url: MYOWNMESH_TURN_URL,
      username: MYOWNMESH_TURN_USER,
      credential: MYOWNMESH_TURN_PASS,
    },
  ];
  return {
    id: newNetworkInternalId(),
    network_id: args.networkId,
    label: args.label?.trim() || undefined,
    signaling: signaling.length > 0 ? { servers: signaling } : undefined,
    stun_servers:
      stun.length > 0 ? stun.map((u) => ({ urls: [u] })) : undefined,
    turn_servers:
      turn.length > 0
        ? turn.map((t) => ({
            urls: [t.url],
            username: t.username || undefined,
            credential: t.credential || undefined,
          }))
        : undefined,
    auto_approve: args.autoApprove ?? true,
  };
}

/** Save a network-settings envelope to a user-chosen `.json` file. Opens the
 *  native save dialog for the path, then writes via the backend (the webview
 *  can't write arbitrary paths itself). Returns the saved path, or null when
 *  the user cancels (or there's no desktop backend). */
export async function exportNetworkFile(
  defaultName: string,
  envelope: unknown,
): Promise<string | null> {
  if (!isTauri()) return null;
  const { save } = await import("@tauri-apps/plugin-dialog");
  const path = await save({
    defaultPath: defaultName,
    filters: [{ name: "AllMyStuff network settings", extensions: ["json"] }],
  });
  if (!path) return null;
  await invokeReq<unknown>("mesh_network_export_file", {
    path,
    config: envelope,
  });
  return path;
}
