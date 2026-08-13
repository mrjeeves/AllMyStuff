<script lang="ts">
  // The remote console — a pikvm-style session for another machine,
  // shaped like a KVM's web console: the picture takes the whole pane and
  // one floating control bar rides over it, top-center. Every control
  // lives on that bar — the screens/cameras drop-down, the keyboard &
  // mouse / audio / clipboard toggles, the quality menu, fullscreen, and
  // the way out — so the same layout works from a desktop window down to
  // a phone held upright. The bar drags out of the way, sleeps after a
  // few idle seconds, and comes back from a slim handle at the top edge.
  // It owns the real routes the session runs on, so toggles here actually
  // wire (and unwire) the mesh.
  //
  // Two skins, one component: the desktop renders it `windowed` — filling
  // a dedicated per-machine OS window (see ConsoleHost) so several
  // consoles can be open at once — while the web preview and the phone
  // run it in-page. Either way the console IS the viewport it's given:
  // no modal card, no border padding — full size, respecting only the
  // hardware (notch, home indicator) via safe-area insets.
  //
  // The stage is a live MJPEG sink: the backend pushes each inbound frame
  // for the watched route over a per-route IPC channel (raw JPEG bytes —
  // see `watchVideo`), and this component shows the latest one. When
  // "Keyboard & mouse" is on, the stage captures pointer/key events,
  // normalizes coordinates onto the streamed frame, and forwards them down
  // the control route. Touch input speaks the trackpad dialect instead of
  // the pen one (see console-touch.ts): drags glide the cursor,
  // tap-then-drag holds the button, and two fingers pinch-zoom the view.
  import { flushSync, onMount, untrack } from "svelte";
  import { makeKeyForwarder } from "../input-keys";
  import { makeRelativeMotionForwarder } from "../relative-motion";
  import { makeTouchMouse, type ViewTransform } from "../console-touch";
  import { app } from "../store.svelte";
  import {
    clientLog,
    closeThisWindow,
    focusThisWindow,
    isMobile,
    isTauri,
    onNativeFileDrag,
    onThisWindowClose,
    refreshRoute,
    setMacRelativePointerCapture,
    sendVideoFeedback,
    toggleWindowFullscreen,
    watchVideo,
    watchVideoStatus,
    type NativeDecoderPreference,
    type VideoHostStatus,
  } from "../tauri";
  import {
    displayName,
    originIcon,
    mediaColor,
    MEDIA,
    type Capability,
    type InputAction,
    type MediaKind,
  } from "../types";
  import ConsoleKeys from "./ConsoleKeys.svelte";
  import ModePill from "./ModePill.svelte";
  import EffectiveReadout from "./EffectiveReadout.svelte";
  import DrivePanel from "./DrivePanel.svelte";

  let { windowed = false }: { windowed?: boolean } = $props();

  const node = $derived(app.consoleNode);
  const kvmSource = $derived(app.isKvm(node ?? undefined));
  const consoleKvm = $derived(
    node ? (kvmSource ? node : app.kvmAttachedTo(node.id)) : undefined,
  );
  // What this machine actually shared with us — the console activates with
  // whatever subset is available and hides the toggles for the rest (a
  // screen-only share shows the screen, no inert Audio/Control buttons).
  const access = $derived(
    node
      ? app.consoleAccess(node)
      : { remote: false, files: false, terminal: false, sites: false, audio: false, control: false, clipboard: false },
  );
  const drivesAllowed = $derived(
    !!node &&
      (kvmSource
        ? app.kvmDoors(node)
        : app.isFleetMember(node.id) || app.filesAllowed(node) || app.isCecCustomer(node.id)),
  );
  const kvmMediaAllowed = $derived(
    !!node &&
      !!consoleKvm &&
      (kvmSource ? app.kvmDoors(consoleKvm) : app.kvmPassthroughDoors(consoleKvm, node)),
  );
  const kvmPowerAllowed = $derived(kvmMediaAllowed);
  const inputs = $derived(node ? app.consoleVideoInputs(node.id) : []);
  const selectedId = $derived(app.consoleInput);
  const selected = $derived<Capability | null>(
    (selectedId
      ? inputs.find((input) => input.id === selectedId) ?? app.capability(selectedId)
      : null) ?? null,
  );
  // Auto-select the first input (the screen leads the list) once the remote's
  // video sources land. The console opens before the caps arrive, so the
  // open-time pick can come up empty; pick reactively when they show up. Guarded
  // on nothing being selected, so it never overrides a manual pick.
  $effect(() => {
    if ((!selectedId || !inputs.some((input) => input.id === selectedId)) && inputs.length > 0) {
      app.setConsoleInput(inputs[0].id);
    }
  });
  // Whether the machine's build streams its cameras at all — an older one
  // advertises the tabs but has no transport behind them, and the stage
  // says so instead of waiting on pixels that never come.
  const cameraSupported = $derived(node ? app.cameraStreamSupported(node) : false);
  // The selected input is off in its own popout window — the stage shows
  // the big "Return video here" instead of competing for the stream.
  const selectedPopped = $derived(!!selectedId && app.isVideoPopped(`cap:${selectedId}`));
  // Pointer forwarding is live only over a *desktop* picture: coordinates
  // normalize onto the streamed frame, and only a screen's frame maps
  // back onto the remote desktop — a camera tab is a viewing surface.
  // (Keys keep the session rule: with control on they always belong to
  // the remote, whichever tab is showing.)
  const stagePointerActive = $derived(app.consoleControl && selected?.media === "display");
  // Availability is authorization, not current route state. A shared console
  // may open before its granted control leg has come up; keep relative mode
  // visible there and let its click turn that leg on.
  const relativeMouseAvailable = $derived(
    access.control && selected?.media === "display" && !kvmSource,
  );
  // Fullscreen ("theater"): the stage takes the whole window over and —
  // windowed — the OS window goes fullscreen too, so exactly this video
  // fills the screen. The bar's ⛶ flips it both ways (no hover-only
  // corner: on touch there is no hover), and Esc exits while control is
  // off; with control on every key belongs to the remote.
  let theater = $state(false);

  async function flipTheater() {
    theater = !theater;
    // The in-page card grows/shrinks to the viewport — a dragged bar must
    // re-clamp into the new bounds (measured after the layout lands).
    requestAnimationFrame(clampBarPos);
    if (windowed) await toggleWindowFullscreen();
    // The OS fullscreen transition blurs the stage (firing keys.releaseAll via
    // onblur), and claimFocus won't re-pin once the window itself holds focus —
    // so with control on, re-pin here or keyboard forwarding silently stops
    // until the user clicks back into the picture ("controls stopped mapping").
    if (app.consoleControl && !keysOpen) {
      await focusThisWindow();
      stageEl?.focus({ preventScroll: true });
    }
  }
  // Which remote monitor the stage is showing (`<node>:screen:<id>`),
  // undefined for the primary `screen` (and for cameras) — rides every
  // mouse move so the remote lands the cursor on the screen being viewed.
  const controlScreen = $derived.by(() => {
    const m = selectedId?.match(/:screen:(\d+)$/);
    return m ? Number(m[1]) : undefined;
  });

  // ---- live video ---------------------------------------------------
  //
  // The stage is a canvas with three decode paths: H.264 access units
  // through WebCodecs (hardware where the webview has it), JPEG frames
  // through createImageBitmap, and — when this webview has no WebCodecs
  // (Linux WebKitGTK) or its decoder stalled out — the backend's native
  // openh264 decoder, which delivers ready-to-paint RGBA frames this
  // component just blits. The WebCodecs decoder is created lazily on the
  // first key unit and rebuilt on the next one after any error.
  let canvasEl = $state<HTMLCanvasElement | null>(null);
  // The role=application stage. Control key forwarding is bound here (not to
  // `<svelte:window>`) so it fires whenever the stage holds focus — and
  // hover/click/toggle pin that focus, the reliable way a dedicated console
  // window's keyboard reaches the remote (window-level setFocus alone doesn't
  // push document focus into the webview on hover).
  let stageEl = $state<HTMLElement | null>(null);
  let dropHover = $state(false);
  let dropBusy = $state(false);
  let dropCount = $state(0);

  function pointIsOnStage(x: number, y: number): boolean {
    const r = stageEl?.getBoundingClientRect();
    return !!r && x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
  }

  async function dropFiles(paths: string[], x: number, y: number) {
    dropHover = false;
    if (!pointIsOnStage(x, y)) return;
    const point = normPoint({ clientX: x, clientY: y });
    if (!point || !stagePointerActive || !app.consoleClipboard) {
      app.toast("warn", "Turn on Keyboard & mouse and Clipboard before dropping files");
      return;
    }
    const targetName = node ? displayName(node) : "the remote machine";
    dropBusy = true;
    try {
      await app.dropConsoleFiles(paths, { ...point, screen: controlScreen });
      const noun = paths.length === 1 ? "file" : "files";
      app.toast("info", `${paths.length} ${noun} sent to ${targetName}`);
    } catch (error) {
      app.toast("warn", `Couldn't send dropped files: ${error}`);
    } finally {
      dropBusy = false;
      dropCount = 0;
    }
  }
  // A thin aiming crosshair at the position we're COMMANDING (`virt`) — drawn
  // instantly, with none of the video's latency, so you can line things up
  // precisely instead of guessing where the cursor will land. It complements
  // the real remote cursor in the video (which shows where the cursor actually
  // is, a beat behind), rather than replacing it. Positioned imperatively (see
  // updateCrosshair) since `virt` isn't reactive state.
  let crosshairEl = $state<HTMLElement | null>(null);
  let hasFrame = $state(false);
  // A monitor/input switch in progress: the old picture stays on the
  // (still-mounted) canvas, dimmed under a "Switching…" chip, instead of
  // flashing to the placeholder and back — the switch reads as a
  // crossfade, not a teardown. Cleared by the first fresh frame; a 5 s
  // timeout falls back to the honest placeholder (host status text) if
  // the new stream never comes. Input stays gated on hasFrame the whole
  // time, so pointer mapping can never land on the stale picture.
  let switching = $state(false);
  let switchingTimer: ReturnType<typeof setTimeout> | undefined;
  function armSwitching() {
    switching = true;
    clearTimeout(switchingTimer);
    switchingTimer = setTimeout(() => (switching = false), 5000);
  }
  // The host's word on why pixels aren't flowing (`vstat`): shown on the
  // placeholder before the first frame, and as a banner if the stream
  // stalls after one. Null = no condition reported (or it cleared).
  let hostStatus = $state<VideoHostStatus | null>(null);
  // The video route was refused, or its offer expired unanswered (the far
  // side's app isn't running, or it NACKed a route it no longer holds) —
  // the reason replaces "Connecting…" so a dead stage explains itself.
  const videoRefused = $derived.by(() => {
    const live = app.consoleVideoLive;
    if (!live) return null;
    const st = app.routeStates[live];
    return st?.state === "rejected" ? st.reason || "the far side refused the stream" : null;
  });
  let frameW = $state(0);
  let frameH = $state(0);
  let fps = $state(0);
  // Honest per-stage rates so a pinned number names its own bottleneck:
  // `inFps` arrives on the wire, `decFps` leaves the decoder, `fps` is what
  // actually paints. dec ≫ paint ⇒ the blit/compositor is the wall; in ≈ dec ≈
  // paint and all low ⇒ the decoder is (software H.264 of a 4K frame tops out
  // ~15 fps on a CPU). `decodePath` names which decoder is live — the crux of
  // the whole readout, since a silent fall to software is exactly the 4K60 killer.
  let decFps = $state(0);
  let inFps = $state(0);
  let decodePath = $state("");
  let transport = $state("");
  let decodeFails = $state(0);
  /// Pipeline anomaly readout — empty while healthy, e.g.
  /// "in 14/s · out 0/s · q 38 · sw" when packets arrive but frames
  /// don't, so a stall names its stage on the chip itself.
  let pipeDiag = $state("");
  let frameCount = 0;
  let inCount = 0;
  let decCount = 0;
  // Received bitrate (viewer truth): bytes seen this second → Mbps, shown in
  // the effective panel beside the sender's encode target when co-located.
  let recvMbps = $state(0);
  let inBytes = 0;
  let queuePeek = () => 0;
  let stallKick = () => {};
  let decodeModeNote = "";
  // Whether the backend decodes for us (raw RGBA in, no webview codec).
  // Starts true where WebCodecs doesn't exist at all, and flips true at
  // mount when it exists but can't actually decode H.264 (the
  // isConfigSupported probe — WebKitGTK ships the API shape with the
  // codecs delegated to GStreamer plugins that usually aren't there).
  // The decode ladder also lands here after WebCodecs stalls or dies
  // repeatedly. Sticky for the session — a webview whose decoder wedged
  // once isn't owed a third chance.
  let nativeDecode = $state(typeof VideoDecoder === "undefined");
  // The decode location and the selected native rung are separate. Automatic
  // native fallback may still use a GPU decoder; the visible software choice
  // must force OpenH264 on this viewer only.
  let nativeDecoderPreference = $state<NativeDecoderPreference>("automatic");

  // ---- the quality choices --------------------------------------------
  //
  // Resolution, frame rate, and bitrate ride a `Tune` ask to the machine
  // being viewed (its capture restarts with the picks); the codec choice
  // re-offers the route on the chosen transport and picks where H.264 is
  // decoded. Auto everywhere = exactly the automatic pipeline.
  type PillChoice = { label: string; value: number | null };
  const RES_CHOICES: PillChoice[] = [
    { label: "Auto", value: null },
    { label: "4K", value: 3840 },
    { label: "1440p", value: 2560 },
    { label: "1080p", value: 1920 },
    { label: "720p", value: 1280 },
  ];
  const FPS_CHOICES: PillChoice[] = [
    { label: "Auto", value: null },
    { label: "144", value: 144 },
    { label: "120", value: 120 },
    { label: "90", value: 90 },
    { label: "60", value: 60 },
    { label: "30", value: 30 },
    { label: "24", value: 24 },
    { label: "15", value: 15 },
  ];
  // Rate ceiling matches the posture pipeline (Studio spends 150 Mbps+ on a
  // fast LAN; the backend clamps into each posture's lane), not the old
  // 40 Mbps cap that silently gated Studio from the console. Mirrors the
  // popout bar's scale so the two hosts offer the same picks.
  const RATE_CHOICES: PillChoice[] = [
    { label: "Auto", value: null },
    { label: "200 Mbps", value: 200_000_000 },
    { label: "100 Mbps", value: 100_000_000 },
    { label: "60 Mbps", value: 60_000_000 },
    { label: "40 Mbps", value: 40_000_000 },
    { label: "25 Mbps", value: 25_000_000 },
    { label: "15 Mbps", value: 15_000_000 },
    { label: "8 Mbps", value: 8_000_000 },
    { label: "4 Mbps", value: 4_000_000 },
  ];
  type CodecChoice = "auto" | "h264" | "native" | "mjpeg";
  // These pick the *transport + decode path* for a watched stream, not a
  // specific encoder — the host picks its best encoder itself (NVENC on
  // NVIDIA, the GPU's Media Foundation encoder otherwise, software as the
  // floor), all of which produce H.264. So the labels describe what the
  // viewer chooses between: hardware-accelerated H.264 decode here,
  // software H.264 decode for webviews that can't, and the MJPEG
  // compatibility fallback.
  const CODEC_CHOICES: Array<{ label: string; value: CodecChoice; hint: string }> = [
    { label: "Auto", value: "auto", hint: "Best available — H.264, hardware decode where the viewer supports it" },
    { label: "H.264 · hardware decode", value: "h264", hint: "GPU-accelerated decode in the viewer (lowest CPU)" },
    { label: "H.264 · software decode", value: "native", hint: "Decoded on the CPU — for viewers without hardware H.264" },
    { label: "MJPEG · compatibility", value: "mjpeg", hint: "Per-frame JPEG — the universal fallback, higher bandwidth" },
  ];

  // ---- source aspect (mouse letterbox correction) --------------------
  //
  // A machine whose native resolution isn't 16:9 gets letterboxed into the
  // capture; the mouse must map over the desktop inside the bars. "Auto" reads
  // the bars off the picture (detectActiveRegion); an explicit aspect computes
  // the exact symmetric bars with no sampling — the confident manual path.
  const ASPECTS: Array<{ label: string; value: string; ratio: number | null }> = [
    { label: "Auto", value: "auto", ratio: null },
    { label: "16:9", value: "16:9", ratio: 16 / 9 },
    { label: "16:10", value: "16:10", ratio: 16 / 10 },
    { label: "3:2", value: "3:2", ratio: 3 / 2 },
    { label: "4:3", value: "4:3", ratio: 4 / 3 },
    { label: "21:9", value: "21:9", ratio: 21 / 9 },
  ];
  let aspectChoice = $state<string>(
    (typeof localStorage !== "undefined" && localStorage.getItem("ams.consoleAspect")) || "auto",
  );
  function pickAspect(v: string) {
    aspectChoice = v;
    openSub = null;
    try {
      localStorage.setItem("ams.consoleAspect", v);
    } catch {
      // storage disabled — the pick still applies for this session
    }
  }
  const aspectLabel = $derived(ASPECTS.find((a) => a.value === aspectChoice)?.label ?? "Auto");
  // The codec row reflects the *selected source's* transport (per-source in
  // the store) plus this window's decode choice — so switching sources shows
  // that source's codec, and "native" stays a window-local decode preference.
  const codecChoice = $derived<CodecChoice>(
    app.consoleCodec === "mjpeg"
      ? "mjpeg"
      : app.consoleCodec === "auto"
        ? "auto"
        : nativeDecode
          ? "native"
          : "h264",
  );

  // The Mode control (shared with the popout via ModePill) is the video
  // headline now — the Speed↔Quality slider it replaced could not reach
  // Game/Studio/Lossless at all. Res/FPS/Rate stay as the Advanced rows.
  // The shared control resolves the wire posture; fold it into the
  // console's per-source tune (the same shape the popout applies).
  function applyModeWire(
    wireMode: "game" | "studio" | "studio-lossless" | undefined,
    gameFlag: boolean | undefined,
  ) {
    app.setConsoleTune({ mode: wireMode, game: gameFlag });
  }

  const pillLabel = (choices: PillChoice[], v: number | null | undefined) =>
    choices.find((c) => c.value === (v ?? null))?.label ?? "Auto";

  function pickRes(v: number | null) {
    app.setConsoleTune({ maxEdge: v ?? undefined });
    openSub = null;
  }
  function pickFps(v: number | null) {
    app.setConsoleTune({ fps: v ?? undefined });
    openSub = null;
  }
  function pickRate(v: number | null) {
    app.setConsoleTune({ bitrate: v ?? undefined });
    openSub = null;
  }
  function pickCodec(v: CodecChoice) {
    openSub = null;
    // Where to decode is this window's choice; which transport to offer
    // is the store's (it re-offers the route when that part changes).
    nativeDecoderPreference = v === "native" ? "software" : "automatic";
    nativeDecode = v === "native" || (v === "auto" && typeof VideoDecoder === "undefined");
    app.setConsoleCodec(v === "mjpeg" ? "mjpeg" : v === "auto" ? "auto" : "h264");
  }

  // ---- the control bar ------------------------------------------------
  //
  // One floating toolbar carries the whole session (the KVM-console
  // pattern), in two orientations for two worlds:
  //
  // - DESKTOP: a horizontal bar across the top, with every monitor and
  //   camera as its own icon button (hover for the name; the active one
  //   filled, a popped-out one hollow) plus a pop-out button for the
  //   current screen — the mouse-and-hover world the old tab bar served.
  // - PHONE: a vertical rail on the right edge — thumbable, clear of the
  //   top edge's system gestures — with the inputs folded into a Screens
  //   menu instead of a button row.
  //
  // Both slide along their edge by the grip, and hide/return ONLY by the
  // handle tab on their outer side (no auto-hide in any mode: chrome that
  // disappears on its own is chrome you chase). One menu open at a time;
  // a press anywhere else closes it.
  let consoleEl = $state<HTMLElement | null>(null);
  let barWrapEl = $state<HTMLElement | null>(null);
  let menuEl = $state<HTMLElement | null>(null);
  type MenuKind = "session" | "screens" | "drives" | "kvm-media" | "power" | "video";
  let openMenu = $state<MenuKind | null>(null);
  let openSub = $state<"res" | "fps" | "rate" | "codec" | "aspect" | null>(null);
  // The advanced rows' disclosure remembers the old slider/pills toggle:
  // whoever preferred the pills gets the rows open by default.
  let advOpen = $state(app.consoleControlMode === "pills");
  function toggleAdv() {
    advOpen = !advOpen;
    if (advOpen !== (app.consoleControlMode === "pills")) app.toggleConsoleControlMode();
  }
  // Whether this device can even produce a touch — the soft-keyboard
  // button only earns bar space where a finger might need it.
  const touchDevice = typeof navigator !== "undefined" && navigator.maxTouchPoints > 0;
  const mobileShell = isMobile();
  // The rail orientation: vertical on the phone shell, horizontal on top
  // everywhere else.
  const vertical = mobileShell;

  function toggleMenu(m: MenuKind) {
    openMenu = openMenu === m ? null : m;
    openSub = null;
  }

  // A press outside the bar (stage, backdrop, anywhere) closes the open
  // menu — pointerdown, not click, so a touch that becomes a drag still
  // dismisses it.
  function onWindowPointerDown(e: PointerEvent) {
    if (!openMenu) return;
    const t = e.target as Node | null;
    if (t && barWrapEl?.contains(t)) return;
    openMenu = null;
  }

  // Keep the open menu on-screen: it opens off the bar's inner side
  // (below a top bar, left of the right rail), centered on it — a bar
  // slid toward an edge would carry the menu past the pane, so measure
  // and shift back along the bar's axis.
  let menuShift = $state(0);
  $effect(() => {
    void openSub;
    void advOpen;
    // Sliding the bar with a menu open must re-clamp the menu too.
    void barPos;
    if (!openMenu || !menuEl) {
      menuShift = 0;
      return;
    }
    const el = menuEl;
    menuShift = 0;
    requestAnimationFrame(() => {
      const r = el.getBoundingClientRect();
      const pad = 8;
      if (vertical) {
        if (r.top < pad) menuShift = pad - r.top;
        else if (r.bottom > window.innerHeight - pad)
          menuShift = window.innerHeight - pad - r.bottom;
      } else {
        if (r.left < pad) menuShift = pad - r.left;
        else if (r.right > window.innerWidth - pad) menuShift = window.innerWidth - pad - r.right;
      }
    });
  });

  // ---- bar drag (the grip — along the bar's own edge) ----
  let barPos = $state(0); // offset from the bar's centered resting spot
  // A dragged bar must survive the pane shrinking (window resize, phone
  // rotation) — re-clamp it into the new bounds instead of stranding it
  // off-view.
  function barSpan(): number {
    const c = consoleEl?.getBoundingClientRect();
    const b = barWrapEl?.getBoundingClientRect();
    if (!c || !b) return 0;
    return vertical
      ? Math.max(0, (c.height - b.height) / 2 - 8)
      : Math.max(0, (c.width - b.width) / 2 - 8);
  }
  function clampBarPos() {
    if (barPos === 0) return;
    const span = barSpan();
    barPos = Math.min(span, Math.max(-span, barPos));
  }
  function onGripDown(e: PointerEvent) {
    if (e.pointerType === "mouse" && e.button !== 0) return;
    const grip = e.currentTarget as HTMLElement;
    const span = barSpan();
    const coord = (ev: PointerEvent) => (vertical ? ev.clientY : ev.clientX);
    const s0 = coord(e) - barPos;
    try {
      grip.setPointerCapture(e.pointerId);
    } catch {
      // synthetic pointer — capture is best-effort
    }
    const move = (ev: PointerEvent) => {
      barPos = Math.min(span, Math.max(-span, coord(ev) - s0));
    };
    const up = () => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", up);
      grip.removeEventListener("pointercancel", up);
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", up);
    grip.addEventListener("pointercancel", up);
  }

  // ---- bar hide/show (manual only) ----
  //
  // The handle tab on the bar's outer edge is the ONE way the bar leaves
  // or returns, in every mode — it slides off past its edge and the tab
  // stays put. No idle timer: chrome that disappears on its own is
  // chrome you have to chase.
  let barHidden = $state(false);
  function toggleBar() {
    barHidden = !barHidden;
    if (barHidden) openMenu = null;
  }

  // ---- soft keyboard ----
  //
  // The ⌨️ button summons the OS keyboard through ConsoleKeys. It needs a
  // live control route, so pressing it with control off arms control on
  // the way in; control dropping (refused route, remote revoked) takes
  // the strip down with it.
  let keysOpen = $state(false);
  // How many CSS px the OS keyboard (or any bottom overlay) is covering —
  // `window.innerHeight - visualViewport.height - offsetTop`, the same
  // measure ConsoleKeys uses to float its strip. Zero on desktop (the
  // visual viewport equals the layout viewport). Drives the stage's bottom
  // inset so the video recenters into the space ABOVE the keyboard instead
  // of hiding behind it. Tracked in onMount.
  let kbInset = $state(0);
  function toggleKeys() {
    if (!keysOpen && !app.consoleControl) toggleControl();
    keysOpen = !keysOpen;
    // Mount the strip inside this very tap: iOS only raises its keyboard
    // for a focus() that happens within a user gesture, and Svelte's
    // batched flush would land the mount (and the focus) after it.
    if (keysOpen) flushSync();
  }
  $effect(() => {
    if (!app.consoleControl) keysOpen = false;
  });
  // What the strip is holding down on the remote (its one-shot modifiers
  // go down on arm). The strip discharges them itself on unmount — but
  // unmount runs AFTER endSession/toggle-off has torn the control route
  // down, so those keyups would die on the floor and the remote would
  // keep the modifier. This registry lets the release hygiene lift them
  // while the route still carries (mirroring keys.releaseAll()).
  const stripHeld = new Map<string, { key: string; code?: string }>();
  function sendFromStrip(a: InputAction) {
    if (a.kind === "key") {
      const id = a.code || a.key;
      if (a.down) stripHeld.set(id, { key: a.key, code: a.code });
      else stripHeld.delete(id);
    }
    app.sendConsoleInput(a);
  }
  function releaseStrip() {
    for (const [, k] of [...stripHeld].reverse()) {
      app.sendConsoleInput({ kind: "key", key: k.key, code: k.code, down: false });
    }
    stripHeld.clear();
  }

  // ---- pinch zoom / pan (the view transform) ---------------------------
  //
  // The canvas wears a translate+scale transform, nothing else changes:
  // normPoint reads the canvas's *transformed* rect off
  // getBoundingClientRect, so pointer mapping keeps working at any zoom
  // for free. Scale is clamped 1–8 and pan so the picture always covers
  // the pane edge it exceeds — zooming out past fit snaps home.
  let view = $state<ViewTransform>({ scale: 1, x: 0, y: 0 });
  function clampView(t: ViewTransform): ViewTransform {
    const scale = Math.min(8, Math.max(1, t.scale));
    const c = canvasEl;
    const s = stageEl;
    // At 1× with no keyboard there is nothing to pan; otherwise fall through so
    // the keyboard's upward shift is allowed even un-zoomed.
    if (!c || !s || (scale === 1 && kbInset <= 0)) return { scale, x: 0, y: 0 };
    // offsetWidth/Height are the LAYOUT box — unaffected by the current
    // transform, which is exactly what the clamp must scale from.
    const mx = Math.max(0, (c.offsetWidth * scale - s.clientWidth) / 2);
    const my = Math.max(0, (c.offsetHeight * scale - s.clientHeight) / 2);
    // The soft keyboard covers the bottom `kbInset` px; showing background
    // there is invisible, so the picture may shift UP that much further to lift
    // a text field above the keys. Only the upward (negative-y) room grows.
    const yLo = -(my + Math.max(0, kbInset));
    return {
      scale,
      x: Math.min(mx, Math.max(-mx, t.x)),
      y: Math.min(my, Math.max(yLo, t.y)),
    };
  }
  function setView(t: ViewTransform) {
    view = clampView(t);
  }
  function stageCenter() {
    const r = stageEl?.getBoundingClientRect();
    return r ? { x: r.left + r.width / 2, y: r.top + r.height / 2 } : { x: 0, y: 0 };
  }
  // Zoom keeping the content point at (cx, cy) pinned under (cx, cy).
  // No snap-home here: a slow trackpad pinch arrives as many tiny
  // ctrl+wheel steps, and snapping every sub-1.02 result back to 1 would
  // eat them all — zoom could never leave 100%. clampView already zeroes
  // the pan at exactly 1, and the touch machine snaps on gesture end.
  function zoomAt(scale: number, cx: number, cy: number) {
    const c = stageCenter();
    const sc = Math.min(8, Math.max(1, scale));
    const p0x = (cx - c.x - view.x) / view.scale;
    const p0y = (cy - c.y - view.y) / view.scale;
    setView({ scale: sc, x: cx - c.x - p0x * sc, y: cy - c.y - p0y * sc });
  }
  const ZOOM_STEP = 0.125;
  function zoomStep(dir: 1 | -1) {
    const c = stageCenter();
    // Buttons land on a stable 12.5-point grid: 100, 112.5, 125… If a
    // freeform pinch/trackpad gesture left the view between ticks, the next
    // button press moves to the nearest tick in that direction.
    const ticks = (view.scale - 1) / ZOOM_STEP;
    const nextTick = dir > 0
      ? Math.floor(ticks + 1e-6) + 1
      : Math.ceil(ticks - 1e-6) - 1;
    zoomAt(1 + nextTick * ZOOM_STEP, c.x, c.y);
  }
  function zoomPercent(scale: number): string {
    const value = Math.round(scale * 1000) / 10;
    return Number.isInteger(value) ? `${value}%` : `${value.toFixed(1)}%`;
  }
  function resetView() {
    view = { scale: 1, x: 0, y: 0 };
  }

  // Decode errors ask the sender for a clean entry (rate-limited again
  // backend-side) instead of sitting out the periodic IDR interval.
  let lastRefreshAsk = 0;
  function askRefresh() {
    const route = app.consoleVideoLive;
    if (!route) return;
    const now = performance.now();
    // 300 ms floor (was 700): a re-key recovers visible corruption, so ask
    // fast. The backend throttles again (300 ms) so a storm can't form.
    if (now - lastRefreshAsk < 300) return;
    lastRefreshAsk = now;
    void refreshRoute(route);
  }

  onMount(() => {
    let unlistenClose: (() => void) | undefined;
    let unlistenFileDrag: (() => void) | undefined;
    let mounted = true;
    void onNativeFileDrag((event) => {
      if (event.type === "leave") {
        dropHover = false;
        dropCount = 0;
        return;
      }
      if (event.type === "enter") dropCount = event.paths.length;
      dropHover = pointIsOnStage(event.x, event.y);
      if (event.type === "drop") void dropFiles(event.paths, event.x, event.y);
    }).then((unlisten) => {
      if (!mounted) unlisten();
      else unlistenFileDrag = unlisten;
    });
    // "VideoDecoder exists" stopped meaning "H.264 decode works":
    // WebKitGTK 2.4x ships the WebCodecs shape with codec support
    // delegated to GStreamer plugins that usually aren't installed. Ask
    // the API itself; anything short of a clean "supported" starts the
    // session on native decode instead of feeding a decoder that can
    // only die. (A probe that *lies* is still caught by the born-dead
    // ladder below — this just skips the few seconds it costs.)
    if (!nativeDecode && typeof VideoDecoder !== "undefined") {
      try {
        void VideoDecoder.isConfigSupported({ codec: "avc1.42E01F" })
          .then((s) => {
            if (!s.supported) {
              nativeDecoderPreference = "automatic";
              nativeDecode = true;
            }
          })
          .catch(() => {
            nativeDecoderPreference = "automatic";
            nativeDecode = true;
          });
      } catch {
        nativeDecoderPreference = "automatic";
        nativeDecode = true;
      }
    }
    let fbTick = 0;
    let fbFailsSent = 0;
    const fpsTimer = setInterval(() => {
      fps = frameCount;
      const inRate = inCount;
      inFps = inRate;
      decFps = decCount;
      frameCount = 0;
      inCount = 0;
      decCount = 0;
      recvMbps = (inBytes * 8) / 1e6;
      inBytes = 0;
      // Healthy: most of what arrives gets painted. Anything else is an
      // anomaly worth wearing on the chip.
      pipeDiag =
        inRate > 2 && fps < inRate / 2
          ? `in ${inRate}/s · out ${fps}/s · q ${queuePeek()}${decodeModeNote ? ` · ${decodeModeNote}` : ""}`
          : "";
      // Chunks flowing in, paints collapsed, queue deep: the decoder
      // stopped consuming (the hardware-pool stall). Rebuild it — the
      // ladder steps to software decode on the way.
      if (inRate > 5 && fps < inRate / 4 && queuePeek() > 8) stallKick();
      // (Letterbox auto-detect no longer runs here — it samples the decoded
      // frame from the paint path via maybeDetect(), so it never reads the
      // live canvas and can't trigger Chromium's CPU-raster demotion.)
      // Every other tick, report our decode health back to the streamer so
      // it can adapt (receiver → sender). decode_fails is the delta since the
      // last report; recv_fps is what we actually painted.
      const fbRoute = app.consoleVideoLive;
      if (fbRoute && ++fbTick % 2 === 0) {
        void sendVideoFeedback(fbRoute, fps, decodeFails - fbFailsSent, queuePeek());
        fbFailsSent = decodeFails;
      }
    }, 1000);
    if (windowed) {
      // The OS chrome's ✕ must tear the session's routes down too — the
      // close is held until they're on the wire (see onThisWindowClose).
      void onThisWindowClose(() => void endSession()).then((u) => (unlistenClose = u));
    }
    // Track the keyboard's bite out of the viewport so the stage can
    // recenter the video above it. `resize` fires when the OS keyboard
    // shows/hides; `scroll` covers iOS shifting the visual viewport while
    // it's up. No-op on desktop (vv.height === innerHeight → 0).
    const vv = window.visualViewport;
    const onViewport = () => {
      kbInset = vv ? Math.max(0, window.innerHeight - vv.height - vv.offsetTop) : 0;
    };
    if (vv) {
      vv.addEventListener("resize", onViewport);
      vv.addEventListener("scroll", onViewport);
      onViewport();
    }
    return () => {
      mounted = false;
      unlistenClose?.();
      unlistenFileDrag?.();
      clearInterval(fpsTimer);
      if (vv) {
        vv.removeEventListener("resize", onViewport);
        vv.removeEventListener("scroll", onViewport);
      }
    };
  });

  // The exact codec string for the incoming stream, read off its SPS
  // (profile/constraints/level are the three bytes after the NAL header)
  // — a guessed string risks the decoder sizing reorder buffers for a
  // stream we're not sending.
  function spsCodecString(au: Uint8Array): string | null {
    for (let i = 0; i + 4 < au.length; i++) {
      if (au[i] !== 0 || au[i + 1] !== 0) continue;
      const off = au[i + 2] === 1 ? i + 3 : au[i + 2] === 0 && au[i + 3] === 1 ? i + 4 : 0;
      if (!off) continue;
      if ((au[off] & 0x1f) === 7 && off + 3 < au.length) {
        const hex = (n: number) => n.toString(16).padStart(2, "0").toUpperCase();
        return `avc1.${hex(au[off + 1])}${hex(au[off + 2])}${hex(au[off + 3])}`;
      }
      i = off;
    }
    return null;
  }

  // Follow the live video route: watch its packet channel while it's up,
  // and show the placeholder rather than a stale frame whenever it
  // changes (input switch, session end). The effect ALSO re-runs on a
  // decode-mode flip (same route, new watch) — `lastRoute` tells the two
  // apart below.
  let lastRoute: string | null = null;
  $effect(() => {
    const route = app.consoleVideoLive;
    // Reading this here makes the ladder's last rung re-run the effect:
    // flipping it tears the watch down and re-watches in native mode.
    const native = nativeDecode;
    const nativeDecoder = nativeDecoderPreference;
    // A route CHANGE (screen tab, camera tab, or the teardown between
    // them) with a picture on the glass = a switch: hold the old frame
    // dimmed instead of flashing the placeholder. A decode-mode re-wire
    // on the SAME route keeps today's behavior.
    // `hasFrame` is paint output, not watch identity. Tracking it here made
    // every first decoded frame invalidate this effect, tear down the watcher,
    // clear its queue, and subscribe again. The viewer therefore oscillated
    // between "window watching" and "no window is watching" while a healthy
    // 60 fps stream flowed underneath it. Only route/decode-mode changes own
    // this lifecycle; sample the paint state without subscribing to it.
    if (untrack(() => hasFrame) && route !== lastRoute) {
      untrack(armSwitching);
    }
    hasFrame = false;
    // Clear the previous stream's frame dimensions too: normPoint letterboxes
    // against frameW/frameH, so leaving them live would map pointer events onto
    // the OLD source's aspect (and a hidden, repositioned canvas) during a
    // re-wire — the "mouse doesn't map cleanly after a transition" report. The
    // hasFrame gate below then drops events until the first fresh frame repaints.
    frameW = 0;
    frameH = 0;
    // A new source may have a different (or no) letterbox — start from the full
    // frame, unlock, and let the one-shot detector re-measure and re-lock.
    activeRegion = { x0: 0, y0: 0, x1: 1, y1: 1 };
    detectLocked = false;
    detectPrev = null;
    hostStatus = null;
    fps = 0;
    transport = "";
    decodeFails = 0;
    pipeDiag = "";
    decFps = 0;
    inFps = 0;
    decodePath = "";
    frameCount = 0;
    inCount = 0;
    decCount = 0;
    recvMbps = 0;
    inBytes = 0;
    // A new stream starts at its natural fit, with the trackpad cursor
    // re-centered, and any touch gesture from the old one is over — but
    // ONLY on an actual route change. The decode ladder re-runs this
    // effect with the SAME route (nativeDecode flip); yanking the zoom
    // to 1x and lifting a held drag there would punish the user for the
    // decoder's stall. Untracked: lifting a held button reads store
    // state, and this effect's deps are the route and decode mode only.
    if (route !== lastRoute) {
      lastRoute = route;
      view = { scale: 1, x: 0, y: 0 };
      virt.x = 0.5;
      virt.y = 0.5;
      untrack(() => touchMouse.reset());
    }
    if (!route) return;
    let cancelled = false;
    let unwatch: (() => void) | undefined;
    let unwatchStatus: (() => void) | undefined;
    // The host's capture-state reports for this route — they explain the
    // placeholder (and any mid-stream stall) in the host's own words.
    void watchVideoStatus(route, (s) => {
      if (cancelled) return;
      hostStatus = s.state === "ok" ? null : s;
    }).then((u) => {
      if (cancelled) u();
      else unwatchStatus = u;
    });
    let decoder: VideoDecoder | null = null;
    let codecString: string | null = null;
    // The decode ladder: hardware-preference first; any stall (born dead
    // *or* mid-stream — the hardware-pool failure shape is bursts, then a
    // growing queue with no output and no error) rebuilds the decoder one
    // rung down on software decode, and a stall *there* hands the stream
    // to the backend's native decoder. The chip's diag line records the
    // step.
    // Start on GPU decode (VideoToolbox/D3D11/VA-API behind WebCodecs) — it's
    // the lowest-latency, lowest-CPU rung. The ladder steps down to software
    // then native on a stall, so a box without HW decode still recovers.
    let decodeMode: HardwareAcceleration = "prefer-hardware";
    let decodeCalls = 0;
    let decodeOutputs = 0;
    // Decoded frames don't paint inside the output callback: the freshest
    // one is parked here (superseded frames close immediately — freshness
    // over completeness) and a rAF paints it. The decoder's frame pool can
    // never be starved by the paint path, throttled window or not.
    let pendingFrame: VideoFrame | null = null;
    let paintScheduled = false;
    queuePeek = () => decoder?.decodeQueueSize ?? 0;
    decodeModeNote = native ? `native ${nativeDecoder === "software" ? "sw" : "auto"}` : "";
    // "webview (hw)" only asserts we *asked* for hardware (prefer-hardware);
    // whether WKWebView honours it shows in whether decFps reaches 60.
    decodePath = native
      ? nativeDecoder === "software"
        ? "native (sw)"
        : "native (auto)"
      : "webview (hw)";

    const rebuildDecoder = () => {
      if (decodeMode !== "prefer-software") {
        decodeMode = "prefer-software";
        decodeModeNote = "sw";
        decodePath = "webview (sw)";
      } else {
        // Software decode stalled too — hand the stream to the backend's
        // openh264 decoder. Setting the flag re-runs this effect, which
        // re-watches the route in native mode (and tears this rung down).
        console.warn(`video decoder (${codecString}) stalled twice — switching to native decode`);
        nativeDecoderPreference = "software";
        nativeDecode = true;
        decodePath = "native (sw)";
        askRefresh();
        return;
      }
      console.warn(
        `video decoder (${codecString}) stalled — rebuilding with ${decodeMode}`,
      );
      try {
        if (decoder && decoder.state !== "closed") decoder.close();
      } catch {
        // already closed
      }
      decoder = null; // re-created on the next key unit (≤2s away)
    };
    stallKick = () => {
      if (!cancelled && decoder) rebuildDecoder();
    };

    const paintPending = () => {
      paintScheduled = false;
      const frame = pendingFrame;
      pendingFrame = null;
      if (!frame) return;
      try {
        if (!cancelled) {
          paint(frame.displayWidth, frame.displayHeight, (ctx) =>
            ctx.drawImage(frame, 0, 0),
          );
          // Letterbox detection samples this frame directly (never the live
          // canvas). VideoFrame is a valid drawImage source.
          maybeDetect(frame);
        }
      } finally {
        frame.close();
      }
    };

    // JPEG bitmap decodes are async — chain them so frames paint in
    // arrival order.
    // MJPEG paint slot, latest-wins: exactly one undrawn frame is ever
    // held — a newer one supersedes it (each JPEG is a complete picture,
    // so painting history is pure lag). The old serial promise chain
    // decoded and painted EVERY frame in arrival order, which is where
    // "the video is always catching up" lived when decode or the wire
    // ran behind capture. Superseded frames land in recv-fps: painted
    // frames drop below the sender's target, and the existing feedback
    // tuner downshifts the encode edge on that same honest signal.
    let pendingJpeg: Blob | null = null;
    let jpegPainting = false;
    const paintNewestJpeg = async () => {
      if (jpegPainting) return; // the running loop will pick ours up
      jpegPainting = true;
      try {
        while (!cancelled && pendingJpeg) {
          const blob = pendingJpeg;
          pendingJpeg = null;
          try {
            const bmp = await createImageBitmap(blob);
            decCount += 1;
            paint(bmp.width, bmp.height, (ctx) => ctx.drawImage(bmp, 0, 0));
            maybeDetect(bmp); // sample this bitmap, not the live canvas
            bmp.close();
          } catch {
            // A torn frame decodes as nothing; the next one stands alone.
          }
        }
      } finally {
        jpegPainting = false;
      }
    };

    const paint = (w: number, h: number, draw: (ctx: CanvasRenderingContext2D) => void) => {
      const c = canvasEl;
      if (cancelled || !c) return;
      if (c.width !== w) c.width = w;
      if (c.height !== h) c.height = h;
      const ctx = c.getContext("2d");
      if (!ctx) return;
      draw(ctx);
      frameW = w;
      frameH = h;
      hasFrame = true;
      // The fresh picture ends any switch crossfade.
      if (switching) {
        switching = false;
        clearTimeout(switchingTimer);
      }
      frameCount += 1;
    };

    // Decoder instances lost on this rung before they ever produced a
    // frame. The queue-stall detectors above can't see this failure
    // shape: a decoder whose configure() throws — or whose very first
    // decode errors — dies and resets the counters each time, so the
    // ladder never stepped and the stage sat black forever. WebKitGTK
    // 2.4x is the live case: it exposes the VideoDecoder *shape* with no
    // working H.264 behind it (GStreamer-dependent), so the old "does
    // VideoDecoder exist" test chose a decoder that can only die.
    let bornDeadDrops = 0;

    const dropDecoder = (why: unknown) => {
      // Surfaced, not swallowed: the chip counts these and the console
      // log names them — a decoder that quietly dies reads as a freeze.
      decodeFails += 1;
      askRefresh();
      console.warn("video decode error:", why);
      try {
        if (decoder && decoder.state !== "closed") decoder.close();
      } catch {
        // already closed
      }
      decoder = null;
      // Nothing ever decoded on this rung and the decoder keeps dying:
      // that's a rung failure, not a glitch — walk the same ladder the
      // stall detectors use, which ends at the backend's native decoder.
      if (decodeOutputs === 0) {
        bornDeadDrops += 1;
        if (bornDeadDrops >= 3) {
          bornDeadDrops = 0;
          rebuildDecoder();
        }
      }
    };

    void watchVideo(
      route,
      (f) => {
      if (cancelled) return;
      inBytes += f.data.byteLength;
      transport = f.kind === "jpeg" ? "MJPEG" : "H.264";
      if (f.kind === "jpeg") {
        decodePath = "mjpeg";
        // Supersede, never queue: the newest picture is the only one worth
        // painting, and the loop below drains at whatever rate decode allows.
        pendingJpeg = new Blob([f.data], { type: "image/jpeg" });
        void paintNewestJpeg();
        return;
      }
      if (f.kind === "raw") {
        // The backend already decoded — RGBA in, one blit out. Arrival IS a
        // decoded frame here, so it counts on both the in and dec stages; the
        // native openh264 decode rate is exactly what shows up on the wire.
        inCount += 1;
        decCount += 1;
        if (f.data.byteLength !== f.width * f.height * 4) return;
        const pixels = new Uint8ClampedArray(f.data.buffer, f.data.byteOffset, f.data.byteLength);
        const img = new ImageData(pixels, f.width, f.height);
        paint(f.width, f.height, (ctx) => ctx.putImageData(img, 0, 0));
        return;
      }
      // H.264 — decode entry is a key unit; deltas before one wait. Every
      // access unit is fed straight to the decoder; the freshest decoded
      // frame wins at paint time (output callback below supersedes any
      // earlier pending frame, one rAF blits the newest). No per-frame
      // compressed-queue valve here: a hardware decoder keeps up with 4K60
      // natively, and the one that tried to bound the queue by force-dropping
      // to keyframes thrashed a normally-pipelined decoder down to ~15 fps
      // (and could trip the software-decode fallback). If a decoder genuinely
      // can't keep up, the stall ladder below steps it down.
      //
      // What DOES exist is the live-edge snap: when the input queue has
      // ballooned well past any normal pipeline depth (a decoder running
      // just slightly behind arrival accumulates unboundedly — the picture
      // "keeps catching up" seconds behind the cursor) and a fresh KEY unit
      // is in hand, drop the queued backlog and re-enter at that key. It
      // costs the stale frames only — never quality, never arriving frames —
      // and normal pipelining (a handful queued) can't trip it.
      inCount += 1;
      if (
        decoder &&
        decoder.state !== "closed" &&
        f.key &&
        decoder.decodeQueueSize > 12
      ) {
        try {
          decoder.close();
        } catch {
          // already closed
        }
        decoder = null; // re-created right below, at this key unit
      }
      if (!decoder || decoder.state === "closed") {
        if (!f.key) return;
        codecString = spsCodecString(f.data) ?? codecString ?? "avc1.42E01F";
        decoder = new VideoDecoder({
          output: (frame) => {
            decodeOutputs += 1;
            decCount += 1;
            if (pendingFrame) pendingFrame.close();
            pendingFrame = frame;
            if (!paintScheduled) {
              paintScheduled = true;
              requestAnimationFrame(paintPending);
            }
          },
          // Recovery is the sender's periodic IDR: drop the decoder,
          // re-init on the next key unit.
          error: dropDecoder,
        });
        try {
          decoder.configure({
            codec: codecString,
            optimizeForLatency: true,
            hardwareAcceleration: decodeMode,
          });
          decodeCalls = 0;
          decodeOutputs = 0;
        } catch (e) {
          dropDecoder(e);
          return;
        }
      }
      try {
        decoder.decode(
          new EncodedVideoChunk({
            type: f.key ? "key" : "delta",
            timestamp: f.seq,
            data: f.data,
          }),
        );
        decodeCalls += 1;
      } catch (e) {
        dropDecoder(e);
        return;
      }
      // Born-dead decoders (key accepted, nothing ever out) get the same
      // rebuild as mid-stream stalls — without waiting for the 1s sweep.
      if (decodeOutputs === 0 && decodeCalls >= 20) rebuildDecoder();
      },
      { decode: native, decoder: nativeDecoder },
    ).then((u) => {
      // The route may have changed while the subscribe was in flight.
      if (cancelled) u();
      else unwatch = u;
    });
    return () => {
      cancelled = true;
      unwatch?.();
      unwatchStatus?.();
      stallKick = () => {};
      if (pendingFrame) {
        pendingFrame.close();
        pendingFrame = null;
      }
      try {
        if (decoder && decoder.state !== "closed") decoder.close();
      } catch {
        // already closed
      }
      decoder = null;
    };
  });

  /** The host's capture condition as a human sentence — what the stage
   *  shows instead of (or over) silent black. */
  function hostStatusText(s: VideoHostStatus): string {
    switch (s.state) {
      case "waiting_consent":
        return "Waiting for someone at the remote machine to approve screen sharing (a one-time consent dialog is open there).";
      case "display_asleep":
        return "The remote display is asleep or blank — forcing it awake (clicking here helps too)…";
      case "no_monitor":
        return "No monitor to capture on the remote machine — its displays are detached or in deep sleep.";
      case "grab_failed":
        return `Screen capture is failing on the remote machine${s.detail ? `: ${s.detail}` : "."}`;
      case "no_camera":
        return "No camera to capture on the remote machine — it may have been unplugged since the scan.";
      case "camera_failed":
        return `The remote camera won't stream — another app may be holding it, or its camera permission is off${s.detail ? ` (${s.detail})` : ""}.`;
      default:
        return "";
    }
  }

  let closing = false;
  async function endSession() {
    if (closing) return;
    closing = true;
    // Keys still held ride out *before* the control route goes — closing
    // a console mid-chord (⌘W closes this very window) must not leave
    // the remote holding the modifier. The strip's armed modifiers too:
    // its own unmount discharge would fire after the route is gone.
    keys.releaseAll();
    releaseStrip();
    touchMouse.reset();
    // UI resets synchronously; the await is only so a console window's
    // teardown reaches the backend before the webview dies. Bounded — a
    // wedged daemon must never hold a closing window hostage.
    const teardown = app.closeConsole();
    if (windowed) {
      await Promise.race([teardown, new Promise((r) => setTimeout(r, 600))]);
      void closeThisWindow();
    }
    closing = false;
  }

  // ---- keyboard & mouse forwarding -----------------------------------
  //
  // Coordinates are normalized 0..1 over the *streamed frame's* content
  // box (the canvas is letterboxed with object-fit: contain), which the
  // remote denormalizes onto its own screen — neither side needs the
  // other's resolution. Only clientX/clientY are read, so the same math
  // serves pointer events, wheel events, and the touch machine's
  // synthesized points.
  function normPoint(e: { clientX: number; clientY: number }): { x: number; y: number } | null {
    const img = canvasEl;
    // Gate on hasFrame, not just truthy frameW/frameH: during a re-wire the
    // canvas is `.waiting` (visibility:hidden; position:absolute), so its rect
    // has moved out of the centered grid cell — normalizing against it (or the
    // stale prior dims) lands the remote cursor in the wrong place. Wait for the
    // first fresh frame, then the live-rect letterbox math below maps cleanly.
    if (!img || !hasFrame || !frameW || !frameH) return null;
    // The rect reflects the pinch-zoom transform too — zoomed in, the same
    // math maps over the enlarged (partly off-pane) picture unchanged.
    const r = img.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return null;
    const scale = Math.min(r.width / frameW, r.height / frameH);
    const cw = frameW * scale;
    const ch = frameH * scale;
    const ox = r.left + (r.width - cw) / 2;
    const oy = r.top + (r.height - ch) / 2;
    // Fraction over the streamed FRAME.
    const fx = (e.clientX - ox) / cw;
    const fy = (e.clientY - oy) / ch;
    if (fx < 0 || fx > 1 || fy < 0 || fy > 1) return null;
    // Remap over the ACTIVE region — inside any baked-in letterbox bars. A
    // source whose native aspect ≠ the 16:9 capture (e.g. a 16:10 laptop output
    // as 1080p) bakes black bars into the frame, and the remote's absolute
    // pointer maps over its DESKTOP, not the whole frame — so mapping over the
    // full frame lands the cursor off by the bar width. activeRegion is the
    // detected desktop box (0..1), or the full frame when there are no clear,
    // symmetric bars. Bar-hits clamp to the desktop edge.
    const ar = activeRegion;
    const x = Math.min(1, Math.max(0, (fx - ar.x0) / (ar.x1 - ar.x0)));
    const y = Math.min(1, Math.max(0, (fy - ar.y0) / (ar.y1 - ar.y0)));
    return { x, y };
  }

  // ---- letterbox / active-area detection -----------------------------
  //
  // The active region of the streamed frame in 0..1 fractions (the desktop
  // inside any baked-in black bars); the full frame until detection runs.
  let activeRegion = $state({ x0: 0, y0: 0, x1: 1, y1: 1 });
  // The bars are baked pixels with no sidechannel (HDMI reports only the signal
  // size, the SPS only the coded size), so they must be measured off the frame —
  // but they're STATIC for a source mode, so this is a one-shot: it measures on
  // the health tick only until two content-bearing frames agree, then LOCKS and
  // stops sampling. Reset (unlock) on a stream re-wire.
  let detectLocked = false;
  let detectPrev: { x0: number; x1: number; y0: number; y1: number } | null = null;
  // The detector's CPU-side scratch surface (see detectActiveRegion for why
  // the live canvas must never be read directly). Small on purpose: one
  // ~500 KB readback per pass instead of twelve full-width strips of 4K.
  const DETECT_W = 480;
  const DETECT_H = 270;
  let detectScratch: HTMLCanvasElement | null = null;
  // Throttle for the frame-sourced detector below (~1 Hz, the old health-tick
  // cadence) since it now runs from the paint path, which fires per frame.
  let lastDetectAt = 0;

  // A decoded frame just painted — maybe measure its letterbox. Runs from the
  // paint path (not a canvas read) so the detector NEVER touches the live
  // presentation canvas: Chromium permanently demotes an accelerated 2D
  // canvas to CPU raster after just two getImageData readbacks when it wasn't
  // created willReadFrequently (and ours can't be — paint() fixed its context
  // to plain-GPU on the first frame). A demoted 4K canvas turns every
  // subsequent drawImage of a hardware-decoded frame into a ~33 MB GPU→CPU
  // copy — the exact "video is choppy and ~100 ms behind while the mouse is
  // instant" regression. Sampling the frame we were handed sidesteps the
  // canvas entirely. `src` is the VideoFrame / ImageBitmap that was painted.
  function maybeDetect(src: CanvasImageSource) {
    if (detectLocked || aspectChoice !== "auto" || !stagePointerActive) return;
    const now = performance.now();
    if (now - lastDetectAt < 1000) return;
    lastDetectAt = now;
    detectActiveRegion(src);
  }

  // Measure the frame's active region: mirror the decoded FRAME into the small
  // CPU-side scratch surface (created once, willReadFrequently from birth) and
  // scan that one downscaled readback for symmetric black letterbox/pillarbox
  // bars. The live presentation canvas is never read or drawn-from, so it stays
  // GPU-accelerated for the whole session. Conservative: only crops when clear
  // bars sit on BOTH opposite edges and are near-symmetric (a real letterbox),
  // so ordinary dark content is never mistaken for a bar; otherwise it maps
  // over the whole frame.
  function detectActiveRegion(src: CanvasImageSource) {
    if (detectLocked) return;
    if (!detectScratch) {
      detectScratch = document.createElement("canvas");
      detectScratch.width = DETECT_W;
      detectScratch.height = DETECT_H;
    }
    const ctx = detectScratch.getContext("2d", { willReadFrequently: true });
    if (!ctx) return;
    const w = DETECT_W;
    const h = DETECT_H;
    const DARK = 24;
    const median = (a: number[]) => a.slice().sort((p, q) => p - q)[a.length >> 1];

    let x0 = 0;
    let x1 = w;
    let y0 = 0;
    let y1 = h;
    let found = false; // did the frame carry real (non-black) content this pass?
    try {
      ctx.drawImage(src, 0, 0, w, h);
      const data = ctx.getImageData(0, 0, w, h).data;
      const bright = (x: number, y: number) => {
        const px = (y * w + x) * 4;
        return data[px] > DARK || data[px + 1] > DARK || data[px + 2] > DARK;
      };
      const L: number[] = [];
      const R: number[] = [];
      for (let k = 1; k <= 6; k++) {
        const y = Math.floor((h * k) / 7);
        let l = 0;
        while (l < w && !bright(l, y)) l++;
        let rr = w - 1;
        while (rr > l && !bright(rr, y)) rr--;
        if (l < rr) {
          L.push(l);
          R.push(rr);
        }
      }
      const T: number[] = [];
      const B: number[] = [];
      for (let k = 1; k <= 6; k++) {
        const x = Math.floor((w * k) / 7);
        let t = 0;
        while (t < h && !bright(x, t)) t++;
        let b = h - 1;
        while (b > t && !bright(x, b)) b--;
        if (t < b) {
          T.push(t);
          B.push(b);
        }
      }
      found = L.length >= 4;
      if (L.length >= 4) {
        x0 = median(L);
        x1 = median(R) + 1;
      }
      if (T.length >= 4) {
        y0 = median(T);
        y1 = median(B) + 1;
      }
    } catch {
      return; // canvas not readable this tick — keep the last region
    }

    // A near-all-black frame (screensaver, dark boot screen) tells us nothing —
    // don't measure or lock on it, just try again on the next tick.
    if (!found) {
      detectPrev = null;
      return;
    }

    const barL = x0;
    const barR = w - x1;
    const barT = y0;
    const barB = h - y1;
    const okX = Math.min(barL, barR) > w * 0.015 && Math.abs(barL - barR) < w * 0.02;
    const okY = Math.min(barT, barB) > h * 0.015 && Math.abs(barT - barB) < h * 0.02;
    const next = {
      x0: okX ? x0 / w : 0,
      x1: okX ? x1 / w : 1,
      y0: okY ? y0 / h : 0,
      y1: okY ? y1 / h : 1,
    };
    activeRegion = next;
    // Lock once two consecutive content-bearing frames agree — then stop
    // sampling entirely until the next re-wire resets it.
    const near = (a: number, b: number) => Math.abs(a - b) < 0.005;
    if (
      detectPrev &&
      near(detectPrev.x0, next.x0) &&
      near(detectPrev.x1, next.x1) &&
      near(detectPrev.y0, next.y0) &&
      near(detectPrev.y1, next.y1)
    ) {
      detectLocked = true;
    }
    detectPrev = next;
  }

  // The exact symmetric active region for a known source aspect within the
  // current frame: a source narrower than the frame pillarboxes (L/R bars),
  // a wider one letterboxes (T/B bars).
  function activeRegionForAspect(ratio: number, fw: number, fh: number) {
    const frameRatio = fw / fh;
    if (ratio < frameRatio - 1e-4) {
      const x0 = (1 - ratio / frameRatio) / 2;
      return { x0, x1: 1 - x0, y0: 0, y1: 1 };
    }
    if (ratio > frameRatio + 1e-4) {
      const y0 = (1 - frameRatio / ratio) / 2;
      return { x0: 0, x1: 1, y0, y1: 1 - y0 };
    }
    return { x0: 0, y0: 0, x1: 1, y1: 1 };
  }

  // Apply the Aspect pick: "Auto" hands the active region back to the one-shot
  // pixel detector; an explicit aspect computes the exact bars and disables
  // detection. Re-runs when the pick or the frame geometry changes.
  $effect(() => {
    const choice = aspectChoice;
    const fw = frameW;
    const fh = frameH;
    const ratio = ASPECTS.find((a) => a.value === choice)?.ratio ?? null;
    if (ratio == null) {
      // Auto — unlock and let detectActiveRegion re-measure.
      detectLocked = false;
      detectPrev = null;
      activeRegion = { x0: 0, y0: 0, x1: 1, y1: 1 };
      return;
    }
    detectLocked = true; // stop the detector; the aspect is authoritative
    activeRegion = fw && fh ? activeRegionForAspect(ratio, fw, fh) : { x0: 0, y0: 0, x1: 1, y1: 1 };
  });

  // The KVM rule: with control live, the window your keyboard should reach is
  // the one you're working in — claimed without a click in between, because a
  // click would go to the *remote*. Raise the OS window AND pin keyboard focus
  // on the stage element: setFocus() alone doesn't reliably push document
  // focus into the webview without a click, so without the element focus the
  // key handlers (now on the stage) never fire in a dedicated console window.
  //
  // Callers decide *when* this is allowed to fire, and a bare hover is not one
  // of the times (see the pointer-move handler): with several consoles open,
  // crossing one en route to somewhere else would yank focus off whatever you
  // were doing. Gated here on the document not already holding focus so it
  // never steals from an open bar menu once the window is active — and parked
  // entirely while the soft keyboard types (its hidden input owns focus;
  // stealing it drops the OS keyboard mid-word).
  function claimFocus() {
    if (keysOpen) return;
    if (document.hasFocus()) return;
    void focusThisWindow();
    stageEl?.focus({ preventScroll: true });
  }

  // ---- the touch machine ----------------------------------------------
  //
  // Touch pointers detour through console-touch.ts (trackpad semantics +
  // the two-finger view gestures). The trackpad needs a cursor that
  // persists between touches — `virt`, in the same active-region
  // normalized space the wire speaks. Finger deltas steer it (converted
  // through the RENDERED frame size, so the cursor moves at finger speed
  // on the glass at any zoom), taps and holds click at it, and the mouse
  // path re-seats it on every absolute move so the two input families
  // never disagree about where the cursor is. `heldButtons` stays the
  // single registry of what the remote believes is pressed.
  const virt = { x: 0.5, y: 0.5 };
  function sendVirt() {
    app.sendConsoleInput({ kind: "mouse_move", x: virt.x, y: virt.y, screen: controlScreen });
  }
  // The TeamViewer camera: zoomed in, the view keeps the remote cursor
  // CENTERED. Every steer pans the picture so the cursor slides back to the
  // middle of the stage — from the first pixel of a drag, so the picture
  // tracks the cursor directly instead of waiting for it to reach a
  // screen-edge margin. `setView`'s clamp stops the pan at the content
  // edges; there the picture can't scroll any further, so the cursor rides
  // off-center into that edge/corner on its own — "centered as much as it
  // can, until you reach the edges". The whole desktop stays reachable with
  // one thumb.
  function followCursor() {
    // Runs when zoomed, and also at 1× while the keyboard is up (there the
    // clamp permits a vertical shift to lift the cursor above the keys).
    if (view.scale <= 1.001 && kbInset <= 0) return;
    const c = canvasEl;
    const s = stageEl;
    if (!c || !s || !hasFrame) return;
    const r = c.getBoundingClientRect();
    const box = s.getBoundingClientRect();
    const ar = activeRegion;
    // Where the cursor sits on screen right now (through the active region
    // and the current zoom), and where the stage centre is. Mapped over
    // the CONTENT box — in theater the element carries letterbox bars.
    const cb = contentBox(r);
    const px = cb.left + (ar.x0 + virt.x * (ar.x1 - ar.x0)) * cb.width;
    const py = cb.top + (ar.y0 + virt.y * (ar.y1 - ar.y0)) * cb.height;
    // Centre of the VISIBLE area: when the soft keyboard has eaten the bottom
    // `kbInset` px, the middle of what the user can still see sits that much
    // higher — so a cursor centred here stays above the keyboard where they're
    // typing, without reframing or dropping the zoom.
    const cx = box.left + box.width / 2;
    const cy = box.top + (box.height - kbInset) / 2;
    setView({ scale: view.scale, x: view.x + (cx - px), y: view.y + (cy - py) });
  }
  // The picture's content box within the canvas ELEMENT — object-fit:
  // contain's inset, the same min-scale math normPoint runs. Outside
  // theater the element IS the content box (auto-sized), so the inset is
  // zero and this degrades to the rect itself; in theater the element
  // fills the stage and the picture letterboxes inside it, and every
  // virt→client mapping must go through here or it drifts by a bar width.
  function contentBox(r: DOMRect): { left: number; top: number; width: number; height: number } {
    if (!frameW || !frameH || r.width === 0 || r.height === 0) {
      return { left: r.left, top: r.top, width: r.width, height: r.height };
    }
    const scale = Math.min(r.width / frameW, r.height / frameH);
    const w = frameW * scale;
    const h = frameH * scale;
    return {
      left: r.left + (r.width - w) / 2,
      top: r.top + (r.height - h) / 2,
      width: w,
      height: h,
    };
  }

  // Pin the aiming crosshair to the commanded position. Same client-space math
  // as followCursor (getBoundingClientRect reflects the zoom transform), made
  // relative to the stage so a windowed console's own transform can't offset
  // it. `virt` isn't reactive, so callers invoke this wherever the cursor
  // moves; the $effect below re-runs it on reactive view/region/frame changes.
  function updateCrosshair() {
    const el = crosshairEl;
    const c = canvasEl;
    const s = stageEl;
    if (!el || !c || !s || !hasFrame) return;
    const cb = contentBox(c.getBoundingClientRect());
    const sb = s.getBoundingClientRect();
    const ar = activeRegion;
    const x = cb.left - sb.left + (ar.x0 + virt.x * (ar.x1 - ar.x0)) * cb.width;
    const y = cb.top - sb.top + (ar.y0 + virt.y * (ar.y1 - ar.y0)) * cb.height;
    el.style.transform = `translate(${x}px, ${y}px)`;
  }
  $effect(() => {
    void view;
    void activeRegion;
    void hasFrame;
    void stagePointerActive;
    void frameW;
    void frameH;
    void kbInset;
    updateCrosshair();
  });
  // The soft keyboard opening (or closing) recentres the cursor into the space
  // that's left, keeping the current zoom — so the field being typed into is
  // visible above the keys instead of behind them.
  $effect(() => {
    void kbInset;
    untrack(() => {
      followCursor(); // lift the cursor into the visible area (no-op at 1×/no-kb)
      setView(view); // re-clamp — closing the keyboard removes the extra pan room
    });
  });
  const touchMouse = makeTouchMouse({
    active: () => stagePointerActive,
    moveBy: (dx, dy) => {
      const c = canvasEl;
      if (!c || !hasFrame) return;
      // The transformed rect IS the rendered frame (element == content
      // box), so dividing finger px by it — and by the active region's
      // share — locks cursor speed to finger speed at any zoom.
      const r = c.getBoundingClientRect();
      if (!r.width || !r.height) return;
      const ar = activeRegion;
      virt.x = Math.min(1, Math.max(0, virt.x + dx / (r.width * (ar.x1 - ar.x0))));
      virt.y = Math.min(1, Math.max(0, virt.y + dy / (r.height * (ar.y1 - ar.y0))));
      // Deltas accumulate on every event; only the (absolute) send is
      // throttled, so nothing is ever lost to the rate cap.
      const now = performance.now();
      if (now - lastMoveAt >= 16) {
        lastMoveAt = now;
        sendVirt();
      }
      followCursor();
      updateCrosshair();
    },
    button: (button, down) => {
      if (down) {
        // Land the cursor exactly where the trackpad believes it is,
        // then press — the same order the mouse path uses.
        sendVirt();
        app.sendConsoleInput({ kind: "mouse_button", button, down: true });
        heldButtons.add(button);
      } else {
        if (!heldButtons.delete(button)) return;
        app.sendConsoleInput({ kind: "mouse_button", button, down: false });
      }
    },
    wheel: (dx, dy) => app.sendConsoleInput({ kind: "wheel", dx, dy }),
    view: () => view,
    setView,
    viewCenter: stageCenter,
    onGesture: () => {
      openMenu = null;
    },
  });
  // Control dropping mid-gesture: whatever the fingers were holding lifts
  // while the route can still carry it.
  $effect(() => {
    if (!app.consoleControl) untrack(() => touchMouse.reset());
  });

  // ---- pointer lock (theater mouse capture) --------------------------
  // The popout's game-aiming path, brought to the console: in THEATER
  // with control live, a click captures the mouse and RELATIVE deltas
  // stream to the host (`mouse_move_rel`) — FPS camera control instead
  // of a cursor pinned to the stage edge, which is exactly the
  // "fullscreen game doesn't capture the cursor" field report. Esc (the
  // browser's own gesture) releases; leaving theater or control drops it.
  let pointerLocked = $state(false);
  let nativePointerLocked = $state(false);
  // Relative mouse, asked for deliberately rather than inferred from theater.
  //
  // A remote fullscreen app or game captures the mouse on ITS machine: it
  // warps the cursor to centre each frame and reads the delta. Absolute
  // coordinates fight that — every move we send yanks the cursor back to
  // where our pointer is, so aiming is unusable — and it doesn't matter one
  // bit whether the *viewer* happens to be fullscreen. Tying capture to
  // theater meant the only way to drive such an app was to also give up your
  // own screen, so this is its own switch.
  //
  // Turning it on grabs the pointer on the next click (pointer lock needs a
  // user gesture); Esc — the browser's own gesture — releases the lock, and
  // the effect below turns the mode off with it, so one press hands your
  // cursor back rather than leaving a mode armed to re-grab.
  let relativeMouse = $state(false);
  function lockChanged() {
    pointerLocked = document.pointerLockElement === stageEl;
    lockedMotion.reset();
    if (pointerLocked && nativePointerLocked) void releaseNativePointerLock(false);
    if (!pointerLocked && !nativePointerLocked) relativeMouse = false;
  }
  /** Whether capture is wanted at all right now — theater as before, or an
   *  explicit relative-mouse session. Control over a real desktop either way;
   *  a KVM source has no pointer to capture. */
  const wantsCapture = $derived((theater || relativeMouse) && stagePointerActive && !kvmSource);
  let pointerLockPending = false;
  async function maybePointerLock(relativeIntent = relativeMouse) {
    const target = stageEl;
    // Read the click's intent directly. Svelte derives flush after the event;
    // gating this user-activation call only through `wantsCapture` could still
    // see the pre-click false value and silently spend the one valid gesture.
    const captureWanted =
      (theater || relativeIntent) &&
      app.consoleControl &&
      selected?.media === "display" &&
      !kvmSource;
    if (!captureWanted || pointerLocked || nativePointerLocked || pointerLockPending || !target) return;

    pointerLockPending = true;
    try {
      // WebKit rejects pointer lock with WrongDocumentError when the native
      // Tauri window has not finished taking focus. This is easy to hit when
      // clicking into a fullscreen or secondary console. Focus both layers
      // before asking. Do not await native focus here: crossing an async IPC
      // boundary consumes WebKit's transient user activation before
      // requestPointerLock gets it. The browser remains the final focus gate;
      // a refusal leaves the mode armed so a click on the stage retries.
      target.focus({ preventScroll: true });
      if (!document.hasFocus()) {
        void focusThisWindow();
        target.focus({ preventScroll: true });
      }
      if (target !== stageEl || !target.isConnected) {
        return;
      }
      await target.requestPointerLock();
      // Older WKWebView builds return `void` before they report success or
      // failure. If no pointerlock event follows, use the native macOS grab
      // instead of leaving a permanently-lit but uncaptured switch.
      setTimeout(() => {
        if (relativeMouse && !pointerLocked && !nativePointerLocked) {
          void activateNativePointerLock(target);
        }
      }, 150);
    } catch (error) {
      // A focus transition can still win the race on WebKit. Keep relative
      // mode armed so the next click retries, but never turn that expected
      // browser refusal into an unhandled-promise error screen.
      console.warn("pointer lock request failed:", error);
      await activateNativePointerLock(target);
    } finally {
      pointerLockPending = false;
    }
  }
  let nativePointerLockPending = false;
  async function activateNativePointerLock(target = stageEl) {
    if (!target || pointerLocked || nativePointerLocked || nativePointerLockPending) return;
    nativePointerLockPending = true;
    try {
      const rect = target.getBoundingClientRect();
      const captured = await setMacRelativePointerCapture(true, {
        x: rect.left + rect.width / 2,
        y: rect.top + rect.height / 2,
      });
      if (!captured) return;
      if (
        pointerLocked ||
        !relativeMouse ||
        !app.consoleControl ||
        selected?.media !== "display" ||
        kvmSource
      ) {
        await setMacRelativePointerCapture(false);
        return;
      }
      nativePointerLocked = true;
      lockedMotion.reset();
      clientLog("[relative-mouse] macOS native pointer capture active");
    } finally {
      nativePointerLockPending = false;
    }
  }
  async function releaseNativePointerLock(disarm = true) {
    if (disarm) relativeMouse = false;
    if (!nativePointerLocked) return;
    nativePointerLocked = false;
    lockedMotion.reset();
    await setMacRelativePointerCapture(false);
  }
  let nativeEscapeHeld = false;
  function toggleRelativeMouse() {
    if (pointerLocked || nativePointerLocked) {
      relativeMouse = false;
      if (pointerLocked) document.exitPointerLock();
      if (nativePointerLocked) void releaseNativePointerLock();
      return;
    }
    // A standing share can authorize control before its route is live. The
    // relative button is the user's intent to drive, so bring up that already-
    // granted leg just like the Keyboard & mouse toggle does.
    if (!app.consoleControl) app.toggleConsoleControl();
    if (!app.consoleControl) return;
    relativeMouse = true;
    // This click IS the user gesture, so grab now rather than waiting for
    // the next one.
    void maybePointerLock(true);
  }
  $effect(() => {
    document.addEventListener("pointerlockchange", lockChanged);
    const lockFailed = () => void activateNativePointerLock();
    document.addEventListener("pointerlockerror", lockFailed);
    return () => {
      document.removeEventListener("pointerlockchange", lockChanged);
      document.removeEventListener("pointerlockerror", lockFailed);
      if (nativePointerLocked) void setMacRelativePointerCapture(false);
    };
  });
  $effect(() => {
    // Losing control (or the desktop under it) releases the capture, and
    // leaving theater releases it only when relative mouse isn't holding it.
    if (!wantsCapture) {
      if (pointerLocked) document.exitPointerLock();
      if (nativePointerLocked) void releaseNativePointerLock();
    }
  });

  // Pointer moves stream constantly; cap at ~60/s — the events are tiny
  // and the finer cadence keeps remote cursor motion feeling direct.
  let lastMoveAt = 0;
  const lockedMotion = makeRelativeMotionForwarder((dx, dy) => {
    app.sendConsoleInput({ kind: "mouse_move_rel", dx, dy });
  });
  /** WebView2's unbounded edge stream arrives as `mousemove`; WKWebView's
   *  usable locked stream can arrive as `pointermove`. Feed both through the
   *  compatibility-event de-duplicator. */
  function onLockedMouseMove(e: MouseEvent) {
    if ((!pointerLocked && !nativePointerLocked) || !stagePointerActive || kvmSource) return;
    lockedMotion.forward(e, "mouse");
  }
  // Mouse-drag panning of a zoomed picture while control is off — the
  // only time a mouse drag means the VIEW and not the remote.
  let panFrom: { x: number; y: number; vx: number; vy: number } | null = null;
  function onPointerMove(e: PointerEvent) {
    if (e.pointerType === "touch") {
      touchMouse.move(e);
      return;
    }
    if ((pointerLocked || nativePointerLocked) && stagePointerActive && !kvmSource) {
      lockedMotion.forward(e, "pointer");
      return;
    }
    // Keep keyboard focus on the stage whenever control is on (even over a
    // camera input, where pointer forwarding is off but typing still flows) —
    // but only while a button is held. A bare hover raising and stealing the
    // OS window is the KVM rule taken too literally: with several console
    // windows open, just crossing one on the way somewhere else yanks focus
    // off whatever you were doing. A held button means a *drag*, which is the
    // one move that genuinely has to carry focus with it — dragging from one
    // screen's window onto another must land on the window it ends over. A
    // click still pins focus outright (see the button handler), so nothing
    // that needs focus deliberately has lost a way to ask for it.
    if (app.consoleControl && e.buttons !== 0) claimFocus();
    if (!stagePointerActive) {
      if (panFrom && view.scale > 1.001) {
        setView({
          scale: view.scale,
          x: panFrom.vx + (e.clientX - panFrom.x),
          y: panFrom.vy + (e.clientY - panFrom.y),
        });
      }
      return;
    }
    const now = performance.now();
    if (now - lastMoveAt < 16) return;
    const p = normPoint(e);
    if (!p) return;
    lastMoveAt = now;
    // An absolute mouse move re-seats the trackpad's virtual cursor too.
    virt.x = p.x;
    virt.y = p.y;
    updateCrosshair();
    app.sendConsoleInput({ kind: "mouse_move", ...p, screen: controlScreen });
  }

  function onPointerButton(e: PointerEvent, down: boolean) {
    // Real buttons living ON the stage ("Return video here") own their
    // taps: capturing the pointer here would retarget the derived click
    // onto the stage and the button would never fire.
    if ((e.target as HTMLElement | null)?.closest?.("button")) return;
    // The press that dismisses an open menu is spent on the dismissal —
    // it must not also click the remote.
    if (down && openMenu) {
      openMenu = null;
      return;
    }
    if (e.pointerType === "touch") {
      // Hold every touch for its whole life — glides and pinches that
      // wander off the element must keep streaming here.
      if (down) {
        // The touch counterpart of the mouse click-pin below: an iPad
        // with a hardware keyboard needs the stage refocused after a bar
        // tap stole it, or key forwarding silently stops.
        if (app.consoleControl && !keysOpen) stageEl?.focus({ preventScroll: true });
        try {
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        } catch {
          // a stale/synthetic pointer id — capture is best-effort
        }
        touchMouse.down(e);
      } else {
        touchMouse.up(e);
      }
      return;
    }
    // A click is the most reliable focus pin (whatever was last focused) —
    // unless the soft keyboard holds it on purpose.
    if (down && app.consoleControl && !keysOpen) stageEl?.focus({ preventScroll: true });
    if (!stagePointerActive) {
      // View-only: a mouse drag pans the zoomed picture.
      if (down && view.scale > 1.001) {
        panFrom = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y };
        try {
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        } catch {
          // best-effort
        }
      } else if (!down) {
        panFrom = null;
      }
      return;
    }
    if (down) {
      // Hold the pointer for the whole press: a mouse drag that wanders
      // off the element keeps streaming its moves here, and the matching
      // up always lands — capture auto-releases on pointerup.
      try {
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      } catch {
        // a stale/synthetic pointer id — capture is best-effort
      }
    }
    // Theater aiming: the click that lands in the picture captures the
    // mouse; while captured, buttons forward raw (no position re-seat —
    // the relative stream owns the cursor).
    if (down) void maybePointerLock();
    if ((pointerLocked || nativePointerLocked) && !kvmSource) {
      e.preventDefault();
      app.sendConsoleInput({ kind: "mouse_button", button: e.button, down });
      if (down) heldButtons.add(e.button);
      else heldButtons.delete(e.button);
      return;
    }
    const p = normPoint(e);
    if (!p) {
      // A release outside the streamed frame (a captured drag that wandered
      // onto the letterbox bars or past the edge before lifting): still lift
      // the button we pressed, or the remote is stranded mid-drag. Presses
      // outside the frame stay ignored, as ever.
      if (!down && heldButtons.delete(e.button)) {
        e.preventDefault();
        app.sendConsoleInput({ kind: "mouse_button", button: e.button, down: false });
      }
      return;
    }
    e.preventDefault();
    // Land the cursor exactly where the click happened, then click.
    virt.x = p.x;
    virt.y = p.y;
    updateCrosshair();
    app.sendConsoleInput({ kind: "mouse_move", ...p, screen: controlScreen });
    app.sendConsoleInput({ kind: "mouse_button", button: e.button, down });
    if (down) heldButtons.add(e.button);
    else heldButtons.delete(e.button);
  }

  // Buttons currently pressed on the remote, so a pointer that *cancels*
  // (iOS reclaiming the touch for a system gesture, the OS eating the
  // pointer mid-drag) can lift what it pressed — its matching pointerup is
  // never coming, and without this the remote is stranded mid-drag with a
  // button held.
  const heldButtons = new Set<number>();
  function onPointerCancel(e: PointerEvent) {
    if (e.pointerType === "touch") touchMouse.cancel(e);
    panFrom = null;
    for (const b of heldButtons) {
      app.sendConsoleInput({ kind: "mouse_button", button: b, down: false });
    }
    heldButtons.clear();
  }

  // iPhone/iPad WebKit: touches already arrive as Pointer Events (that's
  // what drives the remote mouse above), but WebKit *also* synthesizes
  // compatibility mouse events and gesture defaults (double-tap zoom, the
  // long-press callout) off the raw touches. Cancelling touchstart's
  // default keeps a tap exactly one click (or one trackpad gesture) at its
  // coordinates — except on the stage's real buttons ("Return video
  // here"), which need their native taps. Bound via an action, not
  // `ontouchstart` — Svelte registers touch listeners passive, where
  // preventDefault is a no-op. (Scroll/pan is already opted out with
  // `touch-action: none` in CSS, so a drag streams pointermoves instead of
  // being claimed as a pan.)
  function touchGuard(el: HTMLElement) {
    const onTouchStart = (e: TouchEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("button")) return;
      if (stagePointerActive || hasFrame) e.preventDefault();
    };
    el.addEventListener("touchstart", onTouchStart, { passive: false });
    return {
      destroy() {
        el.removeEventListener("touchstart", onTouchStart);
      },
    };
  }

  function onWheel(e: WheelEvent) {
    if (!stagePointerActive) {
      // Trackpad pinches arrive as ctrl+wheel — zoom the view with them
      // while control is off (with it on, the wheel belongs to the remote).
      if (e.ctrlKey && hasFrame) {
        e.preventDefault();
        zoomAt(view.scale * Math.exp(-e.deltaY / 240), e.clientX, e.clientY);
      }
      return;
    }
    if (!normPoint(e)) return;
    e.preventDefault();
    // Normalize the browser's delta modes to wheel lines.
    const lines = e.deltaMode === 1 ? 1 : 1 / 40;
    app.sendConsoleInput({ kind: "wheel", dx: e.deltaX * lines, dy: e.deltaY * lines });
  }

  // Key forwarding with the bookkeeping combinations need: the physical
  // `code` rides along, and held keys are lifted in a burst whenever
  // their keyups can no longer arrive (blur, control off, session end).
  const keys = makeKeyForwarder((a) => app.sendConsoleInput(a));

  // The physical paste chord — Cmd+V (mac) / Ctrl+V (win·linux). `code` is
  // layout-independent, so this fires on the V key whatever it composes.
  function isPasteChord(e: KeyboardEvent): boolean {
    const isV = e.code === "KeyV" || (!e.code && e.key.toLowerCase() === "v");
    return isV && (e.metaKey || e.ctrlKey) && !e.altKey;
  }

  // The physical copy/cut chords — Cmd+C·X (mac) / Ctrl+C·X (win·linux). The
  // mirror of paste: with clipboard passthrough on, these copy/cut *from* the
  // remote. `code` is layout-independent, so it fires on the C/X key whatever
  // it composes.
  function isCopyCutChord(e: KeyboardEvent): boolean {
    const isCorX =
      e.code === "KeyC" ||
      e.code === "KeyX" ||
      (!e.code && (e.key.toLowerCase() === "c" || e.key.toLowerCase() === "x"));
    return isCorX && (e.metaKey || e.ctrlKey) && !e.altKey;
  }

  // Copy/cut/paste keyups already accounted for: the chord is replayed as a
  // synthesized press (a paste *after* the clipboard frame, a copy/cut *before*
  // pulling the remote clipboard back — see onKey), so the matching natural
  // keyup is a straggler to swallow — no double event, no stuck key.
  const chordHandled = new Set<string>();

  // No-control keys ride the window, so Escape works without the stage being
  // focused: it steps back through the console's layers — open menu first,
  // fullscreen next, then (the popover habit) closes the session; in a
  // window it closes the window too.
  function onWindowKey(e: KeyboardEvent) {
    if (nativePointerLocked && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      nativeEscapeHeld = true;
      void releaseNativePointerLock();
      return;
    }
    if (!node || app.consoleControl) return;
    if (e.key === "Escape") {
      if (openMenu) {
        openMenu = null;
        return;
      }
      if (theater) void flipTheater();
      else endSession();
    }
  }

  // Control forwarding — bound to the focusable stage, so it fires only while
  // the stage holds focus. With control on, *every* key belongs to the
  // remote — including Escape and chords like Ctrl+W, exactly like sitting
  // at the machine.
  function onKey(e: KeyboardEvent, down: boolean) {
    if (!node || !app.consoleControl) return;
    if (e.key === "Escape" && (nativePointerLocked || nativeEscapeHeld)) {
      e.preventDefault();
      e.stopPropagation();
      if (down) {
        nativeEscapeHeld = true;
        void releaseNativePointerLock();
      } else {
        nativeEscapeHeld = false;
      }
      return;
    }
    e.preventDefault();
    // Send-on-paste: with clipboard passthrough on, a paste pushes this
    // machine's clipboard to the remote first, then replays the paste
    // keystroke — so the remote writes our clipboard before it pastes.
    // Both ride the same ordered channel to the same peer, so sending the
    // frame and only then the keystroke keeps the order the remote needs.
    if (down && app.consoleClipboard && isPasteChord(e)) {
      // Paste once per press: a held paste chord must not repeat-paste, and
      // its forwarded repeat-downs would never get a matching keyup (the
      // straggler line below swallows it), stranding the key down on the
      // remote.
      if (e.repeat) return;
      chordHandled.add(e.code || e.key);
      void app.pasteConsoleClipboard(e.key, e.code || undefined, e.metaKey);
      return;
    }
    // Copy/cut-from-remote: forward the chord so the remote copies its
    // selection into its own clipboard, then pull that clipboard back here.
    // Same once-per-press guard as paste — the forwarded keystroke is what
    // does the copying; its straggler keyup is swallowed below.
    if (down && app.consoleClipboard && isCopyCutChord(e)) {
      if (e.repeat) return;
      chordHandled.add(e.code || e.key);
      void app.copyConsoleClipboard(e.key, e.code || undefined, e.metaKey);
      return;
    }
    if (!down && chordHandled.delete(e.code || e.key)) return; // straggler keyup
    keys.onKey(e, down);
  }

  function toggleControl() {
    // Turning control off mid-chord: lift what's held while the route
    // can still carry the keyups — the hardware keys, the strip's armed
    // modifiers, and any touch-held button alike.
    if (app.consoleControl) {
      keys.releaseAll();
      releaseStrip();
      touchMouse.reset();
    }
    app.toggleConsoleControl();
    // Turning it on: focus the stage so keys forward immediately, without
    // needing a click into the picture first (a `tabindex=-1` element is
    // still focusable programmatically, so this works before the reactive
    // tabindex flips to 0).
    if (app.consoleControl && !keysOpen) stageEl?.focus({ preventScroll: true });
  }

  function inputIcon(c: Capability): string {
    return originIcon(c.origin, c.media);
  }

  // Files/Terminal from inside a console: on the desktop they open beside
  // this window and the session keeps running — but the single-window
  // shells (phone, web preview) stack overlays, and a console left
  // streaming behind Files is a battery bill nobody asked for. Moving on
  // ends the session; whatever surface closes last lands on the graph
  // with nothing running underneath.
  function launchFiles() {
    if (!node) return;
    const id = node.id;
    if (!windowed) void endSession();
    app.openFiles(id);
  }
  function launchTerminal() {
    if (!node) return;
    const id = node.id;
    if (!windowed) void endSession();
    app.openTerminal(id);
  }
