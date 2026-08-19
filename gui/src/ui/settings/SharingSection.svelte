<script lang="ts">
  // Sharing pane: every person/fleet you're connected with: which of
  // their machines you can see, and exactly what you've allowed them
  // (grants apply to the *person*, so anything you grant works to any of
  // their devices). Each grant can be taken back one at a time, or the
  // whole connection rescinded.
  import { app } from "../../store.svelte";
  import { displayName, mediaColor, MEDIA } from "../../types";

  const partners = $derived(app.sharePartners);

  /** Which partners are expanded to show their grants/devices. */
  let open = $state<string[]>([]);
  function toggle(personId: string) {
    open = open.includes(personId) ? open.filter((id) => id !== personId) : [...open, personId];
  }

  // Two-step confirm for "stop sharing" (the Fleet pattern).
  let armed = $state<string | null>(null);
  function stopSharing(personId: string) {
    if (armed === personId) {
      armed = null;
      app.stopSharingWith(personId);
    } else {
      armed = personId;
      setTimeout(() => {
        if (armed === personId) armed = null;
      }, 3500);
    }
  }
</script>

<div class="section">
  <h3>Sharing</h3>
  <p class="lead">
    Review what <b>you share with them</b> and what <b>they share with you</b>.
  </p>

  <div class="new-share-row">
    <button class="btn primary new-share" onclick={() => app.openShareFlow()}>
      <span aria-hidden="true">＋</span> New Share
    </button>
  </div>

  {#if partners.length === 0}
    <div class="empty">
      <div class="glyph">🤝</div>
      <p>
        You aren't sharing with anyone yet. Select a device on the graph to start.
      </p>
    </div>
  {:else}
    <ul class="partners">
      {#each partners as p (p.person.id)}
        {@const expanded = open.includes(p.person.id)}
        {@const sharedByYou = p.sharedByYou}
        {@const sharedWithYou = p.sharedWithYou}
        <li class="partner">
          <button class="p-head" onclick={() => toggle(p.person.id)} aria-expanded={expanded}>
            <span class="chev" class:open={expanded} aria-hidden="true">▸</span>
            <span class="p-avatar" aria-hidden="true">🧑</span>
            <span class="p-name">{p.person.name}</span>
            <span class="p-sums">
              <span class="sum">{p.nodes.length} device{p.nodes.length === 1 ? "" : "s"}</span>
              <span class="sum out" class:none={sharedByYou.length === 0}>
                ↑ {sharedByYou.length} shared by you
              </span>
              <span class="sum in" class:none={sharedWithYou.length === 0}>
                ↓ {sharedWithYou.length} shared with you
              </span>
            </span>
          </button>

          {#if expanded}
            <div class="p-body">
              <div class="p-block">
                <h4>Their devices</h4>
                <ul class="nodes">
                  {#each p.nodes as n (n.id)}
                    <li>
                      <span class="n-dot" class:on={n.online}></span>
                      <span class="n-name">{displayName(n)}</span>
                    </li>
                  {/each}
                </ul>
              </div>

              <div class="p-block">
                <h4><span class="direction out">↑</span> You share with them</h4>
                {#if sharedByYou.length === 0}
                  <p class="muted">
                    Nothing from your devices is shared with them.
                  </p>
                {:else}
                  <ul class="grants">
                    {#each sharedByYou as { node: holder, grant: g } (g.id)}
                      <li>
                        <span class="g-dot" style="background: {mediaColor(g.media)}"></span>
                        <span class="g-label">{g.label || `${g.role} ${MEDIA[g.media].label}`}</span>
                        <button
                          class="revoke"
                          title="Take this back"
                          onclick={() => app.revokeGrant(holder.id, g.id)}
                        >✕</button>
                      </li>
                    {/each}
                  </ul>
                {/if}
              </div>

              <div class="p-block incoming">
                <h4><span class="direction in">↓</span> They share with you</h4>
                {#if sharedWithYou.length === 0}
                  <p class="muted">They aren't sharing anything from their devices with you.</p>
                {:else}
                  <ul class="grants inbound">
                    {#each sharedWithYou as { grant: g } (g.id)}
                      <li>
                        <span class="g-dot" style="background: {mediaColor(g.media)}"></span>
                        <span class="g-label">{g.label || `${g.role} ${MEDIA[g.media].label}`}</span>
                        <span class="received">available to you</span>
                      </li>
                    {/each}
                  </ul>
                {/if}
              </div>

              <div class="p-actions">
                <button
                  class="btn small stop"
                  class:armed={armed === p.person.id}
                  title="Rescind the whole connection: every grant goes, and any connection riding one stops"
                  onclick={() => stopSharing(p.person.id)}
                >
                  {armed === p.person.id ? "Stop sharing: sure?" : `Stop sharing with ${p.person.name}`}
                </button>
              </div>
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .section h3 {
    margin: 0 0 0.4rem;
    font-size: 1.15rem;
  }
  .lead {
    color: var(--ink-soft);
    font-size: 0.84rem;
    line-height: 1.5;
    margin: 0 0 0.9rem;
  }
  .new-share-row {
    margin: 0 0 1.1rem;
  }
  /* The builder's entry point: the sharing concept's violet, matching the
     Start Share button inside the flow. */
  .new-share.primary {
    background: linear-gradient(180deg, var(--c-share-ink), var(--c-share));
    border-color: var(--c-share);
    box-shadow: var(--shadow-sm), 0 4px 12px -4px var(--c-share),
      inset 0 1px 0 oklch(1 0 0 / 0.25);
  }
  .empty {
    text-align: center;
    color: var(--ink-faint);
    padding: 2.5rem 1rem;
  }
  .empty .glyph {
    font-size: 2.4rem;
    margin-bottom: 0.4rem;
  }
  .empty p {
    max-width: 24rem;
    margin: 0 auto;
    font-size: 0.86rem;
    line-height: 1.5;
  }
  .partners {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .partner {
    border: 1px solid var(--line);
    border-radius: var(--r-md, 10px);
    overflow: hidden;
  }
  .p-head {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    width: 100%;
    border: none;
    background: var(--surface);
    padding: 0.65rem 0.8rem;
    text-align: left;
    cursor: pointer;
  }
  .p-head:hover {
    background: var(--surface-2);
  }
  .chev {
    font-size: 0.72rem;
    color: var(--ink-faint);
    transition: transform 0.12s ease;
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .p-avatar {
    font-size: 1.15rem;
  }
  .p-name {
    font-weight: 700;
    font-size: 0.92rem;
  }
  .p-sums {
    margin-left: auto;
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 0.35rem;
  }
  .sum {
    font-size: 0.68rem;
    font-weight: 650;
    background: var(--surface-2);
    color: var(--ink-soft);
    border-radius: var(--r-pill);
    padding: 0.12rem 0.5rem;
  }
  .sum.none {
    color: var(--ink-faint);
  }
  .sum.out:not(.none) {
    color: var(--c-share-ink);
    background: var(--c-share-soft);
  }
  .sum.in:not(.none) {
    color: var(--ok);
    background: var(--ok-soft);
  }
  .p-body {
    border-top: 1px solid var(--line);
    padding: 0.7rem 0.9rem 0.8rem;
    background: var(--surface);
  }
  .p-block + .p-block {
    margin-top: 0.7rem;
  }
  h4 {
    margin: 0 0 0.4rem;
    font-size: 0.74rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
    font-weight: 700;
  }
  .direction {
    display: inline-grid;
    place-items: center;
    width: 1.15rem;
    height: 1.15rem;
    margin-right: 0.18rem;
    border-radius: 50%;
    font-size: 0.72rem;
  }
  .direction.out {
    color: var(--c-share-ink);
    background: var(--c-share-soft);
  }
  .direction.in {
    color: var(--ok);
    background: var(--ok-soft);
  }
  .nodes,
  .grants {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .nodes li,
  .grants li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.38rem 0.55rem;
    font-size: 0.82rem;
  }
  .n-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .n-dot.on {
    background: var(--ok);
  }
  .n-name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .g-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .g-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .received {
    flex-shrink: 0;
    color: var(--ok);
    font-size: 0.66rem;
    font-weight: 700;
  }
  .revoke {
    border: none;
    background: transparent;
    color: var(--ink-faint);
    width: 1.4rem;
    height: 1.4rem;
    border-radius: 50%;
    font-size: 0.72rem;
  }
  .revoke:hover {
    background: var(--danger-soft);
    color: var(--danger);
  }
  .muted {
    color: var(--ink-faint);
    font-size: 0.8rem;
    margin: 0.2rem 0 0.3rem;
    line-height: 1.45;
  }
  .p-actions {
    margin-top: 0.8rem;
    display: flex;
    justify-content: flex-end;
  }
  .stop {
    color: var(--danger);
    border-color: oklch(0.7 0.19 14 / 0.4);
  }
  .stop:hover,
  .stop.armed {
    background: var(--danger-soft);
  }
</style>
