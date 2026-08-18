<script lang="ts">
  // "How it works" sheet — the plain-English explainer for the four layers of
  // the model, opened from the top-bar "?" button. It answers the questions
  // people actually ask before they trust the app with their machines, and it
  // does it with one concrete scene (a museum field trip) rather than network
  // jargon: the venue is the place you meet, the mesh is the group name you
  // call out, the fleet is your own backpack of devices, and sharing is lending
  // a friend your umbrella for the day.
  //
  // Three views of the same four concepts are bound together by colour and by
  // hover: the highlighted word in the story, the icon chip in the diagram, and
  // the expanded card all light up together. The cards are fed by the user's
  // real venues / meshes / fleet / shares, so the picture describes *their*
  // setup, not a placeholder.
  import { app } from "../store.svelte";
  import { displayName } from "../types";

  let { onclose }: { onclose: () => void } = $props();

  type Key = "venue" | "mesh" | "fleet" | "share";

  // The hovered concept — set from any of the three surfaces, drives `.lit` on
  // all of them (cross-highlighting). Null = nothing hot.
  let hot = $state<Key | null>(null);

  interface Layer {
    key: Key;
    title: string;
    blurb: string;
  }
  const LAYERS: Layer[] = [
    {
      key: "venue",
      title: "Venue",
      blurb:
        "Where your devices meet: the signaling, STUN & TURN servers. Public by default; a Private Line is a venue that's just yours.",
    },
    {
      key: "mesh",
      title: "Mesh",
      blurb:
        "A name your devices call out. Anyone answering the same name, at the same venue, can find each other.",
    },
    {
      key: "fleet",
      title: "Fleet",
      blurb:
        "Every device you own, riding under your name: private and closed. This machine plus the rest of your own devices.",
    },
    {
      key: "share",
      title: "Sharing",
      blurb:
        "Give one person one thing: any of their devices can use it, and you can take it back whenever you like.",
    },
  ];

  // Real data behind each card, capped so a long list stays a tidy row of
  // chips with a "+N more" tail (the same shape as the screenshots).
  interface Chip {
    label: string;
    tag?: string;
  }
  function cap(items: Chip[], n = 3): { shown: Chip[]; extra: number } {
    return { shown: items.slice(0, n), extra: Math.max(0, items.length - n) };
  }

  const venueChips = $derived(
    cap(app.venues.map((v) => ({ label: v.label, tag: v.builtin ? "public" : "private" }))),
  );
  // Same list the Meshes pane shows — the node-managed CEC Support area is
  // client-support plumbing, not one of "your meshes".
  const meshChips = $derived(
    cap(
      (Array.isArray(app.normalNetworks) ? app.normalNetworks : []).map((n) => ({
        label: app.meshLabel(n),
      })),
    ),
  );
  const fleetChips = $derived(
    cap(
      app.catalog.nodes
        .filter((n) => n.relationship.kind === "mine")
        .map((n) => ({ label: displayName(n), tag: n.kind === "this" ? "this device" : undefined })),
    ),
  );
  const shareChips = $derived(cap(app.sharePartners.map((p) => ({ label: p.person.name }))));

  const chipsFor = $derived<Record<Key, { shown: Chip[]; extra: number }>>({
    venue: venueChips,
    mesh: meshChips,
    fleet: fleetChips,
    share: shareChips,
  });

  // Per-concept tint, set on each element so its accent bar, icon and `.lit`
  // glow all read in that concept's colour.
  function tint(key: Key): string {
    return `--c: var(--c-${key}); --c-soft: var(--c-${key}-soft); --c-ink: var(--c-${key}-ink);`;
  }
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && onclose()} />