</script>

<svelte:window
  onkeydown={onWindowKey}
  onmousemove={onLockedMouseMove}
  onpointerdown={onWindowPointerDown}
  onresize={clampBarPos}
  onblur={() => void releaseNativePointerLock()}
/>

{#if node}
  <div class="scrim" class:windowed>
    <div
      bind:this={consoleEl}
      class="console"
      class:theater
      role="dialog"
      aria-modal={!windowed}
      aria-label="Console for {displayName(node)}"
    >
      <!-- Video stage -->
      <!-- role=application: a remote-desktop surface — every pointer/key
           event belongs to the far machine while control is on. Focusable
           only while control is on, so keys forward from here and nowhere
           else. -->
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <div
        bind:this={stageEl}
        class="stage"
        class:grabbing={stagePointerActive}
        role="application"
        aria-label="Remote screen — input is forwarded while keyboard & mouse control is on"
        tabindex={app.consoleControl ? 0 : -1}
        use:touchGuard
        onpointermove={onPointerMove}
        onpointerdown={(e) => onPointerButton(e, true)}
        onpointerup={(e) => onPointerButton(e, false)}
        onpointercancel={onPointerCancel}
        onwheel={onWheel}
        onkeydown={(e) => onKey(e, true)}
        onkeyup={(e) => onKey(e, false)}
        onblur={() => keys.releaseAll()}
        oncontextmenu={(e) => app.consoleControl && e.preventDefault()}
      >
        {#if dropHover || dropBusy}
          <div class="file-drop" class:busy={dropBusy} aria-live="polite">
            <div class="file-drop-icon">{dropBusy ? "↗" : "⇩"}</div>
            <strong>{dropBusy ? "Sending files…" : "Drop onto the remote"}</strong>
            <span>
              {dropBusy
                ? "The remote clipboard and paste are being completed"
                : `${dropCount || 1} ${dropCount === 1 ? "file" : "files"} · releases at this location`}
            </span>
          </div>
        {/if}
        {#if selectedPopped}
          <!-- This input lives in its own window right now; here's its
               way home — findable even when that window is fullscreen on
               another monitor. -->
          <div class="screen">
            <div class="screen-glyph">{selected ? inputIcon(selected) : "🪟"}</div>
            <div class="screen-title">{selected?.label ?? ""} is in its own window</div>
            <button class="return-btn" onclick={() => app.askReturnVideo(`cap:${selectedId}`)}>
              ⤓ Return video here
            </button>
          </div>
        {:else}
          {#if app.consoleVideoLive || switching}
            <!-- Kept mounted through a switch: the canvas retains the old
                 picture (dimmed, under the chip) so changing monitors
                 reads as a crossfade instead of a placeholder flash. -->
            <canvas
              bind:this={canvasEl}
              class="live"
              class:waiting={!hasFrame && !switching}
              class:switching={!hasFrame && switching}
              style:transform={view.scale !== 1 || view.x !== 0 || view.y !== 0
                ? `translate(${view.x}px, ${view.y}px) scale(${view.scale})`
                : undefined}
              aria-label="Live {selected?.media === 'video' ? 'camera' : 'screen'} view of {displayName(
                node,
              )}"
            ></canvas>
          {/if}
          {#if switching && !hasFrame}
            <div class="switching-chip" aria-live="polite">
              Switching view…{hostStatus ? ` (${hostStatusText(hostStatus)})` : ""}
            </div>
          {/if}
          {#if hasFrame && stagePointerActive && mobileShell}
            <!-- Thin aiming crosshair at the commanded position (crosshairEl):
                 where the cursor SHOULD be, drawn with no video latency so you
                 can line things up precisely. Mobile only — on desktop the real
                 mouse already shows as a crosshair (`.stage.grabbing`), so
                 drawing our own would be a second one on top of it. -->
            <div class="crosshair" bind:this={crosshairEl} aria-hidden="true">
              <svg width="24" height="24" viewBox="0 0 24 24">
                <g stroke="#fff" stroke-width="1" shape-rendering="crispEdges">
                  <line x1="12" y1="1.5" x2="12" y2="9" />
                  <line x1="12" y1="15" x2="12" y2="22.5" />
                  <line x1="1.5" y1="12" x2="9" y2="12" />
                  <line x1="15" y1="12" x2="22.5" y2="12" />
                </g>
              </svg>
            </div>
          {/if}
          {#if hasFrame}
            <!-- the canvas above is the stage; a host-reported stall (the
                 remote display sleeping mid-session) banners over it. -->
            {#if videoRefused}
              <div class="host-status">{videoRefused}</div>
            {:else if hostStatus}
              <div class="host-status">{hostStatusText(hostStatus)}</div>
            {/if}
            {#if view.scale > 1.001}
              <!-- Zoomed: say so, and offer the way back without a pinch.
                   Pointer events stop here so control forwarding never
                   mistakes the tap for a remote click. -->
              <button
                class="zoom-chip"
                title="Reset zoom"
                onpointerdown={(e) => e.stopPropagation()}
                onpointerup={(e) => e.stopPropagation()}
                onclick={(e) => {
                  e.stopPropagation();
                  resetView();
                }}
              >
                {zoomPercent(view.scale)} ✕
              </button>
            {/if}
          {:else if selected}
            <div class="screen" style="--mc: {mediaColor(selected.media)}">
              <div class="screen-glyph">{inputIcon(selected)}</div>
              <div class="screen-title">{selected.label}</div>
              {#if selected.media === "display"}
                <div class="screen-note">
                  {videoRefused ??
                    (hostStatus ? hostStatusText(hostStatus) : "Connecting this machine's display…")}
                </div>
              {:else if cameraSupported}
                <div class="screen-note">
                  {videoRefused ??
                    (hostStatus ? hostStatusText(hostStatus) : "Connecting this camera…")}
                </div>
              {:else}
                <div class="screen-note">
                  This machine runs an older AllMyStuff — update it there and its cameras will
                  stream here.
                </div>
              {/if}
            </div>
          {:else}
            <div class="screen empty">
              <div class="screen-glyph">🪟</div>
              <div class="screen-note">
                Pick a video input from the Screens menu in the bar above.
              </div>
            </div>
          {/if}
        {/if}
      </div>

      <!-- The control bar — a horizontal bar across the top on desktop, a
           vertical rail on the right on the phone. The handle tab on its
           outer side is the one and only hide/show control; the tab never
           moves, the bar slides out past its edge behind it. -->
      <div
        bind:this={barWrapEl}
        class="bar-anchor"
        class:v={vertical}
        class:h={!vertical}
        role="group"
        aria-label="Console control bar"
        style:transform={vertical
          ? `translateY(calc(-50% + ${barPos}px))`
          : `translateX(calc(-50% + ${barPos}px))`}
        onpointerdowncapture={(e) => {
          // With the soft keyboard up, bar taps must not steal focus from
          // its hidden input (a blur drops the OS keyboard mid-word) —
          // suppress the focus default; the click still fires.
          if (keysOpen && (e.target as HTMLElement).closest("button")) e.preventDefault();
        }}
      >
        <button
          class="bar-tab"
          class:hidden={barHidden}
          title={barHidden ? "Show controls" : "Hide controls"}
          aria-label={barHidden ? "Show console controls" : "Hide console controls"}
          onclick={toggleBar}>{vertical ? (barHidden ? "‹" : "›") : barHidden ? "▾" : "▴"}</button
        >
        <div class="kvmbar" class:asleep={barHidden} role="toolbar" aria-label="Console controls">
          <!-- svelte-ignore a11y_consider_explicit_label -->
          <button class="grip" title="Move the bar" onpointerdown={onGripDown}>⠿</button>
          <span class="vsep"></span>
          <button
            class="kbtn"
            class:open={openMenu === "session"}
            title="{displayName(node)} — session"
            aria-label="Session menu"
            onclick={() => toggleMenu("session")}
          >
            🖥<span class="presence" class:on={node.online}></span>
          </button>
          {#if vertical}
            <!-- The phone folds the inputs into a menu… -->
            <button
              class="kbtn"
              class:open={openMenu === "screens"}
              title="Screens & cameras{selected ? ` — ${selected.label}` : ''}"
              aria-label="Screens and cameras menu"
              onclick={() => toggleMenu("screens")}
            >
              {selected ? inputIcon(selected) : "🪟"}
            </button>
          {:else}
            <!-- …the desktop wears them on the bar: one icon per monitor
                 and camera (hover for its name), the active one filled, a
                 popped-out one hollow (click brings its video home), and
                 a pop-out button for the screen being viewed. -->
            <span class="vsep"></span>
            {#each inputs as inp (inp.id)}
              {@const inpPopped = app.isVideoPopped(`cap:${inp.id}`)}
              <!-- A popped-out input still SELECTS on click — the stage
                   then shows its "in its own window" card with the
                   Return-video-here button, the deliberate way home. -->
              <button
                class="kbtn input"
                class:active={inp.id === selectedId && !inpPopped}
                class:hollow={inpPopped}
                title={inpPopped ? `${inp.label} — in its own window` : inp.label}
                aria-label={inp.label}
                onclick={() => app.setConsoleInput(inp.id)}
              >
                {inputIcon(inp)}
                {#if inp.default}<span class="kdef" title="Default input">★</span>{/if}
              </button>
            {/each}
            {#if isTauri() && selected && !selectedPopped}
              <button
                class="kbtn"
                title="Pop {selected.label} out into its own window"
                aria-label="Pop {selected.label} out into its own window"
                onclick={() =>
                  selectedId &&
                  void app.popOutConsoleInput(selectedId, nativeDecoderPreference)}>⧉</button
              >
            {/if}
          {/if}
          <span class="vsep"></span>
          {#if access.control}
            <button
              class="kbtn"
              class:on={app.consoleControl}
              title="Send this machine's keyboard & mouse to the remote"
              aria-label="Keyboard & mouse control"
              aria-pressed={app.consoleControl}
              onclick={toggleControl}>🕹</button
            >
          {/if}
          {#if access.control && touchDevice}
            <button
              class="kbtn"
              class:on={keysOpen}
              title="Type on the remote (soft keyboard)"
              aria-label="Soft keyboard"
              aria-pressed={keysOpen}
              onclick={toggleKeys}>⌨️</button
            >
          {/if}
          {#if access.audio}
            <button
              class="kbtn slim"
              class:on={app.consoleAudio}
              title="Play that machine's audio on this machine (listen-only — nothing is sent back)"
              aria-label="Audio"
              aria-pressed={app.consoleAudio}
              onclick={() => app.toggleConsoleAudio()}>🔊</button
            >
          {/if}
          {#if access.clipboard}
            <button
              class="kbtn slim"
              class:on={app.consoleClipboard}
              title="Share clipboard on paste — pasting here sends this machine's clipboard so it lands on the remote"
              aria-label="Clipboard passthrough"
              aria-pressed={app.consoleClipboard}
              onclick={() => app.toggleConsoleClipboard()}>📋</button
            >
          {/if}
          {#if app.filesAllowed(node) || app.terminalAllowed(node)}
            <span class="vsep slim"></span>
            {#if app.filesAllowed(node)}
              <button
                class="kbtn slim"
                title="Browse this machine's files over the mesh"
                aria-label="Files"
                onclick={launchFiles}>🗂</button
              >
            {/if}
            {#if app.terminalAllowed(node)}
              <button
                class="kbtn slim"
                title="Open a shell on this machine over the mesh"
                aria-label="Terminal"
                onclick={launchTerminal}>📟</button
              >
            {/if}
          {/if}
          {#if drivesAllowed}
            <button
              class="kbtn slim"
              class:open={openMenu === "drives"}
              title={kvmSource ? "Present install media to the attached computer through this KVM" : "Map a local drive or folder onto this machine"}
              aria-label="Mapped drives"
              onclick={() => toggleMenu("drives")}>💾</button
            >
          {/if}
          {#if !kvmSource && kvmMediaAllowed}
            <button
              class="kbtn slim"
              class:open={openMenu === "kvm-media"}
              title="Present install or recovery media through the KVM attached to this computer"
              aria-label="KVM drives"
              onclick={() => toggleMenu("kvm-media")}>💿</button
            >
          {/if}
          {#if kvmPowerAllowed}
            <button
              class="kbtn slim"
              class:open={openMenu === "power"}
              title="Power controls for the computer attached to this KVM"
              aria-label="Power via KVM"
              aria-haspopup="menu"
              aria-expanded={openMenu === "power"}
              onclick={() => toggleMenu("power")}>⏻</button
            >
          {/if}
          <span class="vsep"></span>
          <button
            class="kbtn"
            class:open={openMenu === "video"}
            class:warn={!!pipeDiag}
            title="Stream quality & zoom"
            aria-label="Video menu"
            onclick={() => toggleMenu("video")}>🎚</button
          >
          {#if relativeMouseAvailable}
            <!-- Relative mouse. Only meaningful over a desktop we're driving:
                 a game on the far side captures its own cursor and wants
                 deltas, not coordinates. Esc gives the pointer back. -->
            <button
              class="kbtn"
              class:on={pointerLocked || nativePointerLocked}
              title={pointerLocked || nativePointerLocked
                ? "Release the mouse (Esc)"
                : relativeMouse
                  ? "Relative mouse is armed — click the screen to capture"
                : "Capture the mouse — relative motion for a fullscreen app or game"}
              aria-label="Relative mouse"
              aria-pressed={pointerLocked || nativePointerLocked}
              onclick={toggleRelativeMouse}>🎯</button
            >
          {/if}
          {#if !mobileShell}
            <!-- The phone shell IS full-screen — a fullscreen button
                 there is a knob with nothing behind it. -->
            <button
              class="kbtn"
              title={theater ? `Exit fullscreen${app.consoleControl ? "" : " (Esc)"}` : "Fullscreen"}
              aria-label={theater ? "Exit fullscreen" : "Fullscreen"}
              onclick={() => void flipTheater()}>{theater ? "⤡" : "⛶"}</button
            >
          {/if}
          <button class="kbtn end" title="End session" aria-label="End session" onclick={endSession}
            >✕</button
          >
        </div>

        <!-- The bar's menu — one at a time, opening to the LEFT of the
             bar (and nudged back on-screen when the bar sits near the
             top or bottom, see menuShift). -->
        {#if openMenu}
          <div
            bind:this={menuEl}
            class="kvmenu"
            class:drives-menu={openMenu === "drives" || openMenu === "kvm-media"}
            style:transform={vertical
              ? `translateY(calc(-50% + ${menuShift}px))`
              : `translateX(calc(-50% + ${menuShift}px))`}
            role="menu"
          >
            {#if openMenu === "session"}
              <div class="mhead">
                <span class="mavatar">🖥</span>
                <div class="mid">
                  <div class="mname">{displayName(node)}</div>
                  <div class="msub">
                    <span class="dot" class:on={node.online}></span>
                    {node.online ? "online" : "offline"} · remote console
                  </div>
                </div>
              </div>
              {#if hasFrame || app.consoleSessionRoutes.length > 0}
                <div class="mchips">
                  {#if hasFrame}
                    <span class="chip stream" title="Live stream — frame size · rate">
                      <span class="chip-dot live-dot"></span>{frameW}×{frameH} · {fps} fps · {transport}
                    </span>
                  {/if}
                  {#each app.consoleSessionRoutes as r (r.id)}
                    <span class="chip" style="--mc: {mediaColor(r.media as MediaKind)}">
                      <span class="chip-dot"></span>{MEDIA[r.media as MediaKind].label}
                    </span>
                  {/each}
                </div>
              {/if}
              <div class="msep"></div>
              {#if access.control}
                <button class="mrow" onclick={toggleControl}>
                  <span class="micon">🕹</span>Keyboard &amp; mouse
                  <span class="pip" class:lit={app.consoleControl}></span>
                </button>
              {/if}
              {#if access.audio}
                <button class="mrow" onclick={() => app.toggleConsoleAudio()}>
                  <span class="micon">🔊</span>Audio
                  <span class="pip" class:lit={app.consoleAudio}></span>
                </button>
              {/if}
              {#if access.clipboard}
                <button class="mrow" onclick={() => app.toggleConsoleClipboard()}>
                  <span class="micon">📋</span>Clipboard
                  <span class="pip" class:lit={app.consoleClipboard}></span>
                </button>
              {/if}
              {#if app.filesAllowed(node) || app.terminalAllowed(node)}
                <div class="msep"></div>
                {#if app.filesAllowed(node)}
                  <button class="mrow" onclick={() => app.openFiles(node.id)}>
                    <span class="micon">🗂</span>Files
                  </button>
                {/if}
                {#if app.terminalAllowed(node)}
                  <button class="mrow" onclick={() => app.openTerminal(node.id)}>
                    <span class="micon">📟</span>Terminal
                  </button>
                {/if}
              {/if}
              {#if drivesAllowed}
                <button class="mrow" onclick={() => toggleMenu("drives")}>
                  <span class="micon">💾</span>Drives
                </button>
              {/if}
              {#if !kvmSource && kvmMediaAllowed}
                <button class="mrow" onclick={() => toggleMenu("kvm-media")}>
                  <span class="micon">💿</span>KVM drives
                </button>
              {/if}
              {#if kvmPowerAllowed}
                <button class="mrow" onclick={() => toggleMenu("power")}>
                  <span class="micon">⏻</span>Power
                </button>
              {/if}
              <div class="msep"></div>
              <button class="mrow danger" onclick={endSession}>
                <span class="micon">⏻</span>End session
              </button>
            {:else if openMenu === "drives"}
              <DrivePanel target={node.id} supportSession={app.isCecCustomer(node.id)} />
            {:else if openMenu === "kvm-media" && consoleKvm}
              <DrivePanel target={consoleKvm.id} />
            {:else if openMenu === "power"}
              <div class="mhead">
                <span class="mavatar">⏻</span>
                <div class="mid">
                  <div class="mname">Power via {consoleKvm ? displayName(consoleKvm) : "KVM"}</div>
                  <div class="msub">Controls the computer attached to this KVM</div>
                </div>
              </div>
              <div class="msep"></div>
              {#each [
                { action: "wake", icon: "⌨", label: "Wake", detail: "send a Shift key press" },
                { action: "short", icon: "⏻", label: "Short press", detail: "pulse the power button" },
                { action: "long", icon: "⏻", label: "Long press", detail: "hold power for 12 seconds" },
                { action: "reset", icon: "↻", label: "Reset", detail: "pulse the reset line" },
              ] as item (item.action)}
                <button
                  class="mrow power-row"
                  onclick={() => {
                    openMenu = null;
                    if (consoleKvm) {
                      void app.kvmFeature(consoleKvm.id, item.action as "wake" | "short" | "long" | "reset");
                    }
                  }}
                >
                  <span class="micon">{item.icon}</span>
                  <span class="power-copy"><strong>{item.label}</strong><small>{item.detail}</small></span>
                </button>
              {/each}
            {:else if openMenu === "screens"}
              {#each inputs as inp (inp.id)}
                {@const inpPopped = app.isVideoPopped(`cap:${inp.id}`)}
                <div class="mrow-wrap">
                  <button
                    class="mrow"
                    class:sel={inp.id === selectedId}
                    title={inpPopped ? `${inp.label} — in its own window` : inp.label}
                    onclick={() => {
                      app.setConsoleInput(inp.id);
                      openMenu = null;
                    }}
                  >
                    <span class="micon">{inputIcon(inp)}</span>
                    <span class="mlabel">{inp.label}</span>
                    {#if inp.default}<span class="mdef" title="Default input">★</span>{/if}
                    {#if inpPopped}<span class="mout" title="In its own window">↗</span>{/if}
                    <span class="mcheck">{inp.id === selectedId ? "✓" : ""}</span>
                  </button>
                  {#if inpPopped}
                    <button
                      class="mside"
                      title="Return this video here"
                      aria-label="Return {inp.label} here"
                      onclick={(e) => {
                        e.stopPropagation();
                        app.askReturnVideo(`cap:${inp.id}`);
                      }}>⤓</button
                    >
                  {:else if isTauri() && !mobileShell}
                    <button
                      class="mside"
                      title="Pop this video out into its own window"
                      aria-label="Pop {inp.label} out into its own window"
                      onclick={(e) => {
                        e.stopPropagation();
                        void app.popOutConsoleInput(inp.id, nativeDecoderPreference);
                      }}>⧉</button
                    >
                  {/if}
                </div>
              {/each}
              {#if inputs.length === 0}
                <div class="mempty">No video inputs advertised</div>
              {/if}
            {:else}
              <!-- Video: the Mode control (shared with the popout bar),
                   the advanced rows, the zoom. -->
              {#if app.consoleVideoLive}
                <div class="mode-row" role="group" aria-label="Stream mode">
                  <span class="mode-label">Mode</span>
                  <ModePill
                    mode={app.consoleTune.mode}
                    game={app.consoleTune.game}
                    onapply={applyModeWire}
                    experimental={app.labsTier}
                    onexperimental={(on) => app.setLabsTier(on)}
                    placement="down"
                  />
                </div>
                <!-- Requested picks vs the live effective reality (polls the
                     streamer's dials ~1 Hz while this menu is open; falls back
                     to what's arriving here for a remote stream). -->
                <div class="eff-wrap">
                  <EffectiveReadout
                    routeId={app.consoleVideoLive}
                    requested={app.consoleTune}
                    w={frameW}
                    h={frameH}
                    {fps}
                    {transport}
                    mbps={recvMbps}
                  />
                </div>
                <button class="mrow" onclick={toggleAdv}>
                  <span class="micon">⚙</span>Advanced
                  <span class="mcheck">{advOpen ? "▾" : "▸"}</span>
                </button>
                {#if advOpen}
                  {#snippet valueRow(
                    key: "res" | "fps" | "rate",
                    name: string,
                    choices: PillChoice[],
                    current: number | null | undefined,
                    pick: (v: number | null) => void,
                  )}
                    <button
                      class="vrow"
                      class:tuned={(current ?? null) !== null}
                      onclick={() => (openSub = openSub === key ? null : key)}
                    >
                      <span>{name}</span>
                      <span class="vval">{pillLabel(choices, current)}</span>
                      <span class="vchev">{openSub === key ? "▾" : "▸"}</span>
                    </button>
                    {#if openSub === key}
                      <div class="vopts">
                        {#each choices as c (c.label)}
                          <button
                            class="vopt"
                            class:sel={(current ?? null) === c.value}
                            onclick={() => pick(c.value)}>{c.label}</button
                          >
                        {/each}
                      </div>
                    {/if}
                  {/snippet}
                  {@render valueRow("res", "Resolution", RES_CHOICES, app.consoleTune.maxEdge, pickRes)}
                  {@render valueRow("fps", "Frame rate", FPS_CHOICES, app.consoleTune.fps, pickFps)}
                  {@render valueRow("rate", "Bitrate", RATE_CHOICES, app.consoleTune.bitrate, pickRate)}
                  <button
                    class="vrow"
                    class:tuned={codecChoice !== "auto"}
                    onclick={() => (openSub = openSub === "codec" ? null : "codec")}
                  >
                    <span>Codec</span>
                    <span class="vval"
                      >{CODEC_CHOICES.find((c) => c.value === codecChoice)?.label ?? "Auto"}</span
                    >
                    <span class="vchev">{openSub === "codec" ? "▾" : "▸"}</span>
                  </button>
                  {#if openSub === "codec"}
                    <div class="vopts">
                      {#each CODEC_CHOICES as c (c.value)}
                        <button
                          class="vopt"
                          class:sel={codecChoice === c.value}
                          title={c.hint}
                          onclick={() => pickCodec(c.value)}>{c.label}</button
                        >
                      {/each}
                    </div>
                  {/if}
                  {#if stagePointerActive}
                    <button
                      class="vrow"
                      class:tuned={aspectChoice !== "auto"}
                      title="Source aspect — corrects the mouse when a machine whose native resolution isn't 16:9 is letterboxed into the capture. Auto detects the bars from the picture."
                      onclick={() => (openSub = openSub === "aspect" ? null : "aspect")}
                    >
                      <span>Aspect</span>
                      <span class="vval">{aspectLabel}</span>
                      <span class="vchev">{openSub === "aspect" ? "▾" : "▸"}</span>
                    </button>
                    {#if openSub === "aspect"}
                      <div class="vopts">
                        {#each ASPECTS as a (a.value)}
                          <button
                            class="vopt"
                            class:sel={aspectChoice === a.value}
                            onclick={() => pickAspect(a.value)}>{a.label}</button
                          >
                        {/each}
                      </div>
                    {/if}
                  {/if}
                {/if}
                <div class="msep"></div>
              {/if}
              <div class="zoom-row" role="group" aria-label="Zoom">
                <span class="zlabel">Zoom</span>
                <button class="zbtn" aria-label="Zoom out" onclick={() => zoomStep(-1)}>−</button>
                <span class="znow">{zoomPercent(view.scale)}</span>
                <button class="zbtn" aria-label="Zoom in" onclick={() => zoomStep(1)}>+</button>
                {#if view.scale > 1.001}
                  <button class="zreset" onclick={resetView}>Reset</button>
                {/if}
              </div>
              {#if hasFrame}
                <div class="mstats">
                  {frameW}×{frameH} · {transport} · {decodePath}
                  · in {inFps} → dec {decFps} → paint {fps}/s{pipeDiag ? ` · ⚠ ${pipeDiag}` : ""}
                </div>
              {/if}
            {/if}
          </div>
        {/if}
      </div>

      {#if keysOpen && app.consoleControl}
        <!-- Hold the strip's right edge clear of the vertical control rail
             (only the phone shell puts a rail on the right edge). The inset
             spans the rail body (2.7rem — `.bar-anchor.v .kvmbar`) plus its
             hide/show handle tab, which juts ~1.15rem further inward
             (`.bar-anchor.v .bar-tab`), so the tray never covers that tab.
             Desktop's rail sits on top, so the strip keeps full width there. -->
        <ConsoleKeys
          send={sendFromStrip}
          onclose={() => (keysOpen = false)}
          rightInset={vertical
            ? "calc(2.7rem + 1.15rem + env(safe-area-inset-right, 0px) + 0.5rem)"
            : "0px"}
        />
      {/if}
    </div>
  </div>
{/if}

<style>
  /* The console IS its viewport — in-page or windowed, it takes every
     pixel it's given (bumps and bars are handled with safe-area insets,
     not margins). No modal card, no border gutter. */
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 60;
    background: #14121f;
    padding: 0;
  }
  .console {
    position: relative;
    z-index: 1;
    width: 100%;
    height: 100%;
    background: #14121f;
    overflow: hidden;
    animation: rise 0.16s ease;
  }
  /* A dedicated console window animates via the OS, not the DOM. */
  .windowed .console {
    animation: none;
  }
  @keyframes rise {
    from {
      transform: translateY(14px) scale(0.98);
      opacity: 0;
    }
  }

  /* ---- the stage — the whole pane ---- */
  .stage {
    position: absolute;
    inset: 0;
    /* Touch drives the remote pointer (or a view gesture): opt out of the
       browser's own gestures (scroll/pan, double-tap zoom) so a finger
       drag streams pointermoves instead of being claimed — and
       pointercancelled — as a pan. The stage never scrolls, and mouse
       input is unaffected. */
    touch-action: none;
    /* And no long-press text-selection callout / magnifier over the
       picture — a held finger is a held mouse button, nothing more. */
    -webkit-user-select: none;
    user-select: none;
    -webkit-touch-callout: none;
    display: grid;
    /* The single track must be the stage's size, never the content's:
       an auto track grows to the canvas's intrinsic width (1920), which
       overflowed narrower windows and clipped the sides — the letterbox
       must come from object-fit inside the element, both axes. */
    grid-template-rows: minmax(0, 1fr);
    grid-template-columns: minmax(0, 1fr);
    place-items: center;
    /* The pinch-zoomed canvas may exceed the pane — clip, don't scroll. */
    overflow: hidden;
    background:
      radial-gradient(1200px 400px at 50% -10%, oklch(0.62 0.2 292 / 0.1), transparent),
      #0c0b14;
  }

  .file-drop {
    position: absolute;
    inset: 18px;
    z-index: 40;
    display: grid;
    place-content: center;
    justify-items: center;
    gap: 8px;
    padding: 28px;
    border: 2px dashed color-mix(in srgb, var(--green, #4fe29a) 82%, white);
    border-radius: 18px;
    color: white;
    background: color-mix(in srgb, #07150f 86%, transparent);
    box-shadow: inset 0 0 60px color-mix(in srgb, var(--green, #4fe29a) 16%, transparent);
    pointer-events: none;
    text-align: center;
  }
  .file-drop.busy {
    border-style: solid;
  }
  .file-drop-icon {
    display: grid;
    place-items: center;
    width: 54px;
    height: 54px;
    border-radius: 16px;
    color: #07150f;
    background: var(--green, #4fe29a);
    font-size: 30px;
    font-weight: 900;
  }
  .file-drop strong {
    font-size: 18px;
  }
  .file-drop span {
    color: #cbd8d1;
    font-size: 13px;
  }
  .stage.grabbing {
    cursor: crosshair;
  }
  /* The stage is focusable (so keys forward) but fills its pane — a focus
     ring around it would just be noise. */
  .stage:focus,
  .stage:focus-visible {
    outline: none;
  }
  .console.theater > .stage {
    background: #000;
  }
  .console.theater .live {
    border-radius: 0;
    box-shadow: none;
    /* Theater FILLS the monitor: the element takes the whole stage and
       the picture letterboxes inside it (bars where the aspect differs)
       instead of sitting at native size in a black field. Every
       virt→client mapping goes through contentBox() so the inset never
       skews the pointer, crosshair, or follow-cursor math; normPoint
       always computed it. Windowed keeps the element==content-box
       pattern below. */
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
  .live {
    /* Windowed: size the element to the video's OWN box (its intrinsic
       backing store scaled down to fit the stage), centered by the stage's
       place-items — the responsive-replaced-element pattern. Theater
       overrides to width/height:100% + object-fit (fills the monitor,
       bars inside the element); every virt→client mapping goes through
       contentBox() so the inset is handled in BOTH shapes — normPoint
       always computed it, and the crosshair/follow paths do now too. The
       pinch-zoom transform scales the same box; getBoundingClientRect
       keeps reporting the truth. */
    display: block;
    width: auto;
    height: auto;
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    user-select: none;
    -webkit-user-drag: none;
    border-radius: 4px;
    box-shadow: 0 6px 30px rgba(0, 0, 0, 0.5);
    transform-origin: center center;
  }
  /* Mounted (so the first frame has somewhere to land) but invisible
     until it does — the placeholder shows through. */
  .live.waiting {
    visibility: hidden;
    position: absolute;
  }
  /* A switch in progress: the retained previous picture, dimmed under
     the switching chip. Input never maps onto it (hasFrame gates). */
  .live.switching {
    opacity: 0.45;
    filter: saturate(0.55);
    transition: opacity 160ms ease;
  }
  .switching-chip {
    position: absolute;
    top: 0.8rem;
    left: 50%;
    transform: translateX(-50%);
    background: rgba(0, 0, 0, 0.7);
    border: 1px solid rgba(255, 255, 255, 0.18);
    color: #f0ecff;
    border-radius: var(--r-pill);
    padding: 0.35rem 0.9rem;
    font-size: 0.8rem;
    z-index: 3;
  }
  /* The aiming crosshair. Absolutely placed within the stage (never
     position:fixed — a windowed console's own transform would reparent that),
     centred on the aim point and moved by a transform set imperatively. Thin
     white lines with a dark halo so they read on any wallpaper; a small centre
     gap keeps the exact target pixel visible. */
  .crosshair {
    position: absolute;
    left: 0;
    top: 0;
    width: 24px;
    height: 24px;
    margin-left: -12px;
    margin-top: -12px;
    pointer-events: none;
    z-index: 8; /* over the video + host-status, under the zoom chip / bar */
    will-change: transform;
  }
  .crosshair svg {
    display: block;
    filter: drop-shadow(0 0 0.6px rgba(0, 0, 0, 0.9));
    opacity: 0.85;
  }
  .screen {
    width: calc(100% - 1.6rem);
    height: calc(100% - 1.6rem);
    border: 1px solid #2c2740;
    border-radius: var(--r-md);
    background: radial-gradient(900px 360px at 50% 30%, oklch(0.62 0.2 292 / 0.1), #0c0b14);
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    text-align: center;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.02);
  }
  .screen-glyph {
    font-size: 3.4rem;
    filter: drop-shadow(0 4px 12px var(--mc, oklch(0.62 0.2 292 / 0.4)));
    opacity: 0.92;
  }
  .screen-title {
    color: #efecf9;
    font-weight: 700;
    font-size: 1.05rem;
  }
  .screen-note {
    color: #9a93b8;
    font-size: 0.82rem;
    max-width: 28rem;
    line-height: 1.45;
    padding: 0 1rem;
  }
  /* The way home for a popped-out input. */
  .return-btn {
    margin-top: 0.4rem;
    border: 1px solid var(--line-strong);
    background: var(--accent);
    color: #fff;
    border-radius: var(--r-md);
    padding: 0.75rem 1.3rem;
    font-size: 1rem;
    font-weight: 700;
    cursor: pointer;
  }
  .return-btn:hover {
    filter: brightness(1.12);
  }
  /* The host's capture condition, bannered over a live stage when the
     stream stalls mid-session (display fell asleep, grabs failing). */
  .host-status {
    position: absolute;
    left: 50%;
    bottom: calc(1.2rem + env(safe-area-inset-bottom, 0px));
    transform: translateX(-50%);
    max-width: min(34rem, 85%);
    padding: 0.45rem 0.85rem;
    border-radius: 0.55rem;
    background: rgba(26, 23, 48, 0.92);
    border: 1px solid #2c2740;
    color: #c7c0e2;
    font-size: 0.8rem;
    line-height: 1.4;
    text-align: center;
    pointer-events: none;
  }
  /* The zoomed badge — bottom-left, out of the picture's way. */
  .zoom-chip {
    position: absolute;
    left: 0.7rem;
    bottom: calc(0.7rem + env(safe-area-inset-bottom, 0px));
    border: 1px solid rgba(255, 255, 255, 0.22);
    background: rgba(8, 8, 14, 0.85);
    color: #fff;
    border-radius: var(--r-pill);
    padding: 0.28rem 0.6rem;
    font-size: 0.74rem;
    font-weight: 650;
    cursor: pointer;
  }
  .zoom-chip:hover {
    background: rgba(0, 0, 0, 0.8);
  }

  /* ---- the control bar — horizontal on top (desktop), a vertical
     rail on the right (phone) ---- */
  .bar-anchor {
    position: absolute;
    z-index: 10;
    /* The anchor keeps the tray's full layout box even after the tray slides
       away. Never let that transparent box steal input from the remote video;
       only the visible controls below opt back into hit-testing. */
    pointer-events: none;
  }
  .kvmbar,
  .bar-tab,
  .kvmenu {
    pointer-events: auto;
  }
  /* Hugging the screen edge — the bar is a tray in the bezel, spending
     the least picture real estate a bar can. */
  .bar-anchor.h {
    top: env(safe-area-inset-top, 0px);
    left: 50%;
    width: max-content;
  }
  .bar-anchor.v {
    top: 50%;
    right: env(safe-area-inset-right, 0px);
  }
  .kvmbar {
    display: flex;
    align-items: center;
    gap: 2px;
    border-radius: 12px;
    border: 1px solid oklch(0.3 0.035 285 / 0.8);
    /* Near-opaque paint, NO backdrop-filter: the bar floats over a
       canvas repainting at stream rate, and a blur here makes the
       compositor re-blur that region every video frame — measured as a
       real desktop cost. Opaque-ish paint composites once. */
    background: oklch(0.19 0.028 285 / 0.96);
    box-shadow: var(--shadow-md);
    transition: transform 0.3s ease, opacity 0.3s ease;
    scrollbar-width: none;
  }
  .kvmbar::-webkit-scrollbar {
    display: none;
  }
  .bar-anchor.h .kvmbar {
    flex-direction: row;
    height: 2.7rem;
    padding: 0 0.4rem 0 0.15rem;
    /* Many monitors on a narrow window: the bar scrolls sideways rather
       than growing past the pane. */
    max-width: calc(100vw - 2rem);
    overflow-x: auto;
    overflow-y: hidden;
    /* Flush against the top edge: square where it meets the bezel. */
    border-top: none;
    border-radius: 0 0 12px 12px;
  }
  .bar-anchor.v .kvmbar {
    flex-direction: column;
    width: 2.7rem;
    /* Never taller than the pane — a landscape phone scrolls the rail
       instead of clipping its ends. */
    max-height: calc(100vh - 1.2rem);
    overflow-y: auto;
    overflow-x: hidden;
    padding: 0.15rem 0 0.4rem;
    /* Flush against the right edge. */
    border-right: none;
    border-radius: 12px 0 0 12px;
  }
  /* Hidden: slid out past the bar's own edge (safe-area included),
     leaving only the handle tab. */
  .bar-anchor.h .kvmbar.asleep {
    transform: translateY(calc(-100% - 1.5rem - env(safe-area-inset-top, 0px)));
    opacity: 0;
    pointer-events: none;
  }
  .bar-anchor.v .kvmbar.asleep {
    transform: translateX(calc(100% + 1.5rem + env(safe-area-inset-right, 0px)));
    opacity: 0;
    pointer-events: none;
  }
  /* The handle tab — the one and only hide/show control, pinned to the
     anchor so it never travels with the sliding bar. */
  .bar-tab {
    position: absolute;
    border: 1px solid oklch(0.3 0.035 285 / 0.8);
    /* Same rule as the bar: no per-frame compositor blur over live video. */
    background: oklch(0.19 0.028 285 / 0.92);
    color: #9a93b8;
    font-size: 0.85rem;
    line-height: 1;
    padding: 0;
    cursor: pointer;
  }
  .bar-anchor.h .bar-tab {
    top: 100%;
    left: 50%;
    transform: translateX(-50%);
    margin-top: 3px;
    width: 3.4rem;
    height: 1.15rem;
    border-radius: 0 0 8px 8px;
    transition: transform 0.3s ease, background 0.12s ease;
  }
  .bar-anchor.v .bar-tab {
    right: 100%;
    top: 50%;
    transform: translateY(-50%);
    margin-right: 3px;
    width: 1.15rem;
    height: 3.4rem;
    border-radius: 8px 0 0 8px;
    transition: transform 0.3s ease, background 0.12s ease;
  }
  /* Hidden: the tab rides along to hug the screen edge the bar left
     through — not float mid-air where the bar used to be. The offsets
     retrace the bar's thickness + the tab's own margin. */
  .bar-anchor.h .bar-tab.hidden {
    transform: translateX(-50%) translateY(calc(-2.7rem - 3px - env(safe-area-inset-top, 0px)));
  }
  .bar-anchor.v .bar-tab.hidden {
    transform: translateY(-50%) translateX(calc(2.7rem + 3px + env(safe-area-inset-right, 0px)));
  }
  .bar-tab:hover {
    color: #fff;
    background: oklch(0.24 0.03 285 / 0.9);
  }
  .grip {
    border: none;
    background: transparent;
    color: #6b6486;
    font-size: 0.9rem;
    line-height: 1;
    padding: 0.3rem 0.5rem;
    cursor: move;
    touch-action: none;
    border-radius: 8px;
    flex-shrink: 0;
  }
  .grip:hover {
    color: #9a93b8;
  }
  .vsep {
    background: oklch(0.3 0.035 285 / 0.7);
    flex-shrink: 0;
  }
  .bar-anchor.h .vsep {
    width: 1px;
    height: 1.3rem;
    margin: 0 0.2rem;
  }
  .bar-anchor.v .vsep {
    width: 1.3rem;
    height: 1px;
    margin: 0.2rem 0;
  }
  /* Desktop input buttons: the active source filled like the old tabs;
     a popped-out one hollow — an outline where its picture used to be. */
  .kbtn.input {
    flex-shrink: 0;
  }
  .kbtn.input.active {
    background: var(--accent);
    color: #fff;
  }
  .kbtn.input.hollow {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--accent);
    opacity: 0.8;
  }
  .kdef {
    position: absolute;
    top: 0;
    right: 1px;
    font-size: 0.5rem;
    color: var(--warn);
  }
  .kbtn {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 0.3rem;
    height: 2.15rem;
    min-width: 2.15rem;
    padding: 0 0.3rem;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: #c8c2e0;
    font-size: 0.95rem;
    line-height: 1;
    cursor: pointer;
    transition: background 0.12s ease;
  }
  .kbtn:hover,
  .kbtn.open {
    background: oklch(0.27 0.032 285 / 0.9);
  }
  /* A toggle that's live wears the session green — same lamp as the old
     footer toggles, one glance tells what's flowing. */
  .kbtn.on {
    background: oklch(0.8 0.17 150 / 0.16);
    box-shadow: inset 0 0 0 1px oklch(0.8 0.17 150 / 0.45);
  }
  .kbtn.end {
    /* A hair of extra distance from its neighbor — the one destructive
       button on the rail shouldn't share an edge under a thumb. */
    margin-top: 0.25rem;
  }
  .kbtn.end:hover {
    background: oklch(0.25 0.07 14);
    color: oklch(0.85 0.1 14);
  }
  .kbtn.warn::after {
    content: "";
    position: absolute;
    top: 3px;
    right: 3px;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--warn);
  }
  .presence {
    position: absolute;
    bottom: 2px;
    right: 2px;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #6b6486;
    border: 1px solid #14121f;
  }
  .presence.on {
    background: var(--ok);
  }
  /* ---- the menu — below the top bar, or left of the right rail ---- */
  .bar-anchor.h .kvmenu {
    top: calc(100% + 8px);
    left: 50%;
  }
  .bar-anchor.v .kvmenu {
    right: calc(100% + 1.5rem);
    top: 50%;
  }
  .kvmenu {
    position: absolute;
    width: max-content;
    min-width: 15rem;
    max-width: min(22rem, calc(100vw - 5.5rem - env(safe-area-inset-right, 0px)));
    max-height: min(26rem, calc(100vh - 1.5rem));
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 1px;
    background: oklch(0.18 0.027 285 / 0.98);
    border: 1px solid #322c47;
    border-radius: var(--r-md);
    box-shadow: var(--shadow-lg);
    padding: 0.35rem;
    animation: drop 0.12s ease;
  }
  .kvmenu.drives-menu {
    box-sizing: border-box;
    width: min(22rem, calc(100vw - 1rem));
    overflow-x: hidden;
  }
  @keyframes drop {
    from {
      opacity: 0;
      translate: 0 -4px;
    }
  }
  .mhead {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.5rem;
  }
  .mavatar {
    font-size: 1.3rem;
  }
  .mid {
    min-width: 0;
  }
  .mname {
    font-weight: 700;
    font-size: 0.92rem;
    color: #f3f1fb;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .msub {
    font-size: 0.7rem;
    color: #9a93b8;
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #6b6486;
    flex-shrink: 0;
  }
  .dot.on {
    background: var(--ok);
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.25);
  }
  .mchips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    padding: 0.15rem 0.5rem 0.35rem;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    font-size: 0.7rem;
    font-weight: 650;
    color: #d7d2ec;
    background: #14121f;
    border: 1px solid #322c47;
    border-radius: var(--r-pill);
    padding: 0.14rem 0.5rem;
  }
  .chip-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--mc);
  }
  .chip.stream {
    color: oklch(0.88 0.09 150);
    border-color: oklch(0.8 0.17 150 / 0.5);
  }
  .live-dot {
    background: var(--ok);
    animation: blink 1.6s ease-in-out infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0.35;
    }
  }
  .msep {
    height: 1px;
    background: #2c2740;
    margin: 0.25rem 0.2rem;
    flex-shrink: 0;
  }
  .mrow {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    border: none;
    background: transparent;
    color: #c8c2e0;
    text-align: left;
    font-size: 0.84rem;
    font-weight: 600;
    padding: 0.5rem 0.55rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .mrow:hover {
    background: #241f38;
    color: #fff;
  }
  .mrow.sel {
    color: var(--accent-ink);
  }
  .mrow.danger {
    color: oklch(0.82 0.1 14);
  }
  .mrow.danger:hover {
    background: oklch(0.25 0.07 14);
  }
  .micon {
    font-size: 1rem;
    width: 1.4rem;
    text-align: center;
    flex-shrink: 0;
  }
  .power-copy {
    min-width: 0;
    display: grid;
    gap: 0.08rem;
  }
  .power-copy strong {
    color: inherit;
    font-size: 0.84rem;
  }
  .power-copy small {
    color: #8f88aa;
    font-size: 0.68rem;
    font-weight: 500;
  }
  .mlabel {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .mdef {
    color: var(--warn);
    font-size: 0.72rem;
    flex-shrink: 0;
  }
  .mout {
    color: var(--accent-2, #9be3ff);
    font-size: 0.72rem;
    flex-shrink: 0;
  }
  .mcheck {
    margin-left: auto;
    color: var(--accent-ink);
    font-size: 0.8rem;
    min-width: 1rem;
    text-align: right;
    flex-shrink: 0;
  }
  .pip {
    margin-left: auto;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #4a4366;
    flex-shrink: 0;
  }
  .pip.lit {
    background: var(--ok);
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.25);
  }
  .mrow-wrap {
    display: flex;
    align-items: center;
    gap: 2px;
  }
  .mrow-wrap .mrow {
    flex: 1;
    min-width: 0;
  }
  .mside {
    flex-shrink: 0;
    width: 1.9rem;
    height: 1.9rem;
    border: 1px solid #443d63;
    background: #241f38;
    color: #c8c2e0;
    border-radius: var(--r-sm);
    font-size: 0.8rem;
    line-height: 1;
    cursor: pointer;
  }
  .mside:hover {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }
  .mempty {
    font-size: 0.8rem;
    color: #8b84a8;
    padding: 0.6rem 0.55rem;
  }

  /* ---- the video menu's guts ---- */
  .mode-row {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.5rem 0.55rem 0.4rem;
  }
  /* Keep the "Mode" label vertically aligned with the pill when collapsed,
     even though the row now top-aligns for the inline dropdown. */
  .mode-row .mode-label {
    padding-top: 0.32rem;
  }
  .mode-label {
    font-size: 0.72rem;
    color: #8a83a6;
    flex-shrink: 0;
  }
  .eff-wrap {
    padding: 0.2rem 0.55rem 0.4rem;
  }
  .vrow {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    border: none;
    background: transparent;
    color: #c8c2e0;
    font-size: 0.8rem;
    font-weight: 600;
    text-align: left;
    padding: 0.42rem 0.55rem 0.42rem 2.45rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .vrow:hover {
    background: #241f38;
  }
  /* A dial off Auto reads as deliberately set. */
  .vrow.tuned .vval {
    color: var(--accent-ink);
  }
  .vval {
    margin-left: auto;
    color: #9a93b8;
    font-weight: 650;
    white-space: nowrap;
  }
  .vchev {
    color: #6b6486;
    font-size: 0.66rem;
    flex-shrink: 0;
  }
  .vopts {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    padding: 0.2rem 0.55rem 0.45rem 2.45rem;
  }
  .vopt {
    border: 1px solid #322c47;
    background: #14121f;
    color: #c8c2e0;
    border-radius: var(--r-pill);
    padding: 0.3rem 0.6rem;
    font-size: 0.72rem;
    font-weight: 650;
    cursor: pointer;
    white-space: nowrap;
  }
  .vopt:hover {
    border-color: var(--accent);
  }
  .vopt.sel {
    background: var(--accent-soft);
    border-color: var(--accent);
    color: var(--accent-ink);
  }
  .zoom-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.4rem 0.55rem;
  }
  .zlabel {
    font-size: 0.8rem;
    font-weight: 600;
    color: #c8c2e0;
    margin-right: auto;
  }
  .zbtn {
    width: 1.8rem;
    height: 1.8rem;
    border: 1px solid #322c47;
    background: #14121f;
    color: #c8c2e0;
    border-radius: var(--r-sm);
    font-size: 1rem;
    line-height: 1;
    cursor: pointer;
  }
  .zbtn:hover {
    border-color: var(--accent);
  }
  .znow {
    min-width: 3rem;
    text-align: center;
    font-size: 0.78rem;
    font-weight: 650;
    color: #d7d2ec;
  }
  .zreset {
    border: none;
    background: transparent;
    color: var(--accent-ink);
    font-size: 0.76rem;
    font-weight: 650;
    cursor: pointer;
    padding: 0.3rem 0.3rem;
  }
  .mstats {
    font-size: 0.72rem;
    color: oklch(0.88 0.09 150);
    padding: 0.15rem 0.55rem 0.4rem;
  }

  /* Small screens (portrait phones by width, landscape phones by
     height): the secondary buttons leave the rail for the session menu,
     so the rail always fits with air around it. */
  @media (max-width: 700px), (max-height: 500px) {
    .kbtn.slim,
    .vsep.slim {
      display: none;
    }
  }
</style>
