<script lang="ts">
  // One video stream in its own OS window — a console input or a room
  // share lifted out of its tab so it can live beside other work (or on
  // another monitor), with its own controls. The store already wired or
  // adopted the stream (`initVideoPopout`): a `cap:` popout owns a real
  // route it tears down on close; a `share:` popout merely watches the
  // sender's route, so closing it costs nothing but the window.
  //
  // The stage always asks the backend to decode (`decode: true` — the
  // console ladder's universal bottom rung, the same plane room tiles
  // ride), so it blits RGBA / JPEG frames and works in every webview.
  // Hover reveals the controls: the quality pills for a stream this
  // window owns, and fullscreen in the corner everyone looks for it.
  // Input forwards over the picture exactly when a live control route to
  // that machine exists (the console's toggle, a room's "share control")
  // — injection stays gated on the far side regardless.
  //
  // Closing the window — the OS ✕, or a tab's "Return video here" ask
  // arriving over the local lane — runs the same teardown, announces
  // `closed`, and the tab that popped it re-wires itself.
  import { onMount } from "svelte";
  import { Fsr1 } from "../fsr1";
  import ModePill from "./ModePill.svelte";
  import EffectiveReadout from "./EffectiveReadout.svelte";
  import { makeKeyForwarder } from "../input-keys";
  import { makeRelativeMotionForwarder } from "../relative-motion";
  import { app } from "../store.svelte";
  import {
    clientLog,
    focusThisWindow,
    isWindowFullscreen,
    onThisWindowClose,
    sendInput,
    setMacRelativePointerCapture,
    toggleWindowFullscreen,
    tuneRoute,
    videoWindowDecoderPreference,
    watchVideo,
    watchVideoStatus,
    type StreamTune,
    type VideoHostStatus,
  } from "../tauri";
  import { mediaColor, originIcon, type Capability, type InputAction } from "../types";

  const popKey = $derived(app.videoPopoutKey);
  const capId = $derived(popKey?.startsWith("cap:") ? popKey.slice(4) : null);
  /** The capability this window streams from — the input itself for a
   *  `cap:` popout, the share's source for a `share:` one. */
  const sourceCap = $derived.by((): Capability | null => {
    if (capId) return app.capability(capId) ?? null;
    const route = app.catalog.routes.find((r) => r.id === app.videoPopoutLive);
    return (route && app.capabilityForDisplay(route.from)) ?? null;
  });
  const sourceMachine = $derived(
    sourceCap ? app.machineByAnyId(sourceCap.node) : undefined,
  );
  const kvmSource = $derived(app.isKvm(sourceMachine));
  const decoderPreference = videoWindowDecoderPreference();
  /** Whether this window owns its route (a console input it wired) —
   *  what gates the quality pills. A watched room share is the sender's
   *  stream to shape. */
  const ownsRoute = $derived(!!capId);
  const controlRoute = $derived(sourceCap ? app.controlRouteTo(sourceCap.node) : null);
  /** Forwarding is live only over a *desktop* picture — pointer
   *  coordinates normalize onto the streamed frame, and only a screen's
   *  frame maps back onto the remote desktop (a camera feed has no
   *  sensible mapping, so it stays a pure viewing surface). */
  const controlActive = $derived(!!controlRoute && sourceCap?.media === "display");
  // Which remote monitor coordinates normalize over (a `screen:<id>`
  // input); undefined = the primary, and what camera streams send.
  const controlScreen = $derived.by(() => {
    const m = capId?.match(/:screen:(\d+)$/);
    return m ? Number(m[1]) : undefined;
  });
  /** The stream's live negotiation state — what tells "connecting" from
   *  "ended" (peer gone, sender stopped sharing) on the placeholder. */
  const routeState = $derived(
    app.videoPopoutLive ? (app.routeStates[app.videoPopoutLive]?.state ?? null) : null,
  );
  const ended = $derived(routeState === "torn_down" || routeState === "rejected");

  let canvasEl = $state<HTMLCanvasElement | null>(null);

  // ---- FSR1-style upscale (edge-adaptive, text-biased sharpening) ----
  // When the decoded stream is smaller than the display box, the plain
  // path is the browser's bilinear stretch — soft edges, smeared text.
  // The overlay canvas runs the WebGL2 EASU+RCAS pipeline instead,
  // engaged automatically at >1.1× upscale and killable from the Scale
  // pill. Pointer events stay on the base canvas (the normalizer and
  // pointer lock never see the overlay). This is also Reach mode's
  // presentation layer: a deliberately low-res stream over a starved
  // link gets rebuilt here.
  let glCanvasEl = $state<HTMLCanvasElement | null>(null);
  let fsrOn = $state(localStorage.getItem("ams.fsrOff") !== "1");
  let fsrShow = $state(false);
  let fsrEngine: Fsr1 | null = null;
  let fsrDead = false;
  function presentFsr(w: number, h: number) {
    const base = canvasEl;
    const gl = glCanvasEl;
    if (!base || !gl) return;
    // The base element now fills the window with the picture letterboxed
    // inside it (object-fit: contain) — compute that content box, because
    // the FSR pass stretches its whole source onto its whole target: the
    // overlay must be exactly the picture's box or the bars get "scaled
    // into" the image. Same fit math as `norm`.
    const dpr = window.devicePixelRatio || 1;
    const bw = base.clientWidth;
    const bh = base.clientHeight;
    if (!bw || !bh || !w || !h) {
      fsrShow = false;
      return;
    }
    const scale = Math.min(bw / w, bh / h);
    const cw = w * scale;
    const ch = h * scale;
    const dw = Math.round(cw * dpr);
    const dh = Math.round(ch * dpr);
    if (!(fsrOn && !fsrDead && dw > w * 1.1)) {
      fsrShow = false;
      return;
    }
    try {
      fsrEngine ??= new Fsr1(gl);
      // Explicit CSS size = the content box; `inset: 0; margin: auto`
      // centers it over the letterboxed picture. The buffer is dpr-exact
      // underneath, so hi-DPI monitors get a crisp 1:1 present.
      gl.style.width = `${cw}px`;
      gl.style.height = `${ch}px`;
      fsrEngine.render(base, w, h, dw, dh);
      fsrShow = true;
    } catch (e) {
      console.warn("FSR upscale unavailable; plain scaling:", e);
      fsrDead = true;
      fsrShow = false;
    }
  }
  function toggleFsr() {
    fsrOn = !fsrOn;
    localStorage.setItem("ams.fsrOff", fsrOn ? "0" : "1");
    if (!fsrOn) fsrShow = false;
  }
  // Fullscreen/resize refit: the base canvas rescales via CSS the instant
  // the window changes shape, but the FSR overlay renders at fixed pixel
  // sizes — re-present on any stage resize so entering fullscreen snaps
  // to the new fit immediately, even on a static stream where no new
  // frame would otherwise trigger a paint.
  $effect(() => {
    const stage = stageEl;
    if (!stage) return undefined;
    const ro = new ResizeObserver(() => {
      if (hasFrame && frameW && frameH) presentFsr(frameW, frameH);
    });
    ro.observe(stage);
    return () => ro.disconnect();
  });
  // The role=application surface. Key forwarding lives on this element (not
  // `<svelte:window>`) so it fires whenever the element holds focus — and
  // hovering pins that focus (below), the reliable way to make a secondary
  // window's keyboard reach the remote where window-level setFocus alone
  // doesn't push document focus into the webview on hover (WebKitGTK).
  let stageEl = $state<HTMLElement | null>(null);
  let hasFrame = $state(false);
  let frameW = $state(0);
  let frameH = $state(0);
  let fps = $state(0);
  let transport = $state("");
  let frameCount = 0;
  // Received bitrate (viewer truth): bytes seen this second → Mbps. The
  // effective panel shows it beside the sender's target when co-located.
  let recvMbps = $state(0);
  let inBytes = 0;
  let hostStatus = $state<VideoHostStatus | null>(null);
  let fullscreen = $state(false);

  async function flipFullscreen() {
    fullscreen = await toggleWindowFullscreen();
    // The OS fullscreen transition blurs the stage (firing keys.releaseAll via
    // onblur); re-pin focus so keyboard forwarding resumes without a click back
    // into the picture.
    if (controlActive) {
      await focusThisWindow();
      stageEl?.focus({ preventScroll: true });
    }
  }

  // The OS can flip fullscreen without our ⛶ (Win+Up, a shell gesture) —
  // and pointer lock engages off this flag, which is exactly the field's
  // "fullscreen game doesn't capture the cursor": the flag said windowed,
  // so the click never locked. Re-query the REAL state on every resize.
  $effect(() => {
    let alive = true;
    const sync = () =>
      void isWindowFullscreen().then((v) => {
        if (alive) fullscreen = v;
      });
    sync();
    window.addEventListener("resize", sync);
    return () => {
      alive = false;
      window.removeEventListener("resize", sync);
    };
  });

  // ---- quality pills (streams this window owns) ----------------------
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
  let tune = $state<StreamTune>({});
  let openPill = $state<"res" | "fps" | "rate" | "live" | null>(null);
  const pillLabel = (choices: PillChoice[], v: number | null | undefined) =>
    choices.find((c) => c.value === (v ?? null))?.label ?? "Auto";
  function pick(field: keyof StreamTune, v: number | null) {
    tune = { ...tune, [field]: v ?? undefined };
    openPill = null;
    if (app.videoPopoutLive) void tuneRoute(app.videoPopoutLive, tune);
  }

  // Mode is the headline control now: Balanced is the stability-first
  // default; Game asks the streamer for the latency-first posture
  // (gradual intra-refresh instead of keyframe walls, 60 fps floor
  // off-LAN); Studio is the LAN fidelity mode (high-bitrate
  // quality-first — the streamer degrades it to Balanced off-LAN). The
  // Res/FPS/Rate pills stay as expert overrides on top. The legacy
  // `game` bool rides along so hosts that predate the tri-state still
  // honor the Game step.
  // The Mode control (Balanced/Game/Studio/Studio · LL) + its bandwidth
  // warning live in the shared ModePill component so the console strip
  // and this bar are the same element. It hands back the resolved wire
  // values; we fold them into the route's tune.
  function applyModeWire(
    wireMode: "game" | "studio" | "studio-lossless" | undefined,
    gameFlag: boolean | undefined,
  ) {
    tune = { ...tune, mode: wireMode, game: gameFlag };
    openPill = null;
    if (app.videoPopoutLive) void tuneRoute(app.videoPopoutLive, tune);
  }

  onMount(() => {
    let unlistenClose: (() => void) | undefined;
    const fpsTimer = setInterval(() => {
      fps = frameCount;
      frameCount = 0;
      recvMbps = (inBytes * 8) / 1e6;
      inBytes = 0;
    }, 1000);
    // The OS chrome's ✕ runs the same teardown as a "Return video here"
    // ask — the close is held until the route teardown is on the wire.
    // Keys still held ride out first: the control route outlives this
    // window, so without the lift the far machine keeps the modifier.
    void onThisWindowClose(() => {
      keys.releaseAll();
      void app.closeVideoPopout();
    }).then((u) => (unlistenClose = u));
    return () => {
      unlistenClose?.();
      clearInterval(fpsTimer);
    };
  });

  // A share whose sender stopped (route torn down / gone) has nothing to
  // return to a tab — close this window after the note has had a beat.
  $effect(() => {
    if (ownsRoute || !ended) return undefined;
    const t = setTimeout(() => void app.closeVideoPopout(), 2500);
    return () => clearTimeout(t);
  });

  // Follow the stream: watch its packets while the window lives. The
  // rewatch counter re-runs this when a console window booting briefly
  // claimed the same route's watch slot (claims replace each other; the
  // census tells it to back off, and we re-assert).
  $effect(() => {
    const route = app.videoPopoutLive;
    void app.videoPopoutRewatch;
    hasFrame = false;
    // Clear the prior stream's frame dims so `norm` can't letterbox pointer
    // events against a stale aspect during a re-wire (see Console.svelte).
    frameW = 0;
    frameH = 0;
    hostStatus = null;
    fps = 0;
    transport = "";
    frameCount = 0;
    recvMbps = 0;
    inBytes = 0;
    if (!route) return;
    let cancelled = false;
    let unwatch: (() => void) | undefined;
    let unwatchStatus: (() => void) | undefined;

    void watchVideoStatus(route, (s) => {
      if (cancelled) return;
      hostStatus = s.state === "ok" ? null : s;
    }).then((u) => {
      if (cancelled) u();
      else unwatchStatus = u;
    });

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
      frameCount += 1;
      presentFsr(w, h);
    };

    // MJPEG paint slot, latest-wins — the same supersede the console uses.
    // Each JPEG is a complete picture, so painting history is pure lag: the
    // old serial promise chain decoded and painted EVERY frame in arrival
    // order, which is where "always catching up" lived whenever decode ran
    // behind the wire in a popped-out stream.
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
            paint(bmp.width, bmp.height, (ctx) => ctx.drawImage(bmp, 0, 0));
            bmp.close();
          } catch {
            // A torn frame decodes as nothing; the next one stands alone.
          }
        }
      } finally {
        jpegPainting = false;
      }
    };

    void watchVideo(
      route,
      (f) => {
        if (cancelled) return;
        inBytes += f.data.byteLength;
        if (f.kind === "raw") {
          transport = "H.264";
          if (f.data.byteLength !== f.width * f.height * 4) return;
          const pixels = new Uint8ClampedArray(f.data.buffer, f.data.byteOffset, f.data.byteLength);
          const img = new ImageData(pixels, f.width, f.height);
          paint(f.width, f.height, (ctx) => ctx.putImageData(img, 0, 0));
        } else if (f.kind === "jpeg") {
          transport = "MJPEG";
          pendingJpeg = new Blob([f.data], { type: "image/jpeg" });
          void paintNewestJpeg();
        }
        // h264 never arrives here — decode: true means the backend hands
        // this window ready-to-paint frames.
      },
      { decode: true, decoder: decoderPreference },
    ).then((u) => {
      if (cancelled) u();
      else unwatch = u;
    });

    return () => {
      cancelled = true;
      unwatch?.();
      unwatchStatus?.();
    };
  });

  // ---- input forwarding (only while a control route exists) ----------
  // Coordinates normalize 0..1 over the streamed frame's content box —
  // the picture inside the letterbox — exactly like the console stage.
  function norm(e: PointerEvent | WheelEvent): { x: number; y: number } | null {
    const c = canvasEl;
    // Gate on hasFrame: during a re-wire the canvas is `.waiting`
    // (visibility:hidden; position:absolute) and frameW/frameH are cleared, so
    // don't map pointer events until the first fresh frame lands.
    if (!c || !hasFrame || !frameW || !frameH) return null;
    const r = c.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return null;
    const scale = Math.min(r.width / frameW, r.height / frameH);
    const cw = frameW * scale;
    const ch = frameH * scale;
    const ox = r.left + (r.width - cw) / 2;
    const oy = r.top + (r.height - ch) / 2;
    const x = (e.clientX - ox) / cw;
    const y = (e.clientY - oy) / ch;
    if (x < 0 || x > 1 || y < 0 || y > 1) return null;
    return { x, y };
  }

  function send(action: InputAction) {
    if (controlRoute) void sendInput(controlRoute, action);
  }

  // The KVM rule: with control live, the window your keyboard should reach is
  // the one you're working in — claimed without a click in between, because a
  // click would go to the *remote*. Raise the OS window AND pin keyboard focus
  // on the surface element: `setFocus()` alone doesn't reliably push document
  // focus into the webview without a click, so without the element focus the
  // key handlers (now on `stageEl`) never fire and only the already-focused
  // window — usually the main one — could drive.
  //
  // Callers decide *when*; a bare hover is deliberately not one of the times
  // (see `onPointerMove`). Gated here on the document not already holding
  // focus so it never steals from an open pill menu once this window is active.
  function claimFocus() {
    if (document.hasFocus()) return;
    void focusThisWindow();
    stageEl?.focus({ preventScroll: true });
  }

  // ---- pointer lock (fullscreen mouse capture) -----------------------
  // Game-mode aiming: in fullscreen control, a click captures the mouse
  // and RELATIVE deltas stream to the host (`mouse_move_rel`) — FPS
  // camera control instead of a cursor pinned to the window edge. Esc
  // (the browser's own gesture) releases it; leaving fullscreen or
  // control drops it too.
  let pointerLocked = $state(false);
  let nativePointerLocked = $state(false);
  // Relative mouse — the console's switch, in the popped-out window. See the
  // note there: a remote game captures its own cursor and wants deltas, and
  // whether *this* window is fullscreen has nothing to do with it. A popped
  // out monitor is if anything the likelier place to want it, since the game
  // is on one screen and the rest of the desk stays usable.
  let relativeMouse = $state(false);
  function lockChanged() {
    pointerLocked = document.pointerLockElement === stageEl;
    lockedMotion.reset();
    if (pointerLocked && nativePointerLocked) void releaseNativePointerLock(false);
    if (!pointerLocked && !nativePointerLocked) relativeMouse = false;
  }
  const wantsCapture = $derived((fullscreen || relativeMouse) && controlActive && !kvmSource);
  let pointerLockPending = false;
  async function maybePointerLock(relativeIntent = relativeMouse) {
    const target = stageEl;
    const captureWanted =
      (fullscreen || relativeIntent) && controlActive && !kvmSource;
    if (!captureWanted || pointerLocked || nativePointerLocked || pointerLockPending || !target) return;

    pointerLockPending = true;
    try {
      // A click can reach a secondary/fullscreen webview before its native
      // window has focus. Ask native focus without awaiting its IPC promise:
      // awaiting here consumes the click's transient user activation before
      // WebKit sees requestPointerLock.
      target.focus({ preventScroll: true });
      if (!document.hasFocus()) {
        void focusThisWindow();
        target.focus({ preventScroll: true });
      }
      if (target !== stageEl || !target.isConnected) {
        return;
      }
      await target.requestPointerLock();
      setTimeout(() => {
        if (relativeMouse && !pointerLocked && !nativePointerLocked) {
          void activateNativePointerLock(target);
        }
      }, 150);
    } catch (error) {
      // Losing a focus race is recoverable: leave the mode armed for the next
      // click and keep the browser rejection out of the global error boundary.
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
      if (pointerLocked || !relativeMouse || !controlActive || kvmSource) {
        await setMacRelativePointerCapture(false);
        return;
      }
      nativePointerLocked = true;
      lockedMotion.reset();
      clientLog("[relative-mouse] macOS native pointer capture active in video popout");
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
    relativeMouse = true;
    // The click is the user gesture pointer lock needs — grab on it.
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
    // Losing control (or the desktop under it) releases the capture; leaving
    // fullscreen releases it only when relative mouse isn't holding it.
    if (!wantsCapture) {
      if (pointerLocked) document.exitPointerLock();
      if (nativePointerLocked) void releaseNativePointerLock();
    }
  });

  let lastMoveAt = 0;
  const lockedMotion = makeRelativeMotionForwarder(
    (dx, dy) => {
      send({ kind: "mouse_move_rel", dx, dy });
    },
    () => (nativePointerLocked ? window.devicePixelRatio : 1),
  );
  function onLockedMouseMove(e: MouseEvent) {
    if ((!pointerLocked && !nativePointerLocked) || !controlActive || kvmSource) return;
    lockedMotion.forward(e, "mouse");
  }
  function onPointerMove(e: PointerEvent) {
    if (!controlActive) return;
    if ((pointerLocked || nativePointerLocked) && !kvmSource) {
      lockedMotion.forward(e, "pointer");
      return;
    }
    // Only a drag carries focus — see the console's pointer-move note. A
    // popped-out monitor sitting on a second display is exactly what a bare
    // hover crosses by accident, so hovering it must not raise it; a held
    // button (dragging across screens) still must.
    if (e.buttons !== 0) claimFocus();
    const now = performance.now();
    if (now - lastMoveAt < 16) return;
    const p = norm(e);
    if (!p) return;
    lastMoveAt = now;
    send({ kind: "mouse_move", ...p, screen: controlScreen });
  }
  function onPointerButton(e: PointerEvent, down: boolean) {
    if (!controlActive) return;
    // A click is the most reliable focus pin — land it on the stage so keys
    // forward even if the cursor was last over a hover-bar button.
    if (down) stageEl?.focus({ preventScroll: true });
    if (down) void maybePointerLock();
    if ((pointerLocked || nativePointerLocked) && !kvmSource) {
      e.preventDefault();
      send({ kind: "mouse_button", button: e.button, down });
      return;
    }
    const p = norm(e);
    if (!p) return;
    e.preventDefault();
    send({ kind: "mouse_move", ...p, screen: controlScreen });
    send({ kind: "mouse_button", button: e.button, down });
  }
  function onWheel(e: WheelEvent) {
    if (!controlActive || !norm(e)) return;
    e.preventDefault();
    const lines = e.deltaMode === 1 ? 1 : 1 / 40;
    send({ kind: "wheel", dx: e.deltaX * lines, dy: e.deltaY * lines });
  }
  // Key forwarding with the bookkeeping combinations need: the physical
  // `code` rides along, and keys still held when this window loses focus
  // are lifted in a burst — otherwise the far machine keeps a stuck
  // modifier.
  const keys = makeKeyForwarder(send);

  // Control forwarding — bound to the focusable surface element, so it only
  // fires while this window actually holds keyboard focus (the KVM rule). With
  // control granted every key belongs to the far machine.
  function onControlKey(e: KeyboardEvent, down: boolean) {
    if (!controlActive) return;
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
    keys.onKey(e, down);
  }

  // No-control keys ride the window so they work without the surface being
  // focused: Escape leaves fullscreen (the hover ⛶ does it while driving).
  function onWindowKey(e: KeyboardEvent) {
    if (nativePointerLocked && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      nativeEscapeHeld = true;
      void releaseNativePointerLock();
      return;
    }
    if (controlActive) return;
    if (e.key === "Escape" && fullscreen) void flipFullscreen();
  }
