<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import type { KvmSupportStatus } from "../types";
  let { node }: { node: string } = $props();
  let status = $state<KvmSupportStatus | null>(null);
  let loading = $state(true);
  let busy = $state(false);
  let observedAt = $state(0);
  let now = $state(performance.now());
  let inFlight = false;
  let revision = 0;
  const seconds = $derived(Math.max(0, Math.ceil((status?.approvalRemainingSeconds ?? 0) - (now - observedAt) / 1000)));
  const countdown = $derived(`${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`);
  const number = $derived(status?.supportId.replace(/(\d{3})(?=\d)/g, "$1 "));
  function update(value: KvmSupportStatus | null) {
    status = value;
    observedAt = now = performance.now();
    loading = false;
  }
  async function refresh(background = true) {
    if (inFlight || busy) return;
    inFlight = true;
    const version = revision;
    try { const value = await app.readKvmSupport(node, background); if (version === revision) update(value); }
    catch { if (version === revision) update(null); }
    finally { inFlight = false; }
  }
  async function decide(action: "arm" | "approve" | "deny", technician?: string, sessionId?: string) {
    if (busy) return;
    busy = true;
    revision++;
    try { update(await app.changeKvmSupport(node, action, technician, sessionId)); }
    catch { update(null); app.toast("warn", "Couldn’t update KVM support approval."); }
    finally { busy = false; }
  }
  onMount(() => {
    void refresh(false);
    const poll = setInterval(() => void refresh(), 5000);
    const tick = setInterval(() => now = performance.now(), 1000);
    return () => { clearInterval(poll); clearInterval(tick); revision++; };
  });
</script>

<svelte:window onkeydown={(event) => { if (event.key === "Escape") app.kvmSupportFor = null; }} />
<div class="scrim">
  <div class="card modal support" role="dialog" aria-modal="true" aria-labelledby="kvm-support-title">
    <header><h2 id="kvm-support-title">KVM support · {app.node(node)?.label || "KVM"}</h2><button class="btn ghost small" onclick={() => app.kvmSupportFor = null}>Close</button></header>
    {#if loading}
      <p>Reading support status…</p>
    {:else if !status?.enabled}
      <p class="err">Couldn't read support status. Check that the KVM is online.</p>
      <button class="btn" disabled={busy} onclick={() => void refresh(false)}>Try again</button>
    {:else}
      <div><p>Support number</p><strong class="number">{number || "Starting…"}</strong></div>
      <p>Share this number with your technician, then approve their request below.</p>
      {#if status.approvalRemainingSeconds !== undefined}
        <p>{seconds > 0 ? `Waiting for one request · ${countdown} left` : "Approval window closed"}</p>
        <button class="btn primary" disabled={busy} onclick={() => void decide("arm")}>
          {seconds > 0 ? "Refresh 5-minute window" : "Approve current or next request"}
        </button>
        <p>Wait up to 5 minutes for one support-number request, then grant 3 hours of access. Press again to refresh the wait.</p>
      {:else}
        <p>Update this KVM to enable support-request approvals here.</p>
      {/if}
      {#if status.authorised}<p>A technician has an active access grant.</p>{/if}
      {#each status.pending ?? [] as request (request.technician + request.sessionId)}
        <div class="request">
          <p><strong>{request.agentName || "A technician"}</strong> wants to connect. Verify code <strong>{request.verificationCode}</strong> with your technician.</p>
          <div class="actions">
            <button class="btn primary" disabled={busy} onclick={() => void decide("approve", request.technician, request.sessionId)}>Approve for 3 hours</button>
            <button class="btn" disabled={busy} onclick={() => void decide("deny", request.technician, request.sessionId)}>Decline</button>
          </div>
        </div>
      {/each}
    {/if}
  </div>
</div>

<style>
  .support { width: min(30rem, 100%); padding: 1.4rem; background: var(--surface); border: 1px solid var(--line-strong); border-radius: var(--r-lg); box-shadow: var(--shadow-lg); display: flex; flex-direction: column; gap: 1rem; max-height: 90vh; overflow-y: auto; }
  header, .actions { display: flex; align-items: center; flex-wrap: wrap; gap: 0.5rem; }
  h2 { flex: 1; margin: 0; font-size: 1.2rem; }
  p { margin: 0; color: var(--ink-soft); line-height: 1.5; }
  .number { font-size: 2rem; font-variant-numeric: tabular-nums; color: var(--ink); }
  .request { padding-top: 1rem; border-top: 1px solid var(--line-strong); display: grid; gap: 0.75rem; }
  .err { color: var(--danger); }
</style>
