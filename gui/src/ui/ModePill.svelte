<script lang="ts">
  // The stream-posture control, shared by the console strip and the
  // popped-out video bar so there is ONE Mode element, not two that drift
  // apart. It is a proper dropdown: the pill names the current posture and
  // opens a menu of all four with one-line descriptions, the active one
  // checked. Balanced (stability-first default), Game (latency-first — GDR,
  // instant recovery), Studio (LAN fidelity), Studio · Lossless (bit-exact
  // HEVC on a capable pair). The Studio flavors warn once about bandwidth
  // before engaging.
  //
  // Presentational + self-contained: the caller owns where the tune lives
  // and how it's applied; this component only decides which posture is next
  // and hands it back through `onapply`. `placement` flips the menu's anchor
  // for the two hosts — the popout bar sits at the window's bottom edge so
  // its menu opens up (default); the console's menu has room below so it
  // opens down — without forking the component.

  type ModeKey = "balanced" | "game" | "studio" | "studio-ll";

  /** The postures in menu order, each with the one-line description the
   *  dropdown shows and whether it's a bandwidth-heavy tier (the Studio
   *  flavors, which gate behind the one-time warning). */
  const MODES: Array<{ key: ModeKey; label: string; blurb: string; heavy: boolean }> = [
    { key: "balanced", label: "Balanced", blurb: "2K/30 target, stability first", heavy: false },
    { key: "game", label: "Game", blurb: "4K/60 target, latency first, GDR healing", heavy: false },
    { key: "studio", label: "Studio", blurb: "Quality first, high bitrate", heavy: true },
    {
      key: "studio-ll",
      label: "Studio · Lossless",
      blurb: "Pixel-exact HEVC, needs a capable pair",
      heavy: true,
    },
  ];
  const MODE_LABEL: Record<ModeKey, string> = {
    balanced: "Balanced",
    game: "Game",
    studio: "Studio",
    "studio-ll": "Studio · LL",
  };

  /** The posture as it rides the wire (matches StreamTune["mode"] minus
   *  the "balanced" alias, which is expressed as undefined). */
  type WireMode = "game" | "studio" | "studio-lossless" | undefined;

  let {
    mode,
    game,
    onapply,
    experimental = false,
    onexperimental,
    placement = "up",
  }: {
    /** The current wire posture ("studio-lossless" for the LL flavor). */
    mode: string | undefined;
    /** Legacy game flag, honored when no named mode is set. */
    game?: boolean;
    /** Apply a chosen posture. `wireMode` is undefined for Balanced,
     *  "studio-lossless" for LL, else the key; `gameFlag` mirrors it for
     *  hosts that predate the tri-state. */
    onapply: (wireMode: WireMode, gameFlag: boolean | undefined) => void;
    /** The Experimental (Labs) tier state — a toggle below the postures,
     *  orthogonal to them (Experimental refines any posture; it is not a
     *  fifth mode). Absent = the toggle isn't shown (a host that doesn't
     *  wire Labs). */
    experimental?: boolean;
    /** Flip the Experimental tier. When absent, the toggle row is hidden. */
    onexperimental?: (on: boolean) => void;
    /** Which way the menu opens off the pill. "up" (default) suits a bar at
     *  the window's bottom edge; "down" suits a menu with room below. */
    placement?: "up" | "down";
  } = $props();

  const STUDIO_ACK_KEY = "ams.studioBandwidthAck";
  let open = $state(false);
  let studioPrompt = $state<ModeKey | null>(null);
  let rootEl = $state<HTMLElement | null>(null);
  let menuEl = $state<HTMLElement | null>(null);

  // Move a node to <body> so its position:fixed resolves against the
  // viewport. Both hosts mount this component inside an element that carries
  // a CSS transform (the console strip / the video bar), and a transformed
  // ancestor becomes the containing block for fixed descendants — which would
  // otherwise trap the full-window scrim inside that ancestor's box.
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return {
      destroy() {
        node.remove();
      },
    };
  }

  const modeKey = (): ModeKey =>
    mode === "studio-lossless"
      ? "studio-ll"
      : ((mode as ModeKey | undefined) ?? (game ? "game" : "balanced"));

  function toWire(next: ModeKey): WireMode {
    return next === "balanced" ? undefined : next === "studio-ll" ? "studio-lossless" : next;
  }
  function apply(next: ModeKey) {
    onapply(toWire(next), next === "game" ? true : undefined);
  }

  /** Pick a posture from the menu: the Studio tiers detour through the
   *  one-time bandwidth warning the first time; everything else applies now. */
  function choose(next: ModeKey) {
    open = false;
    const heavy = next === "studio" || next === "studio-ll";
    if (heavy && localStorage.getItem(STUDIO_ACK_KEY) !== "1") {
      studioPrompt = next;
      return;
    }
    apply(next);
  }
  function confirmStudio(dontAskAgain: boolean) {
    if (dontAskAgain) {
      try {
        localStorage.setItem(STUDIO_ACK_KEY, "1");
      } catch {
        // storage disabled — the pick still applies for this session
      }
    }
    const next = studioPrompt ?? "studio";
    studioPrompt = null;
    apply(next);
  }

  function toggle() {
    open = !open;
  }

  // Focus the active item when the menu opens, so the keyboard lands
  // somewhere sensible and arrow keys rove from there.
  $effect(() => {
    if (!open || !menuEl) return;
    const active = menuEl.querySelector<HTMLElement>('[aria-checked="true"]') ?? menuEl.querySelector<HTMLElement>("button");
    active?.focus({ preventScroll: true });
  });

  // Dismiss the open menu on an outside press, and either surface on Escape.
  // The Studio warning is portaled to <body>; its backdrop owns pointer
  // dismissal below and also stops the host's outside-bar listener. Keeping
  // modal presses out of this capture listener ensures no window-level
  // closer can clear `studioPrompt` before a confirmation click applies it.
  $effect(() => {
    if (!open && !studioPrompt) return;
    const onDown = (e: PointerEvent) => {
      if (studioPrompt) return;
      const t = e.target as Node | null;
      if (t && rootEl?.contains(t)) return;
      open = false;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        open = false;
        studioPrompt = null;
      }
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  });

  // Roving focus across the menu items (proper menu keyboard semantics on
  // top of native button activation).
  function onMenuKey(e: KeyboardEvent) {
    if (!menuEl) return;
    const items = [...menuEl.querySelectorAll<HTMLElement>("button")];
    const i = items.indexOf(document.activeElement as HTMLElement);
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const delta = e.key === "ArrowDown" ? 1 : -1;
      const next = items[(i + delta + items.length) % items.length] ?? items[0];
      next?.focus({ preventScroll: true });
    } else if (e.key === "Home") {
      e.preventDefault();
      items[0]?.focus({ preventScroll: true });
    } else if (e.key === "End") {
      e.preventDefault();
      items[items.length - 1]?.focus({ preventScroll: true });
    }
  }
