// A believable starter graph so the app is alive the moment it opens —
// before any mesh is joined or any scan has run. The Tauri backend
// replaces `this` with a real scan of the machine and the peers with real
// presence adverts; the shapes are identical, so nothing downstream knows
// the difference.

import type { Capability, Catalog, Flow, MediaKind, MeshNode } from "./types";

function cap(
  node: string,
  id: string,
  label: string,
  media: MediaKind,
  flow: Flow,
  origin: string,
  isDefault = false,
): Capability {
  return { id: `${node}:${id}`, node, label, media, flow, origin, default: isDefault };
}

/** The synthetic "the machine itself" set every node exposes (mirrors the
 *  bridge crate: screen out, control in, keyboard & mouse out, audio both,
 *  video in — the landing spot for camera streams — and the clipboard). */
function machineCaps(node: string): Capability[] {
  return [
    cap(node, "screen", "Screen", "display", "source", "screen"),
    cap(node, "control", "Keyboard & mouse control", "input", "sink", "control"),
    cap(node, "keyboard-mouse", "Keyboard & mouse", "input", "source", "controller"),
    cap(node, "system-audio", "System audio", "audio", "duplex", "system"),
    cap(node, "video-in", "Video in", "video", "sink", "viewer"),
    cap(node, "clipboard", "Clipboard", "clipboard", "duplex", "clipboard"),
  ];
}

