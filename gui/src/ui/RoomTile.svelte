<script lang="ts">
  // One member's share inside a room panel — their screen or their
  // camera, told apart by the route's media — a lean cousin of the
  // console stage. It always asks the backend to decode (`decode: true`,
  // the console ladder's universal bottom rung), so the tile just blits
  // RGBA / JPEG frames and works in every webview; a member who wants the
  // full-quality path opens a console on the sharer instead.
  //
  // The picture letterboxes like every call app's: `object-fit: contain`
  // inside the tile, black bars top/bottom *or* left/right as the shapes
  // demand, never a stretch and never a crop.
  //
  // When the sharer also turned "share control" on (there's a live input
  // route from this machine's keyboard & mouse to theirs), the tile
  // captures clicks/keys over the picture and sends them down that route.
  //
  // The hover cluster (bottom-right, every video player's corner) pops
  // the share out into its own OS window or takes it fullscreen; a
  // popped tile holds a big "Return video here" in its middle so a
  // stream lost to another monitor is always one click from home.
  import { makeKeyForwarder } from "../input-keys";
  import { makeRelativeMotionForwarder } from "../relative-motion";
  import { app } from "../store.svelte";
  import { clientLog, focusThisWindow, isTauri, sendInput, setMacRelativePointerCapture, toggleWindowFullscreen, watchVideo } from "../tauri";
  import { type InputAction, type MeshNode, type Route } from "../types";

  let { route, member, windowed = false }: { route: Route; member: MeshNode; windowed?: boolean } =
    $props();

  let canvasEl = $state<HTMLCanvasElement | null>(null);
  // The role=application tile. Key forwarding lives on this element; hover and
  // click pin its focus so a room window's keyboard reaches the sharer (a
  // window-level setFocus alone doesn't push document focus into the webview
  // on hover-without-click).
  let tileEl = $state<HTMLElement | null>(null);
  let hasFrame = $state(false);
  // The streamed frame's pixel size — the content box the letterbox math
  // (and pointer normalization) works against.
  let frameW = $state(0);
  let frameH = $state(0);
  // Fullscreen ("theater"): the tile takes the whole window over (CSS),
  // and — when the room has its own OS window — the window goes
  // fullscreen too, so exactly this video fills the screen.
  let theater = $state(false);

  const who = $derived(app.roomWho(member.id));
  // A video route is a camera feed; a display route is a screen share —
  // the badge says which, since both tile identically.
  const isCamera = $derived(route.media === "video");
  const popKey = $derived(`share:${route.id}`);
  const popped = $derived(app.isVideoPopped(popKey));

  // The live route this tile may drive the sharer with, if any —
  // forwarding is live only over a *desktop* picture: coordinates
  // normalize onto the streamed frame, and only a screen share's frame
  // maps back onto the sharer's desktop (a camera tile is a pure
  // viewing surface).
  const controlRoute = $derived(app.controlRouteTo(member.id));
  const controlActive = $derived(!!controlRoute && route.media === "display" && !popped);

  // Room screen sharing is a real remote-control surface when the sharer also
  // opened control. Give it the same explicit relative-input mode as the main
  // console and its popout so shared gameplay is not forced through absolute
  // pointer coordinates.
  let pointerLocked = $state(false);
  let nativePointerLocked = $state(false);
  let relativeMouse = $state(false);
  let pointerLockPending = false;
  function lockChanged() {
    pointerLocked = document.pointerLockElement === tileEl;
    lockedMotion.reset();
    if (pointerLocked && nativePointerLocked) void releaseNativePointerLock(false);
    if (!pointerLocked && !nativePointerLocked) relativeMouse = false;
  }
  async function maybePointerLock(relativeIntent = relativeMouse) {
    const target = tileEl;
    if (
      !relativeIntent ||
      !controlActive ||
      pointerLocked ||
      nativePointerLocked ||
      pointerLockPending ||
      !target
    ) return;
    pointerLockPending = true;
    try {
      target.focus({ preventScroll: true });
      if (!document.hasFocus()) void focusThisWindow();
      if (target !== tileEl || !target.isConnected || !controlActive) return;
      await target.requestPointerLock();
      setTimeout(() => {
        if (relativeMouse && !pointerLocked && !nativePointerLocked) {
          void activateNativePointerLock(target);
        }
      }, 150);
    } catch (error) {
      // Keep the mode armed. A native focus transition can reject the first
      // request; the next click on the shared screen retries it.
      console.warn("pointer lock request failed:", error);
      await activateNativePointerLock(target);
    } finally {
      pointerLockPending = false;
    }
  }
  let nativePointerLockPending = false;
  async function activateNativePointerLock(target = tileEl) {
    if (!target || pointerLocked || nativePointerLocked || nativePointerLockPending) return;
    nativePointerLockPending = true;
    try {
      const rect = target.getBoundingClientRect();
      const captured = await setMacRelativePointerCapture(true, {
        x: rect.left + rect.width / 2,
        y: rect.top + rect.height / 2,
      });
      if (!captured) return;
      if (pointerLocked || !relativeMouse || !controlActive) {
        await setMacRelativePointerCapture(false);
        return;
      }
      nativePointerLocked = true;
      lockedMotion.reset();
      clientLog("[relative-mouse] macOS native pointer capture active in room share");
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
    if (!controlActive) {
      if (pointerLocked) document.exitPointerLock();
      if (nativePointerLocked) void releaseNativePointerLock();
    }
  });

  async function flipTheater() {
    theater = !theater;
    if (windowed) await toggleWindowFullscreen();
  }

  // A popout opening while this tile is fullscreen would strand the
  // window fullscreen behind the Return note — step out first.
  $effect(() => {
    if (popped && theater) void flipTheater();
  });

  function onWindowKey(e: KeyboardEvent) {
    if (nativePointerLocked && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      nativeEscapeHeld = true;
      void releaseNativePointerLock();
      return;
    }
    // Esc leaves fullscreen — unless control is granted, where every key
    // belongs to the far machine (the hover ⛶ exits instead).
    if (theater && !controlActive && e.key === "Escape") {
      e.preventDefault();
      void flipTheater();
    }
  }

  $effect(() => {
    const routeId = route.id;
    // A popped tile stops watching entirely — the popout window owns the
    // frame watch (claims replace each other), and the tile shows the
    // Return button instead.
    if (popped) {
      hasFrame = false;
      return;
    }
    let cancelled = false;
    let unwatch: (() => void) | null = null;
    hasFrame = false;

    // The receive side's mirror of the backend's "expecting frames from X"
    // line: a tile that watches but never reports a first frame is the
    // "waiting for pixels" stall, told apart from "no inbound route here at
    // all" (no tile, so no line) and "backend has frames, webview can't
    // paint" (this fires, the picture still doesn't).
    clientLog(`[room-call] tile: watching ${routeId} (${route.media} from ${member.id})`);

    const paint = (w: number, h: number, draw: (ctx: CanvasRenderingContext2D) => void) => {
      const c = canvasEl;
      if (cancelled || !c) return;
      if (c.width !== w) c.width = w;
      if (c.height !== h) c.height = h;
      const ctx = c.getContext("2d");
      if (!ctx) return;
      draw(ctx);
      const wasFirst = !hasFrame;
      frameW = w;
      frameH = h;
      hasFrame = true;
      if (wasFirst) clientLog(`[room-call] tile: first frame on ${routeId} (${w}×${h})`);
    };

    // JPEG bitmap decodes are async — chain them so frames paint in order.
    let drawChain = Promise.resolve();

    void watchVideo(
      routeId,
      (f) => {
        if (cancelled) return;
        if (f.kind === "raw") {
          if (f.data.byteLength !== f.width * f.height * 4) return;
          const pixels = new Uint8ClampedArray(f.data.buffer, f.data.byteOffset, f.data.byteLength);
          const img = new ImageData(pixels, f.width, f.height);
          paint(f.width, f.height, (ctx) => ctx.putImageData(img, 0, 0));
        } else if (f.kind === "jpeg") {
          const blob = new Blob([f.data], { type: "image/jpeg" });
          drawChain = drawChain.then(async () => {
            if (cancelled) return;
            try {
              const bmp = await createImageBitmap(blob);
              paint(bmp.width, bmp.height, (ctx) => ctx.drawImage(bmp, 0, 0));
              bmp.close();
            } catch {
              // A torn frame decodes as nothing; the next one stands alone.
            }
          });
        }
        // h264 never arrives here — decode: true means the backend hands
        // this tile ready-to-paint frames.
      },
      { decode: true },
    ).then((u) => {
      if (cancelled) u();
      else unwatch = u;
    });

    return () => {
      cancelled = true;
      unwatch?.();
    };
  });

  // ---- control capture (only when the sharer allowed it) ------------

  function send(action: InputAction) {
    if (controlRoute) void sendInput(controlRoute, action);
  }

  function claimFocus() {
    // The KVM rule: with control live, the surface under the mouse is the
    // one your keyboard should reach — claim focus on hover (a no-op when
    // this window already has it), so keys land on the machine you're
    // pointing at even with popouts of other machines open. Pin focus on the
    // tile element too: window-level setFocus alone doesn't reliably push
    // document focus into the webview on a hover-without-click.
    if (document.hasFocus()) return;
    void focusThisWindow();
    tileEl?.focus({ preventScroll: true });
  }

  // Coordinates are normalized 0..1 over the streamed frame's *content
  // box* — the picture inside the letterbox, not the element — exactly
  // like the console stage, so clicks land where they look like they do.
  function norm(e: PointerEvent): { x: number; y: number } | null {
    const c = canvasEl;
    if (!c || !frameW || !frameH) return null;
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

  function onPointerMove(e: PointerEvent) {
    if (!controlActive) return;
    if (pointerLocked || nativePointerLocked) {
      lockedMotion.forward(e, "pointer");
      return;
    }
    claimFocus();
    const p = norm(e);
    if (p) send({ kind: "mouse_move", x: p.x, y: p.y });
  }
  const lockedMotion = makeRelativeMotionForwarder(
    (dx, dy) => {
      send({ kind: "mouse_move_rel", dx, dy });
    },
    () => window.devicePixelRatio,
  );
  function onLockedMouseMove(e: MouseEvent) {
    if (!controlActive || (!pointerLocked && !nativePointerLocked)) return;
    lockedMotion.forward(e, "mouse");
  }
  function onPointerDown(e: PointerEvent) {
    if (!controlActive) return;
    (e.currentTarget as HTMLElement).focus();
    if (relativeMouse) void maybePointerLock(true);
    if (pointerLocked || nativePointerLocked) {
      e.preventDefault();
      send({ kind: "mouse_button", button: e.button, down: true });
      return;
    }
    const p = norm(e);
    if (!p) return;
    send({ kind: "mouse_move", x: p.x, y: p.y });
    send({ kind: "mouse_button", button: e.button, down: true });
  }
  function onPointerUp(e: PointerEvent) {
    if (!controlActive) return;
    if (pointerLocked || nativePointerLocked) e.preventDefault();
    send({ kind: "mouse_button", button: e.button, down: false });
  }
  function onWheel(e: WheelEvent) {
    if (!controlActive) return;
    e.preventDefault();
    send({ kind: "wheel", dx: e.deltaX, dy: e.deltaY });
  }
  // Key forwarding with the bookkeeping combinations need: the physical
  // `code` rides along, auto-repeat stays local, and keys still held when
  // the tile loses focus are lifted in a burst — otherwise the sharer's
  // machine keeps a stuck modifier.
  const keys = makeKeyForwarder(send);

  function onKey(e: KeyboardEvent, down: boolean) {
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
</script>

<svelte:window
  onkeydown={onWindowKey}
  onmousemove={onLockedMouseMove}
  onblur={() => void releaseNativePointerLock()}
/>

<!-- role=application: like the console stage, a screen-share tile with
     control on is a remote-desktop surface — every pointer/key goes to
     the far machine, not this document. The tile is focusable only while
     control is granted, so keys land here and nowhere else. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  bind:this={tileEl}
  class="tile"
  class:driving={controlActive}
  class:theater
  role="application"
  aria-label="{who.who}'s {isCamera ? 'camera' : 'screen'}{who.machine ? ` (${who.machine})` : ''}"
  tabindex={controlActive ? 0 : -1}
  onpointermove={onPointerMove}
  onpointerdown={onPointerDown}
  onpointerup={onPointerUp}
  onwheel={onWheel}
  onkeydown={(e) => onKey(e, true)}
  onkeyup={(e) => onKey(e, false)}
  onblur={() => keys.releaseAll()}
>
  {#if popped}
    <!-- The stream lives in its own window right now; this is its way
         home — findable even when that window is fullscreen elsewhere. -->
    <div class="popped-note">
      <span class="who">{who.who}'s {isCamera ? "camera" : "screen"} is in its own window</span>
      <button class="return-btn" onclick={() => app.askReturnVideo(popKey)}>
        ⤓ Return video here
      </button>
    </div>
  {:else}
    <canvas bind:this={canvasEl} class:waiting={!hasFrame}></canvas>
    {#if !hasFrame}
      <div class="waiting-note">
        <span class="who">{who.who}{#if who.machine}&nbsp;<span class="machine">· {who.machine}</span>{/if}</span>
        <span class="note">waiting for pixels…</span>
      </div>
    {:else}
      <div class="badge">
        {isCamera ? "📷" : "🖥"} <b>{who.who}</b>{#if who.machine}<span class="machine">· {who.machine}</span>{/if}
        {#if controlActive}<span class="ctl" title="They turned control sharing on: click and type here to drive their machine">🕹 you can drive</span>{/if}
      </div>
    {/if}
    <!-- The video player's corner: fullscreen where everyone looks for
         it, popout beside it. Hover-revealed; pointer events stop here so
         a granted control route never sees these clicks. -->
    <div class="corner">
      {#if controlActive}
        <button
          class="corner-btn"
          class:on={pointerLocked || nativePointerLocked}
          title={pointerLocked || nativePointerLocked
            ? "Release the mouse (Esc)"
            : relativeMouse
              ? "Relative mouse is armed: click the screen to capture"
              : "Capture the mouse: relative motion for a fullscreen app or game"}
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
      {#if isTauri() && !theater}
        <button
          class="corner-btn"
          title="Pop this video out into its own window"
          aria-label="Pop out into its own window"
          onpointerdown={(e) => e.stopPropagation()}
          onpointerup={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            app.popOutRoomShare(route, member);
          }}>⧉</button
        >
      {/if}
      <button
        class="corner-btn"
        title={theater ? "Exit fullscreen (Esc)" : "Fullscreen"}
        aria-label={theater ? "Exit fullscreen" : "Fullscreen"}
        onpointerdown={(e) => e.stopPropagation()}
        onpointerup={(e) => e.stopPropagation()}
        onclick={(e) => {
          e.stopPropagation();
          void flipTheater();
        }}>{theater ? "⤡" : "⛶"}</button
      >
    </div>
  {/if}
</div>

<style>
  .tile {
    position: relative;
    background: #000;
    border-radius: var(--r-md);
    overflow: hidden;
    border: 1px solid var(--line-strong);
    min-height: 8rem;
    min-width: 0;
    /* The single track is the tile's size, never the canvas's intrinsic
       one — the letterbox comes from object-fit inside the element. */
    display: grid;
    grid-template-rows: minmax(0, 1fr);
    grid-template-columns: minmax(0, 1fr);
    place-items: center;
  }
  .tile.driving {
    cursor: crosshair;
  }
  .tile.driving:focus {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  canvas {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
  }
  /* Mounted (so the first frame has somewhere to land) but invisible
     until one does — the waiting note shows through. */
  canvas.waiting {
    visibility: hidden;
  }
  .waiting-note {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.25rem;
  }
  .who {
    font-size: 0.85rem;
    font-weight: 650;
  }
  .note {
    font-size: 0.74rem;
    color: var(--ink-faint);
  }
  .badge {
    position: absolute;
    left: 0.5rem;
    bottom: 0.5rem;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(4px);
    border-radius: var(--r-pill);
    padding: 0.18rem 0.55rem;
    font-size: 0.72rem;
    font-weight: 600;
    max-width: calc(100% - 1rem);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .machine {
    color: oklch(0.75 0.02 285);
    font-weight: 500;
  }
  .ctl {
    color: var(--ok);
  }

  /* Fullscreen: this one video takes the whole window over (and the OS
     window itself goes fullscreen when the room has one). */
  .tile.theater {
    position: fixed;
    inset: 0;
    z-index: 120;
    border-radius: 0;
    border: none;
  }

  /* The hover corner — the video player's bottom-right. */
  .corner {
    position: absolute;
    right: 0.5rem;
    bottom: 0.5rem;
    display: inline-flex;
    gap: 0.3rem;
    opacity: 0;
    transition: opacity 120ms ease;
  }
  .tile:hover .corner,
  .corner:focus-within {
    opacity: 1;
  }
  /* Touch has no hover to reveal them with — and an invisible button can't
     take the focus that would. Keep the corner controls visible. */
  @media (hover: none) {
    .corner {
      opacity: 1;
    }
  }
  .corner-btn {
    border: 1px solid rgba(255, 255, 255, 0.22);
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(4px);
    color: #fff;
    border-radius: var(--r-sm);
    width: 1.9rem;
    height: 1.9rem;
    font-size: 0.95rem;
    line-height: 1;
    cursor: pointer;
  }
  .corner-btn:hover {
    background: rgba(0, 0, 0, 0.8);
  }
  .corner-btn.on {
    border-color: var(--accent, #4ade80);
    background: color-mix(in srgb, var(--accent, #4ade80) 28%, rgba(0, 0, 0, 0.75));
  }

  /* The way home for a popped-out stream. */
  .popped-note {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.7rem;
    padding: 1rem;
    text-align: center;
  }
  .return-btn {
    border: 1px solid var(--line-strong);
    background: var(--accent);
    color: #fff;
    border-radius: var(--r-md);
    padding: 0.7rem 1.2rem;
    font-size: 0.95rem;
    font-weight: 700;
    cursor: pointer;
  }
  .return-btn:hover {
    filter: brightness(1.12);
  }
</style>
