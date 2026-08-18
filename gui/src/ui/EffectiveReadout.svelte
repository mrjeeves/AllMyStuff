<script lang="ts">
  // The "what we're actually doing" panel — requested picks on the left, the
  // live effective reality on the right, shared by the console strip and the
  // popped-out bar so the two never drift.
  //
  // Two sources feed the effective column. The VIEWER always knows what's
  // arriving: the decoded frame's real dimensions, the painted frame rate,
  // the wire codec, and the received bitrate — measured right here, true in
  // every topology. When THIS machine is also the route's streamer (a local
  // or room-shared stream), `routeDials` adds the sender's own dials on top:
  // the AIMD bitrate target the Game loop moves at runtime, the posture the
  // encoder actually resolved to, the encoder rung, and the fps/edge targets.
  // Off a remote machine those are the far side's to know and no wire message
  // carries them, so the panel leans on the measured truth and simply omits
  // the sender targets.
  //
  // It polls `routeDials` ~1 Hz for as long as it's mounted — and it's only
  // mounted while its host panel is open, which is exactly the "poll only
  // while open" the backend asks for.
  import { routeDials, type RouteDials, type StreamTune } from "../tauri";

  let {
    routeId,
    requested,
    w,
    h,
    fps,
    transport,
    mbps,
  }: {
    /** The live route this readout describes, or null when nothing streams. */
    routeId: string | null;
    /** The picks the user asked for (the Tune) — the left column. */
    requested: StreamTune;
    /** Decoded frame width/height (0 before the first frame). */
    w: number;
    h: number;
    /** Painted frames per second (viewer truth). */
    fps: number;
    /** Wire transport as the viewer sees it ("H.264" / "MJPEG"). */
    transport: string;
    /** Received bitrate, Mbps (viewer truth). */
    mbps: number;
  } = $props();

  let dials = $state<RouteDials | null>(null);
  // Poll the sender-side dials while mounted. Null (remote sender, or web
  // mode) just leaves the measured column standing on its own.
  $effect(() => {
    const id = routeId;
    if (!id) {
      dials = null;
      return;
    }
    let alive = true;
    const tick = () =>
      void routeDials(id).then((d) => {
        if (alive) dials = d;
      });
    tick();
    const t = setInterval(tick, 1000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  });

  const RES: Record<number, string> = { 3840: "4K", 2560: "1440p", 1920: "1080p", 1280: "720p" };
  const POSTURE: Record<string, string> = {
    balanced: "Balanced",
    game: "Game",
    studio: "Studio",
    "studio-lossless": "Studio · LL",
  };

  const reqRes = $derived(
    requested.maxEdge == null ? "Auto" : (RES[requested.maxEdge] ?? `${requested.maxEdge}px`),
  );
  const reqFps = $derived(requested.fps == null ? "Auto" : `${requested.fps}`);
  const reqRate = $derived(
    requested.bitrate == null ? "Auto" : `${Math.round(requested.bitrate / 1e6)} Mbps`,
  );
  const reqPosture = $derived(
    requested.mode === "studio-lossless"
      ? "Studio · LL"
      : requested.mode
        ? POSTURE[requested.mode]
        : requested.game
          ? "Game"
          : "Balanced",
  );

  const fmtMbps = (m: number) => (m >= 100 ? m.toFixed(0) : m >= 10 ? m.toFixed(1) : m.toFixed(2));
  // The effective posture the encoder resolved to (dials) — differs from the
  // ask when an env override or the LAN gate stepped in.
  const effPosture = $derived(dials ? (POSTURE[dials.posture] ?? dials.posture) : null);
</script>

<div class="eff">
  <div class="eff-head">
    <span>Requested</span>
    <span class="arrow" aria-hidden="true">→</span>
    <span>Effective</span>
  </div>

  <div class="eff-row">
    <span class="k">Resolution</span>
    <span class="req">{reqRes}</span>
    <span class="eff-val">{w && h ? `${w}×${h}` : "Not reported"}</span>
  </div>

  <div class="eff-row">
    <span class="k">Frame rate</span>
    <span class="req">{reqFps}</span>
    <span class="eff-val">
      {fps ? `${fps}/s` : "Not reported"}
      {#if dials && dials.fpsTarget}<span class="sub">target {dials.fpsTarget}</span>{/if}
    </span>
  </div>

  <div class="eff-row">
    <span class="k">Bitrate</span>
    <span class="req">{reqRate}</span>
    <span class="eff-val">
      {mbps ? `${fmtMbps(mbps)} Mbps` : "Not reported"}<span class="sub">received</span>
      {#if dials && dials.targetBitrateBps}
        <span class="sub strong"
          >target {fmtMbps(dials.targetBitrateBps / 1e6)}{#if dials.ceilingBps} · ceiling {fmtMbps(
              dials.ceilingBps / 1e6,
            )}{/if}</span
        >
      {/if}
    </span>
  </div>

  <div class="eff-row">
    <span class="k">Codec</span>
    <span class="req">None</span>
    <span class="eff-val">{dials?.codec || transport || "Not reported"}</span>
  </div>

  <div class="eff-row">
    <span class="k">Mode</span>
    <span class="req">{reqPosture}</span>
    <span class="eff-val">{effPosture ?? reqPosture}</span>
  </div>

  <div class="eff-row">
    <span class="k">Encoder</span>
    <span class="req">None</span>
    <span class="eff-val">{dials?.encoderLabel || "Not reported"}</span>
  </div>

  {#if !dials}
    <div class="eff-note">
      Sender targets (encode bitrate, rung) show when this machine is the
      streamer; a remote stream shows what's arriving here.
    </div>
  {/if}
</div>

<style>
  .eff {
    display: flex;
    flex-direction: column;
    gap: 0.05rem;
    min-width: 15rem;
    font-size: 0.74rem;
    color: #c8c2e0;
  }
  .eff-head {
    display: grid;
    grid-template-columns: 5.5rem 1fr auto;
    gap: 0.4rem;
    align-items: center;
    padding: 0.1rem 0.15rem 0.3rem;
    font-size: 0.66rem;
    font-weight: 700;
    letter-spacing: 0.03em;
    text-transform: uppercase;
    color: #8b83ab;
  }
  .eff-head .arrow {
    justify-self: end;
    color: #6f6790;
  }
  .eff-row {
    display: grid;
    grid-template-columns: 5.5rem 1fr auto;
    gap: 0.4rem;
    align-items: baseline;
    padding: 0.18rem 0.15rem;
    border-top: 1px solid rgba(255, 255, 255, 0.05);
  }
  .k {
    color: #9a93b8;
  }
  .req {
    color: #b6bdc8;
    font-variant-numeric: tabular-nums;
  }
  .eff-val {
    justify-self: end;
    text-align: right;
    color: #e8ebf0;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 0.05rem;
  }
  .sub {
    font-size: 0.64rem;
    font-weight: 500;
    color: #8b83ab;
  }
  .sub.strong {
    color: var(--accent-2, #9be3ff);
    font-weight: 600;
  }
  .eff-note {
    margin-top: 0.35rem;
    padding: 0.3rem 0.2rem 0.1rem;
    font-size: 0.66rem;
    line-height: 1.35;
    color: #7d769c;
    border-top: 1px solid rgba(255, 255, 255, 0.05);
  }
</style>
