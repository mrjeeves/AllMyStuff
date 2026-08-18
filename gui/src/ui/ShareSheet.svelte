<script lang="ts">
  // The permission moment. When a connection touches someone you share
  // with and isn't covered yet, we don't fail silently or dump a security
  // dialog — we ask one friendly question and offer the exact, minimal
  // grant that unblocks it.
  import { app } from "../store.svelte";
  import { mediaColor } from "../types";

  const share = $derived(app.pendingShare);

  const requests = $derived(share?.requests ?? []);
  const who = $derived(requests[0]?.personName ?? "this person");
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && app.dismissPendingShare()} />

{#if share}
  <div class="scrim">
    <button class="backdrop" aria-label="Dismiss" onclick={() => app.dismissPendingShare()}
    ></button>
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Permission needed" tabindex="-1">
      <div class="emoji">🤝</div>
      <h3>Share with {who}?</h3>
      <p class="lead">
        To connect <b>{share.fromLabel}</b> and <b>{share.toLabel}</b>, you need to let
        {who} do this:
      </p>

      <ul class="asks">
        {#each requests as r}
          <li>
            <span class="d" style="background: {mediaColor(r.media)}"></span>
            {r.description}
          </li>
        {/each}
      </ul>

      <p class="fine">
        Only this is shared: nothing else of yours becomes reachable, and you can take
        it back any time.
      </p>

      <div class="actions">
        <button class="btn ghost" onclick={() => app.dismissPendingShare()}> Not now </button>
        <button class="btn primary" onclick={() => app.approvePendingShare()}>
          Allow &amp; connect
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
  }
  .sheet {
    position: relative;
    z-index: 1;
    width: 24rem;
    max-width: 92vw;
    background: var(--surface);
    border-radius: var(--r-lg);
    padding: 1.4rem 1.4rem 1.1rem;
    box-shadow: var(--shadow-lg);
    text-align: center;
    animation: rise 0.16s ease;
  }
  @keyframes rise {
    from {
      transform: translateY(12px) scale(0.98);
      opacity: 0;
    }
  }
  .emoji {
    font-size: 2.2rem;
  }
  h3 {
    margin: 0.4rem 0 0.3rem;
    font-size: 1.2rem;
  }
  .lead {
    color: var(--ink-soft);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 0.8rem;
  }
  .asks {
    list-style: none;
    margin: 0 0 0.8rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    text-align: left;
  }
  .asks li {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    background: var(--accent-soft);
    color: var(--accent-ink);
    border-radius: var(--r-sm);
    padding: 0.55rem 0.7rem;
    font-weight: 600;
    font-size: 0.88rem;
  }
  .d {
    width: 10px;
    height: 10px;
    border-radius: 50%;
  }
  .fine {
    font-size: 0.76rem;
    color: var(--ink-faint);
    line-height: 1.45;
    margin: 0 0 1rem;
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    justify-content: center;
  }
  .actions .btn {
    flex: 1;
  }
</style>