</script>

<svelte:window
  onkeydown={onWindowKey}
  onmousemove={onLockedMouseMove}
  onblur={() => void releaseNativePointerLock()}
  onclick={() => (openPill = null)}
/>

<!-- role=application: with control granted this is a remote-desktop
     surface — every pointer/key goes to the far machine. It's focusable
     only while driving, so keys forward from here and nowhere else. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  bind:this={stageEl}
  class="popout"
  class:driving={controlActive}
  role="application"
  aria-label="Popped-out video{sourceCap ? ` — ${sourceCap.label}` : ''}"
  tabindex={controlActive ? 0 : -1}
  onpointermove={onPointerMove}
  onpointerdown={(e) => onPointerButton(e, true)}
  onpointerup={(e) => onPointerButton(e, false)}
  onwheel={onWheel}
  onkeydown={(e) => onControlKey(e, true)}
  onkeyup={(e) => onControlKey(e, false)}
  onblur={() => keys.releaseAll()}
  oncontextmenu={(e) => controlActive && e.preventDefault()}
>
  <canvas bind:this={canvasEl} class:waiting={!hasFrame}></canvas>
  <canvas bind:this={glCanvasEl} class="fsr" class:active={fsrShow && hasFrame}></canvas>
  {#if !hasFrame}
    <div class="placeholder" style="--mc: {sourceCap ? mediaColor(sourceCap.media) : '#888'}">
      <div class="glyph">{sourceCap ? originIcon(sourceCap.origin, sourceCap.media) : "🪟"}</div>
      <div class="title">{sourceCap?.label ?? "Video"}</div>
      <div class="note">
        {#if ended}
          The stream ended{ownsRoute ? " — the machine may be offline" : " — the sender stopped sharing"}.
        {:else if hostStatus?.detail}
          {hostStatus.detail}
        {:else if hostStatus}
          The remote capture isn't producing frames yet ({hostStatus.state.replaceAll("_", " ")})…
        {:else}
          Connecting…
        {/if}
      </div>
    </div>
  {:else if ended}
    <div class="ended-banner">
      The stream ended{ownsRoute ? "" : " — the sender stopped sharing"}.
    </div>
  {:else if hostStatus}
    <div class="ended-banner">{hostStatus.detail ?? hostStatus.state.replaceAll("_", " ")}</div>
  {/if}

  <!-- Hover controls: chip + pills along the bottom, fullscreen in the
       corner everyone looks for it. Pointer events stop here so control
       forwarding never sees them. -->
  <footer class="hover-bar">
    {#if hasFrame}
      <span class="chip"
        ><span class="chip-dot"></span>{frameW}×{frameH} · {fps} fps{transport
          ? ` · ${transport}`
          : ""}{recvMbps ? ` · ${recvMbps >= 10 ? recvMbps.toFixed(0) : recvMbps.toFixed(1)} Mbps` : ""}</span
      >
      <span class="pill-wrap">
        <button
          class="pill"
          class:tuned={openPill === "live"}
          title="What this stream is actually doing — requested picks vs the live effective reality"
          onclick={(e) => {
            e.stopPropagation();
            openPill = openPill === "live" ? null : "live";
          }}
        >
          Live ▾
        </button>
        {#if openPill === "live"}
          <div class="pill-menu wide" role="group" aria-label="Effective stream dials">
            <EffectiveReadout
              routeId={app.videoPopoutLive}
              requested={tune}
              w={frameW}
              h={frameH}
              {fps}
              {transport}
              mbps={recvMbps}
            />
          </div>
        {/if}
      </span>
    {/if}
    {#if controlActive}
      <span class="chip ctl" title="A live control route lets you click and type here — hovering this window is what aims your keyboard at it">🕹 control</span>
    {/if}
    <span class="spacer"></span>
    {#if ownsRoute}
      {#snippet pillMenu(
        key: "res" | "fps" | "rate",
        name: string,
        choices: PillChoice[],
        current: number | null | undefined,
        field: keyof StreamTune,
      )}
        <span class="pill-wrap">
          <button
            class="pill"
            class:tuned={(current ?? null) !== null}
            onpointerdown={(e) => e.stopPropagation()}
            onpointerup={(e) => e.stopPropagation()}
            onclick={(e) => {
              e.stopPropagation();
              openPill = openPill === key ? null : key;
            }}
          >
            {name} · {pillLabel(choices, current)} ▾
          </button>
          {#if openPill === key}
            <div class="pill-menu" role="menu">
              {#each choices as c (c.label)}
                <button
                  class="pill-item"
                  class:sel={(current ?? null) === c.value}
                  onpointerdown={(e) => e.stopPropagation()}
                  onpointerup={(e) => e.stopPropagation()}
                  onclick={(e) => {
                    e.stopPropagation();
                    pick(field, c.value);
                  }}
                >
                  {c.label}
                </button>
              {/each}
            </div>
          {/if}
        </span>
      {/snippet}
      <ModePill
        mode={tune.mode}
        game={tune.game}
        onapply={applyModeWire}
        experimental={app.labsTier}
        onexperimental={(on) => app.setLabsTier(on)}
      />
      <button
        class="pill"
        class:tuned={fsrShow}
        title="Edge-adaptive upscaling with text-biased sharpening when the stream is smaller than the window — replaces the browser's soft bilinear stretch"
        onpointerdown={(e) => e.stopPropagation()}
        onpointerup={(e) => e.stopPropagation()}
        onclick={(e) => {
          e.stopPropagation();
          toggleFsr();
        }}
      >
        Scale · {fsrOn ? (fsrShow ? "FSR" : "Auto") : "Off"}
      </button>
      {@render pillMenu("res", "Res", RES_CHOICES, tune.maxEdge, "maxEdge")}
      {@render pillMenu("fps", "FPS", FPS_CHOICES, tune.fps, "fps")}
      {@render pillMenu("rate", "Rate", RATE_CHOICES, tune.bitrate, "bitrate")}
    {/if}
    {#if controlActive && !kvmSource}
      <!-- Relative mouse: deltas for a remote app that captures its own
           cursor. Esc gives the pointer back. -->
      <button
        class="corner-btn"
        class:on={pointerLocked || nativePointerLocked}
        title={pointerLocked || nativePointerLocked
          ? "Release the mouse (Esc)"
          : relativeMouse
            ? "Relative mouse is armed — click the screen to capture"
          : "Capture the mouse — relative motion for a fullscreen app or game"}
        aria-label="Relative mouse"
        aria-pressed={pointerLocked || nativePointerLocked}
        onpointerdown={(e) => e.stopPropagation()}
        onpointerup={(e) => e.stopPropagation()}
        onclick={(e) => {
          e.stopPropagation();
          toggleRelativeMouse();
        }}>🎯</button
      >
    {/if}
    <button
      class="corner-btn"
      title={fullscreen ? `Exit fullscreen${controlActive ? "" : " (Esc)"}` : "Fullscreen"}
      aria-label={fullscreen ? "Exit fullscreen" : "Fullscreen"}
      onpointerdown={(e) => e.stopPropagation()}
      onpointerup={(e) => e.stopPropagation()}
      onclick={(e) => {
        e.stopPropagation();
        void flipFullscreen();
      }}>{fullscreen ? "⤡" : "⛶"}</button
    >
  </footer>
</div>

<style>
  .popout {
    position: relative;
    height: 100vh;
    background: #000;
    display: grid;
    grid-template-rows: minmax(0, 1fr);
    grid-template-columns: minmax(0, 1fr);
    place-items: center;
    overflow: hidden;
  }
  .popout.driving {
    cursor: crosshair;
  }
  /* The surface is focusable (so keys forward), but it fills the window —
     a focus ring at the window edge would just be noise. */
  .popout:focus,
  .popout:focus-visible {
    outline: none;
  }
  canvas {
    /* Fill the window and letterbox INSIDE the element: fullscreen on a
       monitor larger than the stream scales the picture UP to fit (bars
       where the aspect differs) instead of leaving it at native size in
       a black field. The old auto-size-with-max-caps pattern only ever
       shrank. `norm` computes the object-fit inset itself (same math as
       Console's normPoint), so pointer mapping is inset-safe; the FSR
       overlay is pinned to the computed content box in presentFsr. */
    display: block;
    width: 100%;
    height: 100%;
    object-fit: contain;
    user-select: none;
    -webkit-user-drag: none;
  }
  canvas.waiting {
    visibility: hidden;
    position: absolute;
  }
  /* The FSR overlay sits exactly on the fitted video box; input never
     touches it (the pointer normalizer and pointer lock live on the base
     canvas underneath). */
  canvas.fsr {
    display: none;
    position: absolute;
    inset: 0;
    margin: auto;
    pointer-events: none;
  }
  canvas.fsr.active {
    display: block;
  }
  .placeholder {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    color: #9a93b8;
    text-align: center;
    padding: 2rem;
  }
  .glyph {
    font-size: 2.6rem;
  }
  .title {
    font-size: 1rem;
    font-weight: 700;
    color: #d7d2ec;
  }
  .note {
    font-size: 0.84rem;
    max-width: 28rem;
    line-height: 1.5;
  }
  .ended-banner {
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
    max-width: min(40rem, 90%);
  }

  .hover-bar {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.7rem 0.9rem;
    background: linear-gradient(transparent, rgba(0, 0, 0, 0.65));
    opacity: 0;
    transition: opacity 140ms ease;
  }
  .popout:hover .hover-bar,
  .hover-bar:focus-within {
    opacity: 1;
  }
  .spacer {
    flex: 1;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    background: rgba(0, 0, 0, 0.55);
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: var(--r-pill);
    padding: 0.22rem 0.6rem;
    font-size: 0.74rem;
    font-weight: 600;
    color: #e7e2fa;
  }
  .chip-dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--ok, #43d17c);
  }
  .chip.ctl {
    color: var(--ok, #43d17c);
  }

  .pill-wrap {
    position: relative;
  }
  .pill {
    border: 1px solid rgba(255, 255, 255, 0.2);
    background: rgba(0, 0, 0, 0.55);
    color: #e7e2fa;
    border-radius: var(--r-pill);
    padding: 0.28rem 0.65rem;
    font-size: 0.74rem;
    font-weight: 600;
    cursor: pointer;
  }
  .pill:hover {
    border-color: var(--accent);
  }
  .pill.tuned {
    border-color: var(--accent);
    color: #fff;
  }
  .pill-menu {
    position: absolute;
    bottom: 2.1rem;
    right: 0;
    display: flex;
    flex-direction: column;
    background: #1a1730;
    border: 1px solid #322c47;
    border-radius: var(--r-md);
    padding: 0.25rem;
    min-width: 7rem;
    box-shadow: var(--shadow-lg);
    z-index: 5;
  }
  /* The Live effective-readout popover is wider than a value list and holds
     a small table rather than a button column. */
  .pill-menu.wide {
    min-width: 16rem;
    padding: 0.5rem 0.6rem;
  }
  .pill-item {
    border: none;
    background: transparent;
    color: #c8c2e0;
    text-align: left;
    border-radius: var(--r-sm);
    padding: 0.32rem 0.55rem;
    font-size: 0.76rem;
    cursor: pointer;
  }
  .pill-item:hover {
    background: #241f38;
    color: #fff;
  }
  .pill-item.sel {
    color: var(--accent-2, #9be3ff);
    font-weight: 700;
  }

  .corner-btn {
    border: 1px solid rgba(255, 255, 255, 0.22);
    background: rgba(0, 0, 0, 0.55);
    color: #fff;
    border-radius: var(--r-sm);
    width: 2.1rem;
    height: 2.1rem;
    font-size: 1.05rem;
    line-height: 1;
    cursor: pointer;
  }
  .corner-btn:hover {
    background: rgba(0, 0, 0, 0.85);
  }
  /* Mouse captured: the cursor is gone from this machine, so the control
     that took it has to be visibly lit — otherwise "where did my pointer
     go" has no answer on screen. */
  .corner-btn.on {
    border-color: var(--accent, #4ade80);
    background: color-mix(in srgb, var(--accent, #4ade80) 28%, rgba(0, 0, 0, 0.75));
  }
</style>
