<script lang="ts">
  import { tick } from "svelte";
  import { app } from "../store.svelte";
  import { isMobile } from "../tauri";
  import { displayName, mediaColor, humanBytes, isAppNode, MEDIA, type MediaKind } from "../types";
  import type { MeshNode } from "../types";
  import DrivePanel from "./DrivePanel.svelte";

  // Phone/tablet shell: a finger drag only ever pans/scrolls the view — it
  // never marquees and never starts the drag-to-share gesture. Taps still
  // select (the device drawer opens from that). Everything below that touches
  // pointer behaviour is gated on this so desktop stays exactly as it was.
  const mobile = isMobile();
  const normalMode = $derived(app.uiMode === "normal");

  // Viewport size tracked via ResizeObserver so the layout fits its container
  // (same approach as the MyOwnMesh NodeMap). We OBSERVE the outer canvas —
  // whose size is fixed by the surrounding flex layout and never depends on the
  // graph's own scroll content — so the observer can't feed back into itself.
  // (Observing the inner scroller instead lets a sub-pixel horizontal-scrollbar
  // toggle change the observed box and re-fire the observer inside its own
  // delivery: "ResizeObserver loop completed with undelivered notifications".)
  // We READ the scroll viewport's client box, though, so `width` is the space
  // actually available inside any reserved scrollbar gutter and the grid never
  // spills a stray horizontal scrollbar. Integers only (clientWidth/Height are
  // integers; floor guards against a fractional fallback) so the stage lands
  // exactly on the client width with no sub-pixel overflow.
  let width = $state(1000);
  let height = $state(700);
  let canvas = $state<HTMLDivElement | null>(null);

  function measureViewport() {
    // In list mode the scroll viewport is display:none, so its client box reads
    // 0 and would collapse the measurement to the floor — measure the canvas
    // itself there (it's always laid out). Graph views keep reading the
    // scroller so `width` accounts for any reserved scrollbar gutter.
    const box = isList ? canvas : (scroller ?? canvas);
    if (!box) return;
    width = Math.max(360, Math.floor(box.clientWidth));
    height = Math.max(320, Math.floor(box.clientHeight));
  }

  $effect(() => {
    if (!canvas) return;
    const ro = new ResizeObserver(() => measureViewport());
    ro.observe(canvas);
    measureViewport();
    return () => ro.disconnect();
  });

  const nodeW = $derived(normalMode ? 340 : 214);
  const nodeH = $derived(normalMode ? 166 : 86);

  type Placed = { node: MeshNode; x: number; y: number };

  // ---- what the graph draws ---------------------------------------------
  //
  // The graph is the WORKING SET, not the archive: every machine that's
  // reachable right now, plus your own devices even when they're off (a
  // fleet box being down is exactly what you want to notice here). Anything
  // else that's merely rostered-but-disconnected — someone's laptop that
  // joined a mesh once, a shared device that's asleep — stays off the
  // canvas until it comes back; it's still in Settings → Devices and in
  // each mesh's own roster pane, which are the places to go looking for
  // offline members. Ownership reads the same roster-authoritative
  // `standing()` as everything else, so an evicted device doesn't cling to
  // the graph on a stale advert.
  const graphNodes = $derived.by((): MeshNode[] =>
    app.catalog.nodes.filter(
      (n) =>
        !app.isCecOnlyNode(n) &&
        (n.kind === "this" || n.online || app.standingOf(n).ownedByMe),
    ),
  );

  // Power is the attached computer's physical LED input, not the KVM node's
  // mesh-online state. Watch every usable KVM represented on the graph once;
  // both its own card and its attached machine's card read the same store key.
  $effect(() => {
    const ids = [
      ...new Set(
        graphNodes
          .filter(
            (kvm) =>
              app.isKvm(kvm) &&
              (app.kvmDoors(kvm) ||
                graphNodes.some(
                  (target) => !app.isKvm(target) && app.kvmPassthroughDoors(kvm, target),
                )),
          )
          .map((n) => n.id),
      ),
    ];
    const unwatch = ids.map((id) => app.watchKvmPower(id));
    return () => unwatch.forEach((stop) => stop());
  });

  // ---- fleet grouping -------------------------------------------------
  //
  // Every node belongs to exactly one fleet: yours (this device, your
  // owned-fleet co-members, anything you own), a named fleet per *other*
  // owner (people bring fleets — their devices group under them), or
  // "Unknown fleet" for devices that advertise no owner at all (including
  // bare mesh endpoints not running AllMyStuff). Both views read this:
  // the ring seats fleets together, the grid draws them as sections.

  type FleetGroup = { key: string; label: string; nodes: MeshNode[] };

  const mineLabel = (): string =>
    app.fleetName
      ? `${app.fleetName}'s fleet`
      : app.inFleet
        ? "Your fleet"
        : "Your devices";

  function fleetKeyOf(n: MeshNode): { key: string; label: string } {
    // Group by the authoritative `standing()` — the same roster-driven verdict
    // the drawer and every claim/fleet button read — NOT the node's stored
    // `relationship.kind` or its self-advertised `owner`. Those are stale: an
    // evicted/orphaned device keeps a "mine" relationship and keeps advertising
    // "owner = you" for a while, which used to pin it inside your fleet group on
    // the graph long after the signed roster dropped it (the graph-vs-settings
    // divergence). `standing()` only honours the advert when you hold no fleet
    // of your own, so once you do, an evicted device falls to "Unknown fleet"
    // and an unclaimed one never enters your fleet. Because `standing()` reads
    // the reactive fleet roster, a refresh (which reloads it) re-groups — and
    // re-lays-out — these nodes automatically.
    const st = app.standingOf(n);
    if (n.kind === "this") {
      // "This" anchors your group while it's genuinely in a fleet, owns other
      // machines, or is the only device on the graph; otherwise a fleet-less
      // local device drops to "Unknown fleet" rather than a group of one.
      const ownsOthers = graphNodes.some(
        (m) => m.id !== n.id && app.standingOf(m).ownedByMe,
      );
      // "Alone" means alone ON THE GRAPH — hidden offline strangers don't
      // stop a fleet-less local device from anchoring its own group.
      const aloneHere =
        graphNodes.filter((m) => m.id !== n.id && isAppNode(m)).length === 0;
      return st.inFleet || ownsOthers || aloneHere
        ? { key: "mine", label: mineLabel() }
        : { key: "unknown", label: "Unknown fleet" };
    }
    if (st.ownedByMe) {
      return { key: "mine", label: mineLabel() };
    }
    if (st.shared) {
      return { key: `fleet:${st.shared.id}`, label: `${st.shared.name}'s fleet` };
    }
    if (st.ownedByOther) {
      const person = app.personFor(n);
      return { key: `fleet:${person.id}`, label: `${person.name}'s fleet` };
    }
    return { key: "unknown", label: "Unknown fleet" };
  }

  // Bin every node into fleet groups: `keyOf` decides which group a node lands
  // in (and how it's labelled), `groupRank` decides the group order. Within a
  // group this device leads and the rest read alphabetically. Both the graph
  // (grid bands + radial seating) and the list drive this, so their grouping
  // and sort can't drift — only the two callbacks differ.
  function buildFleetGroups(
    keyOf: (n: MeshNode) => { key: string; label: string },
    groupRank: (g: FleetGroup) => number,
  ): FleetGroup[] {
    const groups = new Map<string, FleetGroup>();
    for (const n of graphNodes) {
      const { key, label } = keyOf(n);
      const g = groups.get(key) ?? { key, label, nodes: [] };
      g.nodes.push(n);
      groups.set(key, g);
    }
    for (const g of groups.values()) {
      g.nodes.sort((a, b) => {
        const rank = (n: MeshNode) => (n.kind === "this" ? 0 : 1);
        return rank(a) - rank(b) || a.label.localeCompare(b.label);
      });
    }
    return [...groups.values()].sort(
      (a, b) => groupRank(a) - groupRank(b) || a.label.localeCompare(b.label),
    );
  }

  // Your fleet first, named fleets alphabetically, the unknowns last.
  const fleetGroups = $derived.by((): FleetGroup[] =>
    buildFleetGroups(fleetKeyOf, (g) => (g.key === "mine" ? 0 : g.key === "unknown" ? 2 : 1)),
  );

  // Both modes organize devices by fleet only. Mesh membership remains useful
  // networking detail, but never creates another visual hierarchy.
  type MeshSub = { key: string; label: string; nodes: MeshNode[] };
  function displaySubsOf(nodes: MeshNode[]): MeshSub[] {
    return [{ key: "fleet", label: "", nodes }];
  }

  // ---- views ------------------------------------------------------------
  //
  // Three layouts over the same nodes: the grouped grid (one labelled section
  // per fleet — the desktop default), the radial ("this" centred, fleets
  // seated together around the ring), and the list (fleet-grouped rows with a
  // search bar — the only view on phones, a third option on desktop). The
  // graph views are switched from the zoom controls; the list from the same
  // toggle bar (hidden on mobile, where it's the only view there is).

  type ViewMode = "radial" | "grid" | "list";
  const VIEW_STORE_KEY = "allmystuff.graphView.v1";
  let view = $state<ViewMode>(loadView());
  // The list is the single, un-switchable view on the phone/tablet shell — a
  // scrolling, fleet-grouped roster reads far better on a narrow touch screen
  // than a pannable graph. Desktop keeps all three.
  const isList = $derived(view === "list");

  function loadView(): ViewMode {
    // Phones only ever get the list — it's the whole point of this view.
    if (mobile) return "list";
    // On desktop grid is the default; an explicit stored "radial"/"list" opts
    // out, so a fresh install lands on the grouped grid.
    try {
      const stored = localStorage.getItem(VIEW_STORE_KEY);
      return stored === "radial" || stored === "list" ? stored : "grid";
    } catch {
      return "grid";
    }
  }

  async function setView(v: ViewMode) {
    // The phone has no toggle, so this only ever runs on desktop — but guard
    // anyway so mobile can never be knocked off the list.
    if (mobile) return;
    if (normalMode && v === "radial") v = "grid";
    view = v;
    panX = 0;
    panY = 0;
    zoom = 1;
    scrollLeft = 0;
    scrollTop = 0;
    try {
      localStorage.setItem(VIEW_STORE_KEY, v);
    } catch {
      /* private mode — the toggle just doesn't persist */
    }
    // Wait for the new view to render, then re-measure (the scrollbar gutter is
    // reserved in grid but not radial, so the usable width changes) and park the
    // scroller at the top so the new view starts from a clean position.
    await tick();
    measureViewport();
    if (scroller) {
      scroller.scrollLeft = 0;
      scroller.scrollTop = 0;
    }
    if (listScroll) listScroll.scrollTop = 0;
  }

  // The grid's geometry: one section per fleet, nodes wrapped into rows —
  // sub-banded by mesh when the fleet spans more than one mesh combination.
  const GRID_MARGIN = 28;
  const cellW = $derived(nodeW + 26);
  const cellH = $derived(nodeH + 64); // node + meta rows + breathing room
  const SECTION_HEAD = 34;
  const SECTION_SUBHEAD = 24; // one mesh sub-band label inside a section
  const SECTION_GAP = 26;
  const SECTION_PAD = 14;

  type Section = { key: string; label: string; x: number; y: number; w: number; h: number; count: number };
  type SubHead = { key: string; label: string; x: number; y: number; w: number };

  const gridLayout = $derived.by(
    (): { placed: Placed[]; sections: Section[]; subheads: SubHead[]; height: number } => {
      const placed: Placed[] = [];
      const sections: Section[] = [];
      const subheads: SubHead[] = [];
      const cols = Math.max(1, Math.floor((width - 2 * GRID_MARGIN) / cellW));
      let y = GRID_MARGIN;
      for (const g of fleetGroups) {
        // The section keeps one column count for the whole fleet, so the mesh
        // sub-bands stay aligned instead of each re-fitting its own width.
        const useCols = Math.min(cols, Math.max(1, g.nodes.length));
        const w = useCols * cellW + 2 * SECTION_PAD;
        const x0 = Math.max(GRID_MARGIN, (width - w) / 2);
        const subs = displaySubsOf(g.nodes);
        const showSubs = subs.length > 1;
        let innerY = y + SECTION_HEAD;
        for (const sub of subs) {
          if (showSubs) {
            subheads.push({
              key: `${g.key}//${sub.key}`,
              label: sub.label,
              x: x0 + SECTION_PAD,
              y: innerY,
              w: w - 2 * SECTION_PAD,
            });
            innerY += SECTION_SUBHEAD;
          }
          sub.nodes.forEach((n, i) => {
            const col = i % useCols;
            const row = Math.floor(i / useCols);
            placed.push({
              node: n,
              x: x0 + SECTION_PAD + col * cellW + cellW / 2,
              y: innerY + row * cellH + cellH / 2 - 10,
            });
          });
          innerY += Math.ceil(sub.nodes.length / useCols) * cellH;
        }
        const h = innerY - y + SECTION_PAD;
        sections.push({ key: g.key, label: g.label, x: x0, y, w, h, count: g.nodes.length });
        y += h + SECTION_GAP;
      }
      // A closing margin so the last section isn't flush against the scroll end,
      // and so a drop-out drawer opening under a last-row card has room below it.
      return { placed, sections, subheads, height: y + GRID_MARGIN };
    },
  );

  // Radial layout: "this" in the middle, everything else on a ring seated
  // by fleet — your devices first, then each named fleet, unknowns last —
  // so the eye reads "my fleet, then everyone else's."
  const radialLayout = $derived.by((): Placed[] => {
    const cx = width / 2;
    const cy = height / 2;
    // Centre the local machine by its definitive marker (`kind === "this"`),
    // not by id: a presence snapshot can move `localId` to the real session id
    // before a scan re-homes the node off its first-scan placeholder, and we
    // must never leave the centre empty with "me" stranded out on the ring.
    const me =
      graphNodes.find((n) => n.kind === "this") ??
      graphNodes.find((n) => n.id === app.localId);
    const others = fleetGroups.flatMap((g) => g.nodes).filter((n) => n.id !== me?.id);
    const placed: Placed[] = [];
    if (me) placed.push({ node: me, x: cx, y: cy });
    const radius = Math.max(180, Math.min(width, height) / 2 - 130);
    others.forEach((n, i) => {
      const a = -Math.PI / 2 + (i * 2 * Math.PI) / Math.max(1, others.length);
      placed.push({ node: n, x: cx + Math.cos(a) * radius, y: cy + Math.sin(a) * radius });
    });
    return placed;
  });

  // The list has no on-canvas geometry (it flows as real DOM rows), so both
  // the placed-node layout and the fleet bands are empty there — no wires, no
  // marquee, nothing to pan.
  const layout = $derived(
    view === "grid" ? gridLayout.placed : view === "radial" ? radialLayout : [],
  );
  const sections = $derived(view === "grid" ? gridLayout.sections : []);
  const subheads = $derived(view === "grid" ? gridLayout.subheads : []);

  // ---- list view --------------------------------------------------------
  //
  // The same fleet groups as the graph, but rendered as scrolling rows with a
  // search bar tucked against the top edge. Two things differ from the graph
  // grouping: every claimable device is lifted into a single "Ready to claim"
  // pseudo-fleet that always sits directly under Your Fleet (claiming is the
  // first thing you do with a new box, so it rides up top), and the search
  // narrows the rows while keeping every surviving device under its fleet.

  const CLAIMABLE_KEY = "claimable";

  let listQuery = $state("");
  let listScroll = $state<HTMLDivElement | null>(null);
  const listQueryNorm = $derived(listQuery.trim().toLowerCase());

  /** The searchable text for one device — its name(s), OS/CPU, and the meshes
   *  it's on — so a query matches what a person can actually see on the row. */
  function nodeHaystack(n: MeshNode): string {
    return [
      n.label,
      n.hostname,
      displayName(n),
      n.summary?.os,
      n.summary?.cpu,
      ...(n.networks ?? []),
    ]
      .filter(Boolean)
      .join(" ")
      .toLowerCase();
  }

  // The list's fleet groups: the same grouping as the graph, but every
  // claimable device is hoisted into a single "Ready to claim" pseudo-fleet
  // that ranks right behind Your Fleet (claiming is the first thing you do with
  // a new box), ahead of the named fleets, with the unknowns last.
  const listGroups = $derived.by((): FleetGroup[] =>
    buildFleetGroups(
      (n) =>
        app.standingOf(n).claimable
          ? { key: CLAIMABLE_KEY, label: "Ready to claim" }
          : fleetKeyOf(n),
      (g) => (g.key === "mine" ? 0 : g.key === CLAIMABLE_KEY ? 1 : g.key === "unknown" ? 3 : 2),
    ),
  );

  // The groups after the search filter — still fully fleet-grouped: a group
  // whose *label* matches keeps all its devices (search "Alex" → all of Alex's
  // fleet), otherwise only the matching devices survive, and empty groups drop
  // out entirely. An empty query passes everything through untouched.
  const filteredListGroups = $derived.by((): FleetGroup[] => {
    const q = listQueryNorm;
    if (!q) return listGroups;
    const out: FleetGroup[] = [];
    for (const g of listGroups) {
      const labelHit = g.label.toLowerCase().includes(q);
      const nodes = labelHit ? g.nodes : g.nodes.filter((n) => nodeHaystack(n).includes(q));
      if (nodes.length > 0) out.push({ ...g, nodes });
    }
    return out;
  });

  function listGroupIcon(key: string): string {
    return key === "mine" ? "🔗" : key === CLAIMABLE_KEY ? "＋" : key === "unknown" ? "❓" : "🧑";
  }

  const posOf = $derived.by(() => {
    const m = new Map<string, Placed>();
    for (const p of layout) m.set(p.node.id, p);
    return m;
  });

  // Edges: one per route, connecting the two nodes the capabilities live
  // on. Curved + coloured by media, with parallel routes fanned apart.
  // Endpoints resolve through the display fallback so a live terminal
  // session (whose endpoints aren't catalog capabilities) draws its wire.
  type Edge = { id: string; x1: number; y1: number; x2: number; y2: number; cx: number; cy: number; color: string; media: MediaKind };
  const edges = $derived.by((): Edge[] => {
    const pairCount = new Map<string, number>();
    const out: Edge[] = [];
    for (const r of app.catalog.routes) {
      const from = app.capabilityForDisplay(r.from);
      const to = app.capabilityForDisplay(r.to);
      if (!from || !to) continue;
      const a = posOf.get(from.node);
      const b = posOf.get(to.node);
      if (!a || !b || a === b) continue;
      const key = [from.node, to.node].sort().join("|");
      const idx = pairCount.get(key) ?? 0;
      pairCount.set(key, idx + 1);
      const mx = (a.x + b.x) / 2;
      const my = (a.y + b.y) / 2;
      // Perpendicular offset so multiple wires between the same pair fan.
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const len = Math.hypot(dx, dy) || 1;
      const off = (idx % 2 === 0 ? 1 : -1) * Math.ceil((idx + 1) / 2) * 26;
      out.push({
        id: r.id,
        x1: a.x,
        y1: a.y,
        x2: b.x,
        y2: b.y,
        cx: mx + (-dy / len) * off,
        cy: my + (dx / len) * off,
        color: mediaColor(r.media as MediaKind),
        media: r.media as MediaKind,
      });
    }
    return out;
  });

  // KVM tethers: one per attached KVM, drawn from the KVM to the machine it
  // physically controls (its advertised `kvm.attachedTo`). Not a media route —
  // there's nothing flowing to color or animate — just the physical wiring
  // made visible, so the graph reads "this KVM is plugged into that box".
  type Tether = { id: string; x1: number; y1: number; x2: number; y2: number };
  const kvmTethers = $derived.by((): Tether[] => {
    const out: Tether[] = [];
    for (const n of graphNodes) {
      if (!app.isKvm(n)) continue;
      const target = app.kvmTargetNode(n);
      if (!target) continue;
      const a = posOf.get(n.id);
      const b = posOf.get(target.id);
      if (!a || !b || a === b) continue;
      out.push({ id: `kvm-tether:${n.id}`, x1: a.x, y1: a.y, x2: b.x, y2: b.y });
    }
    return out;
  });

  // Virtual media is a real source-to-KVM relationship: the selected disk or
  // image is streamed by the source node and exposed by the KVM's USB gadget.
  // Keep that distinct from both ordinary app routes and the physical tether.
  const kvmMediaTethers = $derived.by((): Tether[] => {
    const out: Tether[] = [];
    for (const kvm of graphNodes) {
      const media = kvm.kvm?.virtualMedia;
      if (!media) continue;
      const source = app.machineByAnyId(media.source);
      if (!source) continue;
      const a = posOf.get(source.id);
      const b = posOf.get(kvm.id);
      if (!a || !b || a === b) continue;
      out.push({ id: `kvm-media:${kvm.id}:${media.source}`, x1: a.x, y1: a.y, x2: b.x, y2: b.y });
    }
    return out;
  });

  // A small label that follows the cursor along a connection wire, naming what
  // flows down it (1–2 words). Placed in canvas coordinates so it sits right
  // under the pointer — zero eye travel.
  let lineTip = $state<{ x: number; y: number; text: string } | null>(null);
  function onWireMove(e: PointerEvent, media: MediaKind) {
    const rect = canvas?.getBoundingClientRect();
    if (!rect) return;
    lineTip = { x: e.clientX - rect.left, y: e.clientY - rect.top, text: MEDIA[media].label };
  }
  function onWireLeave() {
    lineTip = null;
  }

  // ---- pan / zoom / select ------------------------------------------
  //
  // Matching every graph and design tool: right-drag pans, left-drag on empty
  // space marquee-selects, and a left-drag from one device onto another opens
  // the share builder. A plain left-click still selects a single device.
  let panX = $state(0);
  let panY = $state(0);
  let zoom = $state(1);

  // Normal has exactly two unscaled document views. If someone switches over
  // from an Advanced radial/zoomed workspace, land them on a clean grid.
  $effect(() => {
    if (!normalMode || mobile) return;
    if (view === "radial") view = "grid";
    if (zoom !== 1) zoom = 1;
    panX = 0;
    panY = 0;
  });
  const MIN_ZOOM = 0.4;
  const MAX_ZOOM = 2.2;
  const clampZoom = (z: number) => Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, z));

  // Grid view is navigated like a document — the wheel scrolls, and zoom is the
  // deliberate Ctrl/⌘ gesture. Native scrolling on a real scroll container does
  // the panning, so we track its offset here (mirrored into $state on `scroll`)
  // and keep the pan/zoom transform only for the radial view. `isGrid` gates
  // every place the two navigation models diverge.
  const isGrid = $derived(view === "grid");
  let scroller = $state<HTMLDivElement | null>(null);
  let scrollLeft = $state(0);
  let scrollTop = $state(0);
  let hovering = $state(false);
  function onScroll() {
    if (!scroller) return;
    scrollLeft = scroller.scrollLeft;
    scrollTop = scroller.scrollTop;
  }

  // The scroll "stage": the grid is laid out at its natural width (the viewport
  // width — sections centre themselves within it) and full height, then scaled
  // by `zoom`. The stage element is sized to the *scaled* content so the browser
  // gives it a real scrollbar; a short list still sits at the top. `centreX`
  // re-centres a zoomed-out grid so it doesn't hug the left edge, matching the
  // centred look at 100%. Radial keeps its viewport-sized, transform-panned stage.
  const contentH = $derived(isGrid ? gridLayout.height : height);
  const centreX = $derived(isGrid ? Math.max(0, (width - width * zoom) / 2) : 0);
  const stageW = $derived(isGrid ? Math.max(width, width * zoom) : width);
  const stageH = $derived(isGrid ? contentH * zoom : height);
  // The on-screen offset of a laid-out point: `screenX = p.x * zoom + offX`.
  // In grid that's the centring offset minus the scroll; in radial it's the pan.
  const offX = $derived(isGrid ? centreX - scrollLeft : panX);
  const offY = $derived(isGrid ? -scrollTop : panY);
  // The transform on the node + wire layers. Grid never translates by pan (the
  // scroller does that); it only centres and scales.
  const layerTransformCss = $derived(
    isGrid
      ? `translate(${centreX}px, 0px) scale(${zoom})`
      : `translate(${panX}px, ${panY}px) scale(${zoom})`,
  );
  const layerTransformSvg = $derived(
    isGrid
      ? `translate(${centreX} 0) scale(${zoom})`
      : `translate(${panX} ${panY}) scale(${zoom})`,
  );

  const clampScroll = (v: number, max: number) => Math.max(0, Math.min(v, Math.max(0, max)));

  /** Zoom toward a viewport point (px, py in canvas coordinates), keeping the
   *  content under that point fixed. Radial nudges the pan; grid re-derives the
   *  scroll offset after the stage has resized (hence the `tick`). */
  async function applyZoom(px: number, py: number, target: number) {
    const next = clampZoom(target);
    if (next === zoom) return;
    if (isGrid) {
      const sc = scroller;
      const prevLeft = sc?.scrollLeft ?? 0;
      const prevTop = sc?.scrollTop ?? 0;
      const prevCentre = Math.max(0, (width - width * zoom) / 2);
      // The content-space point under the cursor, before the zoom change.
      const cx = (px - prevCentre + prevLeft) / zoom;
      const cy = (py + prevTop) / zoom;
      zoom = next;
      await tick();
      if (sc) {
        const nextCentre = Math.max(0, (width - width * next) / 2);
        sc.scrollLeft = clampScroll(cx * next + nextCentre - px, sc.scrollWidth - sc.clientWidth);
        sc.scrollTop = clampScroll(cy * next - py, sc.scrollHeight - sc.clientHeight);
        onScroll();
      }
    } else {
      const ratio = next / zoom;
      panX = px - (px - panX) * ratio;
      panY = py - (py - panY) * ratio;
      zoom = next;
    }
  }

  /** The zoombar's "reset" and Ctrl/⌘+0: back to 100% at the top-left/centre. */
  async function resetView() {
    zoom = 1;
    panX = 0;
    panY = 0;
    await tick();
    if (scroller) {
      scroller.scrollLeft = 0;
      scroller.scrollTop = 0;
      onScroll();
    }
  }

  // "Show me this device" from a settings list: when the store bumps a focus
  // request, pan the camera so the node sits at the canvas centre (at the
  // current zoom), so the View buttons actually reveal the node rather than
  // just selecting it somewhere off-screen. Guarded by the request's seq so it
  // fires once per request — a plain counter, not $state, so updating it doesn't
  // re-run this effect.
  let lastFocusSeq = 0;
  $effect(() => {
    const req = app.focusRequest;
    if (!req || req.seq === lastFocusSeq) return;
    lastFocusSeq = req.seq;
    if (isList) {
      // The list has no camera — scroll the device's row into view instead.
      // A cleared search could be hiding it, so drop the filter first, then
      // wait a tick for the row to mount before scrolling to it.
      if (listQueryNorm && !filteredListGroups.some((g) => g.nodes.some((n) => n.id === req.id))) {
        listQuery = "";
      }
      void tick().then(() => {
        listScroll
          ?.querySelector(`.node[data-node-id="${CSS.escape(req.id)}"]`)
          ?.scrollIntoView({ block: "center", behavior: "smooth" });
      });
      return;
    }
    const p = layout.find((pl) => pl.node.id === req.id);
    if (!p) return;
    if (isGrid && scroller) {
      // Scroll so the node lands in the middle of the viewport.
      scroller.scrollLeft = clampScroll(
        p.x * zoom + centreX - width / 2,
        scroller.scrollWidth - scroller.clientWidth,
      );
      scroller.scrollTop = clampScroll(
        p.y * zoom - height / 2,
        scroller.scrollHeight - scroller.clientHeight,
      );
      onScroll();
    } else {
      panX = width / 2 - p.x * zoom;
      panY = height / 2 - p.y * zoom;
    }
  });

  // Ctrl/⌘ + '-' / '=' / '0' zoom the graph — but only while the pointer is over
  // it, so the shortcut never fights the rest of the app (or the webview's own
  // page zoom) when you're somewhere else. Anchored at the viewport centre.
  $effect(() => {
    function onKey(e: KeyboardEvent) {
      // The list scrolls, it doesn't scale — leave the zoom shortcut to the
      // graph views (mirrors the `isList` bail-out in `onWheel`).
      if (normalMode || !hovering || isList || !(e.ctrlKey || e.metaKey) || e.altKey) return;
      if (e.key === "-" || e.key === "_") {
        e.preventDefault();
        void applyZoom(width / 2, height / 2, zoom / 1.2);
      } else if (e.key === "=" || e.key === "+") {
        e.preventDefault();
        void applyZoom(width / 2, height / 2, zoom * 1.2);
      } else if (e.key === "0") {
        e.preventDefault();
        void resetView();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  // The active canvas gesture: panning (right-drag / one finger), marqueeing
  // (left-drag on empty), or the two-finger pinch (mobile radial). Node drags
  // are tracked separately, below.
  type Gesture =
    // `ox`/`oy` is the origin the drag moves from: the scroll offset in grid,
    // the pan in radial. A right/middle-drag "pan" nudges whichever applies.
    // `id` (mobile only) is the finger the pan belongs to — after a pinch
    // hands off to one remaining finger, that finger may not be the primary
    // pointer, so ownership can't ride `isPrimary`.
    | { kind: "pan"; x: number; y: number; ox: number; oy: number; id?: number }
    | { kind: "marquee"; x0: number; y0: number; x1: number; y1: number }
    // `a`/`b` are the two pointer ids; everything else is the gesture's
    // starting state — distance, zoom, midpoint (canvas coords), pan — so
    // each move re-derives absolutely from the anchor instead of
    // accumulating drift.
    | {
        kind: "pinch";
        a: number;
        b: number;
        d0: number;
        z0: number;
        mx0: number;
        my0: number;
        px0: number;
        py0: number;
      };
  let gesture = $state<Gesture | null>(null);
  const panning = $derived(gesture?.kind === "pan");

  // Live client-coordinate positions of the fingers on the canvas — the
  // second one's arrival turns the radial pan into a pinch.
  const touchPts = new Map<number, { x: number; y: number }>();
  const fingerGap = (
    pa: { x: number; y: number },
    pb: { x: number; y: number },
  ) =>
    // Floored: fingertips can land nearly on top of each other, and a
    // near-zero base distance would make the zoom ratio explode.
    Math.max(24, Math.hypot(pb.x - pa.x, pb.y - pa.y));

  // Multi-selection (the marquee's result). The single-click selection still
  // flows through app.selectedNodeId so the drawer always has one focused node;
  // these add the extra highlighted devices on top.
  let selectedIds = $state<Set<string>>(new Set());

  // Live marquee preview — the devices currently inside the box, recomputed as
  // it's dragged so the highlight tracks the rubber-band in real time. Folded
  // into the selected look while dragging, then committed on release.
  const marqueeHits = $derived.by((): Set<string> => {
    const set = new Set<string>();
    if (gesture?.kind !== "marquee") return set;
    const x1 = Math.min(gesture.x0, gesture.x1);
    const x2 = Math.max(gesture.x0, gesture.x1);
    const y1 = Math.min(gesture.y0, gesture.y1);
    const y2 = Math.max(gesture.y0, gesture.y1);
    if (x2 - x1 < 4 && y2 - y1 < 4) return set;
    for (const p of layout) {
      const sx = p.x * zoom + offX;
      const sy = p.y * zoom + offY;
      if (sx >= x1 && sx <= x2 && sy >= y1 && sy <= y2) set.add(p.node.id);
    }
    return set;
  });

  function isSelected(id: string): boolean {
    return app.selectedNodeId === id || selectedIds.has(id) || marqueeHits.has(id);
  }

  // A left-drag that started on a device — used for drag-onto-another-device (or
  // fleet) to open the share builder. `moved` tells a real drag from a plain
  // click. A ghost chip follows the cursor while it's in flight.
  let nodeDrag = $state<{ id: string; sx: number; sy: number; moved: boolean } | null>(null);
  let dragOverId = $state<string | null>(null);
  let dragOverSection = $state<string | null>(null);
  let dragLabel = $state("");
  let dragPos = $state<{ x: number; y: number } | null>(null);

  const dropTargetLabel = $derived.by(() => {
    if (dragOverId) {
      const t = app.node(dragOverId);
      return t ? displayName(t) : "";
    }
    return dragOverSection ?? "";
  });

  function canvasPoint(e: PointerEvent): { x: number; y: number } {
    const r = canvas?.getBoundingClientRect();
    return { x: e.clientX - (r?.left ?? 0), y: e.clientY - (r?.top ?? 0) };
  }

  function onWheel(e: WheelEvent) {
    // The list owns its own native scroll — the canvas never zooms there.
    if (normalMode || isList) return;
    // Grid reads like a list: a bare wheel scrolls it (let the scroller do its
    // thing — don't preventDefault), and zoom is the deliberate Ctrl/⌘ gesture.
    // Radial has nothing to scroll, so the wheel keeps zooming there.
    if (isGrid && !e.ctrlKey && !e.metaKey) return;
    e.preventDefault();
    const rect = canvas?.getBoundingClientRect();
    if (!rect) return;
    const px = e.clientX - rect.left;
    const py = e.clientY - rect.top;
    const factor = Math.exp(-e.deltaY * 0.0012);
    void applyZoom(px, py, zoom * factor);
  }

  function onPointerDown(e: PointerEvent) {
    // The list is plain scrolling DOM — no marquee, no pan, no drag-share. Its
    // rows select on click; empty space does nothing.
    if (isList) return;
    const target = e.target as Element | null;
    // Let node clicks/drags and the floating controls handle themselves —
    // capturing here would swallow their clicks. Exception: on a phone in
    // radial view a drag that starts on a card must still pan (the node
    // handlers are tap-only on mobile and don't stop propagation), so those
    // fall through — unless connect mode is armed, where the node's own tap
    // completes the wire.
    if (target?.closest?.(".zoombar, .arm-banner, .restart-panel")) return;
    if (
      target?.closest?.(".node") &&
      !(mobile && !isGrid && !app.dragFrom)
    ) {
      return;
    }
    if (app.dragFrom) {
      app.cancelConnect();
      return;
    }
    if (mobile) {
      // A finger drag only pans/scrolls — never marquee, never share. Grid
      // scrolls natively (the real scroller + touch-action: pan-y on the
      // canvas); radial pans via this gesture, and a second finger turns the
      // pan into a pinch (the canvas is touch-action: none there, so the
      // browser won't page-zoom underneath it). No preventDefault and no
      // pointer capture here: either could retarget or suppress the tap's
      // click, which is what selects a node.
      if (!isGrid) {
        touchPts.set(e.pointerId, { x: e.clientX, y: e.clientY });
        if (touchPts.size === 2) {
          const [[ida, pa], [idb, pb]] = [...touchPts.entries()];
          const rect = canvas?.getBoundingClientRect();
          gesture = {
            kind: "pinch",
            a: ida,
            b: idb,
            d0: fingerGap(pa, pb),
            z0: zoom,
            mx0: (pa.x + pb.x) / 2 - (rect?.left ?? 0),
            my0: (pa.y + pb.y) / 2 - (rect?.top ?? 0),
            px0: panX,
            py0: panY,
          };
        } else if (e.isPrimary) {
          gesture = {
            kind: "pan",
            x: e.clientX,
            y: e.clientY,
            ox: panX,
            oy: panY,
            id: e.pointerId,
          };
        }
      }
      return;
    }
    // Stop a drag from turning into a native text selection.
    e.preventDefault();
    if (e.button === 2 || e.button === 1) {
      // Right (or middle) drag pans — the platform convention. In grid it drags
      // the scroll; in radial it drags the pan.
      gesture = {
        kind: "pan",
        x: e.clientX,
        y: e.clientY,
        ox: isGrid ? (scroller?.scrollLeft ?? 0) : panX,
        oy: isGrid ? (scroller?.scrollTop ?? 0) : panY,
      };
      (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
    } else if (e.button === 0) {
      // Left drag on empty space marquee-selects. Without a modifier it starts
      // a fresh selection.
      if (!e.shiftKey && !e.metaKey && !e.ctrlKey) {
        selectedIds = new Set();
        app.selectNode(null);
      }
      const p = canvasPoint(e);
      gesture = { kind: "marquee", x0: p.x, y0: p.y, x1: p.x, y1: p.y };
      (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
    }
  }
  function onPointerMove(e: PointerEvent) {
    // Keep every tracked finger's position fresh — the pinch reads both.
    if (mobile && touchPts.has(e.pointerId)) {
      touchPts.set(e.pointerId, { x: e.clientX, y: e.clientY });
    }
    if (!gesture) return;
    if (gesture.kind === "pinch") {
      if (e.pointerId !== gesture.a && e.pointerId !== gesture.b) return;
      const pa = touchPts.get(gesture.a);
      const pb = touchPts.get(gesture.b);
      if (!pa || !pb) return;
      const rect = canvas?.getBoundingClientRect();
      const next = clampZoom(gesture.z0 * (fingerGap(pa, pb) / gesture.d0));
      const ratio = next / gesture.z0;
      // Keep the content that started under the fingers' midpoint under it
      // as they move — spreading zooms, sliding both pans, in one gesture.
      const mx = (pa.x + pb.x) / 2 - (rect?.left ?? 0);
      const my = (pa.y + pb.y) / 2 - (rect?.top ?? 0);
      panX = mx - (gesture.mx0 - gesture.px0) * ratio;
      panY = my - (gesture.my0 - gesture.py0) * ratio;
      zoom = next;
      return;
    }
    // Phone: the pan gesture belongs to one finger — another's moves would
    // yank the origin around.
    if (mobile && gesture.kind === "pan" && gesture.id !== undefined && e.pointerId !== gesture.id)
      return;
    if (mobile && gesture.kind === "marquee" && !e.isPrimary) return;
    if (gesture.kind === "pan") {
      if (isGrid && scroller) {
        // Dragging right/down moves the content right/down — i.e. scroll left/up.
        scroller.scrollLeft = gesture.ox - (e.clientX - gesture.x);
        scroller.scrollTop = gesture.oy - (e.clientY - gesture.y);
      } else {
        panX = gesture.ox + (e.clientX - gesture.x);
        panY = gesture.oy + (e.clientY - gesture.y);
      }
    } else {
      const p = canvasPoint(e);
      gesture = { ...gesture, x1: p.x, y1: p.y };
    }
  }
  function onPointerUp(e: PointerEvent) {
    touchPts.delete(e.pointerId);
    if (gesture?.kind === "pinch") {
      if (e.pointerId !== gesture.a && e.pointerId !== gesture.b) return;
      // One finger up: keep panning with the one still down, from where it
      // is now (the zoom stays where the pinch left it).
      const remId = e.pointerId === gesture.a ? gesture.b : gesture.a;
      const rem = touchPts.get(remId);
      gesture = rem
        ? { kind: "pan", x: rem.x, y: rem.y, ox: panX, oy: panY, id: remId }
        : null;
      return;
    }
    if (gesture?.kind === "marquee") {
      // Commit whatever the live preview is highlighting (read it before the
      // gesture clears, since it's derived from the box).
      const hits = marqueeHits;
      if (hits.size > 0) {
        const next = new Set(selectedIds);
        for (const id of hits) next.add(id);
        selectedIds = next;
        // Keep the drawer useful: focus the single marqueed device, if one.
        if (next.size === 1) app.selectNode([...next][0]);
      }
    }
    gesture = null;
    (e.currentTarget as Element).releasePointerCapture?.(e.pointerId);
  }

  // No browser context menu on the canvas — right-drag is pan, and an empty
  // right-click should do nothing (not pop the OS menu).
  function onContextMenu(e: MouseEvent) {
    e.preventDefault();
  }

  // ---- drag one device onto another → open the share builder ----------
  function onNodePointerDown(e: PointerEvent, n: MeshNode) {
    // Phone: dragging is for scrolling/panning only — never a share. Selection
    // rides the tap's `click` (see the node's onclick), so this handler must
    // not capture the pointer or preventDefault, and it must let the event
    // bubble so the canvas (radial) or the native scroller (grid) can pan.
    // The list is the same story on desktop — rows select on click, and a
    // press must be free to become a scroll.
    if (mobile || isList) return;
    if (e.button !== 0 || app.dragFrom) return; // left only; connect mode uses clicks
    // A press on an inline control (Claim, Make claimable…) is that button's —
    // don't hijack it into a node drag, or capturing the pointer eats its click.
    if ((e.target as Element | null)?.closest?.("button, a, input, select, textarea")) return;
    e.stopPropagation();
    e.preventDefault(); // don't let the drag select the node's text
    nodeDrag = { id: n.id, sx: e.clientX, sy: e.clientY, moved: false };
    dragLabel = displayName(n);
    dragPos = canvasPoint(e);
    (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
  }
  function onNodePointerMove(e: PointerEvent, n: MeshNode) {
    if (!nodeDrag || nodeDrag.id !== n.id) return;
    // Only your own devices are draggable share sources — a foreign device
    // stays a plain click (select), never a drag.
    if (!app.isMyDevice(n.id)) return;
    if (!nodeDrag.moved && Math.hypot(e.clientX - nodeDrag.sx, e.clientY - nodeDrag.sy) < 6) return;
    nodeDrag = { ...nodeDrag, moved: true };
    dragPos = canvasPoint(e);
    // Hit-test what's under the cursor: another device, or a fleet band.
    const hit = document.elementFromPoint(e.clientX, e.clientY);
    const overNode = hit?.closest?.(".node")?.getAttribute("data-node-id") ?? null;
    if (overNode && overNode !== n.id && app.canReceiveShare(overNode)) {
      dragOverId = overNode;
      dragOverSection = null;
    } else {
      dragOverId = null;
      dragOverSection = hit?.closest?.(".section")?.getAttribute("data-section-label") ?? null;
    }
  }
  function onNodePointerUp(e: PointerEvent, n: MeshNode) {
    // Phone and list: the tap's click event selects / completes connect —
    // acting here too would double-fire it.
    if (mobile || isList) return;
    // Only act when this device actually started the gesture (a press that
    // began on an inline button left nodeDrag null — let that button win).
    if (app.dragFrom) {
      // Connect mode: a tap completes the wire, exactly as before.
      onNodeClick(n);
      return;
    }
    if (!nodeDrag || nodeDrag.id !== n.id) return;
    const moved = nodeDrag.moved;
    const overNode = dragOverId;
    const overSection = dragOverSection;
    (e.currentTarget as Element).releasePointerCapture?.(e.pointerId);
    nodeDrag = null;
    dragOverId = null;
    dragOverSection = null;
    dragPos = null;
    if (moved) {
      // Dropped on another device → sender = dragged, receiver = target. Dropped
      // on a fleet band → open the builder with the dragged device as sender and
      // let you pick the receiver in that fleet.
      if (overNode) app.openShareFlow(n.id, overNode);
      else if (overSection) app.openShareFlow(n.id, null);
    } else {
      // Didn't move — it's a plain click: select (the old behaviour).
      onNodeClick(n);
    }
  }

  function nodeAvatar(n: MeshNode): string {
    // A device on the mesh that isn't running AllMyStuff reads as a bare
    // endpoint, not a fleshed-out machine.
    if (!isAppNode(n)) return "📡";
    if (n.relationship.kind === "shared") return "🧑";
    const os = (n.summary?.os ?? "").toLowerCase();
    if (n.kind === "this") return "💻";
    if (os.includes("mac")) return "🍎";
    if (os.includes("win")) return "🪟";
    if (os.includes("android") || os.includes("phone")) return "📱";
    if (n.label.toLowerCase().includes("tv")) return "📺";
    return "🖥";
  }

  // Whether a node is a valid connect-drop target right now — drives the
  // pulsing highlight. A device must be running AllMyStuff and already
  // claimed to be a target.
  const armed = $derived(!!app.dragFrom);
  function targetable(n: MeshNode): boolean {
    return isAppNode(n) && n.relationship.kind !== "unclaimed";
  }

  /** A device offering itself for adoption that you can actually take — the
   *  same `standing().claimable` the node's visual reads, so the tap target
   *  and the look never disagree. */
  function isAdoptable(n: MeshNode): boolean {
    return app.standingOf(n).claimable;
  }

  /** The claimable node whose inline "Claim" button is currently dropped out
   *  (revealed by tapping it). Null = none showing. */
  let claimRevealed = $state<string | null>(null);

  /** The picker's chosen target, defaulted to the KVM's owner when it opens. */
  let kvmTarget = $state("");

  /** Toggle the attach dropdown for `kvmId` — the link button's drop-out.
   *  Opening defaults the target to the KVM's owner; re-toggling (or opening
   *  another KVM's) closes it, so a stale target never carries across. */
  function toggleKvmAttach(kvmId: string) {
    if (app.kvmRevealed === kvmId) {
      app.kvmRevealed = null;
      return;
    }
    kvmTarget = app.kvmDefaultTarget(kvmId) ?? "";
    app.kvmRevealed = kvmId;
    claimRevealed = null;
  }

  // This Device's graph label is the fastest place to rename the machine.
  // The existing identity setter handles persistence and presence gossip.
  let renamingSelf = $state(false);
  let selfNameDraft = $state("");
  let selfNameSaving = $state(false);
  let selfNameInput = $state<HTMLInputElement | null>(null);

  $effect(() => {
    if (!renamingSelf) return;
    void tick().then(() => {
      selfNameInput?.focus();
      selfNameInput?.select();
    });
  });

  function beginSelfRename(n: MeshNode) {
    selfNameDraft = n.label;
    renamingSelf = true;
  }

  // Canvas/node drag handlers prevent the browser's default focus change, so
  // `blur` alone cannot reliably see an outside click. Capture it before those
  // handlers, commit the current draft, and let the original click continue.
  $effect(() => {
    if (!renamingSelf) return;
    const self = app.catalog.nodes.find((n) => n.kind === "this");
    function onOutsideDown(e: PointerEvent) {
      const target = e.target as Element | null;
      if (target?.closest?.(".node-label-input")) return;
      if (self) void saveSelfRename(self);
      else renamingSelf = false;
    }
    window.addEventListener("pointerdown", onOutsideDown, true);
    return () => window.removeEventListener("pointerdown", onOutsideDown, true);
  });

  async function saveSelfRename(n: MeshNode) {
    if (!renamingSelf || selfNameSaving) return;
    const clean = selfNameDraft.trim();
    if (!clean) {
      selfNameDraft = n.label;
      renamingSelf = false;
      return;
    }
    // Exit edit mode immediately; persistence can finish without leaving a
    // stale input sitting onscreen after the user has clicked elsewhere.
    renamingSelf = false;
    selfNameSaving = true;
    // Matching the hostname means "use the default" rather than storing a
    // redundant override that would later display as Name (Name).
    try {
      await app.setIdentityLabel(clean === n.hostname?.trim() ? "" : clean);
    } finally {
      selfNameSaving = false;
    }
  }

  function onNodeClick(n: MeshNode) {
    if (app.dragFrom) {
      // Mesh-only and not-yet-claimed nodes aren't connection targets.
      if (!isAppNode(n)) {
        app.toast("warn", `${n.label} isn't running AllMyStuff`);
        return;
      }
      if (n.relationship.kind === "unclaimed") {
        app.toast("warn", `Claim ${n.label} first: open it to adopt it`);
        return;
      }
      app.dropConnectOnNode(n.id);
    } else if (isAdoptable(n)) {
      // Tapping a claimable node drops an inline "Claim" button out from
      // under it (shimmer + slide) — the fast path to adopt, right on the
      // graph — and opens the drawer for the full story. Close any open KVM
      // attach dropdown so only one drop-out ever shows at a time.
      claimRevealed = claimRevealed === n.id ? null : n.id;
      app.kvmRevealed = null;
      app.selectNode(n.id);
    } else {
      claimRevealed = null;
      app.kvmRevealed = null;
      // Clicking a device always selects it and keeps it selected — re-clicking
      // the focused node no longer toggles it off (close the drawer to deselect).
      app.selectNode(n.id);
    }
  }

  // ---- per-node actions menu (the gear) -----------------------------
  //
  // Opened from a card's gear and positioned in viewport coordinates so it
  // can flip up / left to stay on screen — the cards live in a panned + zoomed
  // layer, so the menu is rendered OUTSIDE that layer (a top-level sibling) and
  // anchored with `position: fixed` from the gear's on-screen rect.
  let nodeMenu = $state<{ id: string; left: number; top: number } | null>(null);
  let kvmPowerMenu = $state<{ kvmId: string; left: number; top: number } | null>(null);
  let fleetMenu = $state<{ id: string; left: number; top: number } | null>(null);
  let driveMenu = $state<{ anchorId: string; target: string; left: number; top: number } | null>(null);
  const MENU_W = 216;
  // Per-item height + container padding for the flip math below — the menu's
  // real height depends on which items this node offers.
  const MENU_ITEM_H = 56;
  const MENU_PAD = 12;

  // "Restart this device" is destructive enough for the two-step arm the
  // fleet's kick button uses: first click arms ("click again to confirm"),
  // the second acts. Disarms itself, on any other menu, and with the menu.
  let restartDeviceArmed = $state<string | null>(null);
  /** Two-step arm for the gear menu's "Forget this node" — same pattern as
   *  the reboot: a stray click must not drop a device off the graph. */
  let forgetNodeArmed = $state<string | null>(null);
  /** Fleet eviction is deliberately two-step: the chip is an everyday status
   *  surface, so an exploratory click must never remove a device. */
  let fleetEvictArmed = $state<string | null>(null);

  function fleetMenuHeight(nodeId: string): number {
    const mn = app.node(nodeId);
    if (!mn || app.isMe(nodeId) || mn.kind === "this") return MENU_PAD + MENU_ITEM_H;
    const role = app.standingOf(mn).role;
    const iOwn = app.myFleetRole === "owner";
    const iManage = iOwn || app.myFleetRole === "manager";
    let items = 1; // Fleet settings is always available.
    if (app.isKvm(mn)) {
      // KVMs are fixed-role fleet appliances: eviction is their only member
      // action. Respect the same role authority as every other eviction.
      if ((role === "manager" && iOwn) || (role !== "owner" && role !== "manager" && iManage)) items += 1;
      return MENU_PAD + items * MENU_ITEM_H;
    }
    if (role === "owner") {
      if (iOwn) items += 1; // Demote to manager.
    } else if (role === "manager") {
      if (iOwn) items += 3; // Promote, demote, evict.
    } else {
      if (iOwn) items += 1; // Promote to manager.
      if (iManage) items += 1; // Evict a plain member.
    }
    return MENU_PAD + items * MENU_ITEM_H;
  }

  function menuHeight(nodeId: string): number {
    const mn = app.node(nodeId);
    const cons = mn ? app.consoleAccess(mn) : null;
    const items =
      1 +
      (normalMode && cons?.terminal ? 1 : 0) +
      (normalMode && cons?.files ? 1 : 0) +
      (mn && app.isKvm(mn) && cons?.sites ? 1 : 0) +
      (normalMode && mn && app.isKvm(mn) && app.kvmAllowed(mn) ? 1 : 0) +
      (normalMode && mn && app.kvmAllowed(mn) ? 3 : 0) +
      (!app.isKvm(mn) && app.canRestartApp(mn) ? 1 : 0) +
      (!app.isKvm(mn) && app.canRestartDevice(mn) ? 1 : 0) +
      (mn && mn.kind !== "this" && !app.isMe(nodeId) ? 1 : 0); // Forget this node
    return MENU_PAD + items * MENU_ITEM_H;
  }

  function openNodeMenu(e: MouseEvent, nodeId: string) {
    e.stopPropagation();
    fleetMenu = null;
    driveMenu = null;
    fleetEvictArmed = null;
    restartDeviceArmed = null;
    forgetNodeArmed = null;
    if (nodeMenu?.id === nodeId) {
      nodeMenu = null; // toggle closed
      return;
    }
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const menuH = menuHeight(nodeId);
    // Flip toward whichever side has room: open up when the gear is near the
    // bottom, and align to the right edge when it's near the right.
    const openUp = r.bottom + menuH + 8 > vh;
    const openLeft = r.left + MENU_W + 8 > vw;
    const left = openLeft
      ? Math.max(8, r.right - MENU_W)
      : Math.min(r.left, vw - MENU_W - 8);
    const top = openUp ? Math.max(8, r.top - menuH - 6) : r.bottom + 6;
    nodeMenu = { id: nodeId, left, top };
  }

  function openKvmPowerMenu(e: MouseEvent, kvmId: string) {
    e.stopPropagation();
    nodeMenu = null;
    fleetMenu = null;
    fleetEvictArmed = null;
    if (kvmPowerMenu?.kvmId === kvmId) {
      kvmPowerMenu = null;
      return;
    }
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const menuHeight = MENU_PAD + 4 * MENU_ITEM_H;
    const left = Math.max(8, Math.min(rect.left, window.innerWidth - MENU_W - 8));
    const top = rect.bottom + menuHeight + 8 > window.innerHeight
      ? Math.max(8, rect.top - menuHeight - 6)
      : rect.bottom + 6;
    kvmPowerMenu = { kvmId, left, top };
  }

  function openFleetMenu(e: MouseEvent, nodeId: string) {
    e.stopPropagation();
    nodeMenu = null;
    driveMenu = null;
    kvmPowerMenu = null;
    restartDeviceArmed = null;
    forgetNodeArmed = null;
    fleetEvictArmed = null;
    if (fleetMenu?.id === nodeId) {
      fleetMenu = null;
      return;
    }
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const menuHeight = fleetMenuHeight(nodeId);
    const left = rect.left + MENU_W + 8 > window.innerWidth
      ? Math.max(8, rect.right - MENU_W)
      : Math.min(rect.left, window.innerWidth - MENU_W - 8);
    const top = rect.bottom + menuHeight + 8 > window.innerHeight
      ? Math.max(8, rect.top - menuHeight - 6)
      : rect.bottom + 6;
    fleetMenu = { id: nodeId, left, top };
  }

  function openDriveMenu(e: MouseEvent, nodeId: string, target = nodeId) {
    e.stopPropagation();
    nodeMenu = null;
    fleetMenu = null;
    kvmPowerMenu = null;
    if (driveMenu?.anchorId === nodeId) {
      driveMenu = null;
      return;
    }
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const menuW = 346;
    const menuH = 430;
    const left = rect.left + menuW + 8 > window.innerWidth
      ? Math.max(8, rect.right - menuW)
      : Math.min(rect.left, window.innerWidth - menuW - 8);
    const top = rect.bottom + menuH + 8 > window.innerHeight
      ? Math.max(8, rect.top - menuH - 6)
      : rect.bottom + 6;
    driveMenu = { anchorId: nodeId, target, left, top };
  }

  function openNodeDriveMenu(e: MouseEvent, node: MeshNode) {
    // A KVM's Drives surface is virtual media on the KVM itself. It must not
    // silently become a drive map to the computer attached behind the KVM.
    openDriveMenu(e, node.id, node.id);
  }

  // Close the menu on any outside pointer-down (the gear + the menu are exempt
  // so they can toggle / be clicked), and on Escape.
  $effect(() => {
    if (!nodeMenu && !kvmPowerMenu && !fleetMenu && !driveMenu) return;
    function onDown(e: PointerEvent) {
      const t = e.target as Element | null;
      if (!t?.closest?.(".node-menu, .node-gear, .kvm-power-trigger, .fleet-trigger, .drive-trigger, .drive-menu")) {
        nodeMenu = null;
        kvmPowerMenu = null;
        fleetMenu = null;
        driveMenu = null;
        fleetEvictArmed = null;
      }
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        nodeMenu = null;
        kvmPowerMenu = null;
        fleetMenu = null;
        driveMenu = null;
        fleetEvictArmed = null;
      }
    }
    window.addEventListener("pointerdown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  });
</script>

<!-- The small console glyphs for the card buttons: remote desktop, files,
     terminal, sites, updates, plus the KVM set (Wi-Fi, firmware update, the
     attach link, and the attached machine's power/reset). Stroke uses
     currentColor. -->
{#snippet cicon(kind: "remote" | "files" | "drives" | "terminal" | "sites" | "kvm" | "update" | "link" | "power" | "reset" | "wifi" | "share" | "room")}
  {#if kind === "remote"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="3" y="4" width="18" height="13" rx="2" /><path d="M8 20h8M12 17v3" />
    </svg>
  {:else if kind === "files"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3 6.5A1.5 1.5 0 0 1 4.5 5h4l2 2.2H19a1.5 1.5 0 0 1 1.5 1.5V18a1.5 1.5 0 0 1-1.5 1.5H4.5A1.5 1.5 0 0 1 3 18Z" />
    </svg>
  {:else if kind === "drives"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M5 5.5h14l2 5.5H3z" /><rect x="3" y="11" width="18" height="7.5" rx="2" /><circle cx="17.5" cy="14.8" r=".8" fill="currentColor" stroke="none" />
    </svg>
  {:else if kind === "terminal"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="3" y="4.5" width="18" height="15" rx="2" /><path d="M7 9.5l3 2.5-3 2.5M12.5 15h4" />
    </svg>
  {:else if kind === "kvm"}
    <!-- The KVM's own web console: a monitor with the keyboard bar below —
         distinct from "remote" (no stand), since this opens the appliance's
         integrated UI, not an AllMyStuff console. -->
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="3" y="4" width="18" height="11" rx="2" /><rect x="5" y="18" width="14" height="3" rx="1" />
    </svg>
  {:else if kind === "update"}
    <!-- Update the app/firmware: download arrow inside a restart loop. -->
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M20 12a8 8 0 1 1-3-6.2" /><polyline points="17 3 17 6.5 20.5 6.5" /><path d="M12 7v7m0 0-3-3m3 3 3-3" />
    </svg>
  {:else if kind === "link"}
    <!-- Attach: the chain link that drops the target picker out. -->
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M10 13.5a4 4 0 0 0 6 .5l2.5-2.5a4 4 0 0 0-5.7-5.7L11.5 7" /><path d="M14 10.5a4 4 0 0 0-6-.5L5.5 12.5a4 4 0 0 0 5.7 5.7L12.5 17" />
    </svg>
  {:else if kind === "wifi"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true">
      <path d="M4 9.5a12 12 0 0 1 16 0M7 13a7.5 7.5 0 0 1 10 0M10 16.5a3 3 0 0 1 4 0" /><circle cx="12" cy="20" r=".8" fill="currentColor" stroke="none" />
    </svg>
  {:else if kind === "share"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="6" cy="12" r="2.5" /><circle cx="18" cy="6" r="2.5" /><circle cx="18" cy="18" r="2.5" /><path d="m8.2 10.8 7.6-3.6M8.2 13.2l7.6 3.6" />
    </svg>
  {:else if kind === "room"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M4 5.5h16v11H9l-5 3.5z" /><circle cx="9" cy="11" r=".7" fill="currentColor" stroke="none" /><circle cx="12" cy="11" r=".7" fill="currentColor" stroke="none" /><circle cx="15" cy="11" r=".7" fill="currentColor" stroke="none" />
    </svg>
  {:else if kind === "power"}
    <!-- The attached machine's power button, driven through the KVM's GPIO. -->
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M12 3v8" /><path d="M6.6 6.6a8 8 0 1 0 10.8 0" />
    </svg>
  {:else if kind === "reset"}
    <!-- The attached machine's reset line: a hard loop-around. -->
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <polyline points="3 5 3 10 8 10" /><path d="M4.6 14a8 8 0 1 0 1.6-7.4L3 10" />
    </svg>
  {:else}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="8.5" /><path d="M3.5 12h17M12 3.5c2.5 2.4 2.5 14.6 0 17M12 3.5c-2.5 2.4-2.5 14.6 0 17" />
    </svg>
  {/if}
{/snippet}

<!-- The device card, shared by the graph (placed on the canvas) and the list
     (a flowing row). `placed` carries the graph coordinates; a null `placed`
     renders the same card as a full-width list row instead. Declared at the
     top level so both the graph's node layer and the list can render it — one
     card body means the two views can never drift apart. -->
{#snippet deviceCard(n: MeshNode, placed: Placed | null)}
  <!-- One derived standing drives every visual + affordance, so the node
       never shows contradictory state (e.g. "unclaimed" while wearing a
       fleet badge). It recomputes live from the fleet roster + the device's
       advert, so claiming or fleet changes reflect immediately. -->
  {@const st = app.standingOf(n)}
  {@const cons = app.consoleAccess(n)}
  {@const kvmDestination = app.isKvm(n) ? app.kvmTargetNode(n) : undefined}
  {@const driveAllowed = app.isKvm(n)
    ? app.kvmAllowed(n)
    : app.isMe(n.id) ||
      app.isFleetMember(n.id) ||
      app.filesAllowed(n) ||
      app.sharedFoldersFrom(n).length > 0}
  {@const externalInteraction =
    st.app &&
    !st.self &&
    !st.mine &&
    !st.inFleet &&
    !app.isKvm(n) &&
    !app.isCecCustomer(n.id)}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="node"
    class:list={!placed}
    class:normal={normalMode}
    class:self={st.self}
    class:shared={st.kind === "shared"}
    class:mine={st.mine && !st.self}
    class:unclaimed={st.kind === "free" || st.kind === "theirs"}
    class:claimable={st.claimable}
    class:meshonly={!st.app}
    class:selected={isSelected(n.id)}
    class:armed={armed && targetable(n)}
    class:dragover={dragOverId === n.id}
    class:dragging-node={nodeDrag?.id === n.id && nodeDrag.moved}
    class:grabbable={app.isMyDevice(n.id) && !isList}
    class:offline={!n.online}
    data-node-id={n.id}
    style={placed
      ? `left: ${placed.x - nodeW / 2}px; top: ${placed.y - nodeH / 2}px; width: ${nodeW}px; min-height: ${nodeH}px;`
      : null}
    onpointerdown={(e) => onNodePointerDown(e, n)}
    onpointermove={(e) => onNodePointerMove(e, n)}
    onpointerup={(e) => onNodePointerUp(e, n)}
    onpointercancel={(e) => onNodePointerUp(e, n)}
    onclick={(e) => {
      // Phone + list: select on the tap's click — a drag that scrolled
      // never fires one, so scrolling can't select. Desktop graph views
      // select from pointerup above; running this too would double-fire.
      if (!mobile && !isList) return;
      if ((e.target as Element | null)?.closest?.("button, a, input, select, textarea")) return;
      onNodeClick(n);
    }}
    onkeydown={(e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        onNodeClick(n);
      }
    }}
    role="button"
    tabindex="0"
    aria-label={displayName(n)}
  >
    {#if st.self}<span class="self-corner" aria-hidden="true">This device</span>{/if}
    <div class="node-top">
      <span class="avatar">{nodeAvatar(n)}</span>
      <div class="node-id">
        {#if st.self && renamingSelf}
          <input
            class="node-label-input"
            aria-label="This device name"
            bind:value={selfNameDraft}
            bind:this={selfNameInput}
            disabled={selfNameSaving}
            onclick={(e) => e.stopPropagation()}
            onblur={() => void saveSelfRename(n)}
            onkeydown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void saveSelfRename(n);
              } else if (e.key === "Escape") {
                e.preventDefault();
                renamingSelf = false;
              }
            }}
          />
        {:else if st.self}
          <button
            class="node-label node-label-edit"
            title="Rename this device"
            onclick={(e) => {
              e.stopPropagation();
              beginSelfRename(n);
            }}
          >{normalMode ? n.label : displayName(n)}<span aria-hidden="true">✎</span></button>
        {:else}
          <div class="node-label" title={displayName(n)}>{normalMode ? n.label : displayName(n)}</div>
        {/if}
        <div class="node-sub">
          {#if normalMode && st.app}
            {n.hostname?.trim() || n.label}
          {:else if st.shared}
            Shares Active
          {:else if !st.app}
            on the mesh · not running AllMyStuff
          {:else if n.summary}
            {n.summary.cpu}
          {:else}
            device
          {/if}
        </div>
      </div>
    </div>
    <!-- Chips fill the row; the refresh control is pinned to its right. The
         online dot ringed by refresh arrows; clicking re-learns this node.
         It spins while a refresh is in flight and is disabled until the
         details land (or the request times out). -->
    <div class="node-meta-row">
      <div class="node-meta">
        {#if !st.app}<span class="tag meshonly">not on AllMyStuff</span>
        {:else if st.shared}<span class="tag guest">guest</span>
        {:else if st.kind === "claimable"}<span class="tag claimable">＋ claim</span>
        {:else if st.kind === "theirs"}<span class="tag theirs">someone else's</span>
        {:else if st.kind === "free"}<span class="tag unclaimed">unclaimed</span>
        {:else if st.mine && !st.inFleet && !st.self}<span class="tag mine">yours</span>{/if}
        {#if st.inFleet}
          <button
            class="fleet-button fleet-trigger"
            class:owner={st.role === "owner"}
            class:manager={st.role === "manager"}
            title="In your fleet · {st.role} · manage fleet"
            aria-label={`${displayName(n)} fleet options`}
            aria-haspopup="menu"
            aria-expanded={fleetMenu?.id === n.id}
            onclick={(e) => openFleetMenu(e, n.id)}
          ><span>{st.role === "owner" ? "★ Owner" : st.role === "manager" ? "⚑ Manager" : "🔗 Fleet"}</span><span class="fleet-caret" aria-hidden="true">▾</span></button>
        {/if}
        {#if !normalMode && n.summary}<span class="tag soft">{n.summary.device_count} things</span>{/if}
        {#if !normalMode && n.summary}<span class="tag soft">{humanBytes(n.summary.ram_bytes)}</span>{/if}
        {#if n.needsTurn && !n.online}
          <!-- The daemon's ICE watchdog verdict, surfaced: this link keeps
               failing with no relay in play. Without the chip the device
               is just… quiet, and nobody guesses "add a TURN server". -->
          <span
            class="tag blocked"
            title="Direct connection keeps failing and no relay is configured: add a TURN server to this mesh's venue (mesh settings → Servers)"
            >⛔ needs relay</span
          >
        {/if}
      </div>
      <button
        class="cbtn status-refresh"
        class:online={n.online}
        class:spinning={app.isRefreshing(n.id)}
        data-tip="Refresh"
        aria-label={`Refresh ${displayName(n)}`}
        disabled={app.isRefreshing(n.id)}
        onclick={(e) => {
          e.stopPropagation();
          void app.refreshNode(n.id);
        }}
      >
        <span class="refresh-face" aria-hidden="true">
          <svg class="refresh-ring" viewBox="0 0 24 24">
            <path d="M3.5 12A8.5 8.5 0 0 1 20.5 12" />
            <polyline points="17.5 9 20.5 12 17.5 15" />
            <path d="M20.5 12A8.5 8.5 0 0 1 3.5 12" />
            <polyline points="6.5 9 3.5 12 6.5 15" />
          </svg>
          <span class="dot" class:on={n.online}></span>
        </span>
        <span class="action-label refresh-label">Refresh</span>
      </button>
    </div>
    <!-- Bottom button row: the consoles you can open on this device (your
         own fleet's, or exactly what a fleet that shared it granted), with
         the settings gear pinned to the right. -->
    <div class="node-consoles">
      {#if cons.remote}
        <button class="cbtn" data-tip="Remote control" aria-label="Remote control {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.openConsoleKind(n.id, "remote"); }}>{@render cicon("remote")}<span class="action-label">Remote</span></button>
      {/if}
      {#if cons.files && !normalMode}
        <button class="cbtn" data-tip="Files" aria-label="Open files on {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.openConsoleKind(n.id, "files"); }}>{@render cicon("files")}<span class="action-label">Files</span></button>
      {/if}
      {#if isAppNode(n) && driveAllowed && (!normalMode || !app.isKvm(n))}
        <button class="cbtn drive-trigger" data-tip={app.isKvm(n) ? "Install media via KVM" : "Mapped drives"} aria-label={app.isKvm(n) ? `Present install media to ${kvmDestination ? displayName(kvmDestination) : "the attached computer"} through ${displayName(n)}` : `Map drives with ${displayName(n)}`}
          aria-haspopup="menu" aria-expanded={driveMenu?.anchorId === n.id}
          onclick={(e) => openNodeDriveMenu(e, n)}>{@render cicon("drives")}<span class="action-label">Drives</span></button>
      {/if}
      {#if cons.terminal && !normalMode}
        <button class="cbtn" data-tip="Terminal" aria-label="Open terminal on {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.openConsoleKind(n.id, "terminal"); }}>{@render cicon("terminal")}<span class="action-label">Terminal</span></button>
      {/if}
      {#if cons.sites && !app.isKvm(n)}
        <button class="cbtn" data-tip="Sites" aria-label="Open sites on {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.openConsoleKind(n.id, "sites"); }}>{@render cicon("sites")}<span class="action-label">Sites</span></button>
      {/if}
      {#if app.isKvm(n) && app.kvmDoors(n)}
        <!-- Sites moved into this appliance's gear; the prime card position is
             the attached computer's power menu, backed by the KVM power LED. -->
        <button class="cbtn kvm-power-trigger"
          class:power-on={app.kvmPowerState(n.id) === true}
          data-tip="Power"
          aria-label="Power menu through {displayName(n)}"
          aria-haspopup="menu"
          aria-expanded={kvmPowerMenu?.kvmId === n.id}
          onclick={(e) => openKvmPowerMenu(e, n.id)}>{@render cicon("power")}<span class="action-label">Power</span></button>
      {/if}
      {#if app.canUpdateAllMyStuff(n)}
        <button class="cbtn update-action" data-tip="Update AllMyStuff" aria-label="Update AllMyStuff on {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.updateAllMyStuff(n.id); }}>{@render cicon("update")}<span class="action-label">Update</span></button>
      {/if}
      {#if externalInteraction}
        <button class="cbtn share-action" data-tip="Manage sharing" aria-label="Manage sharing with {displayName(n)}"
          onclick={(e) => {
            e.stopPropagation();
            app.openShareFlow(app.localId, n.id, app.existingShareCaps(app.localId, n.id));
          }}>{@render cicon("share")}<span class="action-label">Share</span></button>
        <button class="cbtn room-action" data-tip="Private room" aria-label="Open a private room with {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.openDirectRoom(n.id); }}>{@render cicon("room")}<span class="action-label">Room</span></button>
      {/if}
      {#if app.kvmAllowed(n)}
        <button class="cbtn" data-tip="Support number and approvals" aria-label="Support for {displayName(n)}"
          onclick={(e) => { e.stopPropagation(); app.openKvmSupport(n.id); }}><span aria-hidden="true">#</span><span class="action-label">Support</span></button>
        <!-- Advanced-only appliance controls. Sites lives in the settings gear
             in both modes; Normal also keeps these lower-frequency actions in
             that menu. -->
        {#if !normalMode}
          <button class="cbtn" data-tip="Wi-Fi" aria-label="Configure Wi-Fi on {displayName(n)}"
            onclick={(e) => { e.stopPropagation(); void app.openKvmWifi(n.id); }}>{@render cicon("wifi")}<span class="action-label">Wi-Fi</span></button>
        {/if}
        {#if !normalMode}
          <button class="cbtn"
            disabled={app.isKvmUpdating(n.id)}
            data-tip="Update this KVM"
            aria-label="Update {displayName(n)} (the KVM itself)"
            onclick={(e) => { e.stopPropagation(); void app.updateKvm(n.id); }}>{@render cicon("update")}<span class="action-label">{app.isKvmUpdating(n.id) ? "Updating…" : "Update"}</span></button>
        {/if}
        {#if !normalMode}
          <button class="cbtn" class:active={app.kvmRevealed === n.id}
            data-tip="Attach to a machine" aria-label="Point {displayName(n)} at a machine"
            aria-expanded={app.kvmRevealed === n.id}
            onclick={(e) => { e.stopPropagation(); toggleKvmAttach(n.id); }}>{@render cicon("link")}<span class="action-label">Attach</span></button>
        {/if}
      {/if}
      {#if !app.isKvm(n)}
        {@const kvmHere = app.kvmAttachedTo(n.id)}
        {#if kvmHere && app.kvmPassthroughDoors(kvmHere, n)}
          <!-- This machine is wired into a KVM you control: its physical
               power and reset lines ride the KVM's GPIO, so they belong
               on THIS card — they act on this machine. -->
          <button class="cbtn kvm-power-trigger"
            class:power-on={app.kvmPowerState(kvmHere.id) === true}
            data-tip="Power (via KVM)"
            aria-label="Power menu for {displayName(n)} via KVM"
            aria-haspopup="menu"
            aria-expanded={kvmPowerMenu?.kvmId === kvmHere.id}
            onclick={(e) => openKvmPowerMenu(e, kvmHere.id)}>{@render cicon("power")}<span class="action-label">Power</span></button>
        {/if}
      {/if}
      <span class="cbtn-sep" aria-hidden="true"></span>
      <button
        class="cbtn node-gear"
        data-tip="Settings"
        aria-label={`Settings for ${displayName(n)}`}
        aria-haspopup="menu"
        aria-expanded={nodeMenu?.id === n.id}
        onclick={(e) => openNodeMenu(e, n.id)}
      >⚙<span class="action-label">Settings</span></button>
    </div>
    <!-- Claimable affordances drop out from *under* the node, floating
         below it so they never disturb the graph's layout. -->
    {#if st.self && st.app && !st.inFleet && !st.offering}
      <!-- Your own device, not in a fleet: offer it for adoption. -->
      <button
        class="node-drawer make-claimable"
        title="Offer this device so another of your machines can adopt it into a fleet"
        onclick={(e) => { e.stopPropagation(); void app.setLocalClaimable(true); }}
      >🔒 Make claimable</button>
    {:else if st.claimable && claimRevealed === n.id}
      <!-- A claimable device you tapped: the Claim button slides in. -->
      <button
        class="node-drawer claim-go"
        onclick={(e) => { e.stopPropagation(); void app.claim(n.id); claimRevealed = null; }}
      >＋ Claim this device</button>
    {:else if app.kvmAllowed(n) && app.kvmRevealed === n.id}
      {@const targets = app.kvmAttachTargets(n.id)}
      <!-- The attach picker, dropped out by the card's link button: pick
           which machine this KVM is wired into and Set. Everything else
           (Open KVM, reboot, power/reset) lives in the button bar now —
           this drop-out exists only for the one action that needs a
           choice. No Detach here; that's the buried, confirm-gated action
           in the sidebar. -->
      <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
      <div class="node-drawer kvm-drawer" onclick={(e) => e.stopPropagation()}>
        {#if targets.length === 0}
          <p class="kvm-empty">No machines of yours to attach to yet.</p>
        {:else}
          <div class="kvm-row">
            <select
              class="kvm-pick"
              title="Which machine this KVM controls"
              bind:value={kvmTarget}
            >
              {#each targets as t (t.id)}
                <option value={t.id}>{displayName(t)}</option>
              {/each}
            </select>
            <button
              class="kvm-act"
              disabled={!kvmTarget}
              onclick={(e) => {
                e.stopPropagation();
                if (!kvmTarget) return;
                void app.attachKVM(n.id, kvmTarget);
                app.kvmRevealed = null;
              }}
            >Set</button>
          </div>
        {/if}
      </div>
    {/if}
  </div>
{/snippet}

<!-- Phone grid reads like a list: let the browser own the vertical touch
     scroll (pan-y; both axes once zoomed in, so nothing is unreachable).
     Radial (and all of desktop) keeps the stylesheet's touch-action: none so
     the JS pan gesture receives every move — `null` sets no inline style at
     all, leaving desktop untouched. -->
<div
  class="canvas"
  class:panning
  class:marqueeing={gesture?.kind === "marquee"}
  class:armed
  bind:this={canvas}
  style:touch-action={isList
    ? "pan-y"
    : mobile && isGrid
      ? zoom > 1
        ? "pan-x pan-y"
        : "pan-y"
      : null}
  onwheel={onWheel}
  onpointerdown={onPointerDown}
  onpointermove={onPointerMove}
  onpointerup={onPointerUp}
  onpointercancel={onPointerUp}
  onpointerenter={() => (hovering = true)}
  onpointerleave={(e) => {
    hovering = false;
    // The mobile radial pan/pinch runs uncaptured (capture would steal the
    // tap's click from the node) — if a finger slides off the canvas, end
    // the gesture and forget that finger, or a stale entry would make the
    // next touch read as a second finger.
    if (mobile) {
      touchPts.delete(e.pointerId);
      gesture = null;
    }
  }}
  oncontextmenu={onContextMenu}
  role="application"
  aria-label="Your stuff, as a graph"
>
  <!-- The dot backdrop — a static viewport-sized layer that never pans or
       scrolls, so the graph always reads against the same field of dots. -->
  <svg class="dots" {width} {height} aria-hidden="true">
    <defs>
      <pattern id="dots" width="26" height="26" patternUnits="userSpaceOnUse">
        <circle cx="1.5" cy="1.5" r="1.5" fill="oklch(0.62 0.20 292 / 0.16)" />
      </pattern>
    </defs>
    <rect x="0" y="0" {width} {height} fill="url(#dots)" />
  </svg>

  <!-- The scroll viewport. Grid view scrolls it natively (mousewheel, drag,
       scrollbar); radial keeps it pinned and pans via the transform. The
       `.stage` is sized to the scaled content so the browser scrolls it. The
       list replaces this whole apparatus with plain flowing rows, below — so
       it's hidden (not unmounted: the shared `deviceCard` snippet lives inside
       it, and `layout` is empty in list mode, so nothing renders here anyway). -->
  <div
    class="scroller"
    class:scroll={isGrid}
    class:hidden={isList}
    bind:this={scroller}
    onscroll={onScroll}
  >
    <div class="stage" style="width: {stageW}px; height: {stageH}px;">
      <!-- edge layer -->
      <svg class="edges" width={stageW} height={stageH} aria-hidden="true">
        <g transform={layerTransformSvg}>
      {#each kvmTethers as tth (tth.id)}
        <!-- The physical KVM↔machine wiring: a quiet dashed tether under the
             live media wires, with a plug dot at the KVM end. -->
        <path
          class="wire-kvm"
          d="M {tth.x1} {tth.y1} L {tth.x2} {tth.y2}"
          fill="none"
        />
        <circle class="wire-kvm-plug" cx={tth.x1} cy={tth.y1} r="3.5" />
      {/each}
      {#each kvmMediaTethers as tth (tth.id)}
        <path
          class="wire-kvm-media"
          d="M {tth.x1} {tth.y1} L {tth.x2} {tth.y2}"
          fill="none"
        />
        <circle class="wire-kvm-media-source" cx={tth.x1} cy={tth.y1} r="4" />
      {/each}
      {#each edges as e (e.id)}
        <path
          class="wire"
          d="M {e.x1} {e.y1} Q {e.cx} {e.cy} {e.x2} {e.y2}"
          stroke={e.color}
          fill="none"
        />
        <path
          class="wire-flow"
          d="M {e.x1} {e.y1} Q {e.cx} {e.cy} {e.x2} {e.y2}"
          stroke={e.color}
          fill="none"
        />
        <!-- A wide transparent hit path so hovering anywhere near the wire
             raises its cursor-following media label. The edge layer is
             aria-hidden; this is a decorative hover affordance. -->
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <path
          class="wire-hit"
          d="M {e.x1} {e.y1} Q {e.cx} {e.cy} {e.x2} {e.y2}"
          fill="none"
          onpointerenter={(ev) => onWireMove(ev, e.media)}
          onpointermove={(ev) => onWireMove(ev, e.media)}
          onpointerleave={onWireLeave}
        />
      {/each}
    </g>
  </svg>

  <!-- node layer (HTML, shares the same transform) -->
  <div class="nodes" style="transform: {layerTransformCss};">
    {#each sections as s (s.key)}
      <!-- grid view only: one labelled band per fleet -->
      <div
        class="section"
        class:mine={s.key === "mine"}
        class:unknown={s.key === "unknown"}
        class:dragover={dragOverSection === s.label}
        data-section-label={s.label}
        style="left: {s.x}px; top: {s.y}px; width: {s.w}px; height: {s.h}px;"
      >
        <div class="section-head">
          {s.key === "mine" ? "🔗" : s.key === "unknown" ? "❓" : "🧑"}
          {s.label}
          <span class="section-count">{s.count}</span>
        </div>
      </div>
    {/each}
    {#each subheads as sh (sh.key)}
      <!-- grid view only: a mesh sub-band inside a fleet that spans meshes -->
      <div class="section-sub" style="left: {sh.x}px; top: {sh.y}px; width: {sh.w}px;">
        <span class="section-sub-label">{sh.label}</span>
      </div>
    {/each}
    {#each layout as p (p.node.id)}
      {@render deviceCard(p.node, p)}
    {/each}
      </div>
    </div>
  </div>

  {#if isList}
    <!-- The list view: a search bar tucked against the top edge of the centre
         area, then fleet-grouped rows scrolling beneath it. The only view on
         phones, the third on desktop. -->
    <div class="list-view">
      <div class="list-search">
        <div class="list-search-inner">
          <svg class="list-search-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <circle cx="11" cy="11" r="7" /><path d="m20 20-3.2-3.2" />
          </svg>
          <input
            class="list-search-field"
            type="search"
            autocomplete="off"
            autocapitalize="off"
            spellcheck="false"
            placeholder="Search your stuff…"
            aria-label="Search your stuff"
            bind:value={listQuery}
          />
          {#if listQuery}
            <button class="list-search-clear" aria-label="Clear search" onclick={() => (listQuery = "")}>✕</button>
          {/if}
        </div>
      </div>
      <div class="list-scroll" bind:this={listScroll}>
        {#if filteredListGroups.length === 0}
          <div class="list-noresults" aria-live="polite">
            {#if graphNodes.length === 0}
              <div class="list-noresults-orb">🧦</div>
              <div class="list-noresults-title">Getting your stuff together…</div>
              <div class="list-noresults-sub">Scanning this machine and waiting for peers to appear.</div>
            {:else}
              <div class="list-noresults-orb">🔍</div>
              <div class="list-noresults-title">Nothing matches “{listQuery}”</div>
              <div class="list-noresults-sub">Try a device name, an OS, a chip, or a mesh.</div>
            {/if}
          </div>
        {:else}
          {#each filteredListGroups as g (g.key)}
            {@const subs = displaySubsOf(g.nodes)}
            <section
              class="list-group"
              class:mine={g.key === "mine"}
              class:claimable={g.key === CLAIMABLE_KEY}
              class:unknown={g.key === "unknown"}
            >
              <header class="list-group-head">
                <span class="lg-icon" aria-hidden="true">{listGroupIcon(g.key)}</span>
                <span class="lg-label">{g.label}</span>
                <span class="lg-count">{g.nodes.length}</span>
              </header>
              <!-- One run of rows per mesh combination; a fleet living on a
                   single mesh set keeps today's flat list (no sub-header). -->
              {#each subs as sub (sub.key)}
                {#if subs.length > 1}
                  <div class="list-mesh-head">{sub.label}</div>
                {/if}
                <div class="list-rows">
                  {#each sub.nodes as n (n.id)}
                    {@render deviceCard(n, null)}
                  {/each}
                </div>
              {/each}
            </section>
          {/each}
        {/if}
      </div>
    </div>
  {/if}

  {#if gesture?.kind === "marquee"}
    <div
      class="marquee"
      style="left: {Math.min(gesture.x0, gesture.x1)}px; top: {Math.min(gesture.y0, gesture.y1)}px; width: {Math.abs(gesture.x1 - gesture.x0)}px; height: {Math.abs(gesture.y1 - gesture.y0)}px;"
    ></div>
  {/if}

  {#if nodeDrag?.moved && dragPos}
    <!-- The ghost that follows the cursor while a device is being dragged onto
         another device or fleet to start a share. -->
    <div class="ghost" class:ready={!!dropTargetLabel} style="left: {dragPos.x}px; top: {dragPos.y}px;">
      <span class="ghost-card">🔗 {dragLabel}</span>
      <span class="ghost-tip">
        {dropTargetLabel ? `New share → ${dropTargetLabel}` : "Drop on a device or fleet to share"}
      </span>
    </div>
  {/if}

  {#if lineTip}
    <div class="line-tip" style="left: {lineTip.x}px; top: {lineTip.y}px;">{lineTip.text}</div>
  {/if}

  <!-- Refresh progress — Restarting → Reconnecting → Connected, each dot going
       red → yellow → green, floating just above the bottom-centre of the graph
       so the result shows where the connections live. -->
  {#if app.restartFlow}
    <div class="restart-panel" role="status" aria-live="polite">
      {#each app.restartFlow as s, i (s.label)}
        {#if i > 0}
          <span class="restart-sep" class:done={app.restartFlow[i - 1].status === "ok"}></span>
        {/if}
        <span class="restart-step">
          <span class="restart-dot {s.status}"></span>
          <span class="restart-label">{s.label}</span>
        </span>
      {/each}
    </div>
  {/if}

  {#if graphNodes.length === 0 && !isList}
    <div class="empty" aria-live="polite">
      <div class="empty-orb">🧦</div>
      <div class="empty-title">Getting your stuff together…</div>
      <div class="empty-sub">Scanning this machine and waiting for peers to appear.</div>
    </div>
  {/if}

  {#if armed}
    <div class="arm-banner">
      Tap a device to connect: or tap empty space to cancel
      <button class="btn small" onclick={() => app.cancelConnect()}>Cancel</button>
    </div>
  {/if}

  <!-- The view switcher + zoom. Hidden on the phone, where the list is the
       only view (nothing to switch to, nothing to zoom). The zoom controls
       drop away in the list too — it scrolls, it doesn't scale. -->
  {#if !mobile}
    <div class="zoombar">
      {#if !normalMode}
        <button
          class="zbtn"
          class:active={view === "radial"}
          title="Radial view: this device in the centre"
          aria-label="Radial view"
          onclick={() => setView("radial")}>◎</button
        >
      {/if}
      <button
        class="zbtn"
        class:active={view === "grid"}
        title="Grid view: grouped by fleet"
        aria-label="Grid view, grouped by fleet"
        onclick={() => setView("grid")}>⊞</button
      >
      <button
        class="zbtn"
        class:active={view === "list"}
        title="List view: a searchable, fleet-grouped roster"
        aria-label="List view, grouped by fleet"
        onclick={() => setView("list")}>☰</button
      >
      {#if !normalMode && !isList}
        <span class="zsep"></span>
        <button class="zbtn" title="Zoom out (Ctrl/⌘ −)" onclick={() => applyZoom(width / 2, height / 2, zoom / 1.2)}>−</button>
        <button class="zbtn wide" title="Reset view (Ctrl/⌘ 0)" onclick={resetView}>{Math.round(zoom * 100)}%</button>
        <button class="zbtn" title="Zoom in (Ctrl/⌘ +)" onclick={() => applyZoom(width / 2, height / 2, zoom * 1.2)}>+</button>
      {/if}
    </div>
  {/if}
</div>

{#if driveMenu}
  <div
    class="node-menu drive-menu"
    role="menu"
    aria-label="Mapped drives"
    style="left: {driveMenu.left}px; top: {driveMenu.top}px;"
  >
    <DrivePanel target={driveMenu.target} />
  </div>
{/if}

<!-- Fleet actions live on the badge itself. Like the gear menu, this is a
     viewport-fixed sibling of the transformed graph so it never scales,
     clips, or wanders away from its trigger while near an edge. -->
{#if fleetMenu}
  {@const fleetId = fleetMenu.id}
  {@const fleetNode = app.node(fleetId)}
  {@const fleetSt = fleetNode ? app.standingOf(fleetNode) : null}
  {@const fleetSelf = app.isMe(fleetId) || fleetNode?.kind === "this"}
  {@const fleetKvm = app.isKvm(fleetNode)}
  {@const iOwn = app.myFleetRole === "owner"}
  {@const iManage = iOwn || app.myFleetRole === "manager"}
  <div
    class="node-menu fleet-menu"
    role="menu"
    aria-label={`${fleetNode ? displayName(fleetNode) : "Device"} fleet options`}
    style="left: {fleetMenu.left}px; top: {fleetMenu.top}px;"
  >
    {#if !fleetSelf && !fleetKvm && fleetSt?.role === "owner" && iOwn}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          void app.grantFleetRole(fleetId, "manager");
          fleetMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">↓</span>
        <span class="nm-text">
          <span class="nm-label">Demote to manager</span>
          <span class="nm-sub">remove owner authority</span>
        </span>
      </button>
    {:else if !fleetSelf && !fleetKvm && fleetSt?.role === "manager" && iOwn}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          void app.grantFleetRole(fleetId, "owner");
          fleetMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">★</span>
        <span class="nm-text">
          <span class="nm-label">Promote to owner</span>
          <span class="nm-sub">grant full fleet authority</span>
        </span>
      </button>
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          void app.withdrawFleetRole(fleetId);
          fleetMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">↓</span>
        <span class="nm-text">
          <span class="nm-label">Demote to member</span>
          <span class="nm-sub">remove management authority</span>
        </span>
      </button>
    {:else if !fleetSelf && !fleetKvm && fleetSt?.role !== "owner" && fleetSt?.role !== "manager" && iOwn}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          void app.grantFleetRole(fleetId, "manager");
          fleetMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">↑</span>
        <span class="nm-text">
          <span class="nm-label">Promote to manager</span>
          <span class="nm-sub">grant device-management authority</span>
        </span>
      </button>
    {/if}

    {#if !fleetSelf && ((fleetSt?.role === "manager" && iOwn) || (fleetSt?.role !== "owner" && fleetSt?.role !== "manager" && iManage))}
      <button
        class="nm-item evict"
        class:armed={fleetEvictArmed === fleetId}
        role="menuitem"
        onclick={() => {
          if (fleetEvictArmed === fleetId) {
            fleetEvictArmed = null;
            void app.kickFleetMember(fleetId);
            fleetMenu = null;
          } else {
            fleetEvictArmed = fleetId;
            setTimeout(() => {
              if (fleetEvictArmed === fleetId) fleetEvictArmed = null;
            }, 3500);
          }
        }}
      >
        <span class="nm-icon" aria-hidden="true">✕</span>
        <span class="nm-text">
          <span class="nm-label">Evict from fleet</span>
          <span class="nm-sub">{fleetEvictArmed === fleetId ? "click again to confirm" : "remove this device from your fleet"}</span>
        </span>
      </button>
    {/if}

    <button
      class="nm-item fleet-settings-item"
      role="menuitem"
      onclick={() => {
        fleetMenu = null;
        app.openSettings("fleet");
      }}
    >
      <span class="nm-icon" aria-hidden="true">⚙</span>
      <span class="nm-text">
        <span class="nm-label">Fleet settings</span>
        <span class="nm-sub">name, members, roles &amp; fleet key</span>
      </span>
    </button>
  </div>
{/if}

{#if kvmPowerMenu}
  {@const kvmId = kvmPowerMenu.kvmId}
  <div
    class="node-menu kvm-power-menu"
    role="menu"
    aria-label="Power (via KVM)"
    style="left: {kvmPowerMenu.left}px; top: {kvmPowerMenu.top}px;"
  >
    {#each [
      { action: "wake", label: "Wake", detail: "send a Shift key press" },
      { action: "short", label: "Short press", detail: "pulse the power button" },
      { action: "long", label: "Long press", detail: "hold power for 12 seconds" },
      { action: "reset", label: "Reset", detail: "pulse the reset line" },
    ] as item (item.action)}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          void app.kvmFeature(kvmId, item.action as "wake" | "short" | "long" | "reset");
          kvmPowerMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">{item.action === "wake" ? "⌨" : item.action === "reset" ? "↻" : "⏻"}</span>
        <span class="nm-text">
          <span class="nm-label">{item.label}</span>
          <span class="nm-sub">{item.detail}</span>
        </span>
      </button>
    {/each}
  </div>
{/if}

<!-- The per-node actions menu, rendered at the component root (outside the
     panned/zoomed `.nodes` layer) so its `position: fixed` anchors to the
     viewport, and flipped up/left by `openNodeMenu` to stay on screen. -->
{#if nodeMenu}
  {@const menuId = nodeMenu.id}
  {@const mn = app.node(menuId)}
  {@const menuCons = mn ? app.consoleAccess(mn) : null}
  <div class="node-menu" role="menu" style="left: {nodeMenu.left}px; top: {nodeMenu.top}px;">
    <button
      class="nm-item"
      role="menuitem"
      onclick={() => {
        void app.refreshNode(menuId);
        nodeMenu = null;
      }}
    >
      <span class="nm-icon" aria-hidden="true">↻</span>
      <span class="nm-text">
        <span class="nm-label">{app.isMe(menuId) ? "Rescan this device" : "Refresh details"}</span>
        <span class="nm-sub"
          >{app.isMe(menuId)
            ? "re-scan hardware, sites & options"
            : "re-learn its details, options & shares"}</span
        >
      </span>
    </button>
    {#if normalMode && menuCons?.terminal}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          app.openConsoleKind(menuId, "terminal");
          nodeMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">⌨</span>
        <span class="nm-text">
          <span class="nm-label">Open Terminal</span>
          <span class="nm-sub">open a shell on this machine</span>
        </span>
      </button>
    {/if}
    {#if normalMode && menuCons?.files}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          app.openConsoleKind(menuId, "files");
          nodeMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">{@render cicon("files")}</span>
        <span class="nm-text">
          <span class="nm-label">Files</span>
          <span class="nm-sub">browse files on this machine</span>
        </span>
      </button>
    {/if}
    {#if normalMode && mn && app.isKvm(mn) && app.kvmAllowed(mn)}
      <button
        class="nm-item drive-trigger"
        role="menuitem"
        onclick={(e) => {
          const id = menuId;
          openDriveMenu(e, id, id);
        }}
      >
        <span class="nm-icon" aria-hidden="true">{@render cicon("drives")}</span>
        <span class="nm-text">
          <span class="nm-label">Drives</span>
          <span class="nm-sub">present install or recovery media</span>
        </span>
      </button>
    {/if}
    {#if mn && app.isKvm(mn) && menuCons?.sites}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          app.openConsoleKind(menuId, "sites");
          nodeMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">{@render cicon("sites")}</span>
        <span class="nm-text">
          <span class="nm-label">Sites</span>
          <span class="nm-sub">open this KVM's web interface</span>
        </span>
      </button>
    {/if}
    {#if normalMode && mn && app.kvmAllowed(mn)}
      <button
        class="nm-item"
        role="menuitem"
        disabled={app.isKvmUpdating(menuId)}
        onclick={() => {
          // `menuId` is a reactive {@const} backed by `nodeMenu.id`. Capture
          // it before closing the menu or Svelte re-evaluates it against null.
          const id = menuId;
          nodeMenu = null;
          void app.updateKvm(id);
        }}
      >
        <span class="nm-icon" aria-hidden="true">{@render cicon("update")}</span>
        <span class="nm-text"><span class="nm-label">{app.isKvmUpdating(menuId) ? "Updating…" : "Update"}</span><span class="nm-sub">update this KVM</span></span>
      </button>
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          const id = menuId;
          nodeMenu = null;
          void app.openKvmWifi(id);
        }}
      >
        <span class="nm-icon" aria-hidden="true">⌁</span>
        <span class="nm-text"><span class="nm-label">Wi-Fi</span><span class="nm-sub">configure this KVM's network</span></span>
      </button>
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          const id = menuId;
          nodeMenu = null;
          toggleKvmAttach(id);
        }}
      >
        <span class="nm-icon" aria-hidden="true">🔗</span>
        <span class="nm-text"><span class="nm-label">Attach</span><span class="nm-sub">choose the machine behind this KVM</span></span>
      </button>
    {/if}
    {#if !app.isKvm(mn) && app.canRestartApp(mn)}
      <button
        class="nm-item"
        role="menuitem"
        onclick={() => {
          app.restartNodeApp(menuId);
          nodeMenu = null;
        }}
      >
        <span class="nm-icon" aria-hidden="true">⟲</span>
        <span class="nm-text">
          <span class="nm-label">{app.isMe(menuId) ? "Restart this app" : "Restart app"}</span>
          <span class="nm-sub"
            >{app.isMe(menuId) ? "relaunch AllMyStuff here" : "relaunch it on that machine"}</span
          >
        </span>
      </button>
    {/if}
    {#if !app.isKvm(mn) && app.canRestartDevice(mn)}
      <!-- The step past an app relaunch: reboot the whole machine. Two-step
           arm (the fleet-kick pattern) — a reboot is disruptive enough that a
           stray click must not fire it. -->
      <button
        class="nm-item"
        class:armed={restartDeviceArmed === menuId}
        role="menuitem"
        onclick={() => {
          if (restartDeviceArmed === menuId) {
            restartDeviceArmed = null;
            app.restartNodeDevice(menuId);
            nodeMenu = null;
          } else {
            restartDeviceArmed = menuId;
            setTimeout(() => {
              if (restartDeviceArmed === menuId) restartDeviceArmed = null;
            }, 3500);
          }
        }}
      >
        <span class="nm-icon" aria-hidden="true">⏻</span>
        <span class="nm-text">
          <span class="nm-label"
            >{app.isMe(menuId) ? "Restart this device" : "Restart device"}</span
          >
          <span class="nm-sub"
            >{restartDeviceArmed === menuId
              ? "click again to confirm"
              : app.isMe(menuId)
                ? "reboot this whole machine"
                : "reboot that whole machine"}</span
          >
        </span>
      </button>
    {/if}
    {#if mn && mn.kind !== "this" && !app.isMe(menuId)}
      <!-- Forget this node — the drawer's action, surfaced in the gear too.
           Drops it from the graph + roster and tears its session down. Same
           two-step arm as the reboot; never offered on this device. -->
      <button
        class="nm-item"
        class:armed={forgetNodeArmed === menuId}
        role="menuitem"
        onclick={() => {
          if (forgetNodeArmed === menuId) {
            forgetNodeArmed = null;
            void app.forgetNode(menuId);
            nodeMenu = null;
          } else {
            forgetNodeArmed = menuId;
            setTimeout(() => {
              if (forgetNodeArmed === menuId) forgetNodeArmed = null;
            }, 3500);
          }
        }}
      >
        <span class="nm-icon" aria-hidden="true">✕</span>
        <span class="nm-text">
          <span class="nm-label">Forget this node</span>
          <span class="nm-sub"
            >{forgetNodeArmed === menuId
              ? "click again to confirm"
              : "remove it from the graph and end its session"}</span
          >
        </span>
      </button>
    {/if}
  </div>
{/if}

<style>
  .canvas {
    position: relative;
    flex: 1;
    overflow: hidden;
    cursor: default;
    touch-action: none;
    user-select: none;
  }
  /* Right-drag pans (grabbing hand); left-drag on empty marquee-selects
     (crosshair). */
  .canvas.panning {
    cursor: grabbing;
  }
  .canvas.marqueeing {
    cursor: crosshair;
  }
  /* The marquee selection box. */
  .marquee {
    position: absolute;
    z-index: 4;
    border: 1px solid var(--accent);
    background: var(--accent-soft);
    border-radius: 2px;
    pointer-events: none;
  }
  /* The drag-to-share ghost — a chip that rides the cursor with a tooltip
     telling you a new share opens on drop. */
  .ghost {
    position: absolute;
    z-index: 8;
    transform: translate(14px, 14px);
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    pointer-events: none;
  }
  .ghost-card {
    align-self: flex-start;
    background: var(--surface);
    border: 1px solid var(--c-share);
    color: var(--ink);
    border-radius: var(--r-md);
    padding: 0.3rem 0.6rem;
    font-size: 0.8rem;
    font-weight: 700;
    box-shadow: var(--shadow-lg);
  }
  .ghost-tip {
    align-self: flex-start;
    background: oklch(0.16 0.02 285 / 0.97);
    border: 1px solid var(--line-strong);
    color: var(--ink-soft);
    border-radius: var(--r-pill);
    padding: 0.18rem 0.55rem;
    font-size: 0.7rem;
    font-weight: 650;
    box-shadow: var(--shadow-sm);
  }
  .ghost.ready .ghost-tip {
    border-color: var(--c-share);
    color: var(--c-share-ink);
    background: var(--c-share-soft);
  }
  /* The static dot backdrop — pinned to the canvas, never scrolls or pans. */
  .dots {
    position: absolute;
    inset: 0;
    pointer-events: none;
  }
  /* The scroll viewport. Grid view scrolls it; radial keeps it clipped and
     pans the layers inside via their transform. It fills the canvas, sitting
     above the dot backdrop and below the floating overlays (zoombar, banners). */
  .scroller {
    position: absolute;
    inset: 0;
    overflow: hidden;
  }
  .scroller.scroll {
    overflow: auto;
    overscroll-behavior: contain;
    /* Reserve the scrollbar's space so `width` is stable whether or not the
       vertical scrollbar is showing (no reflow jitter, no stray horizontal
       scrollbar). No-op on overlay-scrollbar platforms like macOS. */
    scrollbar-gutter: stable;
  }
  /* A quiet scrollbar that reads as part of the dark surface. */
  .scroller.scroll {
    scrollbar-width: thin;
    scrollbar-color: var(--line-strong) transparent;
  }
  .scroller.scroll::-webkit-scrollbar {
    width: 12px;
    height: 12px;
  }
  .scroller.scroll::-webkit-scrollbar-thumb {
    background: var(--line-strong);
    border: 3px solid transparent;
    background-clip: content-box;
    border-radius: 8px;
  }
  .scroller.scroll::-webkit-scrollbar-thumb:hover {
    background: var(--ink-faint);
    border: 3px solid transparent;
    background-clip: content-box;
  }
  .scroller.scroll::-webkit-scrollbar-corner {
    background: transparent;
  }
  /* The scroll content: sized to the scaled layout so it gives a real scrollbar
     (grid) or exactly fills the viewport (radial). */
  .stage {
    position: relative;
  }
  .edges {
    position: absolute;
    inset: 0;
    pointer-events: none;
  }
  .wire {
    stroke-width: 3;
    opacity: 0.45;
    stroke-linecap: round;
  }
  .wire-flow {
    stroke-width: 3;
    stroke-linecap: round;
    stroke-dasharray: 1 14;
    opacity: 0.9;
    animation: flow 1.1s linear infinite;
  }
  /* Invisible, fat hit target so the cursor-following label is easy to raise —
     opts back into pointer events even though the edge layer ignores them. */
  .wire-hit {
    stroke: transparent;
    stroke-width: 16;
    pointer-events: stroke;
    cursor: help;
  }
  /* The KVM↔machine tether: physical wiring, not a media route — quiet,
     dashed, unanimated, under the live wires. The plug dot marks the KVM end
     so the direction reads at a glance. */
  .wire-kvm {
    stroke: var(--ink-faint);
    stroke-width: 1.6;
    stroke-dasharray: 5 7;
    stroke-linecap: round;
    opacity: 0.7;
  }
  .wire-kvm-plug {
    fill: var(--surface);
    stroke: var(--ink-faint);
    stroke-width: 1.6;
    opacity: 0.8;
  }
  .wire-kvm-media {
    stroke: #45d9c5;
    stroke-width: 3;
    stroke-linecap: round;
    stroke-dasharray: 2 11;
    opacity: 0.9;
    animation: flow 1.1s linear infinite;
  }
  .wire-kvm-media-source {
    fill: #45d9c5;
    stroke: var(--surface);
    stroke-width: 2;
  }
  @keyframes flow {
    to {
      stroke-dashoffset: -30;
    }
  }
  /* The cursor-following wire label — a small black/grey tooltip naming what
     flows down the wire, placed right under the pointer. */
  .line-tip {
    position: absolute;
    z-index: 5;
    transform: translate(-50%, calc(-100% - 0.5rem));
    background: oklch(0.16 0.02 285 / 0.96);
    color: var(--ink);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.12rem 0.45rem;
    font-size: 0.7rem;
    font-weight: 650;
    white-space: nowrap;
    pointer-events: none;
    box-shadow: var(--shadow-sm);
  }
  /* The refresh 3-step panel — floats above the zoom bar at bottom centre. */
  .restart-panel {
    position: absolute;
    bottom: 4.4rem;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    align-items: center;
    gap: 0.55rem;
    z-index: 6;
    background: oklch(0.16 0.02 285 / 0.97);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-pill);
    padding: 0.5rem 0.95rem;
    box-shadow: var(--shadow-lg);
    animation: restart-rise 0.18s ease;
  }
  @keyframes restart-rise {
    from {
      transform: translate(-50%, 8px);
      opacity: 0;
    }
  }
  .restart-step {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
  }
  .restart-label {
    font-size: 0.78rem;
    font-weight: 650;
    color: var(--ink-soft);
  }
  .restart-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--danger);
    transition: background 0.3s ease, box-shadow 0.3s ease;
  }
  /* wait = red (default), go = yellow + pulse, ok = green. */
  .restart-dot.go {
    background: var(--warn);
    box-shadow: 0 0 0 3px var(--warn-soft);
    animation: restart-pulse 1s ease-in-out infinite;
  }
  .restart-dot.ok {
    background: var(--ok);
    box-shadow: 0 0 0 3px var(--ok-soft);
  }
  @keyframes restart-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 2px var(--warn-soft);
    }
    50% {
      box-shadow: 0 0 0 5px oklch(0.79 0.14 75 / 0.06);
    }
  }
  .restart-sep {
    width: 1.5rem;
    height: 2px;
    border-radius: 2px;
    background: var(--line-strong);
    transition: background 0.3s ease;
  }
  .restart-sep.done {
    background: var(--ok);
  }
  /* Zero-sized, top-left anchored: the node cards are absolutely positioned in
     content coordinates, so the layer's own box must stay 0×0 — otherwise the
     scaled box would balloon the scroll area past the actual content. */
  .nodes {
    position: absolute;
    top: 0;
    left: 0;
    width: 0;
    height: 0;
    transform-origin: 0 0;
    pointer-events: none;
  }
  /* Grid view's fleet bands — quiet containers behind the nodes. */
  .section {
    position: absolute;
    border: 1.5px dashed var(--line-strong);
    border-radius: var(--r-lg);
    background: oklch(0.21 0.028 285 / 0.35);
  }
  /* Your fleet band — the green of the "fleet" concept (your own pack). */
  .section.mine {
    border-color: oklch(0.8 0.17 150 / 0.45);
    background: oklch(0.8 0.17 150 / 0.05);
  }
  .section.unknown {
    border-style: dotted;
    background: transparent;
  }
  /* A fleet band lit as a share-drop target. */
  .section.dragover {
    border-color: var(--c-share);
    border-style: solid;
    background: var(--c-share-soft);
  }
  .section-head {
    position: absolute;
    top: 0.45rem;
    left: 0.8rem;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.78rem;
    font-weight: 750;
    color: var(--ink-soft);
    letter-spacing: 0.01em;
  }
  .section.mine .section-head {
    color: var(--c-fleet-ink);
  }
  .section-count {
    font-size: 0.66rem;
    font-weight: 700;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.02rem 0.4rem;
    color: var(--ink-faint);
  }
  /* A mesh sub-band inside a fleet section (grid view): a quiet ruled label
     seating the devices below it on the mesh(es) it names. */
  .section-sub {
    position: absolute;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    height: 24px;
    box-sizing: border-box;
    padding: 0 0.2rem;
    pointer-events: none;
  }
  .section-sub::after {
    content: "";
    flex: 1;
    border-top: 1px dashed var(--line);
  }
  .section-sub-label {
    font-size: 0.66rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 100%;
  }
  .node {
    position: absolute;
    pointer-events: auto;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-md);
    padding: 0.55rem 0.6rem 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    transition: transform 0.1s ease, box-shadow 0.12s ease, border-color 0.12s ease;
  }
  .node.normal {
    padding: 0.72rem 0.78rem 0.68rem;
    gap: 0.55rem;
  }
  .node:hover {
    transform: translateY(-2px);
    box-shadow: var(--shadow-lg), 0 0 0 1px var(--accent-soft),
      0 8px 30px -10px oklch(0.64 0.255 350 / 0.45);
  }
  .node.self {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft), var(--shadow-md);
  }
  /* A device shared with you wears the sharing concept's violet edge. */
  .node.shared {
    border-color: var(--c-share);
    background: linear-gradient(180deg, var(--surface-2), var(--surface));
  }
  .node.unclaimed {
    border-style: dashed;
    border-color: var(--line-strong);
  }
  /* Offering itself for adoption: a solid accent edge and a gentle pulsing
     halo, so a device you can claim invites the click the way a fresh, joinable
     thing should — the graph-level echo of the top-bar claim nudge. */
  .node.claimable {
    border-style: solid;
    border-color: var(--accent);
    animation: claim-halo 1.9s ease-out infinite;
  }
  .node.claimable:hover {
    transform: translateY(-2px);
  }
  @keyframes claim-halo {
    0% {
      box-shadow: 0 0 0 0 oklch(0.64 0.255 350 / 0.4), var(--shadow-md);
    }
    70% {
      box-shadow: 0 0 0 7px oklch(0.64 0.255 350 / 0), var(--shadow-md);
    }
    100% {
      box-shadow: 0 0 0 0 oklch(0.64 0.255 350 / 0), var(--shadow-md);
    }
  }
  /* The claim affordances drop out from *under* the node — floated below it so
     they never push siblings around — with a slide-in, and a shimmer on the
     accent Claim button to pull the eye to the new action. */
  /* A drawer that reads as part of *this* node: it tucks just under the card,
     inset and sharing the bottom edge (no top border, bottom-rounded), with a
     short stem down from the node's centre so it's unmistakably attached to
     the device above it — not floating loose between cards. */
  .node-drawer {
    position: absolute;
    top: calc(100% - 1px);
    left: 14px;
    right: 14px;
    z-index: 6;
    border: 1.5px solid var(--accent);
    border-top: none;
    border-radius: 0 0 var(--r-sm) var(--r-sm);
    padding: 0.42rem 0.5rem 0.4rem;
    font-size: 0.78rem;
    font-weight: 650;
    font-family: inherit;
    cursor: pointer;
    background: var(--surface);
    color: var(--accent-ink);
    box-shadow: 0 8px 16px oklch(0 0 0 / 0.18);
    transform-origin: top center;
    animation: drawer-drop 0.2s ease-out both;
  }
  /* The stem joining the drawer to the node's bottom edge. */
  .node-drawer::before {
    content: "";
    position: absolute;
    top: -8px;
    left: 50%;
    transform: translateX(-50%);
    width: 2px;
    height: 8px;
    background: var(--accent);
  }
  .node-drawer:hover {
    background: var(--accent-soft);
  }
  .claim-go {
    border-color: var(--accent);
    color: var(--accent-ink);
    background: linear-gradient(
      110deg,
      var(--accent-soft) 30%,
      oklch(0.7 0.16 350 / 0.5) 50%,
      var(--accent-soft) 70%
    );
    background-size: 220% 100%;
    animation: drawer-drop 0.22s ease-out both, shimmer 1.6s linear 0.22s infinite;
  }
  /* The KVM quick drawer reuses the .node-drawer chrome (border, stem, drop
     animation) but is a multi-button cluster rather than a single button, so
     it overrides the button-only bits. */
  .kvm-drawer {
    display: flex;
    flex-direction: column;
    gap: 0.32rem;
    cursor: default;
  }
  .kvm-drawer:hover {
    background: var(--surface);
  }
  .kvm-row {
    display: flex;
    gap: 0.32rem;
  }
  .kvm-act {
    flex: 1;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.3rem 0.4rem;
    font-size: 0.74rem;
    font-weight: 650;
    font-family: inherit;
    cursor: pointer;
    background: var(--surface-2);
    color: var(--ink);
    white-space: nowrap;
  }
  .kvm-act:hover {
    background: var(--accent-soft);
    border-color: var(--accent);
  }
  .kvm-pick {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.26rem 0.3rem;
    font-size: 0.74rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  .kvm-act:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .kvm-empty {
    margin: 0;
    font-size: 0.72rem;
    color: var(--ink-faint);
  }
  @keyframes drawer-drop {
    from {
      opacity: 0;
      transform: scaleY(0.4);
    }
    to {
      opacity: 1;
      transform: scaleY(1);
    }
  }
  @keyframes shimmer {
    from {
      background-position: 220% 0;
    }
    to {
      background-position: -20% 0;
    }
  }
  /* A device that isn't running AllMyStuff: quiet, washed-out, and not a
     connection target — present so you can see it's there, but it shouldn't
     invite a click the way your real machines do. */
  .node.meshonly {
    background: repeating-linear-gradient(
      135deg,
      var(--surface-2),
      var(--surface-2) 7px,
      var(--surface) 7px,
      var(--surface) 14px
    );
    border-style: dotted;
    border-color: var(--line-strong);
    box-shadow: var(--shadow-sm);
    opacity: 0.72;
  }
  .node.meshonly .avatar {
    filter: grayscale(1);
    opacity: 0.8;
  }
  .node.meshonly:hover {
    transform: none;
    box-shadow: var(--shadow-sm);
  }
  .node.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft), var(--shadow-lg);
  }
  /* The device being dragged (the original stays in place; a ghost rides the
     cursor), and the one it's hovering over — the share-drop target in the
     sharing concept's violet. */
  /* Your own devices are draggable (to start a share) — show the grab hand,
     except in connect mode (tap to connect) or while already dragging. */
  .node.grabbable:not(.armed):not(.dragging-node) {
    cursor: grab;
  }
  .node.dragging-node {
    opacity: 0.8;
    cursor: grabbing;
  }
  .node.dragover {
    border-color: var(--c-share);
    box-shadow: 0 0 0 3px var(--c-share-soft), var(--shadow-lg);
  }
  .node.offline {
    opacity: 0.6;
  }
  .node.armed {
    cursor: pointer;
  }
  .canvas.armed .node:not(.armed) {
    opacity: 0.5;
  }
  .node.armed:hover {
    border-color: var(--ok);
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.18), var(--shadow-lg);
  }
  .node-top {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .avatar {
    font-size: 1.25rem;
    line-height: 1;
    filter: drop-shadow(0 1px 1px rgba(0, 0, 0, 0.45));
  }
  .node-id {
    min-width: 0;
    flex: 1;
  }
  .node-label {
    font: inherit;
    line-height: 1.2;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .node-label-edit {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    max-width: 100%;
    border: 0;
    padding: 0;
    background: transparent;
    color: inherit;
    font: inherit;
    cursor: text;
    text-align: left;
  }
  .node-label-edit span {
    flex: none;
    font-size: 0.7rem;
    color: var(--ink-faint);
    opacity: 0.55;
  }
  .node-label-edit:hover span,
  .node-label-edit:focus-visible span {
    color: var(--accent-ink);
    opacity: 1;
  }
  .node-label-input {
    width: 100%;
    min-width: 0;
    border: 1px solid var(--accent);
    border-radius: var(--r-sm);
    background: var(--surface-2);
    color: var(--ink);
    padding: 0.16rem 0.3rem;
    font: inherit;
    line-height: 1.2;
    outline: 2px solid var(--accent-soft);
  }
  .node-sub {
    font-size: 0.72rem;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* The refresh control is a cbtn whose face is the online dot ringed by
     refresh arrows (clicking re-learns the node) rather than an icon. The ring
     fills the button, so it overrides cbtn's centred-svg sizing. */
  .status-refresh {
    flex-shrink: 0;
  }
  .refresh-face {
    position: relative;
    display: grid;
    place-items: center;
    width: 100%;
    height: 100%;
  }
  .refresh-face .refresh-ring {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    transform: none;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
    transform-box: fill-box;
    transform-origin: center;
    opacity: 0.6;
    transition:
      opacity 0.12s ease,
      transform 0.5s ease;
  }
  .status-refresh:hover .refresh-ring {
    opacity: 1;
  }
  /* While a refresh is in flight the ring spins continuously and the button
     stops taking clicks/hover; the dot keeps showing live status underneath.
     The spin is the only transform on the ring — there's no competing :active
     press-rotate to jank against it. */
  .status-refresh.spinning {
    pointer-events: none;
    color: var(--ink-faint);
    background: var(--surface-2);
    border-color: var(--line);
    box-shadow: none;
    opacity: 0.58;
  }
  .status-refresh.spinning .refresh-ring {
    opacity: 1;
    animation: refresh-spin 0.9s linear infinite;
  }
  @keyframes refresh-spin {
    to {
      transform: rotate(360deg);
    }
  }
  .cbtn:disabled {
    cursor: default;
  }
  /* Centred inside the ring rather than free-standing. */
  .dot {
    position: relative;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .dot.on {
    background: var(--ok);
    box-shadow: 0 0 0 2px oklch(0.8 0.17 150 / 0.18);
  }
  .refresh-face .dot {
    position: absolute;
    left: 50%;
    top: 50%;
    transform: translate(-50%, -50%);
    z-index: 1;
  }
  /* The settings gear is a cbtn with a glyph face instead of an icon — sized a
     touch larger than the console icons so it reads clearly. It sits right of
     the divider (which carries the flex slack), so it's pinned to the right of
     the bottom row. */
  .node-gear {
    font-size: 1.05rem;
    line-height: 1;
  }
  /* The gear's actions menu — fixed-positioned, flipped on screen by JS. */
  .node-menu {
    position: fixed;
    z-index: 70;
    width: 216px;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-lg);
    padding: 0.3rem;
    display: flex;
    flex-direction: column;
    gap: 0.12rem;
    animation: nmenu 0.12s ease;
    max-height: calc(100vh - 16px);
    overflow-y: auto;
  }
  .node-menu.drive-menu {
    width: min(346px, calc(100vw - 16px));
    padding: 0.3rem;
    overflow-x: hidden;
    overscroll-behavior: contain;
  }
  @keyframes nmenu {
    from {
      opacity: 0;
      transform: translateY(-3px);
    }
  }
  .nm-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    text-align: left;
    border: none;
    background: transparent;
    padding: 0.4rem 0.5rem;
    border-radius: var(--r-sm);
    cursor: pointer;
    color: var(--ink);
  }
  .nm-item:hover {
    background: var(--accent-soft);
  }
  /* The armed half of the reboot's two-step confirm reads as the warning
     it is. */
  .nm-item.armed {
    background: var(--danger-soft);
  }
  .nm-item.armed .nm-label {
    color: var(--danger);
  }
  .fleet-menu .nm-item.evict:not(.armed) .nm-icon,
  .fleet-menu .nm-item.evict:not(.armed) .nm-label {
    color: var(--danger);
  }
  .fleet-menu .nm-item.evict:hover {
    background: var(--danger-soft);
  }
  .fleet-menu .fleet-settings-item {
    margin-top: 0.12rem;
    border-top: 1px solid var(--line);
    border-radius: 0 0 var(--r-sm) var(--r-sm);
  }
  .nm-icon {
    font-size: 0.95rem;
    width: 1.1rem;
    text-align: center;
    flex-shrink: 0;
  }
  .nm-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .nm-label {
    font-weight: 600;
    font-size: 0.85rem;
  }
  .nm-sub {
    font-size: 0.72rem;
    color: var(--ink-faint);
  }
  /* Chips row: the chips fill the line, the refresh control sits at its right
     edge (not just trailing the last chip). */
  .node-meta-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .node-meta {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.25rem;
  }
  /* Console buttons on the card — replace the old mesh pills. One per console
     you can open on the device; clicking opens it. */
  .node-consoles {
    display: flex;
    flex-wrap: nowrap;
    align-items: center;
    gap: 0.26rem;
    margin-top: 0.15rem;
  }
  .action-label {
    display: none;
  }
  .node.normal .node-consoles {
    flex-wrap: wrap;
    gap: 0.42rem;
  }
  .node.normal .cbtn {
    display: inline-flex;
    gap: 0.38rem;
    width: auto;
    min-width: 2.25rem;
    height: 2.2rem;
    padding: 0 0.65rem;
    border-radius: var(--r-md);
    font-weight: 750;
  }
  .node.normal .cbtn .action-label {
    display: inline;
    white-space: nowrap;
    font-size: 0.75rem;
  }
  .node.normal .cbtn :global(svg) {
    width: 1.05rem;
    height: 1.05rem;
    transform: none;
  }
  .node.normal .status-refresh {
    width: auto;
    min-width: 5.4rem;
    height: 2rem;
    padding: 0 0.65rem 0 0.42rem;
    border-radius: var(--r-md);
  }
  .node.normal .status-refresh .refresh-face {
    width: 1.5rem;
    height: 1.5rem;
    flex: none;
  }
  /* The generic Normal-button SVG rule above shrinks ordinary action glyphs.
     Refresh is different: its SVG is the ring *around* the centred status dot,
     so it must fill this dedicated face instead of sitting at its top-left. */
  .node.normal .status-refresh .refresh-ring {
    width: 100%;
    height: 100%;
  }
  .node.normal .node-gear {
    font-size: 1rem;
  }
  .node.normal .cbtn-sep {
    display: none;
  }
  .node.normal .node-gear {
    margin-left: 0 !important;
  }
  /* A hairline divider that caps the action buttons and sets the gear apart —
     it carries the flex slack (margin-left:auto), so the row reads
     "actions … │ ⚙" with the rule sitting right against the gear. */
  .cbtn-sep {
    margin-left: auto;
    align-self: stretch;
    width: 1px;
    min-height: 1.1rem;
    background: var(--line-strong);
    opacity: 0.7;
  }
  /* No action buttons before it (a bare self card, or a device with no
     consoles): the divider would separate nothing — hide it and let the gear
     carry the right-edge slack itself. */
  .cbtn-sep:first-child {
    display: none;
  }
  .cbtn-sep:first-child + .node-gear {
    margin-left: auto;
  }
  .cbtn {
    position: relative;
    display: grid;
    place-items: center;
    flex: none;
    width: 1.42rem;
    height: 1.42rem;
    border-radius: var(--r-sm);
    border: 1px solid var(--line-strong);
    background: var(--surface-2);
    color: var(--ink-soft);
    box-shadow: var(--shadow-sm);
    transition: border-color 0.12s ease, color 0.12s ease, background 0.12s ease,
      transform 0.08s ease;
  }
  .cbtn:hover {
    border-color: var(--accent);
    color: var(--accent-ink);
    background: var(--surface);
  }
  .cbtn:active {
    transform: translateY(1px);
  }
  /* The attach link button while its drop-out is open. */
  .cbtn.active {
    border-color: var(--accent);
    color: var(--accent-ink);
    background: var(--accent-soft);
  }
  /* Physical host-power status from the KVM's LED input. This is deliberately
     green only for a positive `pwr: true`; unreachable/unknown stays neutral. */
  .cbtn.power-on {
    border-color: oklch(0.72 0.16 150 / 0.72);
    color: oklch(0.9 0.12 150);
    background: oklch(0.72 0.16 150 / 0.18);
    box-shadow: inset 0 0 0 1px oklch(0.72 0.16 150 / 0.12), var(--shadow-sm);
  }
  .cbtn.update-action {
    border-color: var(--accent);
    color: var(--accent-ink);
    background: var(--accent-soft);
  }
  .cbtn.share-action {
    border-color: var(--accent);
    color: var(--accent-ink);
    background: var(--accent-soft);
  }
  .cbtn.room-action {
    border-color: color-mix(in oklch, var(--accent) 55%, var(--line-strong));
    color: var(--accent-ink);
  }
  .cbtn :global(svg) {
    width: 0.9rem;
    height: 0.9rem;
    /* the glyphs sit right of centre — nudge them left so the spare pixels
       land on the right. */
    transform: translateX(-3px);
  }
  /* A quick black/grey tooltip (vs the slow native title) — shared by the
     console buttons below and the refresh/gear controls on the chips row. */
  .cbtn[data-tip]::after,
  .status-refresh[data-tip]::after,
  .node-gear[data-tip]::after {
    content: attr(data-tip);
    position: absolute;
    bottom: calc(100% + 5px);
    left: 50%;
    transform: translateX(-50%) translateY(3px);
    background: oklch(0.16 0.02 285 / 0.97);
    color: var(--ink);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.1rem 0.4rem;
    font-size: 0.64rem;
    font-weight: 650;
    white-space: nowrap;
    opacity: 0;
    pointer-events: none;
    box-shadow: var(--shadow-sm);
    transition: opacity 0.1s ease 0.1s, transform 0.1s ease 0.1s;
    z-index: 6;
  }
  .cbtn[data-tip]:hover::after,
  .status-refresh[data-tip]:hover::after,
  .node-gear[data-tip]:hover::after {
    opacity: 1;
    transform: translateX(-50%) translateY(0);
  }
  .tag {
    font-size: 0.64rem;
    font-weight: 650;
    padding: 0.08rem 0.4rem;
    border-radius: var(--r-pill);
    letter-spacing: 0.01em;
  }
  .tag.soft {
    background: var(--surface-2);
    color: var(--ink-soft);
  }
  /* "This device" — a pink chip that bumps over the top-left corner of your
     own card, so it reads as a label on the card rather than another tag. */
  .self-corner {
    position: absolute;
    top: -0.6rem;
    left: 0.7rem;
    z-index: 2;
    font-size: 0.58rem;
    font-weight: 800;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: #fff;
    background: linear-gradient(180deg, var(--accent-ink), var(--accent));
    border: 1px solid oklch(0.58 0.235 350);
    border-radius: var(--r-pill);
    padding: 0.1rem 0.5rem;
    box-shadow: 0 2px 6px -2px oklch(0.64 0.255 350 / 0.6),
      inset 0 1px 0 oklch(1 0 0 / 0.3);
    pointer-events: none;
  }
  .tag.mine {
    background: var(--ok-soft);
    color: var(--ok);
  }
  .tag.guest {
    background: var(--c-share-soft);
    color: var(--c-share-ink);
  }
  .tag.unclaimed {
    background: var(--surface-2);
    color: var(--ink-soft);
    border: 1px dashed var(--line-strong);
  }
  .tag.claimable {
    background: var(--accent-soft);
    color: var(--accent-ink);
    border: 1px solid var(--accent);
    font-weight: 700;
  }
  /* The ICE watchdog's "this link needs a TURN relay" verdict — a warning,
     not a decoration: it names the only fix. */
  .tag.blocked {
    background: var(--warn-soft);
    color: var(--warn);
    border: 1px solid var(--warn);
    font-weight: 700;
  }
  /* "Someone else's, not shared with me" — bronze keeps it distinct from a
     device actually shared with you (violet, above). */
  .tag.theirs {
    background: var(--bronze-soft);
    color: var(--bronze);
  }
  /* Fleet membership is actionable, so this deliberately reads as a compact
     green button rather than another passive pill in the metadata row. */
  .fleet-button {
    appearance: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 0.4rem;
    min-height: 1.8rem;
    font-family: inherit;
    font-size: 0.72rem;
    font-weight: 700;
    line-height: 1.2;
    padding: 0.28rem 0.62rem;
    border: 1px solid color-mix(in oklch, var(--c-fleet) 78%, var(--surface));
    border-radius: var(--r-sm);
    color: oklch(0.96 0.035 150);
    background: linear-gradient(
      180deg,
      color-mix(in oklch, var(--c-fleet) 42%, var(--surface)),
      color-mix(in oklch, var(--c-fleet) 27%, var(--surface))
    );
    cursor: pointer;
    box-shadow: inset 0 1px 0 oklch(1 0 0 / 0.2), 0 2px 5px -3px var(--c-fleet);
    transition: border-color 0.12s ease, filter 0.12s ease, transform 0.08s ease,
      box-shadow 0.12s ease;
  }
  .fleet-trigger:hover,
  .fleet-trigger[aria-expanded="true"] {
    border-color: var(--c-fleet);
    filter: brightness(1.12);
    box-shadow: inset 0 1px 0 oklch(1 0 0 / 0.25), 0 3px 8px -3px var(--c-fleet);
  }
  .fleet-trigger:active {
    transform: translateY(1px);
    box-shadow: inset 0 1px 3px oklch(0 0 0 / 0.25);
  }
  .fleet-trigger:focus-visible {
    outline: 2px solid var(--c-fleet);
    outline-offset: 2px;
  }
  .fleet-button.owner {
    font-weight: 750;
  }
  .fleet-caret {
    font-size: 0.62rem;
    opacity: 0.78;
    transition: transform 0.12s ease;
  }
  .fleet-trigger[aria-expanded="true"] .fleet-caret {
    transform: rotate(180deg);
  }
  .tag.meshonly {
    background: var(--surface-2);
    color: var(--ink-faint);
    border: 1px dotted var(--line-strong);
  }
  .empty {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.35rem;
    text-align: center;
    padding: 1rem;
    pointer-events: none;
  }
  .empty-orb {
    font-size: 2.6rem;
    filter: drop-shadow(0 3px 6px oklch(0.64 0.255 350 / 0.35));
    animation: breathe 2.4s ease-in-out infinite;
  }
  .empty-title {
    font-weight: 750;
    font-size: 1.05rem;
    color: var(--ink);
  }
  .empty-sub {
    font-size: 0.84rem;
    color: var(--ink-faint);
    max-width: 22rem;
  }
  @keyframes breathe {
    0%,
    100% {
      transform: scale(1);
      opacity: 0.85;
    }
    50% {
      transform: scale(1.08);
      opacity: 1;
    }
  }
  .arm-banner {
    position: absolute;
    top: 1rem;
    left: 50%;
    transform: translateX(-50%);
    background: var(--surface-2);
    border: 1px solid var(--line-strong);
    color: var(--ink);
    padding: 0.5rem 0.7rem 0.5rem 1rem;
    border-radius: var(--r-pill);
    font-size: 0.82rem;
    font-weight: 550;
    display: flex;
    align-items: center;
    gap: 0.7rem;
    box-shadow: var(--shadow-lg);
    animation: drop 0.16s ease;
  }
  .arm-banner .btn {
    background: rgba(255, 255, 255, 0.1);
    border-color: transparent;
    color: var(--ink);
    box-shadow: none;
  }
  @keyframes drop {
    from {
      transform: translate(-50%, -8px);
      opacity: 0;
    }
  }
  .zoombar {
    position: absolute;
    right: 1rem;
    bottom: 1rem;
    display: flex;
    align-items: center;
    gap: 0.25rem;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.2rem;
    box-shadow: var(--shadow-sm);
  }
  .zbtn {
    border: none;
    background: transparent;
    color: var(--ink-soft);
    width: 2rem;
    height: 1.8rem;
    border-radius: var(--r-pill);
    font-size: 0.95rem;
  }
  .zbtn:hover {
    background: var(--surface-2);
  }
  .zbtn.active {
    background: var(--accent-soft);
    color: var(--accent-ink);
  }
  .zbtn.wide {
    width: 3rem;
    font-size: 0.74rem;
    font-variant-numeric: tabular-nums;
  }
  .zsep {
    width: 1px;
    height: 1.1rem;
    background: var(--line);
    margin: 0 0.1rem;
  }
  /* Touch: the zoombar is fingers-only there (no wheel), so its buttons get
     the ~40px minimum and clear the home-indicator inset. */
  @media (hover: none) {
    .zoombar {
      bottom: calc(1rem + env(safe-area-inset-bottom, 0px));
    }
    .zbtn {
      width: 2.6rem;
      height: 2.4rem;
      font-size: 1.05rem;
    }
    .zbtn.wide {
      width: 3.4rem;
      font-size: 0.8rem;
    }
  }

  /* ---- list view ----------------------------------------------------- */

  /* The graph scroller is kept mounted but hidden in list mode (the shared
     device-card snippet lives inside it); the list draws over the same canvas. */
  .scroller.hidden {
    display: none;
  }

  /* The list fills the canvas as a flex column: a fixed search bar tucked
     against the top edge, then the scrolling roster beneath it.
     `isolation: isolate` gives it its own stacking context — so the sticky
     fleet headers (z-index) and the cards' drop-out drawers stay contained
     inside the list — without pinning the whole overlay above the floating
     controls: at z-index:auto it stacks by DOM order, so the later-in-markup
     zoombar (and the connect-mode banner) still paint and receive clicks above
     it. A positive z-index here instead would trap a desktop user in the list,
     with the view toggles painted but un-clickable beneath the overlay. */
  .list-view {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    min-height: 0;
    isolation: isolate;
  }

  /* The search bar — hugging the top edge of the centre area, a hairline
     dividing it from the rows that scroll under it. Translucent + blurred so
     the canvas reads through it and it feels part of the surface, not bolted
     on. */
  .list-search {
    flex-shrink: 0;
    padding: 0.7rem 1rem;
    background: oklch(0.135 0.022 285 / 0.72);
    backdrop-filter: blur(14px) saturate(1.15);
    border-bottom: 1px solid var(--line);
  }
  .list-search-inner {
    position: relative;
    display: flex;
    align-items: center;
    max-width: 680px;
    margin: 0 auto;
  }
  .list-search-icon {
    position: absolute;
    left: 0.85rem;
    width: 1.05rem;
    height: 1.05rem;
    color: var(--ink-faint);
    pointer-events: none;
  }
  .list-search-field {
    flex: 1;
    min-width: 0;
    font-family: inherit;
    font-size: 0.92rem;
    color: var(--ink);
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-pill);
    padding: 0.62rem 2.4rem 0.62rem 2.5rem;
    box-shadow: var(--shadow-sm), inset 0 1px 0 oklch(1 0 0 / 0.03);
    transition: border-color 0.12s ease, box-shadow 0.12s ease, background 0.12s ease;
  }
  .list-search-field::placeholder {
    color: var(--ink-faint);
  }
  .list-search-field:focus {
    outline: none;
    border-color: var(--accent);
    background: var(--surface-2);
    box-shadow: 0 0 0 3px var(--accent-soft), var(--shadow-sm);
  }
  /* Drop the WebKit search decorations — we supply our own clear button. */
  .list-search-field::-webkit-search-decoration,
  .list-search-field::-webkit-search-cancel-button {
    -webkit-appearance: none;
    appearance: none;
  }
  .list-search-clear {
    position: absolute;
    right: 0.5rem;
    display: grid;
    place-items: center;
    width: 1.6rem;
    height: 1.6rem;
    border: none;
    border-radius: 50%;
    background: var(--surface-2);
    color: var(--ink-soft);
    font-size: 0.8rem;
    line-height: 1;
  }
  .list-search-clear:hover {
    background: var(--line-strong);
    color: var(--ink);
  }

  /* The scrolling roster. A quiet scrollbar matching the grid's. */
  .list-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
    scrollbar-gutter: stable;
    scrollbar-width: thin;
    scrollbar-color: var(--line-strong) transparent;
    padding: 0.75rem 1rem 3rem;
  }
  .list-scroll::-webkit-scrollbar {
    width: 12px;
  }
  .list-scroll::-webkit-scrollbar-thumb {
    background: var(--line-strong);
    border: 3px solid transparent;
    background-clip: content-box;
    border-radius: 8px;
  }
  .list-scroll::-webkit-scrollbar-thumb:hover {
    background: var(--ink-faint);
    border: 3px solid transparent;
    background-clip: content-box;
  }

  /* One fleet per group, a centred column so the list reads as a column on a
     wide desktop as cleanly as it fills a phone. */
  .list-group {
    max-width: 680px;
    margin: 0 auto 1.4rem;
  }
  .list-group:last-child {
    margin-bottom: 0;
  }
  /* The fleet header sticks under the search bar as its devices scroll past,
     so you never lose which fleet you're looking at in a long roster. */
  .list-group-head {
    position: sticky;
    top: 0;
    z-index: 4;
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.5rem 0.15rem 0.55rem;
    font-size: 0.82rem;
    font-weight: 750;
    letter-spacing: 0.01em;
    color: var(--ink-soft);
    background: linear-gradient(
      180deg,
      var(--bg) 55%,
      oklch(0.135 0.022 285 / 0)
    );
  }
  .lg-icon {
    font-size: 0.9rem;
    line-height: 1;
  }
  .lg-label {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .lg-count {
    font-size: 0.66rem;
    font-weight: 700;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.02rem 0.42rem;
    color: var(--ink-faint);
  }
  /* Your fleet wears the fleet green; the claim shelf wears the brand accent —
     the same concept colours the graph's bands use. */
  .list-group.mine .lg-label {
    color: var(--c-fleet-ink);
  }
  .list-group.claimable .lg-label {
    color: var(--accent-ink);
  }
  .list-group.claimable .lg-icon {
    color: var(--accent);
    font-weight: 800;
  }
  .list-group.claimable .lg-count {
    background: var(--accent-soft);
    border-color: var(--accent);
    color: var(--accent-ink);
  }

  .list-rows {
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  /* A mesh sub-header inside a fleet's list group — same quiet ruled label as
     the grid's sub-bands, in flow between the runs of rows. */
  .list-mesh-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 0.55rem 0 0.4rem;
    font-size: 0.66rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--ink-faint);
    white-space: nowrap;
  }
  .list-mesh-head::after {
    content: "";
    flex: 1;
    border-top: 1px dashed var(--line);
  }

  /* The device card as a list row: a full-width card in normal flow (the graph
     absolutely-positions and fixes its width; the list lets it breathe). */
  .node.list {
    position: relative;
    width: 100%;
  }
  /* No pulsing halo in the claim shelf — a whole group of them would throb;
     the accent border and the group header already say "claimable". */
  .node.list.claimable {
    animation: none;
  }
  /* The claim / KVM drop-outs flow in the row instead of floating under it, so
     opening one grows the card and nudges the rows below rather than covering
     them (there's no graph whitespace to drop into here). */
  .node.list .node-drawer {
    position: static;
    left: auto;
    right: auto;
    margin-top: 0.5rem;
    border: 1.5px solid var(--accent);
    border-radius: var(--r-sm);
    box-shadow: none;
  }
  .node.list .node-drawer::before {
    display: none;
  }

  /* The empty / no-results panel, centred in the roster. */
  .list-noresults {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.35rem;
    text-align: center;
    padding: 3.5rem 1rem;
    color: var(--ink-faint);
  }
  .list-noresults-orb {
    font-size: 2.2rem;
    filter: drop-shadow(0 3px 6px oklch(0.64 0.255 350 / 0.3));
  }
  .list-noresults-title {
    font-weight: 750;
    font-size: 1rem;
    color: var(--ink);
  }
  .list-noresults-sub {
    font-size: 0.82rem;
    max-width: 22rem;
  }

  /* Phone: edge-to-edge rows, a touch more room for thumbs. */
  @media (max-width: 700px) {
    .list-search {
      padding: 0.6rem 0.75rem;
    }
    .list-scroll {
      padding: 0.6rem 0.75rem 3.5rem;
    }
  }
</style>