<!-- The four line glyphs, in the museum vocabulary: a museum (venue), a bus
     (mesh — the group you ride in together), a backpack (fleet) and an
     umbrella (sharing). Stroke uses currentColor so each inherits its tint. -->
{#snippet glyph(key: Key)}
  {#if key === "venue"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3 9.5 12 4l9 5.5" />
      <path d="M4.5 9.5V18M9 9.5V18M15 9.5V18M19.5 9.5V18" />
      <path d="M3 18h18" /><path d="M2.5 20.5h19" />
    </svg>
  {:else if key === "mesh"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="3.5" y="4.5" width="17" height="12" rx="2.5" />
      <path d="M3.5 9.5h17" /><path d="M8 9.5v7M16 9.5v7" />
      <circle cx="7.5" cy="19" r="1.6" /><circle cx="16.5" cy="19" r="1.6" />
    </svg>
  {:else if key === "fleet"}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M7 8.5a5 5 0 0 1 10 0V20a1.5 1.5 0 0 1-1.5 1.5h-7A1.5 1.5 0 0 1 7 20Z" />
      <path d="M9.5 8.5a2.5 2.5 0 0 1 5 0" />
      <rect x="9.5" y="13" width="5" height="4" rx="1" />
    </svg>
  {:else}
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M12 3v1.5" />
      <path d="M3 12a9 9 0 0 1 18 0c-1.6-1.3-3-1.3-4.5 0-1.5-1.3-3-1.3-4.5 0-1.5-1.3-3-1.3-4.5 0C5.9 10.7 4.6 10.7 3 12Z" />
      <path d="M12 12v6.5a2 2 0 0 1-4 0" />
    </svg>
  {/if}
{/snippet}

<!-- A highlighted concept word in the story, with its glyph marker. Hovering it
     lights the matching chip and card. -->
{#snippet mark(key: Key, label: string)}
  <span
    class="mark"
    class:lit={hot === key}
    style={tint(key)}
    role="button"
    tabindex="0"
    onmouseenter={() => (hot = key)}
    onmouseleave={() => (hot = null)}
    onfocus={() => (hot = key)}
    onblur={() => (hot = null)}
  >
    <span class="mark-i">{@render glyph(key)}</span>{label}
  </span>
{/snippet}

<div class="scrim">
  <button class="backdrop" onclick={onclose} aria-label="Close"></button>
  <div class="popup" role="dialog" aria-modal="true" aria-label="How AllMyStuff connects" tabindex="-1">
    <header class="head">
      <div class="head-text">
        <div class="title">How AllMyStuff connects</div>
        <div class="sub"><i>same group (mesh) + same place (venue) = you can find each other</i></div>
      </div>
      <button class="x" onclick={onclose} aria-label="Close">✕</button>
    </header>

    <div class="body">
      <section class="story">
        <p>
          Picture a school trip to a museum. The museum is the place everyone
          meets: that's the {@render mark("venue", "Venue")} (the signal layer).
          You arrive with your group, all wearing the same lanyard: that's the
          {@render mark("mesh", "Mesh")}: a name your devices call out so they
          find each other at the venue.
        </p>
        <p>
          You carry your own backpack: your {@render mark("fleet", "Fleet")},
          the devices you own, private until you hand something over. And if a
          friend needs your umbrella, you lend it for the day and take it back
          whenever: that's {@render mark("share", "Sharing")}: one person, one
          thing, revocable any time.
        </p>
        <p class="rule">Same group (mesh) + same place (venue) = you can find each other.</p>
      </section>

      <div class="diagram">
        {#each LAYERS as l (l.key)}
          <button
            class="dnode"
            class:lit={hot === l.key}
            style={tint(l.key)}
            onmouseenter={() => (hot = l.key)}
            onmouseleave={() => (hot = null)}
            onfocus={() => (hot = l.key)}
            onblur={() => (hot = null)}
            aria-label={l.title}
          >
            <span class="dchip">{@render glyph(l.key)}</span>
            <span class="dlabel">{l.title}</span>
          </button>
        {/each}
      </div>

      <div class="expanded-head">
        <h3>The four layers, expanded</h3>
        <p>Each piece above, in plain terms: hover any card, chip or highlighted word and the matching trio lights up:</p>
      </div>

      <div class="cards">
        {#each LAYERS as l (l.key)}
          {@const data = chipsFor[l.key]}
          <div
            class="card"
            class:lit={hot === l.key}
            style={tint(l.key)}
            role="group"
            onmouseenter={() => (hot = l.key)}
            onmouseleave={() => (hot = null)}
          >
            <span class="card-icon">{@render glyph(l.key)}</span>
            <div class="card-title">{l.title}</div>
            <p class="card-blurb">{l.blurb}</p>
            <div class="card-chips">
              <!-- Keyed by position, not label: these are a static, capped row
                   of display chips, and labels aren't unique (two meshes/venues
                   can share a name), which would otherwise crash with
                   each_key_duplicate. Index is stable here and always unique. -->
              {#each data.shown as c, i (i)}
                <span class="dchiplet">{c.label}{#if c.tag}<span class="dchiplet-tag"> · {c.tag}</span>{/if}</span>
              {/each}
              {#if data.shown.length === 0}
                <span class="dchiplet empty">none yet</span>
              {/if}
              {#if data.extra > 0}
                <span class="dchiplet more">+{data.extra} more</span>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    </div>
  </div>
</div>

<style>
  .backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
  }
  .popup {
    position: relative;
    z-index: 1;
    width: min(940px, 94vw);
    max-height: 92vh;
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
    animation: rise 0.16s ease;
  }
  @keyframes rise {
    from {
      transform: translateY(12px) scale(0.98);
      opacity: 0;
    }
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    padding: 0.9rem 1.2rem;
    border-bottom: 1px solid var(--line);
    flex-shrink: 0;
  }
  .head-text {
    flex: 1;
    min-width: 0;
  }
  .title {
    font-weight: 750;
    font-size: 1.05rem;
  }
  .sub {
    font-size: 0.8rem;
    color: var(--ink-faint);
    margin-top: 0.1rem;
  }
  .x {
    border: none;
    background: var(--surface-2);
    color: var(--ink-soft);
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .x:hover {
    background: var(--line-strong);
  }
  .body {
    overflow: auto;
    padding: 1.2rem 1.4rem 1.5rem;
  }

  /* ---- story ---- */
  .story {
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    padding: 1rem 1.2rem;
  }
  .story p {
    margin: 0 0 0.7rem;
    font-size: 0.92rem;
    line-height: 1.65;
    color: var(--ink-soft);
  }
  .story p:last-child {
    margin-bottom: 0;
  }
  .rule {
    font-weight: 700;
    color: var(--ink) !important;
    border-top: 1px solid var(--line);
    padding-top: 0.7rem;
    margin-top: 0.2rem !important;
  }
  /* The inline concept word — a glyph marker + the word, tinted in the
     concept's colour, that brightens when its trio is hot. */
  .mark {
    display: inline-flex;
    align-items: center;
    gap: 0.28rem;
    padding: 0.02rem 0.4rem 0.02rem 0.3rem;
    border-radius: var(--r-pill);
    font-weight: 700;
    color: var(--c-ink);
    background: var(--c-soft);
    border: 1px solid transparent;
    cursor: default;
    transition: border-color 0.12s ease, box-shadow 0.12s ease,
      background 0.12s ease;
    white-space: nowrap;
  }
  .mark:hover,
  .mark.lit {
    border-color: var(--c);
    box-shadow: 0 0 0 3px var(--c-soft);
  }
  .mark-i {
    display: inline-flex;
    width: 0.95rem;
    height: 0.95rem;
    color: var(--c);
  }
  .mark-i :global(svg) {
    width: 100%;
    height: 100%;
  }

  /* ---- diagram ---- */
  .diagram {
    position: relative;
    display: flex;
    margin: 1.6rem 0 0.4rem;
  }
  /* The connector ends on the first and last icon centres: with four equal
     columns each icon sits at its column midpoint, so the centres are at 12.5%
     and 87.5% of the row. The chips paint over it, so only the gaps show. */
  .diagram::before {
    content: "";
    position: absolute;
    left: 12.5%;
    right: 12.5%;
    top: 1.7rem;
    border-top: 2px dashed var(--line-strong);
    z-index: 0;
  }
  .dnode {
    flex: 1 1 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    position: relative;
    z-index: 1;
    border: none;
    background: transparent;
    padding: 0;
  }
  .dchip {
    display: grid;
    place-items: center;
    width: 3.4rem;
    height: 3.4rem;
    border-radius: var(--r-md);
    background: var(--surface);
    border: 1px solid var(--line-strong);
    color: var(--c);
    box-shadow: var(--shadow-sm);
    transition: transform 0.12s ease, box-shadow 0.12s ease,
      border-color 0.12s ease, background 0.12s ease;
  }
  .dchip :global(svg) {
    width: 1.7rem;
    height: 1.7rem;
  }
  .dnode:hover .dchip,
  .dnode.lit .dchip {
    background: var(--c-soft);
    border-color: var(--c);
    transform: translateY(-2px);
    box-shadow: var(--shadow-sm), 0 6px 16px -8px var(--c);
  }
  .dlabel {
    font-size: 0.82rem;
    font-weight: 650;
    color: var(--ink-soft);
  }
  .dnode:hover .dlabel,
  .dnode.lit .dlabel {
    color: var(--c-ink);
  }

  /* ---- expanded cards ---- */
  .expanded-head {
    margin: 1.4rem 0 0.7rem;
  }
  .expanded-head h3 {
    margin: 0;
    font-size: 1rem;
    font-weight: 750;
  }
  .expanded-head p {
    margin: 0.25rem 0 0;
    font-size: 0.8rem;
    color: var(--ink-faint);
    line-height: 1.45;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 0.7rem;
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-top: 2.5px solid var(--c);
    border-radius: var(--r-md);
    padding: 0.85rem 0.85rem 0.95rem;
    transition: box-shadow 0.12s ease, border-color 0.12s ease,
      transform 0.12s ease;
  }
  .card.lit {
    box-shadow: 0 0 0 1.5px var(--c), 0 10px 26px -16px var(--c);
    transform: translateY(-2px);
  }
  .card-icon {
    display: grid;
    place-items: center;
    width: 2.1rem;
    height: 2.1rem;
    border-radius: var(--r-sm);
    background: var(--c-soft);
    color: var(--c);
  }
  .card-icon :global(svg) {
    width: 1.3rem;
    height: 1.3rem;
  }
  .card-title {
    font-weight: 750;
    font-size: 0.95rem;
  }
  .card-blurb {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--ink-soft);
    flex: 1;
  }
  .card-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    margin-top: 0.15rem;
  }
  .dchiplet {
    font-size: 0.68rem;
    font-weight: 600;
    color: var(--c-ink);
    background: var(--c-soft);
    border: 1px solid transparent;
    border-radius: var(--r-pill);
    padding: 0.12rem 0.5rem;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dchiplet-tag {
    color: var(--ink-faint);
    font-weight: 500;
  }
  .dchiplet.more,
  .dchiplet.empty {
    color: var(--ink-faint);
    background: var(--surface);
    border-color: var(--line);
  }

  /* Stack to two columns, then one, when the sheet is narrow — equal cards the
     whole way down so the 1:1 mapping to the chips never goes ragged. */
  @media (max-width: 760px) {
    .cards {
      grid-template-columns: repeat(2, 1fr);
    }
  }
  @media (max-width: 460px) {
    .cards {
      grid-template-columns: 1fr;
    }
  }
</style>