export function demoCatalog(): Catalog {
  const nodes: MeshNode[] = [
    {
      id: "this",
      label: "My MacBook",
      kind: "this",
      relationship: { kind: "mine" },
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera"],
      owner: "this",
      summary: { os: "macOS 14", cpu: "Apple M2", ram_bytes: 16 * 2 ** 30, device_count: 13 },
    },
    {
      id: "desk",
      label: "Desk PC",
      kind: "machine",
      relationship: { kind: "mine" },
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera", "sites"],
      owner: "this",
      summary: { os: "Windows 11", cpu: "Ryzen 7 7700", ram_bytes: 32 * 2 ** 30, device_count: 11 },
      // A couple of self-hosted services it exposes — a Grafana dashboard
      // (web) and a Postgres (a bare TCP service the proxy still tunnels).
      sites: [
        { id: "tcp:3000", label: "HTTP", port: 3000, scheme: "http", loopback: true },
        { id: "tcp:5432", label: "PostgreSQL", port: 5432, scheme: "postgres", loopback: true },
      ],
    },
    {
      id: "tv",
      label: "Living-room TV",
      kind: "machine",
      relationship: { kind: "mine" },
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera", "sites"],
      owner: "this",
      summary: { os: "Linux", cpu: "Amlogic S905", ram_bytes: 4 * 2 ** 30, device_count: 5 },
      // A media server the living-room box runs.
      sites: [{ id: "tcp:8096", label: "HTTP", port: 8096, scheme: "http", loopback: false }],
    },
    {
      id: "studio",
      label: "Conference puck",
      kind: "machine",
      relationship: { kind: "mine" },
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera"],
      owner: "this",
      summary: { os: "Linux", cpu: "Pi 5", ram_bytes: 8 * 2 ** 30, device_count: 6 },
    },
    {
      // A KVM appliance (a NanoKVM-class device) you own, wired into the Desk
      // PC: it carries its own web UI as a site and is attached to "desk", so
      // its card buttons (Open KVM / reboot / attach link), the tether wire to
      // the Desk PC, and the Desk PC's power/reset + "Controlled by KVM"
      // affordances all light up in the preview. Its joining mesh + membership
      // list drive the drawer's Meshes/Unclaim shelves.
      id: "kvm",
      label: "KVM-Desk PC",
      kind: "machine",
      relationship: { kind: "mine" },
      online: true,
      app: true,
      features: ["kvm", "sites"],
      owner: "this",
      summary: { os: "Linux", cpu: "SG2002", ram_bytes: 256 * 2 ** 20, device_count: 2 },
      sites: [{ id: "tcp:80", label: "KVM web UI", port: 80, scheme: "http", loopback: false }],
      kvm: {
        attachedTo: "desk",
        web: "tcp:80",
        joiningMesh: "cec-kvm-ab3de-fg7hj",
        meshes: ["amber-turing-x3k9q", "den-site-mesh"],
      },
    },
    {
      // A NanoKVM-Pro appliance booted in claim mode — same KVM-class device,
      // just a different SoC (AX630C / 1 GB instead of the NanoKVM's SG2002 /
      // 256 MB). It advertises the identical wire shape (features kvm+sites, a
      // kvm block with its joining mesh), so it drops into the graph and the
      // claim sheet exactly like the NanoKVM above — nothing here is
      // model-specific. Note the site scheme is "http" even though the Pro's own
      // web UI defaults to https: the sites tunnel serves plaintext in-process
      // HTTP, so the advertised scheme is what the viewer speaks to its local
      // end of the tunnel.
      id: "kvm-pro",
      label: "NanoKVM-Pro",
      kind: "machine",
      relationship: { kind: "unclaimed" },
      online: true,
      app: true,
      features: ["kvm", "sites"],
      owner: null,
      claimable: true,
      summary: { os: "Linux", cpu: "AX630C", ram_bytes: 2 ** 30, device_count: 2 },
      sites: [{ id: "tcp:443", label: "KVM web UI", port: 443, scheme: "http", loopback: false }],
      kvm: {
        web: "tcp:443",
        joiningMesh: "cec-kvm-pk4wq-2n8vr",
        meshes: ["cec-kvm-pk4wq-2n8vr"],
      },
    },
    {
      // A spare box that was booted in claim mode — it's offering itself for
      // adoption, so it can be claimed (Task 4). Runs AllMyStuff (has caps).
      id: "nuc",
      label: "Spare NUC",
      kind: "machine",
      relationship: { kind: "unclaimed" },
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera"],
      owner: null,
      claimable: true,
      summary: { os: "Linux", cpu: "Intel N100", ram_bytes: 16 * 2 ** 30, device_count: 7 },
    },
    {
      // A device that's on the mesh but isn't running AllMyStuff (Task 1):
      // no presence advert, so no wireable capabilities. Shown, but quiet
      // and not a connection target.
      id: "garage",
      label: "garage-sensor",
      kind: "machine",
      relationship: { kind: "unclaimed" },
      online: true,
      app: false,
    },
    {
      id: "alex",
      label: "Alex's laptop",
      kind: "machine",
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera", "sites"],
      owner: "alex",
      sites: [{ id: "tcp:8080", label: "HTTP", port: 8080, scheme: "http", loopback: true }],
      relationship: {
        kind: "shared",
        person: { id: "person:alex", name: "Alex" },
        grants: [
          // Share-OUT: you've let Alex's fleet open My MacBook's screen + terminal
          // (scoped to "this" device), so "What Alex can do" lists them and
          // Manage share pre-fills Video + Terminal.
          { id: "g-out-screen", media: "display", role: "provide", capability: "this:screen", label: "My MacBook: see its screen" },
          { id: "g-out-term", media: "generic", role: "provide", capability: "this:terminal", label: "My MacBook: use its terminal" },
          // What Alex's fleet shared back with you: their laptop's consoles, so
          // its card + drawer light up the Remote / Files / Terminal / Sites
          // buttons. Provide = they provide their screen; you consume into them
          // for control. Scoped to alex's caps so only this device unlocks.
          { id: "g-alex-screen", media: "display", role: "provide", capability: "alex:screen", label: "Alex's laptop: see its screen" },
          { id: "g-alex-control", media: "input", role: "consume", capability: "alex:control", label: "Alex's laptop: control it" },
          { id: "g-alex-files", media: "storage", role: "both", capability: "alex:files", label: "Alex's laptop: use its files" },
          { id: "g-alex-term", media: "generic", role: "provide", capability: "alex:terminal", label: "Alex's laptop: use its terminal" },
          { id: "g-alex-sites", media: "generic", role: "provide", capability: "alex:sites", label: "Alex's laptop: reach its sites" },
        ],
      },
    },
    {
      // A second machine of Alex's that hasn't been marked shared yet: it
      // declares an owner that isn't us, so its chip reads "someone
      // else's" (not "unclaimed") — and marking it shared folds it into
      // the same Alex connection, where the existing grants already apply.
      id: "alex-tablet",
      label: "Alex's tablet",
      kind: "machine",
      online: true,
      app: true,
      features: ["terminal", "files", "rooms", "camera"],
      owner: "alex",
      relationship: { kind: "unclaimed" },
      summary: { os: "Android 15", cpu: "Snapdragon 8", ram_bytes: 8 * 2 ** 30, device_count: 6 },
    },
  ];

  const capabilities: Capability[] = [
    // My MacBook. The trailing `true` marks each category's current default.
    ...machineCaps("this"),
    cap("this", "mic", "MacBook mic array", "audio", "source", "microphone", true),
    cap("this", "speakers", "MacBook speakers", "audio", "sink", "speaker", true),
    cap("this", "cam", "FaceTime HD camera", "video", "source", "camera", true),
    cap("this", "display", "Built-in Retina display", "display", "sink", "display", true),
    cap("this", "keyboard", "Magic Keyboard", "input", "source", "keyboard"),
    cap("this", "trackpad", "Trackpad", "input", "source", "touchpad"),
    // Desk PC — the Yeti is the default mic, the Dell the default screen.
    ...machineCaps("desk"),
    cap("desk", "monitor", "Dell U2720Q", "display", "sink", "display", true),
    cap("desk", "speakers", "Desk speakers", "audio", "sink", "speaker", true),
    cap("desk", "yeti", "Blue Yeti", "audio", "source", "microphone", true),
    cap("desk", "data", "Data (D:)", "storage", "duplex", "storage"),
    // Living-room TV.
    ...machineCaps("tv"),
    cap("tv", "oled", "LG OLED", "display", "sink", "display", true),
    cap("tv", "soundbar", "Sonos soundbar", "audio", "sink", "speaker", true),
    // Conference puck (the 6-mic array is its default capture device).
    ...machineCaps("studio"),
    cap("studio", "array", "ReSpeaker 6-mic array", "audio", "source", "microphone", true),
    cap("studio", "speaker", "Puck speaker", "audio", "sink", "speaker", true),
    cap("studio", "cam", "Puck wide camera", "video", "source", "camera", true),
    // Spare NUC (claimable) — has its own devices, ready once adopted.
    ...machineCaps("nuc"),
    cap("nuc", "hdmi", "HDMI capture", "display", "sink", "display", true),
    cap("nuc", "line", "Line out", "audio", "sink", "speaker", true),
    // Alex (shared).
    ...machineCaps("alex"),
    cap("alex", "cam", "Alex's webcam", "video", "source", "camera", true),
    cap("alex", "monitor", "Alex's monitor", "display", "sink", "display", true),
    cap("alex", "mic", "Alex's headset mic", "audio", "source", "microphone", true),
    // Alex's tablet (owned by Alex, not yet marked shared).
    ...machineCaps("alex-tablet"),
    cap("alex-tablet", "screen-tab", "Tablet screen", "display", "sink", "display", true),
  ];

  // A few pre-wired routes (all between things I own, so all valid).
  const routes = [
    { id: "route:this:system-audio→desk:speakers", from: "this:system-audio", to: "desk:speakers", media: "audio" as MediaKind },
    { id: "route:desk:screen→tv:oled", from: "desk:screen", to: "tv:oled", media: "display" as MediaKind },
  ];

  return { nodes, capabilities, routes };
}
