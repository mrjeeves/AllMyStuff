<script lang="ts">
  import { app } from "../store.svelte";
</script>

<div class="toasts" aria-live="polite">
  {#each app.toasts as t (t.id)}
    <div class="toast {t.kind}">
      <span class="ic">{t.kind === "ok" ? "✓" : t.kind === "warn" ? "!" : "›"}</span>
      <span class="copy">{t.text}</span>
      {#if t.actions?.length}
        <span class="toast-actions">
          {#each t.actions as action}
            <button
              class:primary={action.primary}
              disabled={app.updateBusy}
              onclick={() => void app.runToastAction(t.id, action.action)}
            >{action.label}</button>
          {/each}
        </span>
      {/if}
      {#if t.persistent}
        <button class="dismiss" aria-label="Dismiss" onclick={() => app.dismissToast(t.id)}>x</button>
      {/if}
    </div>
  {/each}
</div>

<style>
  .toasts {
    position: fixed;
    bottom: 1.2rem;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    z-index: 80;
    align-items: center;
    pointer-events: none;
  }
  .toast {
    background: var(--surface-2);
    border: 1px solid var(--line-strong);
    color: var(--ink);
    padding: 0.5rem 0.9rem;
    border-radius: var(--r-pill);
    font-size: 0.84rem;
    font-weight: 550;
    box-shadow: var(--shadow-lg);
    display: flex;
    align-items: center;
    gap: 0.5rem;
    animation: pop 0.16s ease;
    pointer-events: auto;
    max-width: min(46rem, calc(100vw - 2rem));
  }
  @keyframes pop {
    from {
      transform: translateY(8px);
      opacity: 0;
    }
  }
  .ic {
    display: grid;
    place-items: center;
    width: 1.15rem;
    height: 1.15rem;
    border-radius: 50%;
    font-size: 0.72rem;
    background: rgba(255, 255, 255, 0.12);
  }
  .toast.ok .ic {
    background: var(--ok);
    color: var(--bg);
  }
  .toast.warn .ic {
    background: var(--warn);
    color: var(--bg);
  }
  .copy {
    min-width: 0;
  }
  .toast-actions {
    display: flex;
    gap: 0.35rem;
    margin-left: 0.25rem;
  }
  .toast-actions button,
  .dismiss {
    border: 1px solid var(--line-strong);
    border-radius: var(--r-pill);
    background: transparent;
    color: var(--ink);
    padding: 0.3rem 0.58rem;
    font: inherit;
    cursor: pointer;
    white-space: nowrap;
  }
  .toast-actions button.primary {
    border-color: color-mix(in srgb, var(--ok) 65%, var(--line-strong));
    background: color-mix(in srgb, var(--ok) 18%, transparent);
  }
  .toast-actions button:disabled {
    opacity: 0.55;
    cursor: wait;
  }
  .dismiss {
    border: 0;
    padding: 0.15rem 0.3rem;
    color: var(--ink-faint);
  }
</style>
