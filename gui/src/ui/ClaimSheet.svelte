<script lang="ts">
  // The "claim a device" sheet — opened from the top-bar nudge (the claim
  // step's answer to the join approvals popup). Claiming is the thing you do
  // right after joining a network, so it gets the same forefront treatment:
  // a clear list of the devices offering themselves for adoption, a plain
  // explanation of what claiming *does*, and one obvious button each.
  //
  // Claiming is authorization — you're vouching for a machine and linking it
  // into your fleet — so the language here is deliberately the same shape the
  // sharing flow uses, because that's the next thing people reach for.
  import { app } from "../store.svelte";
  import { displayName, humanBytes } from "../types";

  const claimables = $derived(app.claimables);

  function close() {
    app.claimOpen = false;
  }

  // Devices a claim has been sent for this session — the button reads
  // "Asking…" until the device re-advertises as ours and drops off the list
  // (the backend path is a round-trip; demo mode flips it on the spot).
  let asked = $state<string[]>([]);
  function claim(id: string) {
    if (!asked.includes(id)) asked = [...asked, id];
    app.claim(id);
  }

  // Once the last device is adopted, there's nothing left to claim — close,
  // exactly as the join popup folds when the queue empties.
  $effect(() => {
    if (app.claimOpen && claimables.length === 0) close();
  });
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && close()} />

<div class="scrim">
  <button class="backdrop" onclick={close} aria-label="Close"></button>
  <div class="popup" role="dialog" aria-modal="true" aria-label="Devices ready to claim" tabindex="-1">
    <header class="head">
      <span class="mark" aria-hidden="true">＋</span>
      <div class="head-text">
        <div class="title">
          {claimables.length === 1 ? "A device is ready to claim" : `${claimables.length} devices ready to claim`}
        </div>
        <div class="sub">Claiming makes a device yours: it joins your fleet.</div>
      </div>
      <button class="x" onclick={close} aria-label="Close">✕</button>
    </header>

    <p class="lead">
      Your devices recognise each other once they share a fleet. Claim one and
      it's linked to everything else you own under a shared key: your machines
      trust each other for screen, files and control. It's the same kind of
      “yes, I allow this” you'll use to share with people.
    </p>

    <div class="list">
      {#each claimables as n (n.id)}
        {@const pending = asked.includes(n.id)}
        <div class="card" class:pending>
          <span class="avatar" aria-hidden="true">🖥</span>
          <div class="who">
            <div class="who-name">{displayName(n)}</div>
            <div class="who-sub">
              {#if n.summary}{n.summary.os} · {n.summary.cpu} · {humanBytes(n.summary.ram_bytes)}{:else}offering itself for adoption{/if}
            </div>
            {#if n.networks && n.networks.length}
              <div class="nets">{#each n.networks as net}<span class="net-chip">{net}</span>{/each}</div>
            {/if}
          </div>
          <button class="btn small primary claim-btn" disabled={pending} onclick={() => claim(n.id)}>
            {pending ? "Asking…" : "Claim"}
          </button>
        </div>
      {/each}

      {#if claimables.length === 0}
        <p class="empty">Nothing's offering itself right now.</p>
      {/if}
    </div>

    <footer class="foot">
      A device only shows up here when it's started in claim mode (or you toggle
      “offer for adoption” on it). Sharing a machine with someone else works the
      same way: from the device on the graph.
    </footer>
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
    width: 32rem;
    max-width: 94vw;
    max-height: 88vh;
    overflow-y: auto;
    background: var(--surface);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
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
    padding: 1.1rem 1.3rem 0.9rem;
    border-bottom: 1px solid var(--line);
  }
  /* The claim mark mirrors the join popup's "!" bang, but in the brand accent
     — claiming is an additive, welcoming act, not an alert. */
  .mark {
    display: grid;
    place-items: center;
    width: 2rem;
    height: 2rem;
    border-radius: 50%;
    background: var(--accent);
    color: var(--bg);
    font-weight: 800;
    font-size: 1.2rem;
    line-height: 1;
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
    font-size: 0.78rem;
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
  .lead {
    margin: 0;
    padding: 0.9rem 1.3rem 0.2rem;
    font-size: 0.82rem;
    line-height: 1.5;
    color: var(--ink-soft);
  }
  .list {
    padding: 0.7rem 1.3rem 0.4rem;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  .card {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    padding: 0.7rem 0.8rem;
    background: var(--surface);
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .card:hover {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .card.pending {
    opacity: 0.7;
  }
  .avatar {
    font-size: 1.5rem;
    line-height: 1;
    flex-shrink: 0;
  }
  .who {
    flex: 1;
    min-width: 0;
  }
  .who-name {
    font-weight: 650;
    font-size: 0.95rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .who-sub {
    font-size: 0.74rem;
    color: var(--ink-faint);
    margin-top: 0.1rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .nets {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem;
    margin-top: 0.3rem;
  }
  .net-chip {
    font-size: 0.62rem;
    font-weight: 650;
    background: var(--violet-soft);
    border: 1px solid oklch(0.62 0.2 292 / 0.35);
    color: var(--violet);
    border-radius: var(--r-pill);
    padding: 0.02rem 0.4rem;
  }
  .claim-btn {
    flex-shrink: 0;
  }
  .claim-btn:disabled {
    opacity: 0.8;
    cursor: default;
  }
  .empty {
    font-size: 0.84rem;
    color: var(--ink-faint);
    text-align: center;
    padding: 1rem;
  }
  .foot {
    padding: 0.4rem 1.3rem 1.2rem;
    font-size: 0.72rem;
    line-height: 1.45;
    color: var(--ink-faint);
  }
</style>
