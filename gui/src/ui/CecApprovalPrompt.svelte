<script lang="ts">
  import { app } from "../store.svelte";
  import type { CecScope } from "../tauri";
  let busy = $state(false);
  const request = $derived(app.cecRequests[0]);
  async function decide(scope: CecScope | null) {
    if (!request || busy) return;
    const current = request;
    busy = true;
    try {
      if (scope) await app.approveCecRequest(current, scope);
      else await app.denyCecRequest(current);
      await app.loadCec();
    } finally { busy = false; }
  }
</script>

{#if request}
  <div class="scrim">
    <div class="card modal support" role="dialog" aria-modal="true" aria-labelledby="cec-request-title">
      <h2 id="cec-request-title">Support connection request</h2>
      <p><strong>{request.agent_name || "A technician"}</strong> wants to {request.want_control ? "view and control" : "view"} this computer.</p>
      <p>Verify this code with your technician: <strong>{request.verification_code}</strong>.</p>
      <div class="actions">
        <button class="btn primary" disabled={busy} onclick={() => void decide("three_hours")}>Approve for 3 hours</button>
        <button class="btn" disabled={busy} onclick={() => void decide("once")}>Approve once</button>
        <button class="btn" disabled={busy} onclick={() => void decide(null)}>Decline</button>
      </div>
    </div>
  </div>
{/if}
<style>
  .support { width: min(28rem, 100%); padding: 1.4rem; background: var(--surface); border: 1px solid var(--line-strong); border-radius: var(--r-lg); box-shadow: var(--shadow-lg); display: grid; gap: 1rem; }
  h2, p { margin: 0; line-height: 1.5; }
  h2 { font-size: 1.25rem; }
  .actions { display: flex; flex-wrap: wrap; gap: 0.5rem; }
</style>