</script>

<span class="mode-root" bind:this={rootEl}>
  <button
    class="mode-pill"
    class:tuned={modeKey() !== "balanced"}
    class:open
    aria-haspopup="menu"
    aria-expanded={open}
    title="Balanced favors stability and quality; Game favors latency and instant recovery; Studio spends bandwidth on fidelity; Studio · Lossless is bit-exact (a capable pair both ends)"
    onpointerdown={(e) => e.stopPropagation()}
    onpointerup={(e) => e.stopPropagation()}
    onclick={(e) => {
      e.stopPropagation();
      toggle();
    }}
  >
    Mode · {MODE_LABEL[modeKey()]}
    <span class="caret">{open ? "▴" : "▾"}</span>
  </button>

  {#if open}
    <div
      class="mode-menu"
      class:inline={placement === "down"}
      role="menu"
      tabindex="-1"
      aria-label="Stream mode"
      bind:this={menuEl}
      onpointerdown={(e) => e.stopPropagation()}
      onpointerup={(e) => e.stopPropagation()}
      onkeydown={onMenuKey}
    >
      {#each MODES as m (m.key)}
        <button
          class="mode-item"
          class:sel={modeKey() === m.key}
          role="menuitemradio"
          aria-checked={modeKey() === m.key}
          onclick={(e) => {
            e.stopPropagation();
            choose(m.key);
          }}
        >
          <span class="mi-check" aria-hidden="true">{modeKey() === m.key ? "✓" : ""}</span>
          <span class="mi-text">
            <span class="mi-label">{m.label}{#if m.heavy}<span class="mi-tag">bandwidth</span>{/if}</span>
            <span class="mi-blurb">{m.blurb}</span>
          </span>
        </button>
      {/each}
      {#if onexperimental}
        <!-- Experimental is a TOGGLE, not a fifth posture: it refines
             whichever mode is active (Labs field trials — off by default,
             every feature fails soft to today's behavior). A checkbox row
             under a divider, so the Mode control owns it and no separate
             control is ever needed. -->
        <div class="mode-divider" role="separator"></div>
        <button
          class="mode-item"
          class:sel={experimental}
          role="menuitemcheckbox"
          aria-checked={experimental}
          onclick={(e) => {
            e.stopPropagation();
            onexperimental?.(!experimental);
          }}
        >
          <span class="mi-check" aria-hidden="true">{experimental ? "✓" : ""}</span>
          <span class="mi-text">
            <span class="mi-label">Experimental{#if experimental}<span class="mi-tag labs">labs on</span>{/if}</span>
            <span class="mi-blurb">Field-trial speedups: off by default, safe to toggle live</span>
          </span>
        </button>
      {/if}
    </div>
  {/if}
</span>

{#if studioPrompt}
  <div
    class="studio-scrim"
    role="presentation"
    use:portal
    onpointerdown={(e) => {
      // The portaled prompt no longer sits inside Console's bar DOM. Stop
      // its press here so the host's outside-bar listener cannot unmount
      // this component before a confirmation button receives its click.
      e.stopPropagation();
      // Only the bare backdrop cancels. Presses in the dialog survive all
      // the way to `click`, including inside a portaled WebView2 subtree.
      if (e.target === e.currentTarget) studioPrompt = null;
    }}
  >
    <div
      class="studio-dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="studio-title"
      tabindex="-1"
    >
      <h3 id="studio-title">
        {studioPrompt === "studio-ll" ? "Turn on Studio · Lossless?" : "Turn on Studio mode?"}
      </h3>
      {#if studioPrompt === "studio-ll"}
        <p>
          Lossless sends <strong>every pixel exactly</strong> over HEVC :
          bandwidth follows what's on screen: near-zero when idle, tens of
          Mbps for desktop work, and it can spike far higher on busy video.
          It needs capable hardware on both machines; anywhere it can't run,
          the stream continues as regular Studio automatically.
        </p>
      {/if}
      <p>
        Studio streams at maximum fidelity and can use <strong>150 Mbps
        and up</strong>: it's built for a fast local network. It runs
        wherever you turn it on, so on a slow or metered connection expect
        stutter until you lower the Rate.
      </p>
      <div class="studio-actions">
        <button class="studio-btn ghost" onclick={() => (studioPrompt = null)}>Cancel</button>
        <button class="studio-btn ghost" onclick={() => confirmStudio(true)}>Don't ask again</button>
        <button class="studio-btn primary" onclick={() => confirmStudio(false)}
          >{studioPrompt === "studio-ll" ? "Use Lossless" : "Use Studio"}</button
        >
      </div>
    </div>
  </div>
{/if}

<style>
  .mode-root {
    position: relative;
    display: inline-flex;
    flex-direction: column;
    align-items: flex-start;
  }
  .mode-pill {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    border: 1px solid rgba(255, 255, 255, 0.2);
    background: rgba(0, 0, 0, 0.55);
    color: #e7e2fa;
    border-radius: var(--r-pill, 999px);
    padding: 0.28rem 0.65rem;
    font-size: 0.74rem;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }
  .mode-pill:hover,
  .mode-pill.open {
    border-color: var(--accent, #7c6cf0);
  }
  .mode-pill.tuned {
    border-color: var(--accent, #7c6cf0);
    color: #fff;
  }
  .caret {
    font-size: 0.6rem;
    opacity: 0.85;
  }
  .mode-menu {
    position: absolute;
    right: 0;
    bottom: calc(100% + 0.4rem);
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 15rem;
    background: #1a1730;
    border: 1px solid #322c47;
    border-radius: var(--r-md, 10px);
    padding: 0.3rem;
    box-shadow: var(--shadow-lg, 0 12px 40px rgba(0, 0, 0, 0.5));
    z-index: 70;
  }
  /* The console strip's menu is a vertical, scrollable panel: render the
     dropdown IN FLOW (it expands the panel, which scrolls to fit) rather
     than as an absolute popover an `overflow: auto` ancestor would clip. */
  .mode-menu.inline {
    position: static;
    right: auto;
    bottom: auto;
    margin-top: 0.4rem;
    box-shadow: none;
    z-index: auto;
  }
  .mode-item {
    display: flex;
    align-items: flex-start;
    gap: 0.4rem;
    border: 1px solid transparent;
    background: transparent;
    color: #c8c2e0;
    text-align: left;
    border-radius: var(--r-sm, 6px);
    padding: 0.4rem 0.5rem;
    cursor: pointer;
    font: inherit;
  }
  .mode-item:hover,
  .mode-item:focus-visible {
    background: #241f38;
    color: #fff;
    outline: none;
  }
  .mode-item:focus-visible {
    border-color: var(--accent, #7c6cf0);
  }
  .mode-item.sel {
    background: #221d3a;
  }
  .mi-check {
    width: 0.9rem;
    flex: none;
    color: var(--accent-2, #9be3ff);
    font-weight: 700;
    line-height: 1.35;
  }
  .mi-text {
    display: flex;
    flex-direction: column;
    gap: 0.05rem;
    min-width: 0;
  }
  .mi-label {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8rem;
    font-weight: 650;
    color: #e8ebf0;
  }
  .mode-item.sel .mi-label {
    color: #fff;
  }
  .mi-tag {
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.02em;
    text-transform: uppercase;
    color: #f0c674;
    border: 1px solid rgba(240, 198, 116, 0.4);
    border-radius: 999px;
    padding: 0.02rem 0.3rem;
  }
  .mi-tag.labs {
    color: #9be3ff;
    border-color: rgba(155, 227, 255, 0.4);
  }
  .mode-divider {
    height: 1px;
    margin: 0.25rem 0.2rem;
    background: #322c47;
  }
  .mi-blurb {
    font-size: 0.72rem;
    line-height: 1.3;
    color: #9a93b8;
  }
  .studio-scrim {
    position: fixed;
    inset: 0;
    z-index: 80;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(2px);
  }
  .studio-dialog {
    width: min(30rem, calc(100vw - 3rem));
    background: #16181d;
    color: #e8ebf0;
    border: 1px solid #2c323b;
    border-radius: 10px;
    padding: 1.25rem 1.35rem 1.1rem;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
  }
  .studio-dialog h3 {
    margin: 0 0 0.5rem;
    font-size: 1.05rem;
    font-weight: 640;
  }
  .studio-dialog p {
    margin: 0 0 1.1rem;
    font-size: 0.9rem;
    line-height: 1.5;
    color: #b6bdc8;
  }
  .studio-dialog strong {
    color: #e8ebf0;
    font-variant-numeric: tabular-nums;
  }
  .studio-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .studio-btn {
    padding: 0.45rem 0.85rem;
    border-radius: 7px;
    font-size: 0.85rem;
    font-weight: 560;
    cursor: pointer;
    border: 1px solid transparent;
  }
  .studio-btn.ghost {
    background: transparent;
    border-color: #363d47;
    color: #c4cbd6;
  }
  .studio-btn.ghost:hover {
    border-color: #4a525e;
    color: #e8ebf0;
  }
  .studio-btn.primary {
    background: #2f6fab;
    color: #fff;
  }
  .studio-btn.primary:hover {
    background: #3a7cbb;
  }
  .studio-btn:focus-visible {
    outline: 2px solid #5b96cf;
    outline-offset: 2px;
  }
</style>
